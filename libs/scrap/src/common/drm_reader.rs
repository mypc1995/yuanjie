// 服务端 DRM/KMS 读取引擎，位于 ROOT `--service` 中：libdrmtap 在进程内读取扫描输出（direct 模式）。这里不会读取 DRM_DEVICE 环境变量。

use super::drmtap_dl::{
    self, drmtap_config, drmtap_ctx, drmtap_cursor_info, drmtap_device, drmtap_display,
    drmtap_dmabuf_desc, drmtap_frame_info, DrmtapLib,
};
use hbb_common::log;
use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::raw::c_int;

// `drm_render`（非特权转换端）导入的信任边界限制与格式：两份副本一旦漂移不一致，就会削弱其中一侧。
// 16384 足以覆盖 8K+ 并留有余量；更大的值会被当作伪造/恶意几何参数拒绝。
pub(crate) const MAX_DIM: u32 = 16384;
// 256 MiB 足以容纳一帧 8K BGRA 图像（7680x4320x4 ~= 127 MiB）并留有余量。
pub(crate) const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
// XRGB/ARGB 在内存中是小端的 B,G,R,{X,A}，等价于 `Pixfmt::BGRA`；XBGR/ABGR 是 R,G,B,{X,A}，等价于 `Pixfmt::RGBA`。
pub(crate) const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258; // 'XR24'
pub(crate) const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241; // 'AR24'
pub(crate) const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258; // 'XB24'
pub(crate) const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241; // 'AB24'

/// 当图层报告光标被隐藏时发布的光标 id，这样 id 会变化；在以 DRM 光标为准的场景下，客户端就会丢弃上一次的形状。
pub const HIDDEN_CURSOR_ID: u64 = u64::MAX;

pub struct CursorSnapshot {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hotx: i32,
    pub hoty: i32,
    /// 热点是否来自图层的 HOTSPOT_X/Y 属性。这些属性只存在于
    /// 半虚拟化驱动上；在裸机上它是 `infer_hotspot` 的猜测，依据的是
    /// 按扫描输出时的位图——在旋转的输出上，这是合成器预先旋转过的精灵图，
    /// 因此消费端必须在转正后的图上重新猜测。
    pub hot_measured: bool,
    pub colors: Vec<u8>,
    /// 光标图层位置（CRTC_X/CRTC_Y），以本读取器的扫描输出像素为单位。
    pub x: i32,
    pub y: i32,
}

/// 把一切使光标成为“不同光标”的因素都折算进它的 id。
///
/// 生产端通过把它和上次发出的那个比较来去重，因此任何被漏掉的东西都是
/// 消费端永远不会被告知的变化。几何尺寸和热点是
/// 显而易见的两个：同样的像素但尺寸或热点变了，就是一个新形状。
///
/// `hot_measured` 也应包含在内，这一点没那么显然。它并不是关于光标的元数据，
/// 而是决定消费端要“怎么做”：false 表示旋转位图并重新推断
/// 热点，true 表示映射给定的点。因此，一个像素和 `(0, 0)` 热点都
/// 未变、但来源发生了翻转的采样，对客户端来说就是不同的光标，
/// 如果不计入这一点，它就会被去重掉，永远不会被发送。
fn cursor_id(hash: u64, width: u32, height: u32, hotx: i32, hoty: i32, hot_measured: bool) -> u64 {
    let mut id = hash;
    for v in [
        width as u64,
        height as u64,
        hotx as u32 as u64,
        hoty as u32 as u64,
        hot_measured as u64,
    ] {
        id ^= v;
        id = id.wrapping_mul(1099511628211);
    }
    id
}

/// 编码一个来源判定结果，供下面的一次性日志使用。-1 表示“尚未上报任何内容”。
fn provenance_code(answered: Option<bool>) -> i8 {
    match answered {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }
}

/// 考虑到上次上报的内容，本次采样的来源是否值得记一行日志。
///
/// 单独写成函数，因为显而易见的写法是每次读光标都记一行，而光标被
/// 读取的频率远高于它变化的频率。这里按采样记录日志曾是 libdrmtap 的一个真实缺陷，原因
/// 完全相同。所以：每轮运行的第一个采样记一次，之后只在状态真正变化时才记。
fn provenance_is_news(last: i8, now: i8) -> bool {
    last != now
}

impl DrmReader {
    /// 把热点的来源说一次，此后仅在变化时再说。没有这个机制，
    /// 在运行中的机器上这三种状态无法区分，这会让该判定在任何地方都无法验证，
    /// 除了单元测试之外都无法验证。
    fn note_hotspot_provenance(&mut self, answered: Option<bool>, hot_measured: bool) {
        let now = provenance_code(answered);
        let last = std::mem::replace(&mut self.last_provenance, now);
        if !provenance_is_news(last, now) {
            return;
        }
        match answered {
            Some(true) => log::info!(
                "drm: cursor hotspot provenance: the driver published HOTSPOT_X/Y, using its value"
            ),
            Some(false) => log::info!(
                "drm: cursor hotspot provenance: the plane exposes no HOTSPOT_X/Y, inferring the \
                 hotspot from the bitmap"
            ),
            None => log::info!(
                "drm: cursor hotspot provenance: unavailable (libdrmtap older than 0.5.6, or a \
                 pre-0.5.6 privileged helper); falling back to the coordinate heuristic, which says \
                 hot_measured={hot_measured}"
            ),
        }
    }
}

