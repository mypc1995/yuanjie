// `_drm` IPC 通道中用于 DRM/KMS 采集的那一半：类型、root 服务生产者、帧封装。

use super::ipc_auth::active_uid_cached;
use super::*;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DrmDisplayInfo {
    pub name: String,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    /// 导出本显示器扫描输出的那块 GPU 的 render node；在多 GPU 主机上，自动选择
    /// 可能绑定到另一块 GPU，而跨厂商导入随后会失败。当服务无法
    /// 确定它时为空：此时消费端在只有单个 render node 的主机上自动选择，而在有多个时强制走 CPU
    /// 路径。
    #[serde(default)]
    pub render_node: String,
    /// 驱动本显示器的 KMS card 节点（`/dev/dri/card*`）。crtc_ids 是 card 局部的，所以这个索引
    /// 单独来看在不同 card 之间是有歧义的。为空表示使用自动检测到的唯一设备。
    #[serde(default)]
    pub device: String,
    /// 主平面在硬件上把它的帧缓冲旋转 90/270，所以每一帧都是这个
    /// 模式 TRANSPOSED，不管合成器报告什么（gamescope 的 wl_output 没有 transform）。
    #[serde(default)]
    pub plane_transposed: bool,
}

/// 与 `scrap::drm_reader::drmtap_dmabuf_desc` 类似，只是 `dma_buf_fd` 不同（从不做序列化——它通过
/// SCM_RIGHTS 辅助数据传递），并增加了 `buffer_id`（带每连接 epoch 标记的 fb_id；目前没有消费端读取它）和 `has_fd`。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DmabufDesc {
    pub buffer_id: u64,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: u64,
    /// KMS 帧缓冲 id——libdrmtap 的“一次导入”缓存键。0 表示这一帧不缓存。
    pub fb_id: u32,
    /// `offsets`/`pitches` 中已使用的项（1..4）；0 视为 1。
    pub num_planes: u32,
    pub offsets: [u32; 4],
    pub pitches: [u32; 4],
    /// DRMTAP_EOTF_*（SDR=0、PQ=2、HLG=3）。PQ 会在转换时触发 HDR->SDR 色调映射。
    pub hdr_eotf: u32,
    pub hdr_max_nits: u32,
    /// 为 true：这个 fd 随本消息的 SCM_RIGHTS cmsg 传递。为 false：对 `fb_id` 命中了一次导入缓存。
    pub has_fd: bool,
    /// 主平面扫描输出这一帧时所用的 DRM `rotation` 位掩码，来自
    /// `drmtap_plane_rotation()`（libdrmtap 0.5.8）；没有该属性的平面会被报告为
    /// rotate-0。当库无法给出时为 `None`（版本低于 0.5.8、没有绑定，或
    /// 属性集不可读）。来自在硬件中做了旋转的平面的帧本身已经
    /// 是正的；来自未做旋转的平面的帧则由输出变换来旋转。参见
    /// `frame_transform`。
    #[serde(default)]
    pub plane_rotation: Option<u32>,
    /// 在这一帧被抓取后立刻读到的光标平面位置，单位是该
    /// 码流所显示显示器的扫描输出像素，所以比这一帧新几毫秒。`None` 表示光标
    /// 被隐藏、读取失败或被拒绝，或者生产者早于该字段。它随
    /// 帧一起传递，而不是单独一条流，因此不会排队，也不会对
    /// 采集 worker 施加背压；消费端只对连续数帧保持不变的位置作出反应。
    #[serde(default)]
    pub cursor_pos: Option<(i32, i32)>,
}

pub(crate) fn drm_ipc_path() -> String {
    let service_path = Config::ipc_path("_service");
    let dir = std::path::Path::new(&service_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/tmp"));
    dir.join("ipc_drm").to_string_lossy().into_owned()
}

pub(crate) async fn connect_drm(ms_timeout: u64) -> ResultType<DrmConn> {
    use std::os::fd::AsRawFd;
    let path = drm_ipc_path();
    let stream = timeout(ms_timeout, tokio::net::UnixStream::connect(&path)).await??;
    // 生产者必须是 root：赢得了套接字路径竞争的非 root 对端不能被信任去
    // 提供显示器列表、帧以及任意的 dma-buf fd。
    if peer_uid_from_fd(stream.as_raw_fd()) != Some(0) {
        bail!("drm: _drm producer is not root; refusing to consume");
    }
    Ok(DrmConn::new(stream))
}

/// 把 `_drm` 监听器绑定为 0666：任何本地 uid 都能连接，授权在 `handle_drm_conn` 中进行。
fn new_drm_listener() -> ResultType<Incoming> {
    let path = drm_ipc_path();
    let _ = ensure_secure_ipc_parent_dir(&path, "_service")?;
    // 不能用 `std::fs::remove_file`：对一个目录类型的抢占者，`unlink(2)` 会返回 EISDIR，
    // 随后绑定会以 EADDRINUSE 失败；基于 fd 的辅助函数会选用 `AT_REMOVEDIR`（仅限空目录）。
    if let Err(err) = remove_ipc_entry_via_secure_parent_fd(&path) {
        log::warn!("drm: could not clear a stale entry at {}: {}", &path, err);
    }
    let mut endpoint = Endpoint::new(path.clone());
    endpoint.set_security_attributes(SecurityAttributes::allow_everyone_create()?);
    let incoming = endpoint.incoming()?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).map_err(|err| {
        std::fs::remove_file(&path).ok();
        err
    })?;
    log::info!("Started drm ipc server at path: {}", &path);
    Ok(incoming)
}

enum DrmProducerMsg {
    /// 枚举出的显示器，在任何帧之前发送一次。
    Displays(Vec<DrmDisplayInfo>),
    /// 零拷贝路径：描述符 + 扫描输出 fd；`OwnedFd` 在发送方 dup 之后就被关闭。
    Frame {
        desc: DmabufDesc,
        fd: Option<OwnedFd>,
    },
    /// CPU 映射的回退路径（紧凑 BGRA）：消费端没有转换上下文（`need_cpu`），或者遇到 ENOTSUP。
    FrameCpu {
        width: u32,
        height: u32,
        data: Bytes,
        /// 参见 `DmabufDesc::plane_rotation`。
        plane_rotation: Option<u32>,
        /// 参见 `DmabufDesc::cursor_pos`。dmabuf 路径把它放在描述符内部；这条
        /// 路径没有描述符，所以它随像素一起传递。
        cursor_pos: Option<(i32, i32)>,
    },
    Cursor {
        id: u64,
        width: u32,
        height: u32,
        hotx: i32,
        hoty: i32,
        hot_measured: bool,
        colors: Vec<u8>,
    },
}

/// 把本次 tick 的光标平面位置放到它被读取时相邻的那一帧上。隐藏哨兵
/// 不携带位置；形状消息则不作处理。
fn stamp_cursor_pos(msg: &mut DrmProducerMsg, c: Option<&scrap::drm_reader::CursorSnapshot>) {
    let pos = c.and_then(|c| (c.id != scrap::drm_reader::HIDDEN_CURSOR_ID).then_some((c.x, c.y)));
    match msg {
        DrmProducerMsg::Frame { desc, .. } => desc.cursor_pos = pos,
        DrmProducerMsg::FrameCpu { cursor_pos, .. } => *cursor_pos = pos,
        DrmProducerMsg::Displays(_) | DrmProducerMsg::Cursor { .. } => {}
    }
}

#[cfg(test)]
mod cursor_pos_tests {
    use super::*;

    fn desc() -> DmabufDesc {
        DmabufDesc {
            buffer_id: 0,
            width: 4,
            height: 4,
            format: 0,
            modifier: 0,
            fb_id: 0,
            num_planes: 1,
            offsets: [0; 4],
            pitches: [16, 0, 0, 0],
            hdr_eotf: 0,
            hdr_max_nits: 0,
            has_fd: false,
            plane_rotation: None,
            cursor_pos: None,
        }
    }

    fn snapshot(id: u64) -> scrap::drm_reader::CursorSnapshot {
        scrap::drm_reader::CursorSnapshot {
            id,
            width: 2,
            height: 2,
            hotx: 0,
            hoty: 0,
            hot_measured: false,
            colors: vec![0; 16],
            x: 10,
            y: 20,
        }
    }

    #[test]
    fn the_plane_position_rides_both_frame_kinds_and_a_hidden_cursor_rides_as_none() {
        let visible = snapshot(7);
        let hidden = snapshot(scrap::drm_reader::HIDDEN_CURSOR_ID);

        let mut frame = DrmProducerMsg::Frame { desc: desc(), fd: None };
        stamp_cursor_pos(&mut frame, Some(&visible));
        assert!(matches!(&frame, DrmProducerMsg::Frame { desc, .. } if desc.cursor_pos == Some((10, 20))));
        stamp_cursor_pos(&mut frame, Some(&hidden));
        assert!(matches!(&frame, DrmProducerMsg::Frame { desc, .. } if desc.cursor_pos.is_none()));

        let mut cpu = DrmProducerMsg::FrameCpu {
            width: 4,
            height: 4,
            data: Bytes::new(),
            plane_rotation: None,
            cursor_pos: None,
        };
        stamp_cursor_pos(&mut cpu, Some(&visible));
        assert!(matches!(&cpu, DrmProducerMsg::FrameCpu { cursor_pos: Some((10, 20)), .. }));
        stamp_cursor_pos(&mut cpu, None);
        assert!(matches!(&cpu, DrmProducerMsg::FrameCpu { cursor_pos: None, .. }));

        let mut shape = DrmProducerMsg::Cursor {
            id: 7,
            width: 2,
            height: 2,
            hotx: 1,
            hoty: 1,
            hot_measured: false,
            colors: vec![0; 16],
        };
        stamp_cursor_pos(&mut shape, Some(&visible));
        assert!(matches!(&shape, DrmProducerMsg::Cursor { hotx: 1, hoty: 1, .. }));
    }
}

