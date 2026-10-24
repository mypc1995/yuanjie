use super::*;
use hbb_common::{allow_err, anyhow};
use base::platform::linux::DISTRO;
use scrap::{
    is_cursor_embedded, set_map_err,
    wayland::pipewire::{fill_displays, try_fix_logical_size},
    Capturer, Display, Frame, TraitCapturer,
};
use std::collections::HashMap;
use std::io;

use crate::{
    client::{
        SCRAP_OTHER_VERSION_OR_X11_REQUIRED, SCRAP_UBUNTU_HIGHER_REQUIRED,
        SCRAP_X11_REQUIRED, SCRAP_XDP_PORTAL_UNAVAILABLE,
    },
    platform::linux::is_x11,
};

lazy_static::lazy_static! {
    static ref CAP_DISPLAY_INFO: RwLock<HashMap<usize, u64>> = RwLock::new(HashMap::new());
    static ref PIPEWIRE_INITIALIZED: RwLock<bool> = RwLock::new(false);
    static ref LOG_SCRAP_COUNT: Mutex<u32> = Mutex::new(0);
    static ref LAST_STAGE_ERR: Mutex<Option<(String, std::time::Instant)>> = Mutex::new(None);
    static ref ACTIVE_DISPLAY_COUNT: RwLock<usize> = RwLock::new(0);
}

pub fn init() {
    set_map_err(map_err_scrap);
}

pub(super) fn increment_active_display_count() -> usize {
    let mut count = ACTIVE_DISPLAY_COUNT.write().unwrap();
    * count += 1;
    * count
}

pub(super) fn decrement_active_display_count() -> usize {
    let mut count = ACTIVE_DISPLAY_COUNT.write().unwrap();
    if *count > 0 {
        * count -= 1;
    }
    * count
}

fn map_err_scrap(err: String) -> io::Error {
    // 待办：更好地处理错误，不要重启服务
    // 由采集循环触发，这正是这个粗糙自愈机制的目的。门户握手期间无回复
    // 的情况在下方被打上标记并被上报而不是退出：在
    // 登录时有人正等着被告知，而门户激活得慢并不
    // 构成让服务宕掉的理由。
    if err.starts_with("Did not receive a reply") {
        log::error!("Fatal pipewire error, {}", &err);
        std::process::exit(-1);
    }

    if let Some(tag) = err.strip_prefix(WAYLAND_STAGE_TAG) {
        log_staged_once(&err);
        return io::Error::new(
            io::ErrorKind::Other,
            staged_message(tag, is_ubuntu_before_21()),
        );
    }

    if DISTRO.name.to_uppercase() == "Ubuntu".to_uppercase() {
        if DISTRO.version_id < "21".to_owned() {
            io::Error::new(io::ErrorKind::Other, SCRAP_UBUNTU_HIGHER_REQUIRED)
        } else {
            try_log(&err);
            io::Error::new(io::ErrorKind::Other, err)
        }
    } else {
        try_log(&err);
        let err_lower = err.to_ascii_lowercase();
        if err_lower.contains("org.freedesktop.portal")
            || err_lower.contains("dbus")
            || err_lower.contains("d-bus")
        {
            // 门户的 D-Bus 接口不可达。这通常意味着
            // xdg-desktop-portal 已经崩溃…… 更多信息请见：Issue #12897
            io::Error::new(io::ErrorKind::Other, SCRAP_XDP_PORTAL_UNAVAILABLE)
        } else if err_lower.contains("pipewire") {
            io::Error::new(io::ErrorKind::Other, SCRAP_OTHER_VERSION_OR_X11_REQUIRED)
        } else {
            io::Error::new(io::ErrorKind::Other, SCRAP_X11_REQUIRED)
        }
    }
}

/// `Display::all` 和 `Capturer::new` 通过 `map_err_scrap` 把错误传给对端，但
/// `fill_displays` 会自己打开一个门户会话并直接返回其错误，因此
/// 标记必须在这里解析，否则就会原样出现在登录对话框中。
fn map_staged_err(err: anyhow::Error) -> anyhow::Error {
    let text = err.to_string();
    match text.strip_prefix(WAYLAND_STAGE_TAG) {
        Some(tag) => {
            log_staged_once(&text);
            anyhow::anyhow!(staged_message(tag, is_ubuntu_before_21()))
        }
        None => err,
    }
}

// 视频服务大约每秒重试一次，因此卡死的门户否则会每秒都写出一条
// 消息，永远不停。只有当原因发生变化时，或者经过足够长的时间后，才重复该消息，
// 这样阅读者才会想看到故障依然存在。
const STAGE_ERR_REPEAT: std::time::Duration = std::time::Duration::from_secs(600);

fn log_staged_once(err: &str) {
    let now = std::time::Instant::now();
    let mut last = LAST_STAGE_ERR.lock().unwrap();
    let repeat = match last.as_ref() {
        Some((seen, at)) => seen != err || now.duration_since(*at) >= STAGE_ERR_REPEAT,
        None => true,
    };
    if repeat {
        log::error!("Wayland portal handshake failed: {}", err);
        * last = Some((err.to_owned(), now));
    }
}

fn try_log(err: &String) {
    let mut lock_count = LOG_SCRAP_COUNT.lock().unwrap();
    if *lock_count >= 1000000 {
        return;
    }
    if *lock_count % 10000 == 0 {
        log::error!("Failed scrap {}", err);
    }
    * lock_count += 1;
}