/// `hot_x`/`hot_y` 是驱动给出的答案而不是猜测。
///
/// `answered` 是 `drmtap_cursor_hotspot_valid` 的返回值：给出答案时为 `Some(v)`，没有答案时为 `None`，当
/// 该符号不存在（库版本低于 0.5.6），或者它报告说没有任何记录给出
/// 本次采样的答案时。
///
/// 只要它给出了答案，那就是答案，不容置疑——包括热点为 `(0, 0)` 时的 `Some(true)`，
/// 这正是这个入口存在的全部理由：半虚拟化驱动确实可能把
/// 热点放在左上角，而仅凭坐标无法把它和一个
/// 完全不暴露 HOTSPOT_X/Y 的图层区分开来。当它无法回答时，回退到旧的判定。该
/// 判定两个方向都可能是错的，但这是这段代码以前的做法，而且它能让已部署的
/// 旧版 `.so` 保持原有行为，而不是在其脚下悄悄改变。
fn hot_measured_from(answered: Option<bool>, hot_x: i32, hot_y: i32) -> bool {
    match answered {
        Some(measured) => measured,
        None => hot_x != 0 || hot_y != 0,
    }
}

/// 镜像在并集的每 100 个像素中要有这么多个一致，才算对称。
/// 在两个主题版本上与 master 做过全量比对：低于 86 时，该规则会开始移动那些
/// 角落猜测本来就已经正确（`grabbing`）的形状；而从 92 起，旧主题的
/// 方向型光标不再被识别并出现回退。86 到 91 是两边都成立的区间，
/// 所以这个取值就放在区间中间。
const MIRROR_AGREEMENT_PERCENT: u64 = 88;