struct DrmStopGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for DrmStopGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn dup_to_drm_conn(stream: &Connection) -> ResultType<DrmConn> {
    let raw = stream.inner.get_ref().as_raw_fd();
    // 用 F_DUPFD_CLOEXEC，而不是 dup()：`dup` 从不复制 close-on-exec 标志，而这个进程会 fork（
    // `loginctl` 查询），所以一个已授权的 `_drm` 套接字会泄漏到子进程中。
    let dup = unsafe { hbb_common::libc::fcntl(raw, hbb_common::libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // 安全性：`dup` 是为已连接的 SOCK_STREAM unix 套接字新 dup 出来的自有 fd。
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(dup) };
    std_stream.set_nonblocking(true)?;
    let tokio_stream = tokio::net::UnixStream::from_std(std_stream)?;
    Ok(DrmConn::new(tokio_stream))
}

static DRM_DISPLAY_CACHE: std::sync::Mutex<Vec<DrmDisplayInfo>> = std::sync::Mutex::new(Vec::new());

/// 只有当某个变更改动了 `DRM_DISPLAY_CACHE` 时才会递增；Release 排序在缓存写入之后。
static DRM_DISPLAY_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 上一次枚举所见到的每个被驱动的 CRTC 的整赫兹刷新率，按 (device, crtc) 记录，用于
/// 那些库读不到精确值的生产者：这样它就不需要自己去探测连接器。
static DRM_WHOLE_HZ: std::sync::Mutex<std::collections::BTreeMap<(String, u32), u32>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// 此 reader 服务的显示器，以及每个未被驱动的输出的标识（`device:connector`）。
fn drm_displays_from_reader(
    reader: &mut scrap::drm_reader::DrmReader,
    device: &str,
) -> (Vec<DrmDisplayInfo>, Vec<String>) {
    let render_node = reader.render_node().unwrap_or_default();
    let mut undriven = Vec::new();
    let snapshots = reader.displays();
    if let Ok(mut hz) = DRM_WHOLE_HZ.lock() {
        for d in snapshots.iter().filter(|d| d.active && d.crtc_id != 0) {
            hz.insert((device.to_owned(), d.crtc_id), d.refresh_hz);
        }
    }
    let displays: Vec<DrmDisplayInfo> = snapshots
        .into_iter()
        // 只包含绑定到 CRTC 的输出：处于 CONNECTED 但未绑定的连接器枚举时
        // `crtc_id == 0`，而 `open(crtc=0)` 会自动选中第一个处于 ACTIVE 的 CRTC 并推流它的帧。
        .filter(|d| {
            if !d.active || d.crtc_id == 0 {
                undriven.push(format!("{device}:{name}", name = d.name));
                return false;
            }
            true
        })
        .map(|d| DrmDisplayInfo {
            plane_transposed: plane_transposed(device, d.crtc_id),
            name: d.name,
            crtc_id: d.crtc_id,
            x: d.x,
            y: d.y,
            width: d.width,
            height: d.height,
            active: d.active,
            render_node: render_node.clone(),
            device: device.to_owned(),
        })
        .collect();
    (displays, undriven)
}

const DRM_MODE_ROTATE_90: u32 = 0x2;
const DRM_MODE_ROTATE_270: u32 = 0x8;

/// 通过绑定到 `crtc_id` 的 reader：枚举用的 reader 绑定的是自动选择挑中的那个 crtc，
/// 无论它是哪一个。
fn plane_transposed(device: &str, crtc_id: u32) -> bool {
    let dev = (!device.is_empty()).then_some(device);
    let Some(mut r) = scrap::drm_reader::DrmReader::open(dev, crtc_id) else {
        return false;
    };
    let transposed = match r.query_plane_rotation() {
        Some(m) => m & (DRM_MODE_ROTATE_90 | DRM_MODE_ROTATE_270) != 0,
        None => {
            hbb_common::throttled_log!(
                std::time::Duration::from_secs(600),
                debug,
                "drm: crtc {crtc_id} ({device:?}): plane rotation unreadable; sized as unrotated"
            );
            false
        }
    };
    if transposed {
        log::trace!("drm: crtc {crtc_id} ({device:?}) is rotated 90/270 by its plane");
    }
    transposed
}

/// 通过一次查询得到所有 DRM 设备的活动显示器 + 已连接但未被驱动的标识。
fn drm_enumerate_all_displays() -> (Vec<DrmDisplayInfo>, Vec<String>) {
    if let Some(devices) = scrap::drm_reader::list_devices() {
        if devices.len() > 1 {
            log::info!(
                "drm: {} DRM devices: {}",
                devices.len(),
                devices
                    .iter()
                    .map(|d| format!(
                        "{} ({}, render {})",
                        d.path,
                        d.display_count,
                        if d.render_node.is_empty() { "none" } else { &d.render_node }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let mut all = Vec::new();
        let mut undriven_total = Vec::new();
        let mut any_opened = false;
        for dev in devices {
            if let Some(mut r) = scrap::drm_reader::DrmReader::open(Some(&dev.path), 0) {
                any_opened = true;
                let (mut got, mut undriven) = drm_displays_from_reader(&mut r, &dev.path);
                all.append(&mut got);
                undriven_total.append(&mut undriven);
            } else if dev.display_count == 0 {
                log::debug!(
                    "drm: {} has no active display and did not open; cannot tell whether it has a \
                     connected output that is merely switched off",
                    dev.path
                );
            }
        }
        // 即使列表为空也要采用这个结果：回退路径会在 `device = ""` 下重新编排标识。
        if any_opened {
            return (all, undriven_total);
        }
    }
    // 只靠自动检测是不够的：它会选中一块正在扫描输出的 card。在 T2 上实测，
    // 面板空闲禁用时它绑定到 card0（Touch Bar）；card2 上的面板它根本看不到。
    let mut all = Vec::new();
    let mut undriven_total = Vec::new();
    let mut paths: Vec<std::path::PathBuf> = match std::fs::read_dir("/dev/dri") {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("card") && n[4..].chars().all(|c| c.is_ascii_digit()))
            })
            .collect(),
        Err(err) => {
            log::debug!("drm: cannot read /dev/dri to enumerate cards: {err}");
            Vec::new()
        }
    };
    // 确定的顺序，这样显示器列表就不依赖目录顺序。
    paths.sort();
    let n_paths = paths.len();
    for p in paths {
        let Some(path) = p.to_str() else { continue };
        if let Some(mut r) = scrap::drm_reader::DrmReader::open(Some(path), 0) {
            let (mut got, mut undriven) = drm_displays_from_reader(&mut r, path);
            all.append(&mut got);
            undriven_total.append(&mut undriven);
        }
    }
    log::info!(
        "drm: enumerated /dev/dri directly ({} card path(s)): {} active display(s), {} connected \
         but undriven",
        n_paths,
        all.len(),
        undriven_total.len()
    );
    if all.is_empty() && undriven_total.is_empty() {
        if let Some(mut r) = scrap::drm_reader::DrmReader::open(None, 0) {
            log::info!("drm: no card enumerated by path; falling back to the auto-detected reader");
            return drm_displays_from_reader(&mut r, "");
        }
    }
    (all, undriven_total)
}

/// 唤醒没有恢复的连接器。可自我推翻：后来被发现处于 DRIVEN 的条目会被移除。
#[cfg(feature = "drm-wake")]
static DRM_WAKE_HOPELESS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[cfg(feature = "drm-wake")]
fn drm_wakeable_undriven(displays: &[DrmDisplayInfo], undriven: &[String]) -> Vec<String> {
    let mut hopeless = DRM_WAKE_HOPELESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !hopeless.is_empty() {
        hopeless.retain(|id| {
            let driven_now = displays
                .iter()
                .any(|d| format!("{}:{}", d.device, d.name) == *id);
            if driven_now {
                log::info!("drm: {id} is scanning out after all; treating it as wakeable again");
            }
            !driven_now
        });
    }
    undriven
        .iter()
        .filter(|id| !hopeless.iter().any(|h| h == *id))
        .cloned()
        .collect()
}

#[cfg(feature = "drm-wake")]
static DRM_LAST_WAKE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "drm-wake")]
static DRM_WAKE_UNAVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 唤醒配置键；`enable-` 是关键：值缺失时读作 `!= "N"`，所以默认为开启。
#[cfg(feature = "drm-wake")]
const OPTION_ENABLE_DRM_DISPLAY_WAKE: &str = "enable-drm-display-wake";

#[cfg(feature = "drm-wake")]
const DRM_WAKE_MIN_GAP: std::time::Duration = std::time::Duration::from_secs(20);
#[cfg(feature = "drm-wake")]
const DRM_WAKE_DEVICE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);
#[cfg(feature = "drm-wake")]
const DRM_WAKE_RECHECK_TOTAL: std::time::Duration = std::time::Duration::from_secs(3);
#[cfg(feature = "drm-wake")]
const DRM_WAKE_SETTLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// 服务启动以来的秒数，单调递增：用 SystemTime 会让时钟跳变重新打开唤醒闸门。
#[cfg(feature = "drm-wake")]
fn drm_wake_clock_secs() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs()
}