// 翻译键，因此键本身就是英文文本：从未听说过这些键的旧版对端
// 会退回到直接显示该键，读起来仍像一句话。
const WAYLAND_DECLINED: &str = "The screen sharing request was declined on the remote device";
const WAYLAND_TIMED_OUT: &str = "The screen sharing request timed out on the remote device";
const WAYLAND_NO_SESSION: &str = "RustDesk cannot reach the desktop session on the remote device, check that a desktop session is running and that RustDesk can use it";
const WAYLAND_UNSUPPORTED: &str = "The desktop portal on the remote device is missing a capability needed for screen sharing or remote control, its backend may not be installed";
const WAYLAND_PIPEWIRE_HANDOVER: &str = "Screen sharing was approved on the remote device, but the PipeWire connection could not be opened";
const WAYLAND_ENDED: &str =
    "The screen sharing request ended without completing on the remote device";
// 它所替换的那条消息原本携带的解决办法，去掉了链接：这就是
// rustdesk/rustdesk#8600 所讨论的内容。
const WAYLAND_NO_USABLE_SCREEN: &str = "RustDesk could not obtain a usable screen from the XDG Desktop Portal, the PipeWire library may be too old";
const WAYLAND_GST_UNAVAILABLE: &str =
    "RustDesk could not load a GStreamer component needed for screen capture ({})";

const WAYLAND_STAGE_TAG: &str = "wl-stage:";

// 对端的 `translate()` 会自行去掉大括号，所以线上传输的是键
// 而详情仍然 *位于* 占位符内部。
fn with_detail(key: &str, detail: &str) -> String {
    key.replace("{}", &format!("{{{}}}", detail))
}

fn is_ubuntu_before_21() -> bool {
    DISTRO.name.to_uppercase() == "Ubuntu".to_uppercase() && DISTRO.version_id < "21".to_owned()
}