/// 为不发布热点的驱动猜测光标位图的点击点。
///
/// 它由三个事实驱动，按各自能读取多少形状信息排序。
///
/// 关于盒子某条中线镜像对称的形状，在该轴上没有可区分的一端：
/// 比如十字准星、I 型光标、双头缩放箭头。在这样的轴上，点击点
/// 就是中心。
///
/// 只有绕半圈旋转或绕对角线镜像才能映射到自身的形状，是双头或
/// 角向缩放光标。它们由一个指针加上被缩放边的标记组成，而
/// 主题把点击点放在那个标记上，也就是较重的那一端。较重的一端同时也是
/// 区分 `e-resize` 和 `w-resize` 的唯一依据：它们是同一张图的镜像。
///
/// 其他所有情况都保留不透明盒子的左上角，因为光标是朝
/// 上和左绘制的，所以那个角就是箭头尖端所在的位置。这是关于
/// 精灵图在自身正立坐标系中的事实，也是这个猜测只有当它是在
/// 该处取得、而不是在合成器预先旋转过的精灵图上取得时才有意义的原因。
///
/// 以主题声明的热点为基准测量，也就是合成器在属性存在时写入
/// HOTSPOT_X/Y 的值；并与 master 在 master 所处理的角度
/// （0、90、270）下给出的结果对比。adwaita-icon-theme 50 在 24 px 下：每个形状的平均误差 12.08 px
/// 降到 4.20，在 105 个（形状、角度）组合中有 83 个更好、4 个更差。
/// 在 48 px 以及 adwaita-icon-theme 46 上结论相同。
///
/// 有两个形状比 master 更靠外，且没有任何位图规则能覆盖它们：`help`，其点击
/// 点被主题放在问号的小圆点上；以及 `alias`，差了半个像素。这就是为什么
/// 驱动发布的热点总是优先于这里的猜测，也是 rustdesk#16122 为什么
/// 能精确地从注入的指针中恢复它而不是靠猜。
pub fn infer_hotspot(rgba: &[u8], w: usize, h: usize) -> (i32, i32) {
    let (mut minx, mut miny, mut maxx, mut maxy) = (w as i32, h as i32, -1i32, -1i32);
    for (i, px) in rgba.chunks_exact(4).take(w * h).enumerate() {
        if px[3] >= 128 {
            let (x, y) = ((i % w) as i32, (i / w) as i32);
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
    }
    if maxx < minx || maxy < miny {
        return (0, 0);
    }
    let (bw, bh) = (maxx - minx + 1, maxy - miny + 1);
    let (cx, cy) = ((minx + maxx) / 2, (miny + maxy) / 2);
    // 细长的盒子无论朝向如何都保留其中心（rustdesk#16242）：横条没有
    // 可谈的角。放在镜像判定之前，这样它仍然能覆盖那些
    // 并不对称的细长形状——主题里恰好没有，但自定义光标可能有。
    if bh > bw * 2 || bw > bh * 2 {
        return (cx, cy);
    }
    let opaque = |x: i32, y: i32| {
        rgba.get((y as usize * w + x as usize) * 4 + 3)
            .is_some_and(|&a| a >= 128)
    };
    let agrees = |i: u64, u: u64| u == 0 || i * 100 >= u * MIRROR_AGREEMENT_PERCENT;
    // 对不透明盒子做一遍扫描，与它自身的三个镜像比较，同时统计每个
    // 半边的像素量。
    let (mut ix, mut ux) = (0u64, 0u64);
    let (mut iy, mut uy) = (0u64, 0u64);
    let (mut ih, mut uh) = (0u64, 0u64);
    let (mut left, mut right, mut top, mut bottom) = (0u64, 0u64, 0u64, 0u64);
    let (half_w, half_h) = (bw / 2, bh / 2);
    for y in miny..=maxy {
        for x in minx..=maxx {
            let (fx, fy) = (maxx - (x - minx), maxy - (y - miny));
            let here = opaque(x, y);
            for (other, (i, u)) in [
                (opaque(fx, y), (&mut ix, &mut ux)),
                (opaque(x, fy), (&mut iy, &mut uy)),
                (opaque(fx, fy), (&mut ih, &mut uh)),
            ] {
                * i += u64::from(here && other);
                * u += u64::from(here || other);
            }
            if here {
                if x - minx < half_w {
                    left += 1;
                } else if maxx - x < half_w {
                    right += 1;
                }
                if y - miny < half_h {
                    top += 1;
                } else if maxy - y < half_h {
                    bottom += 1;
                }
            }
        }
    }
    // 在半圈旋转下对称，且中心是形状唯一无法与
    // 自身区分的点，无论各轴判定结果如何：双头对角箭头在每个轴上单独看都镜像到
    // 另一条对角线，所以两个轴都不读作对称。
    if agrees(ih, uh) {
        return (cx, cy);
    }
    let diagonal = mirrors_about_a_diagonal(&opaque, minx, miny, bw, bh, &agrees);
    // 当形状是一个指针加上一个边缘标记（而不是单纯的
    // 箭头）时，该轴是方向性的：垂直于它的轴镜像对称（n/e/s/w 缩放光标），或者形状绕
    // 对角线镜像（角向的那几个）。标记是点击点所属的那一端，
    // 也是较重的一端，这是区分 `e-resize` 和 `w-resize` 的唯一依据——它们
    // 是同一张图的镜像。其他情况都保留左上角。
    let side = |heavier_is_max: bool, lo: i32, hi: i32| if heavier_is_max { hi } else { lo };
    (
        if agrees(ix, ux) {
            cx
        } else if agrees(iy, uy) || diagonal {
            side(right > left, minx, maxx)
        } else {
            minx
        },
        if agrees(iy, uy) {
            cy
        } else if agrees(ix, ux) || diagonal {
            side(bottom > top, miny, maxy)
        } else {
            miny
        },
    )
}

/// 不透明盒子绕其任意一条对角线镜像时是否映射到自身。
///
/// `bw x bh` 盒子沿对角线反射后是 `bh x bw` 盒子，所以两者只能
/// 在一个正方形内比较；而当盒子不是正方形时，答案取决于它在该正方形中的
/// 摆放位置：角向缩放光标是贴着某一个角绘制的，而从那个角
/// 方向增加留白会让形状偏离它所对称的那条对角线。`ne-resize` 在贴左上角时得分 0.62，
/// 贴右下角时得分 0.98；`nw-resize` 则正好相反。因此两种贴靠方式
/// 都会尝试，取最优结果——一旦允许把反射
/// 平移回原位，“形状是对称的”这句话就是这个意思。
fn mirrors_about_a_diagonal(
    opaque: &impl Fn(i32, i32) -> bool,
    minx: i32,
    miny: i32,
    bw: i32,
    bh: i32,
    agrees: &impl Fn(u64, u64) -> bool,
) -> bool {
    let k = bw.max(bh);
    for (dx, dy) in [(0, 0), (k - bw, k - bh)] {
        // 盒子放在 k x k 正方形内的 (dx, dy) 处，其余位置为空。
        let cell = |r: i32, c: i32| {
            let (x, y) = (c - dx, r - dy);
            (0..bw).contains(&x) && (0..bh).contains(&y) && opaque(minx + x, miny + y)
        };
        let (mut id, mut ud, mut ia, mut ua) = (0u64, 0u64, 0u64, 0u64);
        for r in 0..k {
            for c in 0..k {
                let here = cell(r, c);
                for (other, (i, u)) in [
                    (cell(c, r), (&mut id, &mut ud)),
                    (cell(k - 1 - c, k - 1 - r), (&mut ia, &mut ua)),
                ] {
                    * i += u64::from(here && other);
                    * u += u64::from(here || other);
                }
            }
        }
        if agrees(id, ud) || agrees(ia, ua) {
            return true;
        }
    }
    false
}

/// 一个枚举到的 DRM 显示器，仅包含物理几何信息（服务端在能够匹配上时叠加 Wayland 的逻辑原点/缩放）。
pub struct DisplaySnapshot {
    pub name: String,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// 内核返回的整赫兹垂直刷新率：59.94 会被读成 60。`DrmReader::crtc_refresh` 才是精确值。
    pub refresh_hz: u32,
    pub active: bool,
}

pub struct DrmDevice {
    pub path: String,
    /// 渲染节点（render node），如果该设备没有则为空。
    pub render_node: String,
    pub display_count: u32,
}

/// 把定长的 C 字符数组复制到 `String` 中，在数组内部的第一个 NUL 处停止，这样
/// 某个 libdrmtap 未正确终止的字段就不会越界读取。
fn cstr_field(buf: &[std::os::raw::c_char]) -> String {
    // SAFETY：c_char 与 u8 的大小/对齐方式相同；切片长度与 `buf` 完全一致。
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// 枚举所有带 KMS 资源的 DRM 设备。`None` 表示不可用、版本过旧或失败（此时由调用方自行扫描 /dev/dri/card*）；空的 `Vec` 表示没有找到任何设备。
pub fn list_devices() -> Option<Vec<DrmDevice>> {
    let lib = drmtap_dl::get()?;
    let f = lib.list_devices?;
    const MAX: usize = 16;
    let mut raw: [drmtap_device; MAX] = unsafe { std::mem::zeroed() };
    // SAFETY：`raw` 是 MAX 个有效且已清零的 drmtap_device 槽位；调用最多填充 MAX 个并返回数量。
    let n = unsafe { f(raw.as_mut_ptr(), MAX as std::os::raw::c_int) };
    if n < 0 {
        log::warn!("drmtap_list_devices failed ({n}); using single-device auto-detect");
        return None;
    }
    let n = (n as usize).min(MAX);
    Some(
        raw[..n]
            .iter()
            .map(|d| DrmDevice {
                path: cstr_field(&d.path),
                render_node: cstr_field(&d.render_node),
                display_count: d.display_count,
            })
            .collect(),
    )
}

/// 规范化后的路径——当 `path` 规范化后指向 /dev/dri/ 下的节点时返回它，否则为 `None`。
/// 调用方必须打开这里返回的值：打开原始路径会在检查之后重新解析每一个符号链接分量。
pub(super) fn device_under_dev_dri(path: &str) -> Option<std::path::PathBuf> {
    let p = std::fs::canonicalize(path).ok()?;
    if p.parent() == Some(std::path::Path::new("/dev/dri")) {
        Some(p)
    } else {
        None
    }
}

/// 一个已打开的 DRM 读取上下文。故意不实现 Send/Sync（原始 ctx 只在一个线程上使用）。
pub struct DrmReader {
    lib: &'static DrmtapLib,
    ctx: *mut drmtap_ctx,
    buf: Vec<u8>,
    /// 本读取器上一次上报的来源，用于一次性日志。按读取器记录而非全局记录：
    /// 多个读取器会同时运行，每个被捕获的显示器一个，它们的来源完全可能
    /// 不同而且各自稳定——同一台机器上 virtio-gpu 的输出会发布 HOTSPOT_X/Y，而 i915 的不会。
    /// 用一个全局变量就会把这两路稳定的数据流读成无休止的
    /// 来回交替，从而每个采样都记日志——而这正是这里要避免的逐帧日志。
    /// -1 表示“尚未上报任何内容”。
    last_provenance: i8,
    /// 上一次成功抓取之后、在任何拷贝之前读取的图层旋转值：libdrmtap
    /// 是被问到时才读取该属性的，所以在拷贝之后再读可能描述的是下一帧。
    last_plane_rotation: Option<u32>,
}

impl DrmReader {
    /// 打开 DRM 设备。`device = None` 表示自动探测，`Some(path)` 会用 realpath 限定在 /dev/dri/ 之下。`crtc_id = 0` 表示自动选择第一个活动的 CRTC。
    pub fn open(device: Option<&str>, crtc_id: u32) -> Option<DrmReader> {
        let lib = drmtap_dl::get()?;
        let device_cstr = match device {
            None => None,
            Some(d) => {
                let Some(canonical) = device_under_dev_dri(d) else {
                    log::warn!("DRM device {d:?} is not under /dev/dri; refusing to open");
                    return None;
                };
                match canonical.to_str().and_then(|s| CString::new(s).ok()) {
                    Some(c) => Some(c),
                    None => return None,
                }
            }
        };
        let cfg = drmtap_config {
            device_path: device_cstr.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            crtc_id,
            helper_path: std::ptr::null(),
            debug: 0,
        };
        // SAFETY：cfg 是有效的结构体；device_cstr 的生命周期长于本次调用。
        let ctx = unsafe { (lib.open)(&cfg) };
        drop(device_cstr);
        if ctx.is_null() {
            log::info!("drmtap_open failed; DRM capture unavailable");
            return None;
        }
        Some(DrmReader {
            last_provenance: -1,
            lib,
            ctx,
            buf: Vec::new(),
            last_plane_rotation: None,
        })
    }

    /// 抓取一帧，以紧凑排列的 BGRA（`w*4*h` 字节）写入内部缓冲区；在下一次抓取前有效。
    pub fn grab(&mut self) -> io::Result<(&[u8], usize, usize)> {
        // SAFETY：ctx 有效；调用前 frame 已清零。所有拥有该 frame 的返回路径都会释放它：失败的
        // `drmtap_grab_mapped` 不会留下任何需要释放的东西，强行释放反而会造成重复释放。
        unsafe {
            let mut frame: drmtap_frame_info = std::mem::zeroed();
            let ret = (self.lib.grab_mapped)(self.ctx, &mut frame);
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
                    format!("drmtap_grab_mapped failed: errno {errno}"),
                ));
            }
            self.last_plane_rotation = self.read_plane_rotation();
            if frame.data.is_null() || frame.width == 0 || frame.height == 0 {
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let w = frame.width;
            let h = frame.height;
            let stride = frame.stride as usize;
            // 逐行拷贝会从只有 stride*height 字节的源中读取 w*4 字节：必须拒绝低于 32bpp 或尺寸异常的参数，以避免 OOB 读取。
            if w > MAX_DIM || h > MAX_DIM || stride < (w as usize) * 4 {
                log::warn!(
                    "DRM scanout not 32-bit BGRA-compatible ({w}x{h} stride {stride} fourcc {:#010x}); falling back",
                    frame.format
                );
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "unsupported DRM scanout format",
                ));
            }
            // XBGR8888 能通过 stride 检查，但在下游被当作 BGRA 标记后会导致红蓝通道颠倒；fourcc 为零时会落到 stride 不变式上（保留它是为了兼容未设置该值的 libdrmtap 构建）。
            if frame.format != 0
                && frame.format != DRM_FORMAT_XRGB8888
                && frame.format != DRM_FORMAT_ARGB8888
            {
                log::warn!(
                    "DRM scanout fourcc {:#010x} is not BGRA-compatible; falling back",
                    frame.format
                );
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "unsupported DRM scanout format",
                ));
            }
            let (w, h) = (w as usize, h as usize);
            let frame_size = match w.checked_mul(4).and_then(|x| x.checked_mul(h)) {
                Some(sz) if sz > 0 && sz <= MAX_FRAME_BYTES => sz,
                other => {
                    log::warn!(
                        "DRM scanout geometry {w}x{h} yields an out-of-range frame ({other:?} bytes); falling back"
                    );
                    (self.lib.frame_release)(self.ctx, &mut frame);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "DRM scanout frame too large",
                    ));
                }
            };
            // 源的范围同样要设限：逐行循环最多读取 (h-1)*stride + w*4，而 `y * stride` 可能溢出。
            match stride.checked_mul(h) {
                Some(sz) if sz > 0 && sz <= MAX_FRAME_BYTES => {}
                other => {
                    log::warn!(
                        "DRM scanout stride {stride} x {h} rows is out of range ({other:?} bytes); falling back"
                    );
                    (self.lib.frame_release)(self.ctx, &mut frame);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "DRM scanout stride out of range",
                    ));
                }
            }
            if self.buf.len() != frame_size {
                self.buf.resize(frame_size, 0);
            }
            let src = frame.data as *const u8;
            let dst = self.buf.as_mut_ptr();
            if stride == w * 4 {
                std::ptr::copy_nonoverlapping(src, dst, frame_size);
            } else {
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(src.add(y * stride), dst.add(y * w * 4), w * 4);
                }
            }
            (self.lib.frame_release)(self.ctx, &mut frame);
            Ok((&self.buf, w, h))
        }
    }

    /// 本读取器所捕获的 GPU 的渲染节点，使转换端绑定到“导出”扫描输出的那个设备：
    /// 跨厂商导入可能因 tiling modifier 不兼容而失败。如果该符号不存在或设备仅用于显示，则为 `None`。
    pub fn render_node(&mut self) -> Option<String> {
        let f = self.lib.render_node?;
        // SAFETY：self.ctx 有效；返回的指针归该上下文所有，在上下文关闭之前一直有效。
        let ptr = unsafe { f(self.ctx) };
        if ptr.is_null() {
            return None;
        }
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_str()
            .ok()
            .map(|s| s.to_owned())
    }

    /// 上一次成功抓取所读取的那个图层的 DRM `rotation` 位掩码，取值就是
    /// 那次抓取之后的值。没有该属性的图层会返回 rotate-0：它不可能旋转过
    /// 任何东西，所以合成器绘制出的扫描输出已经是旋转过的，整个输出变换
    /// 仍然需要被撤销。只有当库无法说明时才返回 `None`：库版本早于该调用
    /// （0.5.8）、没有绑定图层，或者无法读取属性集。
    pub fn plane_rotation(&self) -> Option<u32> {
        self.last_plane_rotation
    }

    /// 被捕获 CRTC 的刷新率（赫兹），以其当前模式的最简分数 `(num, den)` 表示，
    /// 此刻读取。当库版本早于该调用（0.5.9）或该 CRTC 没有模式时为 `None`。
    /// 此时读不到刷新率。
    pub fn crtc_refresh(&mut self) -> Option<(u64, u64)> {
        let f = self.lib.crtc_refresh?;
        let (mut num, mut den) = (0u64, 0u64);
        // SAFETY：self.ctx 是有效的上下文，两个输出值的生命周期都长于本次调用。
        let rc = unsafe { f(self.ctx, &mut num, &mut den) };
        (rc == 0 && num != 0 && den != 0).then_some((num, den))
    }

    fn read_plane_rotation(&mut self) -> Option<u32> {
        let f = self.lib.plane_rotation?;
        let mut rotation: u32 = 0;
        // SAFETY：self.ctx 是有效的上下文，`rotation` 的生命周期长于本次调用。
        let rc = unsafe { f(self.ctx, &mut rotation) };
        plane_rotation_answer(rc, rotation)
    }

    /// 此刻读取的主图层旋转值，供尚未抓取（枚举阶段）的调用方使用：
    /// `plane_rotation()` 只反映上一次抓取的情况。
    pub fn query_plane_rotation(&mut self) -> Option<u32> {
        self.read_plane_rotation()
    }

    /// 零拷贝的导出抓取：填充一个 `drmtap_dmabuf_desc`（dma-buf fd、图层布局、HDR 元数据），不做映射、不做去 tiling、不拷贝像素，因此在这条
    /// 路径上 root 进程永远不需要加载 libEGL/libGLESv2。导出的 fd 是只读的（libdrmtap 会去掉 `DRM_RDWR`，而 `dup` 出来的 fd 共享同一个打开的
    /// 文件描述），因此接收它的 `--server` 可以映射扫描输出，但绝不能写入实时帧缓冲。这里的校验仅针对元数据。
    pub fn grab_desc(&mut self) -> io::Result<(OwnedFd, drmtap_dmabuf_desc)> {
        let grab_desc = self.lib.grab_desc;
        // SAFETY：self.ctx 有效；调用前 desc/frame 已清零。只有真正拿到已填充 frame 的路径才会释放它：在 `-EINVAL` 时
        // libdrmtap 会在分配之前返回，内部抓取失败时已经清理完毕，而在 `-ENOTSUP` 时由 libdrmtap 自己释放该帧。
        unsafe {
            let mut desc: drmtap_dmabuf_desc = std::mem::zeroed();
            let mut frame: drmtap_frame_info = std::mem::zeroed();
            let ret = grab_desc(self.ctx, &mut desc, &mut frame);
            if ret < 0 {
                let errno = -ret;
                if errno == hbb_common::libc::EAGAIN
                    || errno == hbb_common::libc::EBUSY
                    || errno == hbb_common::libc::EINTR
                {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                if errno == hbb_common::libc::ENOTSUP {
                    // 用一个专门的错误，让调用方降级到映射/PipeWire 路径，而不是紧密循环地重建。
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "drmtap_grab_desc: no transferable dma-buf (ENOTSUP)",
                    ));
                }
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("drmtap_grab_desc failed: errno {errno}"),
                ));
            }
            self.last_plane_rotation = self.read_plane_rotation();
            // `desc.dma_buf_fd` 是那个规范的 fd（split_capture.c 发送的就是它）；`frame` 也拥有它，而 `frame_release` 会关闭库里的那份副本。
            let raw_fd = if desc.dma_buf_fd >= 0 {
                desc.dma_buf_fd
            } else {
                frame.dma_buf_fd
            };
            if raw_fd < 0 {
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let w = desc.width;
            let h = desc.height;
            if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("DRM scanout geometry {w}x{h} out of range"),
                ));
            }
            // 这里不做 fourcc 门控：转换端能处理 libdrmtap 支持的所有格式，而在这里门控会漏掉像 XR30 这样可转换的扫描输出。
            let planes = if desc.num_planes == 0 { 1 } else { desc.num_planes };
            if planes > 4 {
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("DRM scanout num_planes {} out of range (1..=4)", desc.num_planes),
                ));
            }
            for p in 0..(planes as usize) {
                let extent = (desc.pitches[p] as usize)
                    .checked_mul(h as usize)
                    .and_then(|rows| rows.checked_add(desc.offsets[p] as usize));
                match extent {
                    Some(end) if end <= MAX_FRAME_BYTES => {}
                    other => {
                        (self.lib.frame_release)(self.ctx, &mut frame);
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!(
                                "DRM scanout plane {p} out of range (offset {} pitch {} over {h} rows -> {other:?}, cap {MAX_FRAME_BYTES})",
                                desc.offsets[p], desc.pitches[p]
                            ),
                        ));
                    }
                }
            }
            // 必须在释放帧之前 dup：释放之后库可能会回收它的句柄，而在同一个已打开的 dma-buf 上的独立 fd
            // 可以让对端继续保有该缓冲区。用 F_DUPFD_CLOEXEC 而不是 dup()：`dup` 从不复制 close-on-exec 标记，而这个 root 服务会在别处 fork。
            let dup_fd = hbb_common::libc::fcntl(raw_fd, hbb_common::libc::F_DUPFD_CLOEXEC, 0);
            if dup_fd < 0 {
                let e = io::Error::last_os_error();
                (self.lib.frame_release)(self.ctx, &mut frame);
                return Err(e);
            }
            let owned = OwnedFd::from_raw_fd(dup_fd);
            (self.lib.frame_release)(self.ctx, &mut frame);
            desc.num_planes = planes;
            desc.dma_buf_fd = -1;
            Ok((owned, desc))
        }
    }

    /// 读取硬件光标图层：图层报告光标不可见时返回隐藏哨兵值，可见时返回真实形状，读取失败时返回 `None`。
    pub fn cursor(&mut self) -> Option<CursorSnapshot> {
        // SAFETY：ctx 有效；c 已清零；get_cursor 成功后的每一条路径都会释放。只有 get_cursor 失败时才会不释放就返回，因为那时根本没有需要释放的东西。
        unsafe {
            let mut c: drmtap_cursor_info = std::mem::zeroed();
            let cret = (self.lib.get_cursor)(self.ctx, &mut c);
            if cret != 0 {
                return None;
            }
            let out = if c.visible == 0 {
                Some(CursorSnapshot {
                    id: HIDDEN_CURSOR_ID,
                    width: 1,
                    height: 1,
                    hotx: 0,
                    hoty: 0,
                    hot_measured: false,
                    colors: vec![0, 0, 0, 0],
                    x: 0,
                    y: 0,
                })
            } else if !c.pixels.is_null()
                && c.width > 0
                && c.height > 0
                && (c.width as i64) * (c.height as i64) <= 256 * 256
            {
                let cw = c.width as i32;
                let ch = c.height as i32;
                let n = (cw * ch) as usize;
                let src = std::slice::from_raw_parts(c.pixels, n);
                let mut hash: u64 = 1469598103934665603;
                let mut colors = Vec::with_capacity(n * 4);
                for &p in src.iter() {
                    colors.push(((p >> 16) & 0xff) as u8);
                    colors.push(((p >> 8) & 0xff) as u8);
                    colors.push((p & 0xff) as u8);
                    colors.push(((p >> 24) & 0xff) as u8);
                    hash ^= p as u64;
                    hash = hash.wrapping_mul(1099511628211);
                }
                // 向库询问热点来自哪里，而不是根据
                // 坐标去猜。`hot_x/hot_y == (0, 0)` 有两种截然相反的含义——该图层
                // 没有暴露 HOTSPOT_X/Y（所有裸机驱动都是如此），或者它暴露了，
                // 而驱动的答案就是左上角——下面那个旧判定无法把两者区分开：
                // 它会用一个来自位图的猜测覆盖掉真实的 (0, 0) 测量值，而且
                // 在没有这些属性的驱动上，它会相信一个谁都没有发布过的热点。
                // 在 cursor_release 之前询问——正是它拥有本次采样。
                let answered = self.lib.cursor_hotspot_valid.and_then(|f| {
                    let mut valid: c_int = 0;
                    // 0 表示已给出答案；-ENOTSUP 表示本次采样没有任何记录给出答案，
                    // 而通过 0.5.6 之前的特权辅助程序读取光标时产生的正是这种结果。
                    (f(&c, &mut valid) == 0).then(|| valid != 0)
                });
                let hot_measured = hot_measured_from(answered, c.hot_x, c.hot_y);
                self.note_hotspot_provenance(answered, hot_measured);
                let (hotx, hoty) = if hot_measured {
                    (c.hot_x, c.hot_y)
                } else {
                    infer_hotspot(&colors, cw as usize, ch as usize)
                };
                // 把几何尺寸和热点折算进 id：像素相同但尺寸或
                // 热点变了就必须算作新形状，否则 drm_capture_worker 会抑制这次
                // 更新（它按 id 去重），客户端就会继续渲染过期的光标。
                let id = cursor_id(hash, cw as u32, ch as u32, hotx, hoty, hot_measured);
                Some(CursorSnapshot {
                    id,
                    width: cw as u32,
                    height: ch as u32,
                    hotx,
                    hoty,
                    hot_measured,
                    colors,
                    x: c.x,
                    y: c.y,
                })
            } else {
                None
            };
            (self.lib.cursor_release)(self.ctx, &mut c);
            out
        }
    }

    pub fn displays(&mut self) -> Vec<DisplaySnapshot> {
        // SAFETY：ctx 有效；raw 是已清零且大小正确的数组；索引前会把 count 限制在缓冲区范围内。
        unsafe {
            let mut raw = vec![std::mem::zeroed::<drmtap_display>(); 16];
            let cap = raw.len() as i32;
            let n = (self.lib.list_displays)(self.ctx, raw.as_mut_ptr(), cap);
            if n <= 0 {
                return Vec::new();
            }
            let count = (n as usize).min(raw.len());
            (0..count)
                .map(|i| {
                    let name_bytes: Vec<u8> = raw[i]
                        .name
                        .iter()
                        .take_while(|&&ch| ch != 0)
                        .map(|&ch| ch as u8)
                        .collect();
                    DisplaySnapshot {
                        name: String::from_utf8_lossy(&name_bytes).to_string(),
                        crtc_id: raw[i].crtc_id,
                        x: raw[i].x as i32,
                        y: raw[i].y as i32,
                        width: raw[i].width,
                        height: raw[i].height,
                        refresh_hz: raw[i].refresh_hz,
                        active: raw[i].active != 0,
                    }
                })
                .collect()
        }
    }
}