/// 看起来像用户活动，这样合成器会重新启用空闲时被禁用的连接器（在它这样做之前，
/// 没有任何扫描输出）。在 T2 的 greeter 上实测：一次相对移动就恢复了 2880x1800 的扫描输出。
#[cfg(feature = "drm-wake")]
fn drm_wake_displays(reason: &str) -> bool {
    use std::sync::atomic::Ordering;

    if DRM_WAKE_UNAVAILABLE.load(Ordering::Relaxed) {
        return false;
    }
    let now = drm_wake_clock_secs();
    loop {
        let last = DRM_LAST_WAKE.load(Ordering::Acquire);
        if last != 0 && now.saturating_sub(last) < DRM_WAKE_MIN_GAP.as_secs() {
            log::debug!(
                "drm: not waking displays ({reason}): a wake {}s ago is still recent",
                now.saturating_sub(last)
            );
            return false;
        }
        if DRM_LAST_WAKE
            .compare_exchange(last, now.max(1), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break;
        }
    }

    // 它必须看起来像一只鼠标：libinput 会忽略只有单个相对轴且没有
    // 按键的设备。实测：REL_X + REL_Y + BTN_LEFT 能唤醒面板；仅 REL_X 不行。
    let mut axes = evdev::AttributeSet::<evdev::RelativeAxisType>::new();
    axes.insert(evdev::RelativeAxisType::REL_X);
    axes.insert(evdev::RelativeAxisType::REL_Y);
    let mut keys = evdev::AttributeSet::<evdev::Key>::new();
    keys.insert(evdev::Key::BTN_LEFT);
    let built = evdev::uinput::VirtualDeviceBuilder::new()
        .and_then(|b| b.name("RustDesk DRM display wake").with_relative_axes(&axes))
        .and_then(|b| b.with_keys(&keys))
        .and_then(|b| b.build());
    let mut dev = match built {
        Ok(d) => d,
        Err(err) => {
            DRM_WAKE_UNAVAILABLE.store(true, Ordering::Relaxed);
            log::warn!(
                "drm: cannot wake displays ({reason}): no uinput device ({err}). A compositor that \
                 disabled its outputs will keep them disabled, so there is no scanout to capture \
                 until something else generates input. Note input injection needs uinput too, so \
                 this session cannot control the host either."
            );
            return false;
        }
    };

    // 新建的 uinput 设备尚未绑定；在 udev 绑定之前写入的事件会丢失。实测
    // 连续两次：有这个暂停时面板会 `disabled -> enabled`，没有则不会。
    std::thread::sleep(DRM_WAKE_DEVICE_SETTLE);

    // 先 +1 再 -1：净位移为零的一次活动。emit() 会自行追加 SYN_REPORT。
    let step = |v: i32| {
        evdev::InputEvent::new(
            evdev::EventType::RELATIVE,
            evdev::RelativeAxisType::REL_X.0,
            v,
        )
    };
    let ok = dev.emit(&[step(1)]).and_then(|_| {
        std::thread::sleep(std::time::Duration::from_millis(120));
        dev.emit(&[step(-1)])
    });
    if let Err(err) = ok {
        log::warn!("drm: display wake ({reason}) failed to emit: {err}");
        return false;
    }
    log::info!("drm: no display was scanning out ({reason}); asked the compositor to wake up");
    true
}

#[cfg(not(feature = "drm-wake"))]
fn drm_enumerate_settled(reason: &str) -> Vec<DrmDisplayInfo> {
    let (displays, undriven) = drm_enumerate_all_displays();
    if !undriven.is_empty() {
        log::debug!(
            "drm: {} connected display(s) have no CRTC ({reason}); this build has no display wake",
            undriven.len()
        );
    }
    displays
}

/// 唤醒时的构建：唤醒一个未被驱动的显示器并等待拓扑稳定。这个等待适用于每一个
/// 唤醒可能仍在进行中的握手，而不只是赢得限速的那一次尝试。
#[cfg(feature = "drm-wake")]
fn drm_enumerate_settled(reason: &str) -> Vec<DrmDisplayInfo> {
    use std::sync::atomic::Ordering;

    let (displays, undriven) = drm_enumerate_all_displays();
    if !hbb_common::config::Config::get_bool_option(OPTION_ENABLE_DRM_DISPLAY_WAKE) {
        if !undriven.is_empty() {
            log::info!(
                "drm: {} connected display(s) have no CRTC ({reason}), but the display wake is \
                 disabled by configuration ({OPTION_ENABLE_DRM_DISPLAY_WAKE}=N)",
                undriven.len()
            );
        }
        return displays;
    }
    let wakeable = drm_wakeable_undriven(&displays, &undriven);
    if wakeable.is_empty() {
        return displays;
    }
    let fired = drm_wake_displays(&format!(
        "{reason} and {n} connected display(s) had no CRTC",
        n = wakeable.len()
    ));
    if !fired {
        if DRM_WAKE_UNAVAILABLE.load(Ordering::Relaxed) {
            return displays;
        }
        let last = DRM_LAST_WAKE.load(Ordering::Acquire);
        if last == 0
            || drm_wake_clock_secs().saturating_sub(last) > DRM_WAKE_SETTLE_WINDOW.as_secs()
        {
            return displays;
        }
    }
    let before_len = displays.len();
    let deadline = std::time::Instant::now() + DRM_WAKE_RECHECK_TOTAL;
    let mut cur = displays;
    let mut cur_wakeable = wakeable;
    while !cur_wakeable.is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (next, next_undriven) = drm_enumerate_all_displays();
        cur_wakeable = drm_wakeable_undriven(&next, &next_undriven);
        cur = next;
    }
    if cur.len() > before_len {
        log::info!(
            "drm: {} display(s) came back after the wake ({} -> {}{})",
            cur.len() - before_len,
            before_len,
            cur.len(),
            if cur_wakeable.is_empty() {
                String::new()
            } else {
                format!(", {} still undriven", cur_wakeable.len())
            }
        );
        schedule_drm_cache_refresh();
    }
    if fired && !cur_wakeable.is_empty() {
        // 只有真正触发（FIRE）的那次握手会锁存；落败者的基线是在切换过程中采集的。
        let mut hopeless = DRM_WAKE_HOPELESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for id in &cur_wakeable {
            if !hopeless.iter().any(|h| h == id) {
                hopeless.push(id.clone());
            }
        }
        log::info!(
            "drm: the wake did not bring back {list}; not asking again for {these} until {it_is} \
             seen scanning out",
            list = cur_wakeable.join(", "),
            these = if cur_wakeable.len() == 1 { "it" } else { "them" },
            it_is = if cur_wakeable.len() == 1 { "it is" } else { "they are" },
        );
    }
    cur
}

/// DRM_DISPLAY_CACHE（+ DRM_DISPLAY_GENERATION）的唯一写入者，不在调用方线程上，且是
/// 单飞（single-flight）：运行期间到达的请求会合并成恰好一次后续运行。
fn schedule_drm_cache_refresh() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    static PENDING: AtomicBool = AtomicBool::new(false);
    // 对 RUNNING 的所有权，在每一种退出路径上都会释放，包括栈展开和启动失败；可在循环中重新取得。
    struct RefreshSlot(bool);
    impl RefreshSlot {
        fn release(&mut self) {
            if self.0 {
                self.0 = false;
                RUNNING.store(false, Ordering::Release);
            }
        }
        fn retake(&mut self) -> bool {
            self.0 = !RUNNING.swap(true, Ordering::AcqRel);
            self.0
        }
    }
    impl Drop for RefreshSlot {
        fn drop(&mut self) {
            self.release();
        }
    }
    // 在尝试运行之前先声明需要刷新，这样正在运行的 worker 一定能看到。
    PENDING.store(true, Ordering::Release);
    if RUNNING.swap(true, Ordering::AcqRel) {
        return; // a worker is already active; it will observe PENDING and refresh again
    }
    let mut slot = RefreshSlot(true);
    let spawned = std::thread::Builder::new()
        .name("drm-cache-refresh".into())
        .spawn(move || loop {
            PENDING.store(false, Ordering::Release);
            let fresh = std::panic::catch_unwind(drm_enumerate_all_displays)
                .unwrap_or_else(|_| {
                    log::error!("drm: display enumeration panicked; treating as no displays");
                    (Vec::new(), Vec::new())
                })
                .0;
            let changed = {
                let mut cache = match DRM_DISPLAY_CACHE.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if *cache != fresh {
                    * cache = fresh;
                    true
                } else {
                    false
                }
            };
            if changed {
                DRM_DISPLAY_GENERATION.fetch_add(1, Ordering::Release);
                log::info!("drm: display cache refreshed (topology changed)");
            }
            // 只有当这次枚举期间没有请求到达时才退出。释放槽位之后的
            // 复查弥补了丢失唤醒的窗口（在释放前一刻刚把 PENDING 置位的请求）。
            if !PENDING.load(Ordering::Acquire) {
                slot.release();
                if !PENDING.load(Ordering::Acquire) {
                    break;
                }
                if !slot.retake() {
                    break; // another caller re-acquired the slot; it will handle the pending refresh
                }
            }
        });
    if let Err(err) = spawned {
        log::error!("drm: could not spawn the display-cache refresh worker: {err}");
    }
}

fn uevent_is_drm_change(msg: &[u8]) -> bool {
    let mut is_drm = false;
    let mut is_change = false;
    for rec in msg.split(|&b| b == 0) {
        if rec == b"SUBSYSTEM=drm" {
            is_drm = true;
        } else if rec == b"ACTION=change" || rec == b"HOTPLUG=1" {
            is_change = true;
        }
    }
    is_drm && is_change
}