/// 把门户握手传来的 `<stage>:<kind>:<detail>` 标记映射为应告知对端的内容，参见
/// `scrap::wayland::pipewire`。采集循环报告的一切内容
/// 都不带标记，沿用上面的旧版子串启发式判断。
fn staged_message(tag: &str, ubuntu_before_21: bool) -> String {
    let mut parts = tag.splitn(3, ':');
    let stage = parts.next().unwrap_or_default();
    let kind = parts.next().unwrap_or_default();
    let detail = parts.next().unwrap_or_default().trim();

    // 说明机器自身状况的结果正是 Ubuntu 分支被编写的原因，
    // 因此该分支保留了它。而说明某个人做了什么的结果，是任何发行版
    // 的检查都无法改进的事实。
    let of_the_machine = |msg: &str| {
        if ubuntu_before_21 {
            SCRAP_UBUNTU_HIGHER_REQUIRED.to_owned()
        } else {
            msg.to_owned()
        }
    };

    match (stage, kind) {
        (_, "declined") => WAYLAND_DECLINED.to_owned(),
        (_, "ended") => WAYLAND_ENDED.to_owned(),
        (_, "no-response") => WAYLAND_TIMED_OUT.to_owned(),
        ("streams", _) => of_the_machine(WAYLAND_NO_USABLE_SCREEN),
        ("gst-plugin", _) => of_the_machine(&with_detail(WAYLAND_GST_UNAVAILABLE, detail)),
        // 门户所在的总线根本没有被连通，因此还没有向门户发出过请求，
        // 此时让任何人去重启它都只是猜测。
        ("session-bus", _) => of_the_machine(WAYLAND_NO_SESSION),
        // 门户回答了 `Start`，说明请求已获批准，剩下的唯一事情
        // 就是把 PipeWire 连接交出去。不管出了什么问题，都不是
        // 门户不可用 —— 它刚刚才回复过。
        ("open-pipewire-remote", _) => of_the_machine(WAYLAND_PIPEWIRE_HANDOVER),
        // 门户在那里而且有响应；只是没有实现被调用的功能，
        // 而重启它无法解决这个问题。
        (_, "unsupported") => of_the_machine(WAYLAND_UNSUPPORTED),
        // 其他所有情况都是门户没有交付内容，这正是该键所说的 ——
        // 而且与我们自己写的消息不同，它还携带了 `systemctl --user restart` 这个解决办法。
        _ => of_the_machine(SCRAP_XDP_PORTAL_UNAVAILABLE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_message_names_the_outcome() {
        let m = |tag| staged_message(tag, false);
        assert_eq!(m("start:declined:"), WAYLAND_DECLINED);
        assert_eq!(m("start:ended:"), WAYLAND_ENDED);
        assert_eq!(m("start:no-response:"), WAYLAND_TIMED_OUT);
        // 已恢复的会话根本不会显示选择器，所以任何位置的超时都是超时，而
        // 绝不是关于某人没有回应的主张。
        assert_eq!(m("create-session:no-response:"), WAYLAND_TIMED_OUT);
        assert_eq!(m("streams:empty:"), WAYLAND_NO_USABLE_SCREEN);
    }

    // 对端通过把消息中第一个 `{...}` 替换成 `{}` 并查找该
    // 内容来解析消息，因此携带详情的消息必须精确地还原为它的键。
    #[test]
    fn a_detail_carrying_message_reduces_back_to_its_key() {
        let gst = staged_message("gst-plugin:unavailable:pipewiresrc", false);
        assert_eq!(
            gst,
            "RustDesk could not load a GStreamer component needed for screen capture ({pipewiresrc})"
        );
        let open = gst.find('{').expect("no placeholder");
        let close = gst[open..].find('}').expect("unclosed placeholder") + open;
        assert_eq!(
            format!("{}{{}}{}", &gst[..open], &gst[close + 1..]),
            WAYLAND_GST_UNAVAILABLE
        );
    }
}

struct CapturerPtr(*mut Capturer);

impl Clone for CapturerPtr {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl TraitCapturer for CapturerPtr {
    fn frame<'a>(&'a mut self, timeout: std::time::Duration) -> std::io::Result<Frame<'a>> {
        unsafe { (*self.0).frame(timeout) }
    }
}

struct CapDisplayInfo {
    rects: Vec<((i32, i32), usize, usize)>,
    displays: Vec<DisplayInfo>,
    num: usize,
    primary: usize,
    current: usize,
    capturer: CapturerPtr,
}

/// 从 DRM 显示器列表得到的 Uinput 桌面矩形，用于没有合成器可以
/// 被询问的登录屏幕。`(minx, maxx, miny, maxy)`，以最终显示方向的物理像素计（旋转过的
/// 输出按转置计算，与其帧一致）：这里没有合成器应用缩放，因此
/// 与 `desktop_rect_of` 不同，这里没有需要处理的逻辑尺寸。
#[cfg(feature = "drm")]
fn drm_desktop_rect_for_uinput() -> Option<(i32, i32, i32, i32)> {
    let displays = super::drm_capturer::get_display_infos()?;
    if displays.is_empty() {
        return None;
    }
    let minx = displays.iter().map(|d| d.x).min()?;
    let miny = displays.iter().map(|d| d.y).min()?;
    let maxx = displays.iter().map(|d| d.x + d.width).max()?;
    let maxy = displays.iter().map(|d| d.y + d.height).max()?;
    if maxx <= minx || maxy <= miny {
        return None;
    }
    Some((minx, maxx, miny, maxy))
}

/// 把 uinput 绝对指针范围设置为整个逻辑桌面，这样合成器就能 1:1 映射
/// 注入的坐标，而不是把单显示器的范围拉伸到所有输出上。PipeWire
/// 路径在 `check_init` 中内联做了这件事；DRM 路径绕过了 check_init，因此它必须
/// 也这样做，否则在多显示器主机上注入的指针会落到错误的输出上 —— 而
/// 硬件光标（它位于指针所在的 CRTC 上）也永远不会出现在被采集的
/// CRTC 上（也就是「光标不可见」的现象）。它从 Wayland 输出读取布局，因此与
/// 采集后端无关。
///
/// 这是 DRM 路径中唯一一份 `check_init` 为 PipeWire 内联所做之事的副本，它做的是
/// 同样的三件事，原因也相同：
///
/// - 首先丢弃缓存的 Wayland 布局，因为它可能早于没有会话期间发生的合成器变更
///   （rustdesk#15601），而且在热插拔路径上它按定义就是过期的；
/// - 限制 IPC 等待时间，因为 `uinput::client::set_resolution` 读取回复时自身没有任何超时，
///   否则一个卡死的 uinput socket 会阻塞该分支上视频服务的每一次启动，
///   并把热插拔工作线程卡在 `rt.block_on` 里，使 `UINPUT_REFRESH_BUSY` 一直保持为 true，于是
///   之后进程生命周期内的每次热插拔刷新都会被静默跳过；
/// - 记录已应用的矩形，并对按显示器的布局基线拍快照，这正是启用
///   #15601 漂移重映射的开关。没有它，重映射在 DRM 路径上根本不会被激活。
///
/// 它之所以保持为独立副本而没有并入 `check_init`，是因为 `check_init` 会随
/// 每个 Linux 构建一起发布，而这项特性不能改动未启用 drm 的构建哪怕一行。
#[cfg(feature = "drm")]
pub(super) async fn update_uinput_resolution() {
    if !crate::input_service::wayland_use_uinput() {
        return;
    }
    // 在登录屏幕上同样优先使用合成器：登录界面本身就运行着合成器，而 hbb_common socket
    // 回退方案在没有任何环境变量的情况下也能到达它。DRM 并集才是回退方案，而它
    // 在多显示器主机上落到那里确实会造成损失：DRM 没有原点，因此它的并集矩形
    // 在合成器把各输出并排排列时会导致指针映射错误。
    //
    // 移出执行器：合成器查询可能会阻塞到 socket 探测的超时时间，而这
    // 运行在 current-thread 运行时上（会话初始化和热插拔工作线程）。布局基线
    // 在同一个任务中计算：查找失败不会被缓存，因此之后再索取矩形
    // 就会同步重跑整个 socket 探测。
    //
    // 在计算矩形之前读取的 generation 用于标注 DRM 光标的
    // 校准范围；下面的原始 DRM 并集不是合成器布局，因此不带任何标注。
    let gen = scrap::wayland::display::wayland_snapshot_generation();
    let (rect, layout) = match hbb_common::tokio::task::spawn_blocking(|| {
        scrap::wayland::display::clear_wayland_displays_cache();
        match scrap::wayland::display::get_desktop_rect_for_uinput() {
            // 上面的查找刚刚缓存了显示器，因此矩形来自该快照。
            Some(rect) => Some((rect, scrap::wayland::display::get_display_rects_for_uinput())),
            // 原始 DRM 并集：没有合成器布局可作为基线。留空会让 #15601
            // 重映射保持不激活，反正原点未知时这样才对。
            None => drm_desktop_rect_for_uinput().map(|rect| (rect, Vec::new())),
        }
    })
    .await
    {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            log::warn!("Failed to get desktop rect for uinput");
            return;
        }
        Err(err) => {
            log::warn!("The desktop rect probe task failed: {err}");
            return;
        }
    };
    let gen = super::display_service::input_map_label(gen, !layout.is_empty());
    // 每次调用都重新对基线拍快照：这会在会话初始化时以及每次热插拔后运行，而
    // 基线正是客户端坐标的度量依据。
    let snapshot_layout = move || {
        super::display_service::set_wayland_layout_baseline(layout.clone());
    };
    // 只有范围真正变化时才重新配置设备。卡在重建循环中的显示器
    // 大约每秒就会调用一次这里，而重新应用完全相同的范围意味着一次 IPC 往返，外加
    // 在可能正坐在控制台前的用户之下重新配置 uinput 设备。
    let job = move |rt: &tokio::runtime::Runtime| {
        if super::display_service::wayland_uinput_rect() == Some(rect) {
            super::display_service::note_input_map_ready(gen);
            snapshot_layout();
            return;
        }
        let (minx, maxx, miny, maxy) = rect;
        log::info!("update mouse resolution: ({minx}, {maxx}), ({miny}, {maxy})");
        super::display_service::note_input_map_unknown();
        match rt.block_on(async {
            timeout(
                3_000,
                input_service::update_mouse_resolution(minx, maxx, miny, maxy),
            )
            .await
        }) {
            // 只在成功应用之后才记录矩形，这样瞬时失败会在
            // 下一次调用时被重试，而不是被记为已应用。
            Ok(Ok(())) => {
                super::display_service::set_wayland_uinput_rect(rect);
                super::display_service::note_input_map_adopted(gen);
                snapshot_layout();
            }
            Ok(Err(err)) => log::error!("Failed to update mouse resolution: {}", err),
            Err(err) => log::error!("Failed to update mouse resolution: {}", err),
        }
    };
    if super::display_service::run_uinput_apply(job).await.is_err() {
        hbb_common::throttled_log!(
            super::display_service::UINPUT_APPLY_LOG_INTERVAL,
            error,
            "Failed to update mouse resolution: the uinput apply thread is gone"
        );
    }
}