impl Drop for DrmReader {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY：ctx 来自 drmtap_open，非空。
            unsafe { (self.lib.close)(self.ctx) };
            self.ctx = std::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod hotspot_provenance_tests {
    use super::hot_measured_from;

    /// 这正是整个入口存在的理由，也是旧启发式判断反了的情况：一个
    /// 半虚拟化驱动确实把热点发布在图像的左上角。
    /// 实测得到的 `(0, 0)` 就是实测值，而从位图重新推断会移动一个
    /// 驱动已经放置好的光标。
    #[test]
    fn a_measured_zero_hotspot_is_measured() {
        assert!(hot_measured_from(Some(true), 0, 0));
    }
}

#[cfg(test)]
mod hotspot_guess_tests {
    use super::infer_hotspot;

    /// 用 ASCII 图案表示的精灵图：`#` 表示不透明，其他字符表示透明。之所以这样写，
    /// 是因为这里讨论的每种情况都是关于“形状”的，而形状用字节数组是读不出来的。
    fn sprite(rows: &[&str]) -> (Vec<u8>, usize, usize) {
        let (h, w) = (rows.len(), rows[0].len());
        let mut px = vec![0u8; w * h * 4];
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(row.len(), w, "ragged picture");
            for (x, c) in row.bytes().enumerate() {
                if c == b'#' {
                    px[(y * w + x) * 4 + 3] = 255;
                }
            }
        }
        (px, w, h)
    }