/// 在 DRM 热插拔 uevent 时刷新显示器缓存（原生 NETLINK_KOBJECT_UEVENT，不使用 libudev）。
fn drm_udev_listener() {
    use hbb_common::libc;

    let sock = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if sock < 0 {
        log::info!(
            "drm: udev uevent socket unavailable ({}); hotplug refresh disabled",
            std::io::Error::last_os_error()
        );
        return;
    }
    let _owned = unsafe { OwnedFd::from_raw_fd(sock) };
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as u16;
    // 第 1 组 = 内核发起的 uevent（udev 在第 2 组重新广播）；pid 0 表示由内核分配。
    addr.nl_groups = 1;
    let rc = unsafe {
        libc::bind(
            sock,
            &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        log::info!(
            "drm: udev uevent bind failed ({}); hotplug refresh disabled",
            std::io::Error::last_os_error()
        );
        return;
    }
    log::info!("drm: udev DRM-uevent listener started");
    let mut buf = [0u8; 8192];
    loop {
        // 用 recvmsg 而不是 recv：本地进程可能向这个 root 监听器单播一个伪造的 uevent。
        let mut src: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut mhdr: libc::msghdr = unsafe { std::mem::zeroed() };
        mhdr.msg_name = &mut src as *mut libc::sockaddr_nl as *mut libc::c_void;
        mhdr.msg_namelen = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        mhdr.msg_iov = &mut iov;
        mhdr.msg_iovlen = 1;
        let n = unsafe { libc::recvmsg(sock, &mut mhdr, 0) };
        if n <= 0 {
            let err = std::io::Error::last_os_error();
            if n < 0 && err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::info!("drm: udev uevent recv ended ({err}); hotplug refresh stopped");
            break;
        }
        if (mhdr.msg_namelen as usize) < std::mem::size_of::<libc::sockaddr_nl>()
            || src.nl_pid != 0
            || src.nl_groups == 0
        {
            continue;
        }
        if !uevent_is_drm_change(&buf[..n as usize]) {
            continue;
        }
        schedule_drm_cache_refresh();
    }
}

fn drm_prewarm() {
    // 有界地重新询问：`get_display_server()` 在无法判断时回退为 "x11"（实测：
    // 在 Wayland 主机上开机 0.8 秒时就返回 "x11"）。`is_x11_for_drm()` 就是去掉
    // greeter 盲区后的那条路径，登录界面永远不会离开该盲区。
    const PREWARM_SESSION_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);
    const PREWARM_SESSION_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
    let waited = std::time::Instant::now();
    while crate::platform::linux::is_x11_for_drm() {
        if waited.elapsed() >= PREWARM_SESSION_BUDGET {
            log::info!(
                "drm: session still reads as X11 after {:?}; skipping the pre-warm \
                 (the _drm listener still runs)",
                PREWARM_SESSION_BUDGET
            );
            return;
        }
        std::thread::sleep(PREWARM_SESSION_RECHECK);
    }
    let t = std::time::Instant::now();
    schedule_drm_cache_refresh();
    match scrap::drm_reader::DrmReader::open(None, 0) {
        Some(mut r) => {
            // 用 grab_desc() 而不是 grab()：导出 fd 而不必把 libEGL 加载进 root 服务。
            if let Ok((fd, _desc)) = r.grab_desc() {
                drop(fd); // close the warm-up fd; we only wanted to prime the device/import path
            }
            log::info!("drm: pre-warm framebuffer primed in {:?}", t.elapsed());
        }
        None => log::info!("drm: pre-warm skipped (no reader; cache refresh requested)"),
    }
}

/// ROOT `--service` 中的采集生产者：每个消费端一个任务，reader 跑在 worker 线程上。
#[tokio::main(flavor = "current_thread")]
pub async fn start_drm() {
    match new_drm_listener() {
        Ok(mut incoming) => {
            if let Err(err) = std::thread::Builder::new()
                .name("drm-prewarm".into())
                .spawn(drm_prewarm)
            {
                log::warn!("drm: could not spawn the pre-warm thread ({err}); skipping the warmup");
            }
            if let Err(err) = std::thread::Builder::new()
                .name("drm-udev".into())
                .spawn(drm_udev_listener)
            {
                log::warn!(
                    "drm: could not spawn the udev listener ({err}); a mid-session topology change \
                     will not be pushed, and consumers pick it up on their next handshake"
                );
            }
            loop {
                match incoming.next().await {
                    Some(Ok(stream)) => {
                        tokio::spawn(async move {
                            if let Err(err) = handle_drm_conn(Connection::new(stream)).await {
                                log::info!("drm ipc connection ended: {}", err);
                            }
                        });
                    }
                    Some(Err(err)) => log::error!("Couldn't get drm client: {:?}", err),
                    None => {
                        log::error!("drm ipc listener stream ended; stopping drm producer");
                        break;
                    }
                }
            }
        }
        Err(err) => {
            log::error!("Failed to start drm ipc server: {}", err);
        }
    }
}

const MAX_DRM_CONNS: usize = 8;

fn drm_conn_admitted(prev_count: usize) -> bool {
    prev_count < MAX_DRM_CONNS
}

const MAX_DRM_AUTH_IN_FLIGHT: usize = 4;

fn drm_auth_admitted(prev_in_flight: usize) -> bool {
    prev_in_flight < MAX_DRM_AUTH_IN_FLIGHT
}

fn drm_peer_authorized(peer_uid: Option<u32>, active_uid: Option<u32>) -> bool {
    match peer_uid {
        Some(0) => true,
        Some(uid) => active_uid == Some(uid),
        None => false,
    }
}