#[tokio::main(flavor = "current_thread")]
pub(super) async fn ensure_inited() -> ResultType<()> {
    // DRM/KMS 采集（需显式开启）：root 服务持有读取端，采集器通过 IPC 自行初始化，
    // 因此这里没有需要初始化的 PipeWire 录制器。但我们仍必须设置 uinput 的
    // 桌面矩形（check_init 在 PipeWire 路径上会做这件事，而 DRM 路径跳过了 check_init）。
    #[cfg(feature = "drm")]
    if super::drm_capturer::is_available_cached() {
        update_uinput_resolution().await;
        return Ok(());
    }
    check_init().await
}

pub(super) fn is_inited() -> Option<Message> {
    if is_x11() {
        None
    } else {
        #[cfg(feature = "drm")]
        if super::drm_capturer::is_available_cached() {
            return None;
        }
        // try_read：等待门户选择器的初始化会持有写锁，而这会在
        // 每次桌面登录时运行。忙碌（即那个初始化，或 clear()）时回答尚未初始化。
        if !matches!(CAP_DISPLAY_INFO.try_read(), Ok(map) if !map.is_empty()) {
            let mut msg_out = Message::new();
            let res = MessageBox {
                msgtype: "nook-nocancel-hasclose".to_owned(),
                title: "Wayland".to_owned(),
                text: "Please Select the screen to be shared(Operate on the peer side).".to_owned(),
                link: "".to_owned(),
                ..Default::default()
            };
            msg_out.set_message_box(res);
            Some(msg_out)
        } else {
            None
        }
    }
}

