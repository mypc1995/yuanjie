// 分离式 DRM/KMS 捕获路径中的非特权一半：root `--service` 导出一个扫描输出的
// dma-buf fd 及描述符，这一侧负责导入并用 EGL 做去 tiling。在这条路径上，libEGL/libGLESv2 是在
// 非特权进程中 dlopen 的；root 服务只有在回退到
// 它自己的 CPU 映射抓取（`drmtap_grab_mapped`）时才会加载它们。

use super::drmtap_dl::{self, drmtap_ctx, drmtap_dmabuf_desc, drmtap_frame_info, DrmtapLib};
use super::Pixfmt;
use hbb_common::log;
use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;

use super::drm_reader::{
    DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888,
    MAX_DIM, MAX_FRAME_BYTES,
};

/// 非特权的 DRM 渲染节点转换上下文。通过原始 ctx 指针实现 !Send/!Sync：该上下文
/// 以及 libdrmtap 的线程局部 EGL 状态必须在同一个线程上创建、使用（`convert`）并关闭。
pub struct RenderConverter {
    lib: &'static DrmtapLib,
    ctx: *mut drmtap_ctx,
}

impl RenderConverter {
    /// `node` 是导出扫描输出的那个 GPU 的渲染节点；为 `None` 或路径无效时回退到 libdrmtap 的自动选择。
    pub fn open_render(node: Option<&str>) -> Option<RenderConverter> {
        let lib = drmtap_dl::get()?;
        let open_render = lib.open_render;
        let node_cstr = match node.filter(|n| !n.is_empty()) {
            None => None,
            // 打开门控解析出的规范路径：打开 IPC 字符串会在检查之后重新遍历它的符号链接。
            Some(n) => match super::drm_reader::device_under_dev_dri(n) {
                None => {
                    log::warn!("drm: render node {n:?} is not under /dev/dri; auto-selecting");
                    None
                }
                Some(canonical) => canonical.to_str().and_then(|s| CString::new(s).ok()),
            },
        };
        // SAFETY：已解析的 C 入口点；`node_cstr` 的生命周期长于本次调用，传 NULL 表示请求自动选择。
        let ctx = unsafe {
            open_render(node_cstr.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()))
        };
        if ctx.is_null() {
            log::info!(
                "drmtap_open_render({}) failed; no usable DRM render node",
                node_cstr.as_ref().map_or("NULL".to_owned(), |c| format!("{c:?}"))
            );
            return None;
        }
        match node_cstr {
            Some(c) => log::info!(
                "drm: opened unprivileged convert context on the exporting GPU ({c:?})"
            ),
            None => log::info!(
                "drm: opened unprivileged render-node convert context (auto-selected)"
            ),
        }
        Some(RenderConverter { lib, ctx })
    }

    /// 返回归上下文所有的线性像素，仅在下一次 `convert()` 之前有效；行跨度为 `len / height`。
    pub fn convert(
        &mut self,
        desc: &mut drmtap_dmabuf_desc,
        received_fd: RawFd,
    ) -> io::Result<(&[u8], u32, u32, Pixfmt)> {
        {
            let (w, h) = (desc.width, desc.height);
            if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("drm: refusing a dma-buf descriptor with geometry {w}x{h}"),
                ));
            }
            // 这里是拒绝而不是截断，并把归一化后的数量写回去，让 C 侧读到的数量受这里的边界约束。
            let planes = if desc.num_planes == 0 { 1 } else { desc.num_planes };
            if planes > 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "drm: refusing a dma-buf descriptor with num_planes {} (1..=4)",
                        desc.num_planes
                    ),
                ));
            }
            desc.num_planes = planes;
            let planes = planes as usize;
            for p in 0..planes {
                let extent = (desc.pitches[p] as usize)
                    .checked_mul(h as usize)
                    .and_then(|rows| rows.checked_add(desc.offsets[p] as usize));
                match extent {
                    Some(end) if end <= MAX_FRAME_BYTES => {}
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "drm: refusing dma-buf plane {p} (offset {} pitch {} over {h} rows -> {other:?}, cap {MAX_FRAME_BYTES})",
                                desc.offsets[p], desc.pitches[p]
                            ),
                        ));
                    }
                }
            }
        }
        let convert_dmabuf = self.lib.convert_dmabuf;
        // 关键点：导出方序列化出去的 fd 原本是进程本地的；-1 表示复用为 `fb_id` 缓存的导入句柄。
        desc.dma_buf_fd = received_fd;
        // SAFETY：self.ctx 是有效的渲染上下文；`desc` 已完全初始化；`frame` 在
        // 调用前已清零。`frame.data` 归 libdrmtap 所有：这一侧不做释放/回收（见 drmtap.h）。
        unsafe {
            let mut frame: drmtap_frame_info = std::mem::zeroed();
            let ret = convert_dmabuf(self.ctx, &*desc as *const drmtap_dmabuf_desc, &mut frame);
            if ret < 0 {
                let errno = -ret;
                if errno == hbb_common::libc::EAGAIN
                    || errno == hbb_common::libc::EBUSY
                    || errno == hbb_common::libc::EINTR
                {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("drmtap_convert_dmabuf failed: errno {errno}"),
                ));
            }
            if frame.data.is_null() || frame.width == 0 || frame.height == 0 || frame.stride == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "drmtap_convert_dmabuf produced an empty frame",
                ));
            }
            let w = frame.width;
            let h = frame.height;
            let stride = frame.stride as usize;
            // 低于 32bpp 的跨度会让行缓冲区偏小，一旦在下游被当作 BGRA 读取，就会泄露相邻内存。
            if w > MAX_DIM || h > MAX_DIM || stride < (w as usize) * 4 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "drmtap_convert_dmabuf bad geometry {w}x{h} stride {stride} fourcc {:#010x}",
                        frame.format
                    ),
                ));
            }
            let len = match stride.checked_mul(h as usize) {
                Some(sz) if sz > 0 && sz <= MAX_FRAME_BYTES => sz,
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("drmtap_convert_dmabuf frame size out of range ({other:?} bytes)"),
                    ));
                }
            };
            let pixfmt = match frame.format {
                DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => Pixfmt::BGRA,
                DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => Pixfmt::RGBA,
                // 较旧的 convert 未设置该项 -> 使用 libdrmtap 归一化后的 BGRA。
                0 => Pixfmt::BGRA,
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("drmtap_convert_dmabuf produced an unsupported output fourcc {other:#010x}"),
                    ));
                }
            };
            let data = std::slice::from_raw_parts(frame.data as *const u8, len);
            Ok((data, w, h, pixfmt))
        }
    }
}

impl Drop for RenderConverter {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY：ctx 来自 drmtap_open_render，非空；!Send 的 ctx 指针保证
            // 这次 drop 发生在创建并使用它的那个线程上（线程局部 EGL + 缓存的导入句柄）。
            unsafe { (self.lib.close)(self.ctx) };
            self.ctx = std::ptr::null_mut();
        }
    }
}