/// 处理一个 `_drm` 消费端：一个专用 worker 线程持有 `!Send` 的 reader；这个任务负责转发。
async fn handle_drm_conn(stream: Connection) -> ResultType<()> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    // 这是可被全网连接的套接字，所以对端必须在这里被授权（这个监听器绕过了
    // 通用的 `start()` accept 循环）。在阻塞线程池上：缓存未命中会 fork `loginctl`。
    static DRM_AUTH_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    struct DrmAuthGuard;
    impl Drop for DrmAuthGuard {
        fn drop(&mut self) {
            DRM_AUTH_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
    }
    if !drm_auth_admitted(DRM_AUTH_IN_FLIGHT.fetch_add(1, Ordering::SeqCst)) {
        DRM_AUTH_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        // 刻意用 `debug` 而不是 `warn`：任何本地 uid 都能触发，所以一个
        // 每次尝试都会写入服务日志的级别，对那个对端来说就是一个无界的日志写入原语。
        log::debug!("drm: too many _drm authorizations in flight; dropping this connection");
        return Ok(());
    }
    let auth_guard = DrmAuthGuard;
    let (stream, authorized) = tokio::task::spawn_blocking(move || {
        let ok = authorize_service_scoped_ipc_connection(&stream, "_drm");
        (stream, ok)
    })
    .await?;
    drop(auth_guard);
    if !authorized {
        // 这里刻意不打日志：上面的调用已经报告了它——uid 不匹配通过
        // `log_rejected_service_connection` 报告（限速为每 5 秒一行），可执行文件
        // 不匹配则以普通 warn 报告。这里再来一条未限速的 warn 就是同一个无界的
        // 日志写入原语。
        return Ok(());
    }

    static DRM_CONN_COUNT: AtomicUsize = AtomicUsize::new(0);
    struct DrmConnGuard;
    impl Drop for DrmConnGuard {
        fn drop(&mut self) {
            DRM_CONN_COUNT.fetch_sub(1, Ordering::SeqCst);
        }
    }
    if !drm_conn_admitted(DRM_CONN_COUNT.fetch_add(1, Ordering::SeqCst)) {
        DRM_CONN_COUNT.fetch_sub(1, Ordering::SeqCst);
        log::warn!("drm: too many concurrent _drm connections (>= {MAX_DRM_CONNS}); rejecting");
        return Ok(());
    }
    let _conn_guard = DrmConnGuard;

    // 下面会逐帧重新授权：DRM/KMS 采集不是会话作用域的，所以除非码流在
    // 活动会话变化时停止，发起方用户的 --server 会继续接收接入方
    // 用户的屏幕（以及中途的 greeter）。
    let peer_uid = stream.peer_uid();

    let mut conn = dup_to_drm_conn(&stream)?;
    drop(stream);

    let wake = match conn.recv_msg_timeout2(3_000).await {
        Some(Ok((Data::DrmHello { wake }, _fd))) => wake,
        Some(Ok((_, _fd))) => {
            hbb_common::throttled_log!(
                std::time::Duration::from_secs(5),
                info,
                "drm: peer did not open with DrmHello; closing"
            );
            return Ok(());
        }
        Some(Err(e)) => return Err(e),
        None => return Ok(()),
    };

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<DrmProducerMsg>(2);
    let (crtc_tx, crtc_rx) = std::sync::mpsc::channel::<(String, u32, bool)>();
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_guard = DrmStopGuard(stop.clone());
    let worker_stop = stop.clone();
    let frames_gated = Arc::new(AtomicBool::new(false));
    let worker_gate = frames_gated.clone();
    let demand_fps = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let worker_demand = demand_fps.clone();
    std::thread::Builder::new()
        .name("drm-capture".into())
        .spawn(move || {
            drm_capture_worker(
                frame_tx,
                crtc_rx,
                worker_stop,
                worker_gate,
                worker_demand,
                wake,
            )
        })
        .map_err(|err| anyhow::anyhow!("could not spawn the drm capture worker: {err}"))?;

    let displays = match frame_rx.recv().await {
        Some(DrmProducerMsg::Displays(d)) => d,
        _ => {
            log::info!("drm: reader unavailable; closing _drm connection (client falls back)");
            return Ok(());
        }
    };
    conn.send_msg(&Data::DrmDisplayList(displays.clone()), None).await?;

    let (display_idx, need_cpu) = match conn.recv_msg_timeout2(10_000).await {
        Some(Ok((Data::DrmStart { display, need_cpu }, _fd))) => (display, need_cpu),
        Some(Ok((_, _fd))) => {
            log::info!("drm: peer sent something other than DrmStart in the handshake; closing");
            return Ok(());
        }
        Some(Err(e)) => return Err(e),
        None => return Ok(()), // timed out: client never chose a display
    };
    // 拒绝 crtc 0：`open(crtc=0)` 会自动选中第一个处于 ACTIVE 的 CRTC，从而推流错误的显示器。
    let selected = usize::try_from(display_idx)
        .ok()
        .and_then(|i| displays.get(i));
    let target_crtc = selected.map(|d| d.crtc_id).unwrap_or(0);
    let target_device = selected.map(|d| d.device.clone()).unwrap_or_default();
    if target_crtc == 0 {
        log::warn!(
            "drm: client selected display {display_idx} with no bound CRTC; closing _drm (client falls back)"
        );
        return Ok(());
    }
    if crtc_tx.send((target_device, target_crtc, need_cpu)).is_err() {
        return Ok(());
    }

    let mut seen_gen = DRM_DISPLAY_GENERATION.load(Ordering::Acquire);
    const DRM_FRAME_CREDIT: i32 = 2;
    let mut credit: i32 = DRM_FRAME_CREDIT;
    let mut demand: u8 = 0;
    let mut demand_logged: Option<(u8, std::time::Instant)> = None;
    let mut credit_since = std::time::Instant::now();
    let mut held_frame: Option<DrmProducerMsg> = None;
    loop {
        conn.drain_frame_acks(&mut credit, DRM_FRAME_CREDIT, &mut demand)?;
        note_drm_demand(&demand_fps, demand, target_crtc, &mut demand_logged);
        // 被门控期间 worker 不抓帧，因此无法推进自己的 MAX_STALLED 看门狗：一个
        // 只停止确认却不关闭套接字的消费端，否则会一直占用这条连接、
        // 它的 worker 线程和特权 DRM 上下文。
        const CREDIT_STALL: std::time::Duration = std::time::Duration::from_secs(5);
        if credit > 0 {
            credit_since = std::time::Instant::now();
        } else if credit_since.elapsed() > CREDIT_STALL {
            log::info!("drm: consumer has not acked for {CREDIT_STALL:?}; closing _drm connection");
            break;
        }
        // 这里绝不能同时要求已经持有某一帧：那些抓帧会让已持有的帧保持
        // 新鲜（下面是最新优先），所以以“已持有”为门控条件会把手头的那一帧钉住，
        // 在配额耗尽时的那一帧钉住，并在确认到达后送出过期数据。
        frames_gated.store(credit <= 0, Ordering::Relaxed);
        let first: Option<DrmProducerMsg> = if held_frame.is_some() && credit > 0 {
            frame_rx.try_recv().ok()
        } else if credit <= 0 {
            const CREDIT_POLL: std::time::Duration = std::time::Duration::from_secs(1);
            let waited = tokio::time::timeout(CREDIT_POLL, async {
                tokio::select! {
                    biased;
                    r = conn.wait_readable() => r.map(|_| None),
                    m = frame_rx.recv() => Ok(Some(m)),
                }
            })
            .await;
            match waited {
                Err(_) => None,
                Ok(Err(err)) => return Err(err),
                Ok(Ok(None)) => None,
                Ok(Ok(Some(None))) => break,
                Ok(Ok(Some(Some(m)))) => Some(m),
            }
        } else {
            match frame_rx.recv().await {
                Some(f) => Some(f),
                None => break,
            }
        };
        // 用仅查缓存的活动 uid 逐帧重新授权：一次新的查询会 fork `loginctl`，
        // 在这个单线程运行时上拖垮每一条码流。对非 root 对端，未命中即失败即关闭
            // （root 仍被授权；参见 `drm_peer_authorized`）。
        let peer_ok = drm_peer_authorized(peer_uid, active_uid_cached());
        if !peer_ok {
            log::warn!("drm: _drm peer no longer matches the active session (or it is unknown); closing");
            break;
        }
        let gen = DRM_DISPLAY_GENERATION.load(Ordering::Acquire);
        if gen != seen_gen {
            seen_gen = gen;
            let fresh = DRM_DISPLAY_CACHE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            // 即使列表为空也要发送，否则消费端会继续宣告已被移除的显示器。
            conn.send_msg(&Data::DrmDisplaysChanged(fresh), None).await?;
        }
        let mut latest_frame: Option<DrmProducerMsg> = held_frame.take();
        let mut msg = first.or_else(|| frame_rx.try_recv().ok());
        while let Some(m) = msg.take() {
            match m {
                f @ (DrmProducerMsg::Frame { .. } | DrmProducerMsg::FrameCpu { .. }) => {
                    latest_frame = Some(f);
                }
                DrmProducerMsg::Cursor {
                    id,
                    width,
                    height,
                    hotx,
                    hoty,
                    hot_measured,
                    colors,
                } => {
                    conn.send_msg(
                        &Data::DrmCursor {
                            id,
                            width,
                            height,
                            hotx,
                            hoty,
                            hot_measured,
                        },
                        None,
                    )
                    .await?;
                    conn.send_raw(Bytes::from(colors)).await?;
                }
                DrmProducerMsg::Displays(_) => {}
            }
            msg = frame_rx.try_recv().ok();
        }
        conn.drain_frame_acks(&mut credit, DRM_FRAME_CREDIT, &mut demand)?;
        note_drm_demand(&demand_fps, demand, target_crtc, &mut demand_logged);
        if credit <= 0 {
            held_frame = latest_frame;
            continue;
        }
        match latest_frame {
            Some(DrmProducerMsg::Frame { mut desc, fd }) => {
                // 每一帧导出时都带上它的 fd：内核可能把某个 fb_id 回收给另一个
                // 具有相同 geometry/modifier 的缓冲区，而这一侧看不到能区分两者的 dma-buf inode，
                // 所以省掉它可能送出过期的 EGLImage。libdrmtap 的
                // 导入缓存以 fb_id 和 inode 为键，而且只有拿到真实的 fd 才能重新导入。
                let send_fd = fd.is_some();
                desc.has_fd = send_fd;
                let borrowed = if send_fd { fd.as_ref().map(|f| f.as_fd()) } else { None };
                conn.send_msg(&Data::DrmFrameDmabuf(desc), borrowed).await?;
                credit -= 1; // one frame in flight until the consumer acks it
                // `fd`（OwnedFd）无论是否被附加都会在这里关闭（cmsg 已经把它 dup 到了
                // 对端），这把我们的 fd 用量限制在每帧约 1 个在途。
            }
            Some(DrmProducerMsg::FrameCpu {
                width,
                height,
                data,
                plane_rotation,
                cursor_pos,
            }) => {
                conn.send_msg(
                    &Data::DrmFrame {
                        width,
                        height,
                        plane_rotation,
                        cursor_pos,
                    },
                    None,
                )
                .await?;
                conn.send_raw(data).await?;
                credit -= 1; // one frame in flight until the consumer acks it
            }
            _ => {}
        }
    }
    Ok(())
}

/// 当消费方没有报出速率时生产者的 tick 间隔，以及 WouldBlock 的重试周期。
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
/// 为持续无帧（WouldBlock）的时间设上限，这样卡死的设备会结束码流（约 5 秒）。
const MAX_STALLED: u32 = 150;
const _: () = assert!(
    FRAME_INTERVAL.as_millis() * MAX_STALLED as u128 >= 4_500
        && FRAME_INTERVAL.as_millis() * MAX_STALLED as u128 <= 5_500
);
/// 一帧的确认就是一个字节，而每个生产者只统计字节数、从不看它的值，所以一个
/// 同时携带消费端编码速率的字节，对较早的生产者来说仍然恰好是一份配额。
const DRM_ACK_PLAIN: u8 = 0x01;
const DRM_ACK_FPS_FLAG: u8 = 0x80;
/// video_qos 的上限；它能放进那 7 个低位里。
const DRM_MAX_DEMAND_FPS: u8 = 120;

fn drm_ack_byte(fps: u8) -> u8 {
    if fps == 0 {
        DRM_ACK_PLAIN
    } else {
        DRM_ACK_FPS_FLAG | fps.min(DRM_MAX_DEMAND_FPS)
    }
}

fn drm_ack_demand(b: u8) -> Option<u8> {
    let fps = b & !DRM_ACK_FPS_FLAG;
    (b & DRM_ACK_FPS_FLAG != 0 && (1..=DRM_MAX_DEMAND_FPS).contains(&fps)).then_some(fps)
}