pub(super) async fn check_init() -> ResultType<()> {
    if !is_x11() {
        // try_read 与 is_inited 出于同样的原因；忙碌时，同样的查询会在工作线程之外等待。
        let looked = CAP_DISPLAY_INFO.try_read().map(|map| map.is_empty()).ok();
        let empty = match looked {
            Some(empty) => empty,
            None => hbb_common::tokio::task::spawn_blocking(|| {
                CAP_DISPLAY_INFO.read().unwrap().is_empty()
            })
            .await
            .map_err(|err| anyhow::anyhow!("Wayland display state task failed: {err}"))?,
        };
        if empty {
            if crate::input_service::wayland_use_uinput() {
                // 缓存的布局可能早于没有会话期间发生的合成器变更，
                // https://github.com/rustdesk/rustdesk/issues/15601
                // 在矩形之前读取：与布局移动发生竞争的 apply 会按旧的
                // generation 记录，而只有移动之前构建的流才依据它度量。
                #[cfg(feature = "drm")]
                let gen = scrap::wayland::display::wayland_snapshot_generation();
                scrap::wayland::display::clear_wayland_displays_cache();
                if let Some((minx, maxx, miny, maxy)) =
                    scrap::wayland::display::get_desktop_rect_for_uinput()
                {
                    log::info!(
                        "update mouse resolution: ({}, {}), ({}, {})",
                        minx,
                        maxx,
                        miny,
                        maxy
                    );
                    // 在 DRM 会话中回退到 PipeWire 的显示器也会执行这次 apply
                    // ：它会走 uinput 的 apply 线程，与 DRM 路径轮流执行。
                    #[cfg(feature = "drm")]
                    {
                        let job = move |rt: &tokio::runtime::Runtime| {
                            super::display_service::note_input_map_unknown();
                            match rt.block_on(async {
                                timeout(
                                    3_000,
                                    input_service::update_mouse_resolution(minx, maxx, miny, maxy),
                                )
                                .await
                            }) {
                                Ok(Ok(())) => {
                                    super::display_service::set_wayland_uinput_rect((
                                        minx, maxx, miny, maxy,
                                    ));
                                    super::display_service::note_input_map_adopted(gen);
                                    // 对客户端坐标所依据的按显示器布局
                                    // 拍快照，这样在合成器于会话中途移动显示器时，鼠标路径可以修正它们。
                                    // （合成器在会话中途移动了显示器。）
                                    super::display_service::set_wayland_layout_baseline(
                                        scrap::wayland::display::get_display_rects_for_uinput(),
                                    );
                                }
                                Ok(Err(err)) => {
                                    log::error!("Failed to update mouse resolution: {}", err)
                                }
                                Err(err) => log::error!("Failed to update mouse resolution: {}", err),
                            }
                        };
                        if super::display_service::run_uinput_apply(job).await.is_err() {
                            hbb_common::throttled_log!(
                                super::display_service::UINPUT_APPLY_LOG_INTERVAL,
                                error,
                                "Failed to update mouse resolution: the uinput apply thread is gone"
                            );
                        }
                    }
                    // 像周期性刷新那样限制 IPC 等待时间，这样一个卡死的
                    // 响应不会拖住会话初始化。
                    #[cfg(not(feature = "drm"))]
                    match timeout(
                        3_000,
                        input_service::update_mouse_resolution(minx, maxx, miny, maxy),
                    )
                    .await
                    {
                        Ok(Ok(())) => {
                            super::display_service::set_wayland_uinput_rect((
                                minx, maxx, miny, maxy,
                            ));
                            // 对客户端坐标所依据的按显示器布局
                            // 拍快照，这样在合成器于会话中途移动显示器时，鼠标路径可以修正它们。
                            // （合成器在会话中途移动了显示器。）
                            super::display_service::set_wayland_layout_baseline(
                                scrap::wayland::display::get_display_rects_for_uinput(),
                            );
                        }
                        Ok(Err(err)) => log::error!("Failed to update mouse resolution: {}", err),
                        Err(err) => log::error!("Failed to update mouse resolution: {}", err),
                    }
                } else {
                    log::warn!("Failed to get desktop rect for uinput");
                }
            }

            // 门户可能会在持有写锁的情况下等待用户数分钟来回应它的选择器：
            // 它把各次初始化串行化，并与 clear() 排序。在阻塞线程上等待，这样不会有
            // tokio 工作线程在此期间被挂起。
            hbb_common::tokio::task::spawn_blocking(init_displays)
                .await
                .map_err(|err| anyhow::anyhow!("Wayland display init task failed: {err}"))??;
        }
    }
    Ok(())
}

/// 处于 CAP_DISPLAY_INFO 写锁之下的那部分 `check_init`，包括门户的选择器打开时
/// 等待用户的这段时间。是阻塞的。
fn init_displays() -> ResultType<()> {
    let mut lock = CAP_DISPLAY_INFO.write().unwrap();
    if lock.is_empty() {
        // 检查 PipeWire 是否已初始化，以避免重复创建录制器
        if *PIPEWIRE_INITIALIZED.read().unwrap() {
            log::warn!("wayland_diag: Preventing duplicate PipeWire initialization");
            return Ok(());
        }

        let mut all = Display::all()?;
        log::debug!("Initializing displays with fill_displays()");
        {
            let temp_mouse_move_handle = input_service::TemporaryMouseMoveHandle::new();
            let move_mouse_to = |x, y| temp_mouse_move_handle.move_mouse_to(x, y);
            fill_displays(move_mouse_to, crate::get_cursor_pos, &mut all)
                .map_err(map_staged_err)?;
        }
        log::debug!("Attempting to fix logical size with try_fix_logical_size()");
        try_fix_logical_size(&mut all);
        * PIPEWIRE_INITIALIZED.write().unwrap() = true;
        let num = all.len();
        let primary = super::display_service::get_primary_2(&all);
        let mut displays = super::display_service::update_sync_displays(&all);
        for display in displays.iter_mut() {
            display.cursor_embedded = is_cursor_embedded();
        }

        let mut rects: Vec<((i32, i32), usize, usize)> = Vec::new();
        for d in &all {
            rects.push((d.origin(), d.width(), d.height()));
        }

        log::debug!(
            "#displays={}, primary={}, rects: {:?}, cpus={}/{}",
            num,
            primary,
            rects,
            num_cpus::get_physical(),
            num_cpus::get()
        );

        // 为每个显示器创建各自的 CapDisplayInfo，各带自己的采集器
        for (idx, display) in all.into_iter().enumerate() {
            // 这里不用 `with_context`：对端看到的是 `format!("{}", err)`，它
            // 只渲染最外层，而映射出的原因是内层那个。
            let capturer = Box::into_raw(Box::new(Capturer::new(display)?));
            let capturer = CapturerPtr(capturer);

            let cap_display_info = Box::into_raw(Box::new(CapDisplayInfo {
                rects: rects.clone(),
                displays: displays.clone(),
                num,
                primary,
                current: idx,
                capturer,
            }));

            lock.insert(idx, cap_display_info as u64);
        }
    }
    Ok(())
}