    /// 箭头保留其盒子的左上角。光标是朝上和左绘制的，所以
    /// 那个角就是尖端——这是关于正立的精灵图的事实，这也是为什么
    /// 消费端在把旋转过的精灵图转正之后要重新做这个猜测。
    #[test]
    fn an_arrow_points_at_the_corner_of_its_box() {
        let (px, w, h) = sprite(&[
            ".........",
            ".#.......",
            ".##......",
            ".###.....",
            ".####....",
            ".#####...",
            ".###.....",
            ".#..##...",
            "......##.",
        ]);
        assert_eq!(infer_hotspot(&px, w, h), (1, 1));
    }

    /// 关于盒子中线镜像对称的形状没有尖端可用，所以
    /// 角落说明不了任何事，中心是形状唯一支持的答案。这就是
    /// 十字准星/单元格/缩放这一族，在已安装主题的 35 个形状中占了 17 个。
    #[test]
    fn a_symmetric_shape_is_held_by_its_middle() {
        let (px, w, h) = sprite(&[
            "....#....",
            "....#....",
            "....#....",
            "....#....",
            "#########",
            "....#....",
            "....#....",
            "....#....",
            "....#....",
        ]);
        assert_eq!(infer_hotspot(&px, w, h), (4, 4));
    }

    /// 单向缩放光标是一个箭头加上它所缩放那条边的横条，而
    /// 主题把点击点放在横条上。横条是较重的一端，也是区分
    /// `e-resize` 和 `w-resize` 的唯一依据：它们是同一张图的镜像，所以一条
    /// 总是取左上角的规则，对其中哪一个来说必然是错的。
    #[test]
    fn a_directional_resize_cursor_is_held_by_its_bar() {
        let (px, w, h) = sprite(&[
            "......####",
            "..##..####",
            ".###..####",
            "####..####",
            ".###..####",
            "..##..####",
            "......####",
        ]);
        assert_eq!(infer_hotspot(&px, w, h), (9, 3));
        // 镜像必须给出另一侧的答案，否则这条规则就没有在读形状。
        let (px, w, h) = sprite(&[
            "####......",
            "####..##..",
            "####..###.",
            "####..####",
            "####..###.",
            "####..##..",
            "####......",
        ]);
        assert_eq!(infer_hotspot(&px, w, h), (0, 3));
    }