/// 以 `num/den` Hz 刷新的面板的一个周期，向上取整，这样节奏控制不会超过它。
fn drm_panel_period(num: u64, den: u64) -> std::time::Duration {
    if num == 0 || den == 0 {
        return std::time::Duration::ZERO;
    }
    let nanos = (u128::from(den) * 1_000_000_000).div_ceil(u128::from(num));
    std::time::Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// 以 `demand_fps` 编码的消费端的抓取周期，绝不会快于面板：它自己
/// 在 30 fps 以上时的周期、从 30 降到 15 期间沿用的旧 33 ms tick，以及低于 15 时取其周期的一半，因此
/// 慢速消费端的帧最多只旧半个周期。
fn drm_pacing_interval(demand_fps: u8, panel: std::time::Duration) -> std::time::Duration {
    if demand_fps == 0 {
        return FRAME_INTERVAL;
    }
    let want = std::time::Duration::from_secs(1) / u32::from(demand_fps);
    want.min((want / 2).max(FRAME_INTERVAL)).max(panel)
}

/// worker 下一次 tick 开始的时间。没有需求时是工作结束后的旧 sleep；
/// 有需求时是以上一次 tick 为基准的截止时间，超时之后则为 `now`。
fn drm_next_tick(
    prev: std::time::Instant,
    now: std::time::Instant,
    demand_fps: u8,
    panel: std::time::Duration,
) -> std::time::Instant {
    if demand_fps == 0 {
        return now + FRAME_INTERVAL;
    }
    (prev + drm_pacing_interval(demand_fps, panel)).max(now)
}

/// 把消费端的速率交给 worker。其中可测量的部分几乎每帧都在变化：
/// 这条连接会记录它的第一个速率，之后最多每十秒记录一个不同的速率。
fn note_drm_demand(
    shared: &std::sync::atomic::AtomicU8,
    demand: u8,
    crtc: u32,
    logged: &mut Option<(u8, std::time::Instant)>,
) {
    shared.store(demand, std::sync::atomic::Ordering::Relaxed);
    if drm_demand_log_due(*logged, demand, std::time::Instant::now()) {
        log::debug!("drm: crtc {crtc}: the consumer encodes at {demand} fps");
        * logged = Some((demand, std::time::Instant::now()));
    }
}

fn drm_demand_log_due(
    logged: Option<(u8, std::time::Instant)>,
    demand: u8,
    now: std::time::Instant,
) -> bool {
    demand != 0
        && logged.map_or(true, |(v, at)| {
            v != demand && now.saturating_duration_since(at) >= std::time::Duration::from_secs(10)
        })
}

/// worker 醒来的时机：为了下一帧，且至少每隔 FRAME_INTERVAL 一次，因为每次
/// tick 还要读取光标，而帧率慢不能把光标也拖慢。
fn drm_worker_wake(now: std::time::Instant, next_frame: std::time::Instant) -> std::time::Instant {
    next_frame.min(now + FRAME_INTERVAL)
}

fn drm_capture_worker(
    frame_tx: tokio::sync::mpsc::Sender<DrmProducerMsg>,
    crtc_rx: std::sync::mpsc::Receiver<(String, u32, bool)>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    frames_gated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    demand_fps: std::sync::Arc<std::sync::atomic::AtomicU8>,
    wake: bool,
) {
    use std::sync::atomic::Ordering;

    let t_conn = std::time::Instant::now();

    // 重新枚举而不是直接用缓存：缓存中的显示器可能已经不再被驱动。
    let displays = if wake {
        drm_enumerate_settled("a consumer connected")
    } else {
        let (displays, _undriven) = drm_enumerate_all_displays();
        // 一旦看到它亮起，一次唤醒失败的连接器就又能被唤醒了。
        #[cfg(feature = "drm-wake")]
        drm_wakeable_undriven(&displays, &_undriven);
        displays
    };
    if frame_tx
        .blocking_send(DrmProducerMsg::Displays(displays))
        .is_err()
    {
        return;
    }

    let (target_device, target_crtc, need_cpu) = match crtc_rx.recv() {
        Ok(c) => c,
        Err(_) => return,
    };
    let device_arg = if target_device.is_empty() {
        None
    } else {
        Some(target_device.as_str())
    };
    let t_open = std::time::Instant::now();
    let mut reader = match scrap::drm_reader::DrmReader::open(device_arg, target_crtc) {
        Some(r) => r,
        None => {
            log::warn!(
                "drm: failed to open crtc {target_crtc} on {}; closing _drm connection",
                if target_device.is_empty() { "auto" } else { &target_device }
            );
            schedule_drm_cache_refresh();
            return;
        }
    };
    schedule_drm_cache_refresh();
    log::debug!(
        "drm: capture reader for crtc {target_crtc} opened in {:?}",
        t_open.elapsed()
    );
    let mut panel = drm_read_panel_period(&mut reader, &target_device, target_crtc, None);
    let mut panel_read_at = std::time::Instant::now();

    static DRM_CONN_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let conn_epoch = DRM_CONN_EPOCH.fetch_add(1, Ordering::Relaxed);

    let mut use_dmabuf = !need_cpu;

    let mut last_cursor_id: u64 = 0;
    let mut stalled: u32 = 0;
    let mut logged_first = false;
    let mut next_frame = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let frame_due = std::time::Instant::now() >= next_frame;
        let mut grabbed: Option<std::io::Result<DrmProducerMsg>> = if !frame_due
            || frames_gated.load(Ordering::Relaxed)
        {
            // `stalled` 保持不变，因为设备是健康的——任务本身会为这种
            // 状态设限（CREDIT_STALL），因为我们的看门狗无法推进。
            None
        } else if use_dmabuf {
            // 在抓取后立刻读取：库给出的是那次抓取所读取的那个平面的答案。
            Some(match reader.grab_desc() {
                Ok((fd, d)) => Ok(DrmProducerMsg::Frame {
                    desc: DmabufDesc {
                        buffer_id: (d.fb_id as u64) | ((conn_epoch as u64) << 32),
                        width: d.width,
                        height: d.height,
                        format: d.format,
                        modifier: d.modifier,
                        fb_id: d.fb_id,
                        num_planes: d.num_planes,
                        offsets: d.offsets,
                        pitches: d.pitches,
                        hdr_eotf: d.hdr_eotf,
                        hdr_max_nits: d.hdr_max_nits,
                        has_fd: true, // every exported frame carries its fd; see the send below
                        plane_rotation: reader.plane_rotation(),
                        cursor_pos: None, // stamped below, from the one cursor read of this tick
                    },
                    fd: Some(fd),
                }),
                Err(err) => Err(err),
            })
        } else {
            // 映射好的缓冲区借用了 reader：先把数据拷贝出来，再取抓取时
            // 记录的旋转。
            let copied = reader
                .grab()
                .map(|(buf, w, h)| (Bytes::copy_from_slice(buf), w as u32, h as u32));
            Some(copied.map(|(data, width, height)| DrmProducerMsg::FrameCpu {
                width,
                height,
                data,
                plane_rotation: reader.plane_rotation(),
                cursor_pos: None,
            }))
        };
        // 每个 tick 只读一次光标，在卡顿或抓取失败时不读：平面位置随
        // 它被读取时相邻的那一帧传递，形状则在变化时才发出。这个位置比
        // 帧新几毫秒；消费端只测量连续数帧保持不变的平面，
        // 所以这种偏移不会进入测量结果。
        let cursor = match &grabbed {
            Some(Err(_)) => None,
            _ => reader.cursor(),
        };
        if let Some(Ok(msg)) = grabbed.as_mut() {
            stamp_cursor_pos(msg, cursor.as_ref());
        }
        match grabbed {
            None => {}
            Some(Ok(msg)) => {
                stalled = 0;
                if !logged_first {
                    logged_first = true;
                    log::debug!(
                        "drm: first frame for crtc {target_crtc} in {:?} ({} path)",
                        t_conn.elapsed(),
                        if use_dmabuf { "dma-buf" } else { "cpu" }
                    );
                }
                if frame_tx.blocking_send(msg).is_err() {
                    break;
                }
            }
            Some(Err(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {
                stalled += 1;
                if stalled > MAX_STALLED {
                    log::info!("drm: capture stalled (no frame); closing _drm connection");
                    break;
                }
                std::thread::sleep(FRAME_INTERVAL);
                // 或者卡顿之后的第一次成功抓帧发现自己的截止时间已过，于是抓了两次。
                next_frame = std::time::Instant::now();
                continue;
            }
            Some(Err(err)) if use_dmabuf && err.kind() == std::io::ErrorKind::Unsupported => {
                log::warn!(
                    "drm: grab_desc unsupported ({err}); switching to CPU-mapped fallback for this connection"
                );
                use_dmabuf = false;
                logged_first = false;
                // 卡顿计数测量的是那条被放弃的路径；给回退路径完整的预算。
                stalled = 0;
                continue;
            }
            Some(Err(err)) => {
                log::warn!("drm: capture error: {err}; closing _drm connection");
                break;
            }
        }

        // 只在光标形状变化时才发送它（id 是内容哈希或隐藏哨兵）。
        if let Some(c) = cursor {
            if c.id != last_cursor_id {
                last_cursor_id = c.id;
                if frame_tx
                    .blocking_send(DrmProducerMsg::Cursor {
                        id: c.id,
                        width: c.width,
                        height: c.height,
                        hotx: c.hotx,
                        hoty: c.hoty,
                        hot_measured: c.hot_measured,
                        colors: c.colors,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }

        if panel_read_at.elapsed() >= std::time::Duration::from_secs(1) {
            panel = drm_read_panel_period(&mut reader, &target_device, target_crtc, Some(panel));
            panel_read_at = std::time::Instant::now();
        }
        let now = std::time::Instant::now();
        // 被配额拦下的帧 tick 会在一个节奏间隔之后重试。
        if frame_due {
            next_frame = drm_next_tick(next_frame, now, demand_fps.load(Ordering::Relaxed), panel);
        }
        std::thread::sleep(drm_worker_wake(now, next_frame).saturating_duration_since(now));
    }
}

/// `crtc` 的面板周期，库能给出时是精确值（libdrmtap 0.5.9，一次 GETCRTC），否则
/// 是上一次枚举所见到的整赫兹刷新率；两者都不知道时为零，这会让节奏控制
/// 交给需求决定。
fn drm_read_panel_period(
    reader: &mut scrap::drm_reader::DrmReader,
    device: &str,
    crtc: u32,
    prev: Option<std::time::Duration>,
) -> std::time::Duration {
    let (period, how) = match reader.crtc_refresh() {
        Some((num, den)) => (drm_panel_period(num, den), format!("{num}/{den} Hz")),
        None => {
            let hz = drm_whole_hz(device, crtc);
            (drm_panel_period(u64::from(hz), 1), format!("{hz} Hz (whole hertz)"))
        }
    };
    if prev != Some(period) {
        log::debug!("drm: crtc {crtc} refreshes at {how}: grabs never outpace {period:?}");
    }
    period
}

fn drm_whole_hz(device: &str, crtc: u32) -> u32 {
    DRM_WHOLE_HZ
        .lock()
        .ok()
        .and_then(|hz| hz.get(&(device.to_owned(), crtc)).copied())
        .unwrap_or(0)
}

/// `_drm` 的辅助数据 fd 传输：`Framed`/`BytesCodec` 无法携带 SCM_RIGHTS cmsg，所以
/// 消息和原始负载使用 4 字节大端长度 + 负载，任何 fd 都绑定在第一个
    /// 字节上。反向方向的帧确认是裸字节，不做帧封装。
pub(crate) struct DrmConn {
    stream: tokio::net::UnixStream,
    read_buf: Vec<u8>,
    /// 当前读取消费掉一个字节后就置位：用于区分虚假的 `readable()` 和帧中途卡顿。
    consumed: bool,
}

const MAX_DRM_JSON_BYTES: usize = 8 * 1024 * 1024;
const DRM_BODY_TIMEOUT_MS: u64 = 5_000;
const DRM_SEND_TIMEOUT_MS: u64 = 5_000;

const MAX_DRM_RAW_BYTES: usize = 512 * 1024 * 1024;
/// `CMSG_SPACE(sizeof(int))` 在我们的目标平台上是 24 字节；64 留有余量，而 `align(8)`
/// 与 `cmsghdr` 的对齐要求一致。
const DRM_CMSG_CAP: usize = 64;

/// 用于 SCM_RIGHTS 控制缓冲区的对齐存储（`msg_control` 必须按 `cmsghdr` 对齐）。
#[repr(align(8))]
struct DrmCmsgBuf([u8; DRM_CMSG_CAP]);

/// 一次非阻塞的 `sendmsg`；只有在存在 fd 时才附加 cmsg（-1 会让该调用失败）。
/// 安全性：`fd` 是有效的已打开套接字 fd，`buf` 是可读切片，`pass_fd`（如果有）是有效的已打开 fd。
unsafe fn drm_sendmsg(fd: RawFd, buf: &[u8], pass_fd: Option<RawFd>) -> std::io::Result<usize> {
    use hbb_common::libc;
    let mut iov = libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let mut cbuf = DrmCmsgBuf([0u8; DRM_CMSG_CAP]);
    if let Some(sfd) = pass_fd {
        msg.msg_control = cbuf.0.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "drm: CMSG_FIRSTHDR null",
            ));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
        let sfd_c: libc::c_int = sfd;
        std::ptr::copy_nonoverlapping(
            &sfd_c as *const libc::c_int as *const u8,
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<libc::c_int>(),
        );
    }
    let n = libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL);
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// 一次非阻塞的 `recvmsg`：最多保留一个 SCM_RIGHTS fd（多余的关闭），并拒绝 MSG_CTRUNC。
/// 安全性：`fd` 必须是有效的已打开套接字 fd；`buf` 是有效的可写切片。
unsafe fn drm_recvmsg(fd: RawFd, buf: &mut [u8]) -> std::io::Result<(usize, Option<OwnedFd>)> {
    use hbb_common::libc;
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut cbuf = DrmCmsgBuf([0u8; DRM_CMSG_CAP]);
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cbuf.0.len() as _;
    let n = libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC);
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut got: Option<OwnedFd> = None;
    let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
    while !cmsg.is_null() {
        if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let data = libc::CMSG_DATA(cmsg);
            let hdr = libc::CMSG_LEN(0) as usize;
            let payload = ((*cmsg).cmsg_len as usize).saturating_sub(hdr);
            let count = payload / std::mem::size_of::<libc::c_int>();
            for i in 0..count {
                let mut rawfd: libc::c_int = -1;
                std::ptr::copy_nonoverlapping(
                    data.add(i * std::mem::size_of::<libc::c_int>()),
                    &mut rawfd as *mut libc::c_int as *mut u8,
                    std::mem::size_of::<libc::c_int>(),
                );
                if rawfd >= 0 {
                    let owned = OwnedFd::from_raw_fd(rawfd);
                    if got.is_none() {
                        got = Some(owned);
                    } // else: surplus fd, dropped here -> closed
                }
            }
        }
        cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        drop(got);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "drm: truncated SCM_RIGHTS control message (MSG_CTRUNC)",
        ));
    }
    Ok((n as usize, got))
}