pub(super) async fn get_displays_and_primary() -> ResultType<(Vec<DisplayInfo>, usize)> {
    #[cfg(feature = "drm")]
    if super::drm_capturer::is_available_cached() {
        // 该函数每次登录运行一次（update_get_sync_displays_on_login 是它唯一的
        // 调用方），而登录正是向客户端「承诺」显示器列表的时刻 —— 因此要
        // 先通过一次实时的 `_drm` 握手刷新该列表。服务会唤醒处于休眠状态的显示器，并
        // 以稳定后的真实情况作答，这正是让一台无人值守、且某个
        // 处于 DISABLED 状态的面板仍可连接的机器变得可行的原因：缓存的列表要么漏掉该面板（在
        // 休眠时探测），要么宣示一个背后没有输出画面的显示器（在唤醒时探测），而
        // 无论哪种情况，随后在采集握手中触发的唤醒都会改变客户端
        // 已经拿到的列表。完全异步，因此执行器永远不会被阻塞；在任何
        // 失败情况下，缓存仍像以前一样提供服务。
        super::drm_capturer::refresh_displays_for_login().await;
        let snapshot = hbb_common::tokio::task::spawn_blocking(
            super::drm_capturer::get_display_infos_and_primary,
        )
        .await
        .map_err(|err| anyhow::anyhow!("Wayland display probe task failed: {err}"))?;
        if let Some(snapshot) = snapshot {
            return Ok(snapshot);
        }
    }
    check_init().await?;
    // 读取可能会等待此后由另一个线程启动的初始化：同样要移出工作线程。
    hbb_common::tokio::task::spawn_blocking(|| -> ResultType<(Vec<DisplayInfo>, usize)> {
        // 保持一个读守卫，这样 clear/重新初始化就不会把这些拆分到不同的缓存快照中。
        let cap_map = CAP_DISPLAY_INFO.read().unwrap();
        if let Some(addr) = cap_map.values().next() {
            let cap_display_info: *const CapDisplayInfo = *addr as _;
            unsafe {
                let cap_display_info = &*cap_display_info;
                Ok((cap_display_info.displays.clone(), cap_display_info.primary))
            }
        } else {
            bail!("Failed to get capturer display info");
        }
    })
    .await
    .map_err(|err| anyhow::anyhow!("Wayland display info task failed: {err}"))?
}

pub fn clear() {
    if is_x11() {
        return;
    }
    // DRM 路径会用合成器的 Wayland 输出（逻辑原点 +
    // 缩放）来补充其几何信息，而 scrap 在进程范围内缓存这些数据。PipeWire 路径会在会话关闭时清除该缓存，
    // 但 DRM 路径不会打开任何 PipeWire 会话，因此若没有这一步，它会继续用
    // 过期几何信息去匹配 DRM 输出（在显示器热插拔/旋转/缩放变化之后）。在拆卸时将其失效，
    // 这样下一次会话会重新读取最新几何信息（惰性地，在下次枚举时）并实现自愈。
    #[cfg(feature = "drm")]
    if super::drm_capturer::is_available_cached() {
        scrap::wayland::display::clear_wayland_displays_cache();
    }
    // NOTE: intentionally do NOT reset the DRM probe cache here. `clear()` runs on every capturer
    // 拆卸（每次视频服务重启都会发生），而从异步路径重新探测 `_drm`
    // 会阻塞执行器足够长的时间，从而触发「deadline has elapsed」并陷入
    // 重启循环。DRM 的可用性在服务启动时即已确定，因此缓存始终有效。
    let mut write_lock = CAP_DISPLAY_INFO.write().unwrap();
    for (_, addr) in write_lock.iter() {
        let cap_display_info: *mut CapDisplayInfo = *addr as _;
        unsafe {
            let _box_capturer = Box::from_raw((*cap_display_info).capturer.0);
            let _box_cap_display_info = Box::from_raw(cap_display_info);
        }
    }
    write_lock.clear();

    // 重置 PipeWire 初始化标志，以便下次初始化时重新创建
    * PIPEWIRE_INITIALIZED.write().unwrap() = false;
}

/// 最后一个远程连接断开时的门户会话关闭（`AuthedConnID::drop`）。门户
/// 请求在等待用户时最多会占用会话槽位 3 分钟，而该 drop 可能会在
/// tokio 工作线程上运行：关闭操作在自己的线程上等待，且同一时刻只会有一个在运行。
pub(super) fn try_close_portal_session() {
    static CLOSER: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
    // 没有会话也没有待处理的请求（drm、x11、已经关闭）：无需等待任何东西。
    if matches!(scrap::wayland::pipewire::RDP_SESSION_INFO.try_lock(), Ok(info) if info.is_none()) {
        return;
    }
    let mut closer = CLOSER.lock().unwrap();
    // 仍有一个关闭操作在运行，就覆盖了这一情况。
    if matches!(&*closer, Some(close) if !close.is_finished()) {
        return;
    }
    match std::thread::Builder::new()
        .name("portal-session-close".into())
        .spawn(|| {
            // 这个 drop 在本线程运行之前已被计数：再计一次。
            if !AUTHED_CONNS
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.conn_type == AuthConnType::Remote)
            {
                scrap::wayland::pipewire::try_close_session();
            }
        }) {
        Ok(close) => *closer = Some(close),
        // 对端可能以任意速率断开连接。
        Err(err) => hbb_common::throttled_log!(
            std::time::Duration::from_secs(60),
            error,
            "Failed to spawn the portal session close: {}",
            err
        ),
    }
}