    /// 细长规则仍然排在镜像判定之前，所以即使横条并不对称，也仍会
    /// 保留它的中心。已安装的主题里没有这种形状——它的两个 I 型光标都是
    /// 对称的——但自定义光标可能是，而这就是 rustdesk#16242 的第一轮修复。
    #[test]
    fn an_elongated_shape_keeps_its_centre_even_when_it_is_lopsided() {
        let (px, w, h) = sprite(&["##########.", "#####......", "##########."]);
        assert_eq!(infer_hotspot(&px, w, h), (4, 1));
    }
}

/// DRM_MODE_ROTATE_0：没有旋转任何东西的图层的 `rotation` 位掩码。
const DRM_MODE_ROTATE_0: u32 = 1 << 0;

/// `drmtap_plane_rotation` 的返回值对消费端意味着什么。成功时返回该掩码。`-ENOTSUP`
/// （图层没有 `rotation` 属性）表示 rotate-0，而不是“未知”：这样的图层不可能
/// 旋转过扫描输出，所以帧到达时是被整个输出变换旋转过的，这正是
/// 这个函数为 virtio-gpu 和 vmwgfx 场景而存在的原因。其他任何情况（未绑定图层时的 `-ENOENT`、
/// 属性读取失败）都是 `None`，消费端沿用 0.5.8 之前的规则。
fn plane_rotation_answer(rc: c_int, rotation: u32) -> Option<u32> {
    if rc == 0 {
        Some(rotation)
    } else if rc == -hbb_common::libc::ENOTSUP {
        Some(DRM_MODE_ROTATE_0)
    } else {
        None
    }
}