async fn drm_write_all(
    stream: &tokio::net::UnixStream,
    mut buf: &[u8],
    mut pass_fd: Option<RawFd>,
) -> ResultType<()> {
    // 整个写入共用一个截止时间：每次等待就绪都重新设定它，会让滴流式的对端不断重置。
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(DRM_SEND_TIMEOUT_MS);
    while !buf.is_empty() {
        match tokio::time::timeout_at(deadline, stream.writable()).await {
            Ok(r) => r?,
            Err(_) => bail!(
                "drm: peer did not accept the remaining {} byte(s) within {DRM_SEND_TIMEOUT_MS}ms; closing",
                buf.len()
            ),
        }
        let raw = stream.as_raw_fd();
        let chunk = buf;
        let fd_now = pass_fd;
        match stream.try_io(tokio::io::Interest::WRITABLE, || unsafe {
            drm_sendmsg(raw, chunk, fd_now)
        }) {
            Ok(0) => bail!("drm: socket write returned 0 (peer closed)"),
            Ok(n) => {
                pass_fd = None; // ancillary delivered with these bytes; do not re-send it
                buf = &buf[n..];
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn drm_send_frame(
    stream: &tokio::net::UnixStream,
    payload: &[u8],
    pass_fd: Option<RawFd>,
) -> ResultType<()> {
    if payload.len() > u32::MAX as usize {
        bail!("drm: frame too large ({} bytes)", payload.len());
    }
    let prefix = (payload.len() as u32).to_be_bytes();
    drm_write_all(stream, &prefix, pass_fd).await?;
    drm_write_all(stream, payload, None).await?;
    Ok(())
}

async fn drm_read_full(
    stream: &tokio::net::UnixStream,
    buf: &mut [u8],
    want_cmsg: bool,
    progress: &mut bool,
) -> ResultType<Option<OwnedFd>> {
    use hbb_common::libc;
    let mut off = 0usize;
    let mut got: Option<OwnedFd> = None;
    while off < buf.len() {
        stream.readable().await?;
        let raw = stream.as_raw_fd();
        let use_cmsg = want_cmsg && got.is_none();
        let n = {
            let dst: &mut [u8] = &mut buf[off..];
            match stream.try_io(tokio::io::Interest::READABLE, move || unsafe {
                if use_cmsg {
                    drm_recvmsg(raw, dst)
                } else {
                    let m = libc::read(raw, dst.as_mut_ptr() as *mut libc::c_void, dst.len());
                    if m < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok((m as usize, None))
                    }
                }
            }) {
                Ok((0, _fd)) => bail!("drm: socket closed by peer"),
                Ok((m, fd)) => {
                    if let Some(f) = fd {
                        if got.is_none() {
                            got = Some(f);
                        }
                    }
                    m
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e.into()),
            }
        };
        // 从套接字上读到的任何字节都意味着这一帧已确定：取消无法再通过轮询实现。
        if n > 0 {
            * progress = true;
        }
        off += n;
    }
    Ok(got)
}

impl DrmConn {
    pub fn new(stream: tokio::net::UnixStream) -> Self {
        Self {
            stream,
            read_buf: Vec::new(),
            consumed: false,
        }
    }

    pub async fn send_msg(&mut self, data: &Data, fd: Option<BorrowedFd<'_>>) -> ResultType<()> {
        let payload = serde_json::to_vec(data)?;
        let pass_fd = fd.map(|f| f.as_raw_fd());
        drm_send_frame(&self.stream, &payload, pass_fd).await
    }

    pub async fn send_frame_ack(&self, demand_fps: u8) -> ResultType<()> {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(DRM_SEND_TIMEOUT_MS);
        loop {
            match tokio::time::timeout_at(deadline, self.stream.writable()).await {
                Ok(r) => r?,
                Err(_) => bail!(
                    "drm: _drm frame-ack was not accepted within {DRM_SEND_TIMEOUT_MS}ms; closing"
                ),
            }
            match self.stream.try_write(&[drm_ack_byte(demand_fps)]) {
                Ok(n) if n > 0 => return Ok(()),
                Ok(_) => bail!("drm: _drm frame-ack write returned 0 (peer closed)"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn drain_frame_acks(&self, credit: &mut i32, max: i32, demand: &mut u8) -> ResultType<()> {
        let mut buf = [0u8; 64];
        // 有界："直到 WouldBlock" 是对端的承诺；持续写会对我们造成牵制。
        const MAX_ACK_READS: usize = 64;
        for _ in 0..MAX_ACK_READS {
            match self.stream.try_read(&mut buf) {
                Ok(0) => bail!("drm: _drm frame-ack peer closed"),
                Ok(n) => {
                    * credit = (*credit + n as i32).min(max);
                    if let Some(d) = buf[..n].iter().rev().find_map(|&b| drm_ack_demand(b)) {
                        * demand = d;
                    }
                    if *credit >= max {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub async fn wait_readable(&self) -> ResultType<()> {
        self.stream.readable().await?;
        Ok(())
    }

    pub async fn recv_msg(&mut self) -> ResultType<(Data, Option<OwnedFd>)> {
        self.consumed = false;
        let mut prefix = [0u8; 4];
        let fd = drm_read_full(&self.stream, &mut prefix, true, &mut self.consumed).await?;
        let len = u32::from_be_bytes(prefix) as usize;
        if len > MAX_DRM_JSON_BYTES {
            bail!("drm: message length {len} exceeds cap {MAX_DRM_JSON_BYTES}");
        }
        if self.read_buf.len() < len {
            self.read_buf.resize(len, 0);
        }
        drm_read_full(&self.stream, &mut self.read_buf[..len], false, &mut self.consumed).await?;
        let data: Data = serde_json::from_slice(&self.read_buf[..len])?;
        Ok((data, fd))
    }

    /// 围绕 `recv_msg` 的可取消超时包装。`None` 表示没有消费任何东西，所以重新轮询是
    /// 安全的；过了第一个字节，这一帧就已确定，超时是硬错误。
    pub async fn recv_msg_timeout2(
        &mut self,
        ms_timeout: u64,
    ) -> Option<ResultType<(Data, Option<OwnedFd>)>> {
        let ready = timeout(ms_timeout, self.stream.readable()).await;
        match ready {
            Err(_) => None, // no frame started: clean boundary, caller re-checks `stop`
            Ok(Err(e)) => Some(Err(e.into())),
            Ok(Ok(())) => match timeout(ms_timeout, self.recv_msg()).await {
                Ok(res) => Some(res),
                Err(_) if self.consumed => Some(Err(anyhow::anyhow!(
                    "drm: frame body stalled past {ms_timeout}ms after first byte; closing"
                ))),
                Err(_) => None,
            },
        }
    }

    pub async fn send_raw(&mut self, data: Bytes) -> ResultType<()> {
        drm_send_frame(&self.stream, &data, None).await
    }

    pub async fn next_raw_into(&mut self, out: &mut Vec<u8>) -> ResultType<()> {
        match timeout(DRM_BODY_TIMEOUT_MS, self.next_raw_into_unbounded(out)).await {
            Ok(res) => res,
            Err(_) => bail!(
                "drm: raw body did not arrive within {DRM_BODY_TIMEOUT_MS}ms of its header; closing"
            ),
        }
    }

    async fn next_raw_into_unbounded(&mut self, out: &mut Vec<u8>) -> ResultType<()> {
        let mut prefix = [0u8; 4];
        if drm_read_full(&self.stream, &mut prefix, true, &mut self.consumed)
            .await?
            .is_some()
        {
            log::warn!("drm: unexpected fd on a raw-body frame; dropping");
        }
        let len = u32::from_be_bytes(prefix) as usize;
        if len > MAX_DRM_RAW_BYTES {
            bail!("drm: raw body length {len} exceeds cap {MAX_DRM_RAW_BYTES}");
        }
        out.resize(len, 0);
        drm_read_full(&self.stream, &mut out[..], false, &mut self.consumed).await?;
        Ok(())
    }
}

#[cfg(test)]
mod drm_pacing_tests {
    use super::*;
    use hbb_common::tokio;

    /// 一个确认就是一份配额，与较早的生产者的计数方式相同，而新的生产者也从中读取需求。
    #[tokio::test]
    async fn a_demand_ack_is_one_credit_and_carries_the_demand() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let tx = DrmConn::new(a);
        let rx = DrmConn::new(b);
        tx.send_frame_ack(60).await.unwrap();
        rx.wait_readable().await.unwrap();
        let (mut credit, mut demand) = (0, 0);
        rx.drain_frame_acks(&mut credit, 2, &mut demand).unwrap();
        assert_eq!((credit, demand), (1, 60));
    }

    #[test]
    fn the_pacing_interval_follows_the_demand_with_the_panel_as_floor() {
        let p60 = drm_panel_period(60, 1);
        let p2998 = drm_panel_period(131375, 4382);
        assert_eq!(
            drm_panel_period(148352, 2475),
            std::time::Duration::from_nanos(16_683_294),
            "59.94 Hz, rounded up"
        );
        let cases = [
            (0u8, p60, FRAME_INTERVAL, "no demand: the old tick"),
            (60, p60, p60, "60 on a 60 Hz panel"),
            (120, p60, p60, "more than the panel shows: the panel"),
            (60, p2998, p2998, "a 29.98 Hz panel, exactly"),
        ];
        for (demand, panel, want, why) in cases {
            assert_eq!(drm_pacing_interval(demand, panel), want, "{why}");
        }
    }
}

#[cfg(test)]
mod drm_conn_tests {
    use super::*;
    use hbb_common::libc;
    use hbb_common::tokio::{self, io::AsyncWriteExt};
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[tokio::test]
    async fn roundtrip_msg_with_fd_identity() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let mut tx = DrmConn::new(a);
        let mut rx = DrmConn::new(b);
        let (rd, wr) = pipe();
        tx.send_msg(
            &Data::DrmFrame {
                width: 4,
                height: 4,
                plane_rotation: None,
                cursor_pos: None,
            },
            Some(rd.as_fd()),
        )
            .await
            .unwrap();
        let (_data, fd) = rx.recv_msg().await.unwrap();
        let recv_fd = fd.expect("an fd was attached, it must be received");
        let sentinel = [0xABu8];
        assert_eq!(
            unsafe { libc::write(wr.as_raw_fd(), sentinel.as_ptr() as *const libc::c_void, 1) },
            1
        );
        let mut got = [0u8; 1];
        assert_eq!(
            unsafe { libc::read(recv_fd.as_raw_fd(), got.as_mut_ptr() as *mut libc::c_void, 1) },
            1
        );
        assert_eq!(got[0], 0xAB, "received fd must be the same pipe");
    }

    #[tokio::test]
    async fn roundtrip_raw_body() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let mut tx = DrmConn::new(a);
        let mut rx = DrmConn::new(b);
        let body = Bytes::from(vec![7u8; 5000]);
        tx.send_raw(body.clone()).await.unwrap();
        let mut got = Vec::new();
        rx.next_raw_into(&mut got).await.unwrap();
        assert_eq!(&got[..], &body[..]);
        let short = Bytes::from(vec![9u8; 10]);
        tx.send_raw(short.clone()).await.unwrap();
        rx.next_raw_into(&mut got).await.unwrap();
        assert_eq!(&got[..], &short[..]);
    }

    #[tokio::test]
    async fn rejects_oversized_length_prefix() {
        let (mut a, b) = tokio::net::UnixStream::pair().unwrap();
        let mut rx = DrmConn::new(b);
        let bogus = (MAX_DRM_JSON_BYTES as u32 + 1).to_be_bytes();
        a.write_all(&bogus).await.unwrap();
        let err = rx
            .recv_msg()
            .await
            .err()
            .expect("a length past the cap must be rejected");
        assert!(
            err.to_string().contains("exceeds cap"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn drm_peer_authorized_matrix() {
        assert!(drm_peer_authorized(Some(0), Some(1000)));
        assert!(drm_peer_authorized(Some(0), None));
        assert!(drm_peer_authorized(Some(1000), Some(1000)));
        assert!(!drm_peer_authorized(Some(1000), Some(1001)));
        assert!(!drm_peer_authorized(Some(1000), None));
        assert!(!drm_peer_authorized(None, Some(1000)));
        assert!(!drm_peer_authorized(None, None));
    }

    #[test]
    fn accept_time_exe_match_accepts_only_our_own_executable() {
        let me = std::process::id();
        assert!(
            super::ipc_auth::ensure_peer_executable_matches_current_by_pid_opt(Some(me), "_drm").is_ok(),
            "the test process must match its own executable"
        );

        let mut other = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("/bin/sleep should be spawnable in the test environment");
        // 在子进程完成 exec 之前，/proc/<pid>/exe 仍然指向我们自己的二进制。
        let ours = std::fs::read_link(format!("/proc/{me}/exe")).ok();
        let peer_link = format!("/proc/{}/exe", other.id());
        let mut exec_done = false;
        for _ in 0..200 {
            match std::fs::read_link(&peer_link) {
                Ok(p) if Some(&p) != ours.as_ref() => {
                    exec_done = true;
                    break;
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        let res = if exec_done {
            super::ipc_auth::ensure_peer_executable_matches_current_by_pid_opt(Some(other.id()), "_drm")
        } else {
            Err(anyhow::anyhow!("child never exec'd; nothing was tested"))
        };
        let _ = other.kill();
        let _ = other.wait();
        assert!(exec_done, "the spawned child never exec'd, so the negative case was not exercised");
        assert!(
            res.is_err(),
            "a peer running another executable must be rejected, got {res:?}"
        );

        assert!(super::ipc_auth::ensure_peer_executable_matches_current_by_pid_opt(None, "_drm").is_err());
    }
}