/// 从普通的（同步）视频线程初始化 PipeWire/门户采集路径，这样无法被采集的 DRM 显示器
/// 可以对那一个显示器回退到 PipeWire。只要 DRM 全局可用，`ensure_inited` 就会短路
/// 进入 DRM 分支，因此它从不运行 `check_init`；这个辅助函数
/// 直接驱动同样的异步门户 ScreenCast 初始化（与 `ensure_inited` 的做法一致）。之所以需要它，
/// 是因为 `is_available()` 是一个全局结论 —— 对于仍在正常工作的 DRM 输出它始终为 true —— 因此
/// 若没有按显示器回退的机制，某个失败/被降级的 DRM 显示器就会让视频服务陷入重启循环，
/// 而不是仅仅对它自己降级到 PipeWire。
#[cfg(feature = "drm")]
#[tokio::main(flavor = "current_thread")]
async fn ensure_pipewire_inited() -> ResultType<()> {
    check_init().await
}

pub(super) fn get_capturer_for_display(
    display_idx: usize,
) -> ResultType<super::video_service::CapturerInfo> {
    if is_x11() {
        bail!("Do not call this function if not wayland");
    }
    // DRM/KMS 采集路径：直接从服务的 `_drm` 流构建采集器，完全绕过
    // PipeWire 的 CAP_DISPLAY_INFO 机制。`is_available()` 是全局结论，因此某个
    // 按显示器的 DRM 失败（无法抓取的/被降级的 CRTC，或者在阶段 2 拆分之后，一个
    // 缺少 render node 的 seat，或非特权一侧的转换失败）绝不能向外传播，
    // 也不能让这个按显示器的视频服务陷入重启循环。而是仅针对这个显示器
    // 穿透到 PipeWire；其他 DRM 输出仍继续通过 DRM 推流。
    // 和登录时的处理一样，这个门控有意保留探测式写法：它在视频线程上运行，
    // 而不是异步执行器上，并且它处于采集构建路径上，因此一个确定性的结论值得
    // 在这里花费几秒。与登录时的处理一起，它让冷缓存变得可恢复 —— warm_availability
    // 在尝试若干次后就放弃，因此如果所有门控都只查缓存，那么一个在
    // root 服务之前启动的 --server 此生都再也不会看到 DRM 了。
    #[cfg(feature = "drm")]
    if super::drm_capturer::is_available() {
        match super::drm_capturer::get_capturer_info(display_idx) {
            Ok(info) => return Ok(info),
            Err(e) => {
                log::warn!(
                    "drm capturer for display {} unavailable ({:#}); falling back to PipeWire",
                    display_idx,
                    e
                );
                ensure_pipewire_inited()?;
            }
        }
    }
    // 刻意在下面的读守卫之前解析。`get_display_infos` 会运行
    // `augment_with_wayland_geometry`，这是一次合成器输出往返，而 `clear()` 会取得
    // 每次采集器拆卸时的写守卫 —— 而当 DRM
    // 显示器被降级或反复抖动时（即恰恰是这条路径运行的时候）正是如此。持有读守卫
    // 跨越那次往返会让所有并发的拆卸在其持续期间停滞，而这个值
    // 并不依赖守卫内的任何东西。
    #[cfg(feature = "drm")]
    let drm_advertised = if super::drm_capturer::is_available_cached() {
        match super::drm_capturer::get_display_infos() {
            Some(list) => Some((list.get(display_idx).cloned(), list.len() == 1)),
            None => Some((None, false)),
        }
    } else {
        None
    };
    let cap_map = CAP_DISPLAY_INFO.read().unwrap();
    // 只提供该索引对应的那个确切的 PipeWire 条目。绝不要回退到其他索引的
    // `CapDisplayInfo`：`CapturerPtr` 是一个裸的 `*mut Capturer`，通过原始指针拷贝克隆，因此让
    // 同一个条目对应两个 `display_idx` 值，会让两个视频服务线程在同一个
    // `Recorder` 上无锁调用 `frame()`（数据竞争 / UB），而且还会针对错误的
    // 矩形错误映射输入。DRM 和 PipeWire 不共享索引空间（门户通常在索引 0 暴露一个整桌面
    // 流），因此一个被降级的非主 DRM 索引在这里没有对应的 PipeWire 条目；这种情况
    // 在源头处理，即把被降级的显示器从宣示的列表中移除（参见
    // drm_capturer demotion），这样客户端会基于一致的列表重新枚举，而不是被
    // 用共享的/不匹配的采集器掩盖过去。
    if let Some(addr) = cap_map.get(&display_idx) {
        let cap_display_info: *const CapDisplayInfo = *addr as _;
        unsafe {
            let cap_display_info = &*cap_display_info;
            let rect = cap_display_info.rects[cap_display_info.current];
            // 在 DRM 激活的情况下走到这里，说明 get_capturer_info 已返回错误（显示器被降级），并且
            // 我们已经穿透到了 PipeWire。只有当该流的矩形与
            // 我们为该索引宣示的几何信息一致时，才提供这条流。门户通常暴露一个整桌面
            // 流，所以在多显示器主机上那个矩形是整个桌面，而宣示的 DRM
            // 几何信息只是单个连接器 -> 提供它会拉伸画面并使所有
            // 输入产生偏移。改为返回错误；get_display_infos 已把该显示器宣示为离线，因此客户端
            // 会基于一致的列表重新枚举。单显示器主机是匹配的（整桌面 ==
            // 该显示器），会被正常提供。在纯 PipeWire 主机上 is_available() 为 false，
            // 因此这道守卫会被跳过，完全保持上游行为不变。
            #[cfg(feature = "drm")]
            if let Some((advertised, single_display)) = drm_advertised {
                if let Some(advertised) = advertised {
                    // 两边都是物理尺寸，所以按原始值比较。这是实测得出的而非臆测，
                    // 因为它曾两次被「修正」为某种缩放换算，结果反而弄坏了它：
                    // 上面的 `rect` 是用 `Display::width()/height()` 构建的，而 WAYLAND
                    // 变体返回的是 `physical_width()/physical_height()`
                    // （scrap 的 `common/wayland.rs`），也就是 `PipeWireCapturable.physical_size`。
                    // `try_fix_logical_size` 只修复 capturable 那个独立的
                    // `logical_size` 字段，从不触碰 `physical_size`，因此该矩形并不是
                    // 逻辑尺寸。宣示的 DRM 几何信息同样是物理尺寸，且以最终
                    // 显示方向为准：`augment_with_wayland_geometry` 会为 90/270 度
                    // 的输出转置宽/高（rustdesk#15886）。门户的 caps 到达时是否已旋转
                    // 在旋转的显示器上是未实测的（pipewiresrc 不会应用
                    // SPA_META_VideoTransform），因此尺寸这一半接受任意一种方向，
                    // 而不是在其中一种上押上永久离线。把某一边
                    // 除以缩放比例仍然是错的：拿逻辑尺寸去比物理尺寸。
                    //
                    // 正是这道尺寸检查把单个连接器和门户通常暴露的整桌面
                    // 矩形区分开来。只有当两边都表明
                    // 只有一个显示器时 —— DRM 列表只有一条目且 PipeWire
                    // 映射也只有一条 —— 时才跳过，因为只有那时「整桌面流就是这个
                    // 显示器」才在构造上成立。（门户在选择 Full Workspace 时可能报告与连接器
                    // 模式不同的物理尺寸，这正是
                    // 该情况需要这个例外的原因。）单看 DRM 的数量是不够的：
                    // 接在服务无法打开的显卡上的显示器不会出现在 DRM 列表里，
                    // 而合成器仍在驱动它。
                    let single_display = single_display && cap_display_info.num == 1;
                    // 只允许方向完全一致：转置过的流会按
                    // PipeWire 的尺寸编码，而客户端仍保留宣示的（旋转过的）尺寸，
                    // 而且没有任何 wayland 路径会去协调两者，因此每一帧都会被
                    // 客户端拒绝。改为返回错误则会把该显示器宣示为
                    // 离线，客户端可以通过重新枚举来恢复。
                    let size_matches = advertised.width as usize == rect.1
                        && advertised.height as usize == rect.2;
                    let transposed = advertised.width as usize == rect.2
                        && advertised.height as usize == rect.1;
                    // 单显示器的例外可以容忍尺寸上的差异（Full Workspace
                    // 流报告的可能是工作区而非模式），但绝不容忍转置的
                    // 一对：那与上面所说的「实际提供 vs 宣示」方向分裂是同一回事，
                    // 同样会让客户端黑屏。
                    let consistent = advertised.x == rect.0 .0
                        && advertised.y == rect.0 .1
                        && (size_matches || (single_display && !transposed));
                    if !consistent {
                        // 记录下来，使 `mark_demoted_displays` 中的单显示器例外
                        // 在单显示器情况下也让下面的「宣示为离线」成立，而不是
                        // 对着一条无人能提供的流陷入重启循环。
                        super::drm_capturer::mark_fallback_rejected(display_idx);
                        bail!(
                            "drm display {} demoted with no geometry-consistent PipeWire stream{} (advertised {}x{}+{}+{} vs stream {}x{}+{}+{}); advertised offline",
                            display_idx,
                            if transposed {
                                " - stream is transposed vs advertised"
                            } else {
                                ""
                            },
                            advertised.width,
                            advertised.height,
                            advertised.x,
                            advertised.y,
                            rect.1,
                            rect.2,
                            rect.0 .0,
                            rect.0 .1
                        );
                    }
                }
            }
            Ok(super::video_service::CapturerInfo {
                origin: rect.0,
                width: rect.1,
                height: rect.2,
                ndisplay: cap_display_info.num,
                current: cap_display_info.current,
                privacy_mode_id: 0,
                _capturer_privacy_mode_id: 0,
                capturer: Box::new(cap_display_info.capturer.clone()),
            })
        }
    } else {
        bail!(
            "Failed to get capturer display info for display {}",
            display_idx
        );
    }
}

pub fn common_get_error() -> String {
    if DISTRO.name.to_uppercase() == "Ubuntu".to_uppercase() {
        if DISTRO.version_id < "21".to_owned() {
            return "".to_owned();
        }
    } else {
        // 待办：检查其他发行版
    }
    return "".to_owned();
}