#[cfg(test)]
mod plane_rotation_tests {
    use super::*;

    #[test]
    fn a_plane_without_the_property_turned_nothing() {
        assert_eq!(plane_rotation_answer(0, 0x4), Some(0x4));
        assert_eq!(plane_rotation_answer(0, 0x1), Some(0x1));
        assert_eq!(plane_rotation_answer(-hbb_common::libc::ENOTSUP, 0), Some(0x1));
        assert_eq!(plane_rotation_answer(-hbb_common::libc::ENOENT, 0), None);
        assert_eq!(plane_rotation_answer(-hbb_common::libc::EINVAL, 0), None);
        assert_eq!(plane_rotation_answer(-hbb_common::libc::EIO, 7), None);
    }
}

#[cfg(test)]
mod crtc_refresh_tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

    static RC: AtomicI32 = AtomicI32::new(0);
    static NUM: AtomicU64 = AtomicU64::new(0);
    static DEN: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn crtc_refresh(_: *mut drmtap_ctx, num: *mut u64, den: *mut u64) -> c_int {
        * num = NUM.load(Ordering::SeqCst);
        * den = DEN.load(Ordering::SeqCst);
        RC.load(Ordering::SeqCst)
    }
    unsafe extern "C" fn grab_mapped(_: *mut drmtap_ctx, _: *mut drmtap_frame_info) -> c_int {
        -1
    }
    unsafe extern "C" fn frame_release(_: *mut drmtap_ctx, _: *mut drmtap_frame_info) {}
    unsafe extern "C" fn grab_desc(
        _: *mut drmtap_ctx,
        _: *mut drmtap_dmabuf_desc,
        _: *mut drmtap_frame_info,
    ) -> c_int {
        -1
    }
    unsafe extern "C" fn plane_rotation(_: *mut drmtap_ctx, _: *mut u32) -> c_int {
        -1
    }

    fn reader(with_call: bool) -> DrmReader {
        let mut lib = DrmtapLib::fake(grab_mapped, frame_release, grab_desc, plane_rotation);
        if with_call {
            lib.crtc_refresh = Some(crtc_refresh);
        }
        DrmReader {
            lib: Box::leak(Box::new(lib)),
            ctx: std::ptr::null_mut(),
            buf: Vec::new(),
            last_provenance: -1,
            last_plane_rotation: None,
        }
    }

    /// rustdesk#16175：精确的刷新率用来给生产端定节奏；更旧的库给不出答案。
    #[test]
    fn the_crtc_refresh_is_a_fraction_or_nothing() {
        assert_eq!(reader(false).crtc_refresh(), None, "a library before 0.5.9");
        let mut r = reader(true);
        RC.store(0, Ordering::SeqCst);
        NUM.store(148352, Ordering::SeqCst);
        DEN.store(2475, Ordering::SeqCst);
        assert_eq!(r.crtc_refresh(), Some((148352, 2475)));
        RC.store(-hbb_common::libc::ENODATA, Ordering::SeqCst);
        assert_eq!(r.crtc_refresh(), None, "a CRTC with no mode");
        RC.store(0, Ordering::SeqCst);
        DEN.store(0, Ordering::SeqCst);
        assert_eq!(r.crtc_refresh(), None, "a zero denominator is no answer");
    }
}
