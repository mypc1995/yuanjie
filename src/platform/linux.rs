use super::{gtk_sudo, CursorData, ResultType};
use desktop::Desktop;
pub use base::platform::linux::*;

#[cfg(feature = "drm")]
pub fn dispatch_wayland_display_probe() {
    use std::ffi::OsStr;

    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(WAYLAND_DISPLAY_PROBE_ARG)) {
        wayland_display_probe_child_main();
    }
}
use hbb_common::{
    allow_err,
    anyhow::anyhow,
    bail,
    config::Config,
    libc::{c_char, c_int, c_long, c_uint, c_ulong, c_void},
    log,
    regex::{Captures, Regex},
    users::{get_user_by_name, os::unix::UserExt},
};
use base::message_proto::{DisplayInfo, Resolution};
use libxdo_sys::{self, xdo_t, Window};
use std::{
    cell::RefCell,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Child, Command},
    string::String,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};
use terminfo::{capability as cap, Database};
use wallpaper;

pub const PA_SAMPLE_RATE: u32 = 48000;
static mut UNMODIFIED: bool = true;

#[derive(Clone, Debug)]
struct ActiveUserLookupCache {
    uid: String,
    username: String,
}

const INVALID_TERM_VALUES: [&str; 3] = ["", "unknown", "dumb"];
const SHELL_PROCESSES: [&str; 4] = ["bash", "zsh", "fish", "sh"];

// 终端类型常量
const TERM_XTERM_256COLOR: &str = "xterm-256color";
const TERM_SCREEN_256COLOR: &str = "screen-256color";
const TERM_XTERM: &str = "xterm";

#[cfg(feature = "drm")]
lazy_static::lazy_static! {
    /// 仅供逐帧调用方使用；参见 `is_login_screen_wayland_cached`。
    /// 单独成块，因为共享块中某个条目上的 `#[cfg]` 会破坏该宏。
    static ref IS_LOGIN_SCREEN_WAYLAND: bool = is_login_screen_wayland();
}

lazy_static::lazy_static! {
    /// `is_x11_or_headless()` 在 Wayland 登录界面下也会回答 x11，而 portal 无法
    /// 提供该服务，但 DRM 路径可以。故意不做记忆化查询：这可能在启动过程中运行，而
    /// 在那么早的阶段缓存一个 "no" 会让进程此后的整个生命周期都是错的。
    pub static ref IS_X11: bool = {
        let x11 = base::platform::linux::is_x11_or_headless();
        #[cfg(feature = "drm")]
        {
            if x11 && !display_server_forced() && is_login_screen_wayland() {
                log::info!(
                    "drm: seat0 is a Wayland login screen that reads as x11 upstream; \
                     treating it as Wayland so the DRM path is not disabled at the one \
                     screen it exists for"
                );
                false
            } else {
                x11
            }
        }
        #[cfg(not(feature = "drm"))]
        {
            x11
        }
    };
    // TERM 值的缓存 —— 一旦找到 TERM_XTERM_256COLOR，就直接复用
    static ref CACHED_TERM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    static ref DATABASE_XTERM_256COLOR: Option<Database> = {
        match Database::from_name(TERM_XTERM_256COLOR) {
            Ok(database) => Some(database),
            Err(err) => {
                log::error!("Failed to initialize {} database: {}", TERM_XTERM_256COLOR, err);
                None
            }
        }
    };
    static ref ACTIVE_USER_LOOKUP_CACHE: std::sync::Mutex<Option<ActiveUserLookupCache>> =
        std::sync::Mutex::new(None);
    static ref GNOME_MONITOR_LAYOUT_MODE_CACHE: std::sync::Mutex<
        Option<(Instant, Option<GnomeMonitorLayoutMode>)>,
    > = Default::default();
    // https://github.com/rustdesk/rustdesk/issues/13705
    // 检查 `sudo -E` 是否真的保留了环境变量。
    //
    // 该标志仅由 `run_as_user()`（root 服务 -> 用户会话）使用。如果当前进程不是
    // 以 `root` 身份运行，那么这个检查毫无意义（而且 `sudo -n` 可能失败），所以直接返回 `false`。
    //
    // 在 Ubuntu 25.10 上，`sudo -E` 可能仍然成功但实际上忽略了 `-E`。某些版本会把一条警告
    // 打印到 stderr（措辞可能因区域设置而异），所以我们改为验证其行为：
    // - 向 `sudo` 进程注入一个哨兵环境变量
    // - 运行 `sudo -n -E env` 并检查 stdout 中是否存在该哨兵
    static ref SUDO_E_PRESERVES_ENV: bool = {
        if !is_root() {
            log::warn!("Not running as root, SUDO_E_PRESERVES_ENV check skipped");
            false
        } else {
            let key = format!("__RUSTDESK_SUDO_E_TEST_{}", std::process::id());
            let val = "1";
            let expected = format!("{key}={val}");
            Command::new("sudo")
                // -n 表示非交互，避免弹出密码提示
                .env(&key, val)
                .args(["-n", "-E", "env"])
                .output()
                .map(|o| {
                    o.status.success()
                        && String::from_utf8_lossy(&o.stdout).contains(expected.as_str())
                })
                .unwrap_or(false)
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GnomeMonitorLayoutMode {
    Logical,
    Physical,
}

impl GnomeMonitorLayoutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Logical => "logical",
            Self::Physical => "physical",
        }
    }
}

fn gnome_monitor_layout_mode_from_value(value: u32) -> Option<GnomeMonitorLayoutMode> {
    // 上游：https://gitlab.gnome.org/GNOME/mutter/-/blob/main/data/dbus-interfaces/org.gnome.Mutter.DisplayConfig.xml
    // Ubuntu 模式 3：https://git.launchpad.net/ubuntu/+source/mutter/tree/debian/patches/x11-Add-support-for-fractional-scaling-using-Randr.patch
    match value {
        1 | 3 => Some(GnomeMonitorLayoutMode::Logical),
        2 => Some(GnomeMonitorLayoutMode::Physical),
        _ => None,
    }
}

pub fn gnome_monitor_layout_mode() -> Option<GnomeMonitorLayoutMode> {
    if let Ok(cache) = GNOME_MONITOR_LAYOUT_MODE_CACHE.lock() {
        if let Some((updated_at, result)) = *cache {
            if updated_at.elapsed() < Duration::from_secs(10) {
                return result;
            }
        }
    }

    let result = (|| {
        let is_gnome_desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .any(|desktop| {
                desktop.eq_ignore_ascii_case("gnome") || desktop.eq_ignore_ascii_case("unity")
            });
        let is_gnome_session = std::env::var("DESKTOP_SESSION")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !is_gnome_desktop && !is_gnome_session.contains("gnome") {
            return None;
        }
        use dbus::{arg::PropMap, blocking::BlockingSender};

        let conn = match dbus::blocking::Connection::new_session() {
            Ok(conn) => conn,
            Err(err) => {
                log::warn!("Failed to connect to the session bus for GNOME monitor layout: {err}");
                return None;
            }
        };
        let message = match dbus::Message::new_method_call(
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.gnome.Mutter.DisplayConfig",
            "GetCurrentState",
        ) {
            Ok(message) => message,
            Err(err) => {
                log::warn!("Failed to create GNOME monitor layout query: {err}");
                return None;
            }
        };
        let reply = match conn.send_with_reply_and_block(message, Duration::from_secs(2)) {
            Ok(reply) => reply,
            Err(err) => {
                log::warn!("Failed to query GNOME monitor layout: {err}");
                return None;
            }
        };
        let mut args = reply.iter_init();
        for _ in 0..3 {
            if !args.next() {
                log::warn!("GNOME monitor layout reply is missing properties");
                return None;
            }
        }
        let properties: PropMap = match args.read() {
            Ok(properties) => properties,
            Err(err) => {
                log::warn!("Failed to read GNOME monitor layout properties: {err}");
                return None;
            }
        };
        let Some(value) = dbus::arg::prop_cast::<u32>(&properties, "layout-mode").copied() else {
            log::warn!("GNOME monitor layout reply has no layout-mode");
            return None;
        };
        let mode = gnome_monitor_layout_mode_from_value(value);
        if mode.is_none() {
            log::warn!("GNOME monitor layout reply has unknown layout-mode {value}");
        }
        mode
    })();
    if let Ok(mut cache) = GNOME_MONITOR_LAYOUT_MODE_CACHE.lock() {
        * cache = Some((Instant::now(), result));
    }
    result
}

#[cfg(test)]
mod gnome_monitor_layout_tests {
    use super::*;

    #[test]
    fn maps_logical_layouts() {
        assert_eq!(
            gnome_monitor_layout_mode_from_value(1),
            Some(GnomeMonitorLayoutMode::Logical)
        );
        assert_eq!(
            gnome_monitor_layout_mode_from_value(3),
            Some(GnomeMonitorLayoutMode::Logical)
        );
    }
}

#[inline]
fn update_active_user_lookup_cache(desktop: &Desktop) {
    if let Ok(mut cache) = ACTIVE_USER_LOOKUP_CACHE.lock() {
        if desktop.uid.is_empty() || desktop.username.is_empty() {
            * cache = None;
        } else {
            * cache = Some(ActiveUserLookupCache {
                uid: desktop.uid.clone(),
                username: desktop.username.clone(),
            });
        }
    }
}

#[inline]
fn get_active_user_id_name_from_cache() -> Option<(String, String)> {
    let cache = ACTIVE_USER_LOOKUP_CACHE.lock().ok()?;
    let entry = cache.as_ref()?;
    Some((entry.uid.clone(), entry.username.clone()))
}

thread_local! {
    // XDO 上下文 —— 通过 libxdo-sys 创建（它使用动态加载桩）。
    // 如果 libxdo 不可用，xdo 将为 null，基于 xdo 的函数会变成空操作。
    static XDO: RefCell<*mut xdo_t> = RefCell::new({
        let xdo = unsafe { libxdo_sys::xdo_new(std::ptr::null()) };
        if xdo.is_null() {
            log::warn!("Failed to create xdo context, xdo functions will be disabled");
        } else {
            log::info!("xdo context created successfully");
        }
        xdo
    });
    static DISPLAY: RefCell<*mut c_void> = RefCell::new(unsafe { XOpenDisplay(std::ptr::null())});
}

pub fn right_alt_is_level3_shift() -> ResultType<bool> {
    use hbb_common::x11::{keysym, xlib};
    const XKB_USE_CORE_KBD: c_uint = 0x0100;
    // 新建连接可以避免切换键盘布局后 Xlib 中过期的键盘映射缓存。
    // 该查询仅在匹配到组合键且按住物理 Right Alt 时运行。
    let display = unsafe { xlib::XOpenDisplay(std::ptr::null()) };
    if display.is_null() {
        bail!("X display unavailable");
    }
    let result = (|| {
        let code = rdev::linux_keycode_from_key(rdev::Key::AltGr)
            .ok_or_else(|| anyhow!("Right Alt has no X keycode"))?;
        let mut state: xlib::XkbStateRec = unsafe { std::mem::zeroed() };
        let status = unsafe { xlib::XkbGetState(display, XKB_USE_CORE_KBD, &mut state) };
        if status != c_int::from(xlib::Success) {
            bail!("XkbGetState failed: {}", status);
        }
        let symbol = unsafe {
            xlib::XkbKeycodeToKeysym(display, code as _, state.group.into(), 0)
        };
        // 在美式英语等布局中，物理 Right Alt 就是普通的 Alt。
        Ok(matches!(symbol as u32, keysym::XK_ISO_Level3_Shift
            | keysym::XK_ISO_Level3_Latch | keysym::XK_ISO_Level3_Lock
            | keysym::XK_Mode_switch))
    })();
    unsafe { xlib::XCloseDisplay(display) };
    result
}

// 供自定义错误处理器使用的 X11 错误事件结构。
// 参见：https://www.x.org/releases/current/doc/libX11/libX11/libX11.html#Using-the-Default-Error-Handlers
#[repr(C)]
struct XErrorEvent {
    type_: c_int,
    display: *mut c_void, // Display*
    resourceid: c_ulong,  // XID
    serial: c_ulong,
    error_code: u8,
    request_code: u8,
    minor_code: u8,
}

type XErrorHandler = unsafe extern "C" fn(*mut c_void, *mut XErrorEvent) -> c_int;

const X11_BAD_WINDOW: u8 = 3;
const XDO_SUCCESS: c_int = 0;
const XDO_ERROR: c_int = 1;

/// 发生 BadWindow 错误时由自定义 X 错误处理器设置的原子标志。
static X_BAD_WINDOW_DETECTED: AtomicBool = AtomicBool::new(false);
static X_UNEXPECTED_ERROR_DETECTED: AtomicBool = AtomicBool::new(false);

/// 自定义 X 错误处理器，捕获 BadWindow 错误（error_code == 3），而不是
/// 任由默认处理器终止进程。
/// 参见 issue：https://github.com/rustdesk/rustdesk/issues/9003
unsafe extern "C" fn handle_x_error(_display: *mut c_void, event: *mut XErrorEvent) -> c_int {
    if !event.is_null() && (*event).error_code == X11_BAD_WINDOW {
        X_BAD_WINDOW_DETECTED.store(true, Ordering::SeqCst);
        log::debug!("Caught X11 BadWindow error (suppressed), window was likely destroyed");
        return 0;
    }
    X_UNEXPECTED_ERROR_DETECTED.store(true, Ordering::SeqCst);
    if !event.is_null() {
        log::warn!(
            "X11 error: error_code={}, request_code={}, minor_code={}",
            (*event).error_code,
            (*event).request_code,
            (*event).minor_code,
        );
    }
    0
}

#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(display_name: *const c_char) -> *mut c_void;
    // fn XCloseDisplay(d: *mut c_void) -> c_int;
    fn XSetErrorHandler(handler: Option<XErrorHandler>) -> Option<XErrorHandler>;
}

#[link(name = "Xfixes")]
extern "C" {
    // fn XFixesQueryExtension(dpy: *mut c_void, event: *mut c_int, error: *mut c_int) -> c_int;
    fn XFixesGetCursorImage(dpy: *mut c_void) -> *const xcb_xfixes_get_cursor_image;
    fn XFree(data: *mut c_void);
}

// /usr/include/X11/extensions/Xfixes.h
#[repr(C)]
pub struct xcb_xfixes_get_cursor_image {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub xhot: u16,
    pub yhot: u16,
    pub cursor_serial: c_long,
    pub pixels: *const c_long,
}

#[inline]
pub fn is_login_screen_wayland() -> bool {
    let values = get_values_of_seat0_with_gdm_wayland(&[0, 2]);
    is_gdm_user(&values[1]) && get_display_server_of_session(&values[0]) == DISPLAY_SERVER_WAYLAND
}

/// 显式设置的 `RUSTDESK_FORCED_DISPLAY_SERVER` 属于运维人员的强制覆盖，root 服务
/// 有意把它转发给按用户运行的服务器：登录界面的修正可能只是修正了一个
/// AUTO 检测出的结果，绝不要与运维人员争辩 —— 只落实一半的覆盖会导致
/// `get_display_server()` 与 DRM 路由判断彼此不一致。
#[cfg(feature = "drm")]
pub(crate) fn display_server_forced() -> bool {
    std::env::var("RUSTDESK_FORCED_DISPLAY_SERVER").is_ok()
}

/// 就 DRM 路径而言就是 X11：Wayland 登录界面不是，除非运维人员
/// 强制指定了显示服务器。
///
/// 两半都不做记忆化，供那些必须持续询问直到能确定 seat0 的重试循环使用。
#[cfg(feature = "drm")]
pub fn is_x11_for_drm() -> bool {
    scrap::is_x11() && (display_server_forced() || !is_login_screen_wayland())
}

/// 记忆化的 `is_login_screen_wayland`，供不得运行 `loginctl` 的逐帧调用方使用。
///
/// 仅供按会话启动的 `--server` 使用：它在会话确定之后才被派生，所以
/// 结果已经确定。任何可能在启动过程中运行的代码都必须使用未缓存的形式。
#[cfg(feature = "drm")]
#[inline]
pub fn is_login_screen_wayland_cached() -> bool {
    * IS_LOGIN_SCREEN_WAYLAND
}

#[inline]
fn sleep_millis(millis: u64) {
    std::thread::sleep(Duration::from_millis(millis));
}

pub fn get_cursor_pos() -> Option<(i32, i32)> {
    let mut res = None;
    XDO.with(|xdo| {
        if let Ok(xdo) = xdo.try_borrow() {
            if xdo.is_null() {
                return;
            }
            let mut x: c_int = 0;
            let mut y: c_int = 0;
            unsafe {
                libxdo_sys::xdo_get_mouse_location(
                    * xdo as *const _,
                    &mut x as _,
                    &mut y as _,
                    std::ptr::null_mut(),
                );
            }
            res = Some((x, y));
        }
    });
    res
}

pub fn set_cursor_pos(x: i32, y: i32) -> bool {
    let mut res = false;
    XDO.with(|xdo| {
        match xdo.try_borrow() {
            Ok(xdo) => {
                if xdo.is_null() {
                    log::debug!("set_cursor_pos: xdo is null");
                    return;
                }
                unsafe {
                    let ret = libxdo_sys::xdo_move_mouse(*xdo as *const _, x, y, 0);
                    if ret != 0 {
                        log::debug!(
                            "set_cursor_pos: xdo_move_mouse failed with code {} for coordinates ({}, {})",
                            ret, x, y
                        );
                    }
                    res = ret == 0;
                }
            }
            Err(_) => {
                log::debug!("set_cursor_pos: failed to borrow xdo");
            }
        }
    });
    res
}

/// 裁剪光标 —— Linux 上的实现是空操作。
///
/// 在 X11 上没有与 Windows ClipCursor 直接对应的东西。XGrabPointer
/// 可以限制指针的活动范围，但需要窗口句柄且有副作用。
///
/// 在 Wayland 上，指针约束需要 zwp_pointer_constraints_v1
/// 协议，而它依赖具体的合成器。
///
/// 在 Linux 上实现相对鼠标模式时，Flutter 端使用指针 warp
/// （set_cursor_pos）在每次移动后重新居中光标，从而获得
/// 类似效果，而无需裁剪光标。
///
/// 返回 true（始终以空操作成功）。
pub fn clip_cursor(_rect: Option<(i32, i32, i32, i32)>) -> bool {
    // 每个进程只记录一次日志，避免频繁调用时日志泛滥。
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log::debug!("clip_cursor called (no-op on Linux, this message is logged only once)");
    }
    true
}

pub fn reset_input_cache() {}

pub fn get_focused_display(displays: Vec<DisplayInfo>) -> Option<usize> {
    let mut res = None;
    XDO.with(|xdo| {
        if let Ok(xdo) = xdo.try_borrow() {
            if xdo.is_null() {
                return;
            }
            let mut x: c_int = 0;
            let mut y: c_int = 0;
            let mut width: c_uint = 0;
            let mut height: c_uint = 0;
            let mut window: Window = 0;

            unsafe {
                if libxdo_sys::xdo_get_active_window(*xdo as *const _, &mut window) != 0 {
                    return;
                }

                // XSetErrorHandler 是进程级的，并不局限于这个 Display/线程。
                // 该路径目前由唯一的 window_focus 服务线程调用。
                // 在安装期间，该处理器仍可能观察到来自
                // 其他线程的无关 X11 错误；意外错误会导致这次几何查询失败。
                X_BAD_WINDOW_DETECTED.store(false, Ordering::SeqCst);
                X_UNEXPECTED_ERROR_DETECTED.store(false, Ordering::SeqCst);
                let prev_handler = XSetErrorHandler(Some(handle_x_error));

                let loc_ret = libxdo_sys::xdo_get_window_location(
                    * xdo as *const _,
                    window,
                    &mut x as _,
                    &mut y as _,
                    std::ptr::null_mut(),
                );
                let size_ret = if loc_ret == XDO_SUCCESS {
                    libxdo_sys::xdo_get_window_size(
                        * xdo as *const _,
                        window,
                        &mut width,
                        &mut height,
                    )
                } else {
                    XDO_ERROR
                };

                // 此处不要调用 XSync(DISPLAY)：DISPLAY 是另一个独立的
                // XOpenDisplay() 连接，而 libxdo 拥有的是这些几何查询所用的 Display*
                // 。这些 libxdo 调用是基于 XGetWindowAttributes 的
                // 同步查询，因此目标窗口的
                // BadWindow 预期会在调用返回之前送达。
                XSetErrorHandler(prev_handler);
                if X_BAD_WINDOW_DETECTED.load(Ordering::SeqCst)
                    || X_UNEXPECTED_ERROR_DETECTED.load(Ordering::SeqCst)
                    || loc_ret != XDO_SUCCESS
                    || size_ret != XDO_SUCCESS
                {
                    return;
                }

                let center_x = x + (width / 2) as c_int;
                let center_y = y + (height / 2) as c_int;
                res = displays.iter().position(|d| {
                    center_x >= d.x
                        && center_x < d.x + d.width
                        && center_y >= d.y
                        && center_y < d.y + d.height
                });
            }
        }
    });
    res
}

pub fn get_cursor() -> ResultType<Option<u64>> {
    // DRM/KMS 采集：硬件光标通过 `_drm` 流到达，而不是来自 XFixes。
    //
    // 此处特意使用记忆化的 `is_x11()`，不同于采集路径上的调用方——它们使用的是
    // 未记忆化的 `scrap::is_x11()`，因为这一版是首次使用即锁定。取舍正好相反：
    // 在光标轮询频率下，未记忆化版本每次调用都会 fork `loginctl`，而这里
    // 在每次光标轮询时都会运行。锁定一旦猜错，代价只是直到
    // 进程重启之前光标由错误的来源提供，而不是采集无法启动 —— 而且等到真的在轮询光标时
    // ，说明存在活动会话，而这正是锁定读取正确的情况。
    #[cfg(feature = "drm")]
    if !is_x11() {
        if let Some(id) = crate::server::drm_capturer::drm_cursor_id() {
            // 在 DRM + PipeWire 混合会话中，DRM 流只覆盖由 DRM 支撑的显示器；
            // 当指针位于由 PipeWire 提供的显示器上时，每个 DRM 流都会报告那个隐藏
            // 哨兵值。在这里返回该哨兵值会在全局隐藏光标，包括在
            // 光标依然可见的 PipeWire 显示器上也是如此，所以只有当 DRM 光标
            // 具有权威性时（即纯 DRM 会话）才报告它被隐藏。可见的 DRM 光标始终具有权威性；
            // 否则继续走常规的光标路径。
            if id != scrap::drm_reader::HIDDEN_CURSOR_ID
                || !crate::server::display_service::has_non_drm_backed_display()
            {
                return Ok(Some(id));
            }
        }
    }
    let mut res = None;
    DISPLAY.with(|conn| {
        if let Ok(d) = conn.try_borrow_mut() {
            if !d.is_null() {
                unsafe {
                    let img = XFixesGetCursorImage(*d);
                    if !img.is_null() {
                        res = Some((*img).cursor_serial as u64);
                        XFree(img as _);
                    }
                }
            }
        }
    });
    Ok(res)
}

pub fn get_cursor_data(hcursor: u64) -> ResultType<CursorData> {
    // DRM/KMS 采集：返回来自 `_drm` 流的最新硬件光标快照。它的 id 可能
    // 在 get_cursor() 与此处之间已经越过了 `hcursor`，所以返回最新的那个，而不是
    // 直接放弃（那会触发 MouseCursorService 的退避）。
    //
    // 出于 `get_cursor()` 中详细说明的原因，这里特意使用记忆化的 `is_x11()`；两者必须
    // 保持一致，因为在那里走了 DRM 分支的调用方在这里也必须走该分支。
    #[cfg(feature = "drm")]
    if !is_x11() {
        if let Some(c) = crate::server::drm_capturer::drm_cursor() {
            // 参见 get_cursor()：隐藏的 DRM 哨兵值仅在纯 DRM 会话中才具有权威性。在
            // DRM + PipeWire 混合会话中继续往下走，让 PipeWire 显示器的光标由
            // 常规路径提供，而不是到处都被隐藏。
            if c.id != scrap::drm_reader::HIDDEN_CURSOR_ID
                || !crate::server::display_service::has_non_drm_backed_display()
            {
                let mut cd: CursorData = Default::default();
                cd.id = c.id;
                cd.width = c.width;
                cd.height = c.height;
                cd.hotx = c.hotx;
                cd.hoty = c.hoty;
                cd.colors = c.colors.into();
                return Ok(cd);
            }
        }
    }
    let mut res = None;
    DISPLAY.with(|conn| {
        if let Ok(ref mut d) = conn.try_borrow_mut() {
            if !d.is_null() {
                unsafe {
                    let img = XFixesGetCursorImage(**d);
                    if !img.is_null() && hcursor == (*img).cursor_serial as u64 {
                        let mut cd: CursorData = Default::default();
                        cd.hotx = (*img).xhot as _;
                        cd.hoty = (*img).yhot as _;
                        cd.width = (*img).width as _;
                        cd.height = (*img).height as _;
                        // 待办：如果它是 0 怎么办
                        cd.id = (*img).cursor_serial as _;
                        let pixels =
                            std::slice::from_raw_parts((*img).pixels, (cd.width * cd.height) as _);
                        // cd.colors.resize(pixels.len() * 4, 0);
                        let mut cd_colors = vec![0_u8; pixels.len() * 4];
                        for y in 0..cd.height {
                            for x in 0..cd.width {
                                let pos = (y * cd.width + x) as usize;
                                let p = pixels[pos];
                                let a = (p >> 24) & 0xff;
                                let r = (p >> 16) & 0xff;
                                let g = (p >> 8) & 0xff;
                                let b = (p >> 0) & 0xff;
                                if a == 0 {
                                    continue;
                                }
                                let pos = pos * 4;
                                cd_colors[pos] = r as _;
                                cd_colors[pos + 1] = g as _;
                                cd_colors[pos + 2] = b as _;
                                cd_colors[pos + 3] = a as _;
                            }
                        }
                        cd.colors = cd_colors.into();
                        res = Some(cd);
                    }
                    if !img.is_null() {
                        XFree(img as _);
                    }
                }
            }
        }
    });
    match res {
        Some(x) => Ok(x),
        _ => bail!("Failed to get cursor image of {}", hcursor),
    }
}

fn start_uinput_service() {
    use crate::server::uinput::service;
    std::thread::spawn(|| {
        service::start_service_control();
    });
    std::thread::spawn(|| {
        service::start_service_keyboard();
    });
    std::thread::spawn(|| {
        service::start_service_mouse();
    });
}

/// 根据环境推测最佳终端类型。
///
/// 该函数按以下顺序确定终端类型的优先级：
/// 1. `screen-256color`：在 `tmux` 或 `screen` 会话中运行时优先使用，
///    因为这些多路复用器通常支持高级终端特性。
/// 2. `xterm-256color`：当终端支持 256 色时选用，它
///    适合现代终端应用。
/// 3. `xterm`：作为基本终端兼容性的兜底。
///
/// 像 `linux` 和 `vt100` 这样的终端被排除，因为它们不支持
/// 许多应用所需的现代特性。
fn suggest_best_term() -> String {
    if is_running_in_tmux() || is_running_in_screen() {
        return TERM_SCREEN_256COLOR.to_string();
    }
    if term_supports_256_colors(TERM_XTERM_256COLOR) {
        return TERM_XTERM_256COLOR.to_string();
    }
    TERM_XTERM.to_string()
}

fn is_running_in_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

fn is_running_in_screen() -> bool {
    std::env::var("STY").is_ok()
}

fn supports_256_colors(db: &Database) -> bool {
    db.get::<cap::MaxColors>().map_or(false, |n| n.0 >= 256)
}

fn term_supports_256_colors(term: &str) -> bool {
    match term {
        TERM_XTERM_256COLOR => DATABASE_XTERM_256COLOR
            .as_ref()
            .map_or(false, |db| supports_256_colors(db)),
        _ => Database::from_name(term).map_or(false, |db| supports_256_colors(&db)),
    }
}

fn get_cur_term(uid: &str) -> Option<String> {
    // 先查缓存 —— 如果之前找到了 TERM_XTERM_256COLOR，就复用它
    if let Ok(cache) = CACHED_TERM.lock() {
        if let Some(ref cached) = *cache {
            if cached == TERM_XTERM_256COLOR {
                return Some(cached.clone());
            }
        }
    }

    if uid.is_empty() {
        return None;
    }

    // 检查当前进程的环境变量
    if let Ok(term) = std::env::var("TERM") {
        if term == TERM_XTERM_256COLOR {
            if let Ok(mut cache) = CACHED_TERM.lock() {
                * cache = Some(term.clone());
            }
            return Some(term);
        }
    }

    // 从 shell 进程收集所有 TERM 值，寻找 TERM_XTERM_256COLOR
    let terms = get_all_term_values(uid);

    // 优先使用 TERM_XTERM_256COLOR
    if terms.iter().any(|t| t == TERM_XTERM_256COLOR) {
        if let Ok(mut cache) = CACHED_TERM.lock() {
            * cache = Some(TERM_XTERM_256COLOR.to_string());
        }
        return Some(TERM_XTERM_256COLOR.to_string());
    }

    // 如果没找到 TERM_XTERM_256COLOR，就返回第一个有效的 TERM
    let fallback = terms.into_iter().next();
    if let Some(ref term) = fallback {
        log::debug!(
            "TERM_XTERM_256COLOR not found, using fallback TERM: {}",
            term
        );
    }
    fallback
}

/// 获取所有 shell 进程（bash、zsh、fish、sh）的 TERM 值。
/// 返回由不重复的有效 TERM 值组成的 Vec。
fn get_all_term_values(uid: &str) -> Vec<String> {
    let Ok(uid_num) = uid.parse::<u32>() else {
        return Vec::new();
    };

    // 构造正则表达式，只用 argv[0]（可执行文件路径）匹配 shell 进程
    // 模式：匹配位于开头或 '/' 之后的进程名，后面跟空格或结尾
    // 例如 "bash"、"/bin/bash"、"/usr/bin/zsh"
    let shell_pattern = SHELL_PROCESSES
        .iter()
        .map(|p| format!(r"(^|/){p}(\s|$)"))
        .collect::<Vec<_>>()
        .join("|");
    let Ok(re) = Regex::new(&shell_pattern) else {
        return Vec::new();
    };

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut terms = Vec::new();

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let proc_path = entry.path();

        // 检查进程是否属于指定的 uid
        if let Ok(meta) = std::fs::metadata(&proc_path) {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != uid_num {
                continue;
            }
        } else {
            continue;
        }

        // 检查 cmdline 是否匹配进程模式
        // /proc/<pid>/cmdline 是一串以 null 结尾的字符串；第一个
        // （argv[0]）是可执行文件路径。只对该
        // 做正则匹配，以避免参数带来的误判（例如 "python /path/to/bash-script.py"）。
        let cmdline_path = proc_path.join("cmdline");
        let Ok(cmdline) = std::fs::read(&cmdline_path) else {
            continue;
        };
        let exe_end = cmdline
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(cmdline.len());
        let exe_str = String::from_utf8_lossy(&cmdline[..exe_end]);
        if !re.is_match(&exe_str) {
            continue;
        }

        // 读取 environ 并提取 TERM
        let environ_path = proc_path.join("environ");
        let Ok(environ) = std::fs::read(&environ_path) else {
            continue;
        };

        for part in environ.split(|&b| b == 0) {
            if part.is_empty() {
                continue;
            }
            if let Some(eq) = part.iter().position(|&b| b == b'=') {
                let key_bytes = &part[..eq];
                if key_bytes == b"TERM" {
                    let val_bytes = &part[eq + 1..];
                    let term = String::from_utf8_lossy(val_bytes).into_owned();
                    if !INVALID_TERM_VALUES.contains(&term.as_str()) && !terms.contains(&term) {
                        // 如果找到了首选的 term 就提前返回
                        if term == TERM_XTERM_256COLOR {
                            return vec![term];
                        }
                        terms.push(term);
                    }
                    break;
                }
            }
        }
    }

    terms
}

fn append_audio_latency_envs(
    envs: &mut Vec<(&'static str, String)>,
    get_var: impl Fn(&str) -> Option<String>,
) {
    for key in ["PULSE_LATENCY_MSEC", "PIPEWIRE_LATENCY"] {
        if let Some(value) = get_var(key).filter(|value| !value.is_empty()) {
            envs.push((key, value));
        }
    }
}

#[cfg(test)]
mod audio_latency_env_tests {
    use super::append_audio_latency_envs;

    #[test]
    fn forwards_configured_values_without_creating_defaults() {
        let mut envs = Vec::new();
        append_audio_latency_envs(&mut envs, |key| match key {
            "PULSE_LATENCY_MSEC" => Some("60".to_owned()),
            "PIPEWIRE_LATENCY" => Some("1024/48000".to_owned()),
            _ => None,
        });
        assert_eq!(
            envs,
            vec![
                ("PULSE_LATENCY_MSEC", "60".to_owned()),
                ("PIPEWIRE_LATENCY", "1024/48000".to_owned()),
            ]
        );

        envs.clear();
        append_audio_latency_envs(&mut envs, |_| None);
        assert!(envs.is_empty());
    }
}

#[inline]
fn try_start_server_(desktop: Option<&Desktop>) -> ResultType<Option<Child>> {
    match desktop {
        Some(desktop) => {
            let mut envs = vec![];
            if !desktop.display.is_empty() {
                envs.push(("DISPLAY", desktop.display.clone()));
            }
            if !desktop.xauth.is_empty() {
                envs.push(("XAUTHORITY", desktop.xauth.clone()));
            }
            if !desktop.wl_display.is_empty() {
                envs.push(("WAYLAND_DISPLAY", desktop.wl_display.clone()));
            }
            if !desktop.home.is_empty() {
                envs.push(("HOME", desktop.home.clone()));
            }
            if !desktop.dbus.is_empty() {
                envs.push(("DBUS_SESSION_BUS_ADDRESS", desktop.dbus.clone()));
            }
            if let Ok(forced_display_server) =
                std::env::var("RUSTDESK_FORCED_DISPLAY_SERVER")
            {
                if !forced_display_server.is_empty() {
                    envs.push((
                        "RUSTDESK_FORCED_DISPLAY_SERVER",
                        forced_display_server,
                    ));
                }
            }

            append_audio_latency_envs(&mut envs, |key| std::env::var(key).ok());

            envs.push((
                "TERM",
                get_cur_term(&desktop.uid).unwrap_or_else(|| suggest_best_term()),
            ));
            run_as_user(
                vec!["--server"],
                Some((desktop.uid.clone(), desktop.username.clone())),
                envs,
            )
        }
        None => Ok(Some(crate::run_me(vec!["--server"])?)),
    }
}

#[inline]
fn start_server(desktop: Option<&Desktop>, server: &mut Option<Child>) {
    match try_start_server_(desktop) {
        Ok(ps) => *server = ps,
        Err(err) => {
            log::error!("Failed to start server: {}", err);
        }
    }
}

/// 刚派生的 `--server` 在一段短暂宽限期后是否仍在运行，若不在则接管
/// 这具尸体（并清空 `server`）。`start_server` 只报告派生
/// 是否成功，这与问题并不相同：一个 exec 之后立刻退出的子进程仍然会留下
/// `Some(child)`。
///
/// 退出的子进程一退出就会被检测到；健康的子进程每次启动都要花满整个宽限期，
/// 每次 start 一次。晚于此才死掉的服务器属于另一种（瞬时）故障，而重启
/// 节流已经放在 `should_start_server` 中，限制了这种情况。
#[cfg(feature = "drm")]
fn server_survived_grace(server: &mut Option<Child>) -> bool {
    const GRACE: Duration = Duration::from_millis(1000);
    const STEP_MS: u64 = 100;
    let Some(ps) = server.as_mut() else {
        return false; // spawn itself failed
    };
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline {
        match ps.try_wait() {
            Ok(Some(status)) => {
                log::warn!("--server exited {status} within {GRACE:?} of starting");
                * server = None;
                return false;
            }
            Ok(None) => sleep_millis(STEP_MS),
            // 无法判断；把它当作仍在运行，而不是拆掉一个可能健康的子进程。
            Err(err) => {
                log::error!("error waiting on the just-started --server: {err}");
                return true;
            }
        }
    }
    true
}

fn stop_server(server: &mut Option<Child>) {
    if let Some(mut ps) = server.take() {
        allow_err!(ps.kill());
        sleep_millis(30);
        match ps.try_wait() {
            Ok(Some(_status)) => {}
            Ok(None) => {
                let _res = ps.wait();
            }
            Err(e) => log::error!("error attempting to wait: {e}"),
        }
    }
}

fn set_x11_env(desktop: &Desktop) {
    log::info!("DISPLAY: {}", desktop.display);
    log::info!("XAUTHORITY: {}", desktop.xauth);
    if !desktop.display.is_empty() {
        std::env::set_var("DISPLAY", &desktop.display);
    }
    if !desktop.xauth.is_empty() {
        std::env::set_var("XAUTHORITY", &desktop.xauth);
    }
}

#[inline]
fn stop_rustdesk_servers() {
    let _ = run_cmds(&format!(
        r##"ps -ef | grep -E '{} +--server' | awk '{{print $2}}' | xargs -r kill -9"##,
        crate::get_app_name().to_lowercase(),
    ));
}

fn should_start_server(
    try_x11: bool,
    is_display_changed: bool,
    uid: &mut String,
    desktop: &Desktop,
    cm0: &mut bool,
    last_restart: &mut Instant,
    server: &mut Option<Child>,
) -> bool {
    let cm = get_cm();
    let mut start_new = false;
    let mut should_kill = false;

    if is_display_changed || desktop.uid != *uid && !desktop.uid.is_empty() {
        * uid = desktop.uid.clone();
        if try_x11 {
            set_x11_env(&desktop);
        }
        should_kill = true;
    }

    if !should_kill
        && !cm
        && ((*cm0 && last_restart.elapsed().as_secs() > 60)
            || last_restart.elapsed().as_secs() > 3600)
    {
        let terminal_session_count = crate::ipc::get_terminal_session_count().unwrap_or(0);
        if terminal_session_count > 0 {
            // 存在终端会话，因此我们不重启服务器。
            // 我们还需要保持 `cm0` 不变，这样下次才能再次走到这个分支。
            return false;
        }
        // 当所有新连接都已关闭时，或者每隔一小时，重启服务器，
        // 作为解决 "SpotUdp"（DNS 解析）问题的变通办法
        // 以及 X server 获取显示器失败的问题
        should_kill = true;
        log::info!("restart server");
    }

    if should_kill {
        if let Some(ps) = server.as_mut() {
            allow_err!(ps.kill());
            sleep_millis(30);
            * last_restart = Instant::now();
        }
    }

    if let Some(ps) = server.as_mut() {
        match ps.try_wait() {
            Ok(Some(_)) => {
                * server = None;
                start_new = true;
            }
            _ => {}
        }
    } else {
        start_new = true;
    }
    *cm0 = cm;
    start_new
}

// 待办：stop_server(&mut user_server)；可能无法正确停止子进程
// 这里的 stop_rustdesk_servers() 只是一个临时方案。
fn force_stop_server() {
    stop_rustdesk_servers();
    sleep_millis(super::SERVICE_INTERVAL);
}

pub fn start_os_service() {
    check_if_stop_service();
    stop_rustdesk_servers();
    start_uinput_service();

    std::thread::spawn(|| {
        allow_err!(crate::ipc::start(crate::POSTFIX_SERVICE));
    });

    // DRM/KMS 采集生产者（可选的 `drm` 特性）：一个专用线程 + 运行时，用于把
    // 扫描输出帧通过 `_drm` 服务范围通道送到用户的 `--server`。在这里运行，
    // 因为这个进程就是 root 服务，已经为进程内的
    // （direct-mode）libdrmtap 读取持有 CAP_SYS_ADMIN。
    //
    // Builder，与该特性启动的其他每个线程一样：`thread::spawn` 在无法创建线程时会 PANIC
    // （在线程数或内存限制下会出现 EAGAIN），而这里这个 panic 会
    // 从 `start_os_service` 中展开出去 —— 把 root 服务本身带崩，而该特性的
    // 失败本应只损失 DRM 采集。失去生产者后，消费者只能回退到
    // PipeWire/X11，这与未启用该特性的主机走的路径相同。
    #[cfg(feature = "drm")]
    if let Err(err) = std::thread::Builder::new()
        .name("drm-producer".into())
        .spawn(|| {
            crate::ipc::start_drm();
        })
    {
        log::warn!(
            "failed to spawn the drm capture producer thread: {err}; DRM capture is off for \
             this boot and the consumer falls back to PipeWire/X11"
        );
    }

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    let (mut display, mut xauth): (String, String) = ("".to_owned(), "".to_owned());
    let mut desktop = Desktop::default();
    let mut sid = "".to_owned();
    let mut uid = "".to_owned();
    let mut server: Option<Child> = None;
    let mut user_server: Option<Child> = None;
    if let Err(err) = ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }) {
        println!("Failed to set Ctrl-C handler: {}", err);
    }

    let mut cm0 = false;
    let mut last_restart = Instant::now();
    while running.load(Ordering::SeqCst) {
        desktop.refresh();
        update_active_user_lookup_cache(&desktop);

        // 此处与 should_start_server 的逻辑重复。
        if desktop.username == "root" || desktop.is_login_wayland() {
            // 尝试 kill 子进程 "--server"
            stop_server(&mut user_server);
            // 尝试启动子进程 "--server"
            // 此处无需检查 is_display_changed。
            if should_start_server(
                true,
                false,
                &mut uid,
                &desktop,
                &mut cm0,
                &mut last_restart,
                &mut server,
            ) {
                force_stop_server();
                // 以活动的 seat0 会话用户（登录界面
                // 账户）而非 root 的身份运行登录界面的 --server，这样 DRM 采集的 GPU/EGL 转换就不会在特权进程中加载
                // 厂商的 GPU 用户态。is_login_wayland() 会匹配 GDM 或
                // SDDM 的 Wayland 登录界面（is_gdm_user 覆盖两者），而 desktop.uid 就是该登录界面的
                // uid，因此这里会降权到占有 seat0 的那个登录界面。is_gdm_user 无法识别的登录界面
                // （例如 LightDM）根本不会走到这个分支 —— 它走的是下面那个非特权的
                // else 分支。真正的 root 图形会话（username=="root"）
                // 没有更低的 uid 可降，因此保持 root。整个分支都以 drm
                // 特性为前提，所以关闭 drm 的构建就是上游那一行 `start_server(None, ..)`。
                #[cfg(not(feature = "drm"))]
                start_server(None, &mut server);
                #[cfg(feature = "drm")]
                if desktop.username != "root" && !desktop.uid.is_empty() {
                    start_server(Some(&desktop), &mut server);
                    // 如果降权到登录界面 uid 之后没有产生一个 RUNNING 的服务器，就回退到以
                    // root 运行 --server，让登录界面仍可被远程访问，而不是在
                    // 失败的登录界面派生上反复循环。这样只在那条失败路径上才付出 GPU 在 root 中运行的代价，
                    // 正常的登录界面场景绝不会付出。这里看的是存活，而不只是派生成功：一个
                    // 根本无法真正运行它的登录界面账户（nologin shell、受限的主目录、
                    // 没有可写配置目录）会留下一个立刻退出的子进程，而上面的循环
                    // 只会发现子进程不见了并无休止地重新派生，永远不会
                    // 走到这个回退 —— 于是登录界面会变得永久无法远程访问，
                    // 在原本可以正常工作的主机上。
                    if !server_survived_grace(&mut server) {
                        log::warn!(
                            "greeter --server did not stay up; falling back to a root --server"
                        );
                        start_server(None, &mut server);
                    }
                } else {
                    start_server(None, &mut server);
                }
            }
        } else if desktop.username != "" {
            // 尝试 kill 子进程 "--server"
            stop_server(&mut server);

            let is_display_changed = desktop.display != display || desktop.xauth != xauth;
            display = desktop.display.clone();
            xauth = desktop.xauth.clone();

            // 尝试启动子进程 "--server"
            if should_start_server(
                !desktop.is_wayland(),
                is_display_changed,
                &mut uid,
                &desktop,
                &mut cm0,
                &mut last_restart,
                &mut user_server,
            ) {
                force_stop_server();
                start_server(Some(&desktop), &mut user_server);
            }
        } else {
            force_stop_server();
            stop_server(&mut user_server);
            stop_server(&mut server);
        }

        let keeps_session = sid == desktop.sid;
        if keeps_session {
            // 用于修复 https://github.com/rustdesk/rustdesk/issues/3129，避免过多 dbus 调用，
            sleep_millis(500);
        } else {
            sleep_millis(super::SERVICE_INTERVAL);
        }
        sid = desktop.sid.clone();
    }

    if let Some(ps) = user_server.take().as_mut() {
        allow_err!(ps.kill());
    }
    if let Some(ps) = server.take().as_mut() {
        allow_err!(ps.kill());
    }
    log::info!("Exit");
}

#[inline]
/// 在可用时返回缓存的活动 `(uid, username)` 快照。
/// 需要重新查询 seat0 的调用方应直接调用 `get_values_of_seat0`。
pub fn get_active_user_id_name() -> (String, String) {
    if let Some(id_name) = get_active_user_id_name_from_cache() {
        return id_name;
    }
    let vec_id_name = get_values_of_seat0(&[1, 2]);
    (vec_id_name[0].clone(), vec_id_name[1].clone())
}

#[inline]
/// 在可用时返回缓存的活动 uid。
/// 需要重新查询 seat0 的调用方应直接调用 `get_values_of_seat0`。
pub fn get_active_userid() -> String {
    if let Some((uid, _)) = get_active_user_id_name_from_cache() {
        return uid;
    }
    get_values_of_seat0(&[1])[0].clone()
}

#[inline]
/// 通过重新查询 seat0 返回活动 uid，绕过服务循环的缓存。
pub fn get_active_userid_fresh() -> String {
    // Wayland 登录界面在显示期间占有 seat0，并且由 DRM 后端提供画面，所以一个看不到它的 uid 判断
    // 会拒绝登录界面自己的 `--server`。`Desktop::refresh` 也以同样方式读取它。
    #[cfg(feature = "drm")]
    return get_values_of_seat0_with_gdm_wayland(&[1])[0].clone();
    #[cfg(not(feature = "drm"))]
    get_values_of_seat0(&[1])[0].clone()
}

#[inline]
/// 以数字形式返回缓存的活动 uid，缓存为空时返回 `None`。与 `get_active_userid` 不同，
/// 它绝不会退回到阻塞式的 `loginctl` seat0 查询，因此可以安全地在异步
/// 运行时线程和热点路径（如逐帧重新认证）上调用：缓存未命中时返回 `None`，由
/// 调用方将其视为"活动会话暂时未知"，而不是阻塞在子进程上。
pub fn get_active_userid_cached() -> Option<u32> {
    get_active_user_id_name_from_cache().and_then(|(uid, _)| uid.parse::<u32>().ok())
}

fn get_cm() -> bool {
    // 它在服务循环中每秒运行两次，所以应该遍历 /proc 而不是 fork `ps aux`；那个
    // fork 正是它所替换的 `CMD_PS` 审计消息变通方案的目的。
    let cm = format!(
        "{} --cm",
        std::env::current_exe()
            .unwrap_or_default()
            .to_string_lossy()
    );
    any_process(None, "cmdline", |cmdline| cmdline.contains(&cm))
}

pub fn is_login_wayland() -> bool {
    let files = ["/etc/gdm3/custom.conf", "/etc/gdm/custom.conf"];
    match (
        Regex::new(r"# *WaylandEnable *= *false"),
        Regex::new(r"WaylandEnable *= *true"),
    ) {
        (Ok(pat1), Ok(pat2)) => {
            for file in files {
                if let Ok(contents) = std::fs::read_to_string(file) {
                    return pat1.is_match(&contents) || pat2.is_match(&contents);
                }
            }
        }
        _ => {}
    }
    false
}

#[inline]
pub fn current_is_wayland() -> bool {
    return is_desktop_wayland() && unsafe { UNMODIFIED };
}

// 待办：测试其他显示管理器
fn _get_display_manager() -> String {
    if let Ok(x) = std::fs::read_to_string("/etc/X11/default-display-manager") {
        if let Some(x) = x.split("/").last() {
            return x.to_owned();
        }
    }
    "gdm3".to_owned()
}

#[inline]
/// 在可用时返回缓存的活动用户名。
/// 需要重新查询 seat0 的调用方应直接调用 `get_values_of_seat0`。
pub fn get_active_username() -> String {
    // Flatpak 只在已登录用户的会话内运行，而 Flathub 构建
    // 无法访问主机的 loginctl。
    if is_flatpak() {
        return crate::username();
    }
    if let Some((_, username)) = get_active_user_id_name_from_cache() {
        return username;
    }
    get_values_of_seat0(&[2])[0].clone()
}

pub fn get_user_home_by_name(username: &str) -> Option<PathBuf> {
    return match get_user_by_name(username) {
        None => None,
        Some(user) => {
            let home = user.home_dir();
            if Path::is_dir(home) {
                Some(PathBuf::from(home))
            } else {
                None
            }
        }
    };
}

pub fn get_active_user_home() -> Option<PathBuf> {
    let username = get_active_username();
    if !username.is_empty() {
        match get_user_home_by_name(&username) {
            None => {
                // 回退到最常见的默认模式
                let home = PathBuf::from(format!("/home/{}", username));
                if home.exists() {
                    return Some(home);
                }
            }
            Some(home) => {
                return Some(home);
            }
        }
    }
    None
}

pub fn get_env_var(k: &str) -> String {
    match std::env::var(k) {
        Ok(v) => v,
        Err(_e) => "".to_owned(),
    }
}

fn is_flatpak() -> bool {
    std::path::PathBuf::from("/.flatpak-info").exists()
}

pub fn is_prelogin() -> bool {
    if is_flatpak() {
        return false;
    }
    let name = get_active_username();
    if let Ok(res) = run_cmds(&format!("getent passwd {}", name)) {
        return res.contains("/bin/false") || res.contains("/usr/sbin/nologin");
    }
    false
}

// 检查 "Lock"。
// 无法检查 "Switch user"，因为 `get_values_of_seat0(&[0])` 不会返回该会话。
// 已登录的会话是 "online" 而不是 "active"。
// 而且 "Switch user" 界面通常是 Wayland 登录会话，我们并不支持。
pub fn is_locked() -> bool {
    if is_prelogin() {
        return false;
    }

    let values = get_values_of_seat0(&[0]);
    // 虽然这些值不可能为空，我们仍在这里加上检查以确保安全。
    // 因为我们无法保证内部实现将来是否会变化。
    // https://github.com/rustdesk/hbb_common/blob/ebb4d4a48cf7ed6ca62e93f8ed124065c6408536/src/platform/linux.rs#L119
    if values.is_empty() {
        log::debug!("Failed to check is locked, values vector is empty.");
        return false;
    }
    let session = &values[0];
    if session.is_empty() {
        log::debug!("Failed to check is locked, session is empty.");
        return false;
    }
    is_session_locked(session)
}

pub fn is_root() -> bool {
    crate::username() == "root"
}

fn is_opensuse() -> bool {
    if let Ok(res) = run_cmds("cat /etc/os-release | grep opensuse") {
        if !res.is_empty() {
            return true;
        }
    }
    false
}

pub fn run_as_user<I, K, V>(
    arg: Vec<&str>,
    user: Option<(String, String)>,
    envs: I,
) -> ResultType<Option<Child>>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let (uid, username) = match user {
        Some(id_name) => id_name,
        None => get_active_user_id_name(),
    };
    let cmd = std::env::current_exe()?;
    if uid.is_empty() {
        bail!("No valid uid");
    }

    let xdg = &format!("XDG_RUNTIME_DIR=/run/user/{uid}");
    if *SUDO_E_PRESERVES_ENV {
        // 原有逻辑：使用 sudo -E 保留环境变量
        let mut args = vec![xdg, "-u", &username, cmd.to_str().unwrap_or("")];
        args.append(&mut arg.clone());
        // 需要 -E 才能保留环境变量
        args.insert(0, "-E");
        let task = Command::new("sudo").envs(envs).args(args).spawn()?;
        Ok(Some(task))
    } else {
        // 回退方案：sudo -u username env VAR=VALUE ... cmd args
        // 用于不支持 sudo -E 的系统（例如 Ubuntu 25.10+）
        //
        // 安全：此处不涉及 shell（我们使用 execve 风格的 argv）。
        // 环境变量通过 `env` 参数传递，
        // 因此不存在命令注入的途径。
        //
        // 只接受可移植的环境变量名（shell 的 POSIX 可移植字符集）。
        // 大多数合法的环境变量遵循 [A-Za-z_][A-Za-z0-9_]* 的约定。
        // 带点的变量（例如 "java.home"）是 Java 系统属性，而不是环境变量。
        // 这里刻意采取严格限制，是为了这个 sudo 场景下的安全性。
        fn is_valid_env_key(key: &str) -> bool {
            let mut it = key.chars();
            match it.next() {
                Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
                _ => return false,
            }
            it.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }

        let mut sudo = Command::new("sudo");
        sudo.arg("-u").arg(&username).arg("--").arg("env").arg(xdg);

        for (k, v) in envs {
            let key = k.as_ref().to_string_lossy();
            if !is_valid_env_key(&key) {
                log::warn!("Skipping environment variable with invalid key: '{}'. Only [A-Za-z_][A-Za-z0-9_]* are allowed in sudo context.", key);
                continue;
            }
            // 重要：此处不要添加 shell 引号；`Command` 不会调用 shell。
            // 把 KEY=VALUE 作为单个 argv 元素传递是安全的，并且能保留空格。
            let mut arg = OsString::from(&*key);
            arg.push("=");
            arg.push(v.as_ref());
            sudo.arg(arg);
        }

        sudo.arg(cmd).args(arg);
        let task = sudo.spawn()?;
        Ok(Some(task))
    }
}

pub fn get_pa_monitor() -> String {
    get_pa_sources()
        .drain(..)
        .map(|x| x.0)
        .filter(|x| x.contains("monitor"))
        .next()
        .unwrap_or("".to_owned())
}

pub fn get_pa_source_name(desc: &str) -> String {
    get_pa_sources()
        .drain(..)
        .filter(|x| x.1 == desc)
        .map(|x| x.0)
        .next()
        .unwrap_or("".to_owned())
}

pub fn get_pa_sources() -> Vec<(String, String)> {
    use pulsectl::controllers::*;
    let mut out = Vec::new();
    match SourceController::create() {
        Ok(mut handler) => {
            if let Ok(devices) = handler.list_devices() {
                for dev in devices.clone() {
                    out.push((
                        dev.name.unwrap_or("".to_owned()),
                        dev.description.unwrap_or("".to_owned()),
                    ));
                }
            }
        }
        Err(err) => {
            log::error!("Failed to get_pa_sources: {:?}", err);
        }
    }
    out
}

pub fn get_default_pa_source() -> Option<(String, String)> {
    use pulsectl::controllers::*;
    match SourceController::create() {
        Ok(mut handler) => {
            if let Ok(dev) = handler.get_default_device() {
                return Some((
                    dev.name.unwrap_or("".to_owned()),
                    dev.description.unwrap_or("".to_owned()),
                ));
            }
        }
        Err(err) => {
            log::error!("Failed to get_pa_source: {:?}", err);
        }
    }
    None
}

pub fn lock_screen() {
    Command::new("xdg-screensaver").arg("lock").spawn().ok();
}

pub fn toggle_blank_screen(_v: bool) {
    // https://unix.stackexchange.com/questions/17170/disable-keyboard-mouse-input-on-unix-under-x
}

pub fn block_input(_v: bool) -> (bool, String) {
    (true, "".to_owned())
}

pub fn is_installed() -> bool {
    if let Ok(p) = std::env::current_exe() {
        p.to_str().unwrap_or_default().starts_with("/usr")
            || p.to_str().unwrap_or_default().starts_with("/nix/store")
    } else {
        false
    }
}

/// 从匹配给定条件的进程获取多个环境变量。
/// 该版本直接读取 /proc，而不是派生 shell 命令。
///
/// # 参数
/// * `uid` —— 用于筛选进程的用户 ID
/// * `process_pat` —— 匹配进程 cmdline 的正则表达式
/// * `names` —— 要获取的环境变量名。**必须 <= 64 个元素**，因为
///   内部用于打破平局的位掩码。
///
/// # panic（debug 构建）
/// 当 `names.len() > 64` 时 panic。
///
/// # 实现说明
/// - 只返回来自*单个*最佳匹配 process_pat 的值（为保持一致性）。
/// - 每个进程只解析一次 `environ`，避免重复扫描。
fn get_envs<'a>(
    uid: &str,
    process_pat: &str,
    names: &[&'a str],
) -> std::collections::HashMap<&'a str, String> {
    get_envs_where(uid, process_pat, names, false, |count| count == names.len())
}

/// 匹配 `process_pat` 的最新进程，无论它恰好带着什么：这就是下面这些调用方
/// 此前使用的 `ps -u <uid> -f | grep <pat> | tail -1` 管道语义。该进程
/// 没有的某个变量意味着转向下一个模式，而绝不转向更老的进程——
/// 那个进程可能属于此后已注销的会话。
fn get_envs_of_newest<'a>(
    uid: &str,
    process_pat: &str,
    names: &[&'a str],
) -> std::collections::HashMap<&'a str, String> {
    get_envs_where(uid, process_pat, names, true, |_| true)
}

/// `get_envs` 带有调用方自己的进程顺序和对完整答案的判断，并被告知
/// 该进程携带了 `names` 中的多少项：第一个被 `accept` 接受的进程直接胜出，而
/// 基于数量的排序只是在完全没有进程被接受时的兜底。
fn get_envs_where<'a, F>(
    uid: &str,
    process_pat: &str,
    names: &[&'a str],
    newest_first: bool,
    mut accept: F,
) -> std::collections::HashMap<&'a str, String>
where
    F: FnMut(usize) -> bool,
{
    // 打破平局的逻辑使用 u64 位掩码，把我们限制在 64 个变量以内。
    debug_assert!(
        names.len() <= 64,
        "get_envs: names.len() must be <= 64, got {}",
        names.len()
    );

    let empty: std::collections::HashMap<&'a str, String> =
        names.iter().map(|&n| (n, String::new())).collect();

    let Ok(uid_num) = uid.parse::<u32>() else {
        return empty;
    };
    let Ok(re) = Regex::new(process_pat) else {
        return empty;
    };

    // 当匹配到多个进程时用于稳定地打破平局。
    // 更高的位对应 `names` 中更靠前的条目。
    let name_indices: std::collections::HashMap<&'a str, usize> =
        names.iter().enumerate().map(|(i, &n)| (n, i)).collect();

    let mut best = empty.clone();
    let mut best_count = 0usize;
    let mut best_mask: u64 = 0;

    // 遍历 /proc 以查找匹配的进程。`newest_first` 只用于 `get_envs_of_newest`，
    // 其调用方需要的是它们的 `ps ... | tail -1` 管道所取到的最后一个按 PID 排序的匹配项；
    // 没有它时，顺序取决于 readdir 的返回顺序，`get_envs` 一直都是这么做的。
    // 两种顺序都无法确定活动会话 —— 一个拥有两个活动图形会话的用户
    // 两者各有一个，靠 PID 选就是猜。参见 `Desktop::refresh` 了解谁负责这个问题。
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return best;
    };
    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .collect();
    if newest_first {
        pids.sort_unstable_by(|a, b| b.cmp(a));
    }

    for pid in pids {
        let proc_path = std::path::Path::new("/proc").join(pid.to_string());

        // 检查进程是否属于指定的 uid
        if let Ok(meta) = std::fs::metadata(&proc_path) {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != uid_num {
                continue;
            }
        } else {
            continue;
        }

        // 检查 cmdline 是否匹配进程模式
        let cmdline_path = proc_path.join("cmdline");
        let Ok(cmdline) = std::fs::read(&cmdline_path) else {
            continue;
        };
        let cmdline_str = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        // 相当于它所替换的管道中的 `grep -v 'grep'`。否则，一个 grep 这些
        // 模式之一的用户会得到它的最新匹配项，而它返回的是该用户 shell 所带的
        // 环境变量 —— 比如通过 ssh 转发的 X 转发端点。
        if cmdline_str.contains("grep") || !re.is_match(&cmdline_str) {
            continue;
        }

        // 读取 environ 并提取匹配的变量。读取失败 —— 进程在
        // 这两次读取之间退出 —— 说明这是一个不携带任何 `names` 中变量的进程，而不是一个
        // 应该跳过的进程：跳过它会把 `newest_first` 交给更老的 PID，而它所替换的管道
        // 早已停在 `tail -1` 选中的那一个 PID 上。
        let environ = std::fs::read(proc_path.join("environ")).unwrap_or_default();

        let mut found = empty.clone();
        let mut found_count = 0usize;
        let mut found_mask: u64 = 0;

        for part in environ.split(|&b| b == 0) {
            if part.is_empty() {
                continue;
            }
            let Some(eq) = part.iter().position(|&b| b == b'=') else {
                continue;
            };
            let key_bytes = &part[..eq];
            let val_bytes = &part[eq + 1..];

            let Ok(key) = std::str::from_utf8(key_bytes) else {
                continue;
            };
            if let Some(slot) = found.get_mut(key) {
                if slot.is_empty() {
                    * slot = String::from_utf8_lossy(val_bytes).into_owned();
                    found_count += 1;

                    if let Some(&idx) = name_indices.get(key) {
                        let total = names.len();
                        if total <= 64 {
                            let bit = 1u64 << (total - 1 - idx);
                            found_mask |= bit;
                        }
                    }
                }
            }
        }

        if accept(found_count) {
            return found;
        }

        if found_count > best_count || (found_count == best_count && found_mask > best_mask) {
            best = found;
            best_count = found_count;
            best_mask = found_mask;
        }
    }

    best
}

/// 当 `pred` 接受任意进程的 `/proc/<pid>/<file>`（NUL 转换为空格）时为真，
/// 可选地只限于 `uid` 拥有的进程。
/// 直接读取 `/proc` 而不是 fork `ps` / `pgrep`，供下面的服务循环调用方使用。
fn any_process<F: Fn(&str) -> bool>(uid: Option<u32>, file: &str, pred: F) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let proc_path = entry.path();
        if let Some(uid) = uid {
            use std::os::unix::fs::MetadataExt;
            match std::fs::metadata(&proc_path) {
                Ok(meta) if meta.uid() == uid => {}
                _ => continue,
            }
        }
        let Ok(content) = std::fs::read(proc_path.join(file)) else {
            continue;
        };
        if pred(&String::from_utf8_lossy(&content).replace('\0', " ")) {
            return true;
        }
    }
    false
}

#[inline]
fn get_env_from_pid(name: &str, pid: &str) -> String {
    let cmd = format!("cat /proc/{}/environ 2>/dev/null | tr '\\0' '\\n' | grep '^{}=' | tail -1 | sed 's/{}=//g'", pid, name, name);
    if let Ok(x) = run_cmds(&cmd) {
        x.trim_end().to_string()
    } else {
        "".to_owned()
    }
}

#[link(name = "gtk-3")]
extern "C" {
    fn gtk_main_quit();
}

pub fn quit_gui() {
    unsafe { gtk_main_quit() };
}

/*
pub fn exec_privileged(args: &[&str]) -> ResultType<Child> {
    Ok(Command::new("pkexec").args(args).spawn()?)
}
*/

pub fn check_super_user_permission() -> ResultType<bool> {
    gtk_sudo::run(vec!["echo"])?;
    Ok(true)
}

/*
pub fn elevate(args: Vec<&str>) -> ResultType<bool> {
    let cmd = std::env::current_exe()?;
    match cmd.to_str() {
        Some(cmd) => {
            let mut args_with_exe = vec![cmd];
            args_with_exe.append(&mut args.clone());
            // opensuse 需要 -E
            if is_opensuse() {
                args_with_exe.insert(0, "-E");
            }
            let res = match exec_privileged(&args_with_exe)?.wait() {
                Ok(status) => {
                    if status.success() {
                        true
                    } else {
                        log::error!(
                            "Failed to wait install process, process status: {:?}",
                            status
                        );
                        false
                    }
                }
                Err(e) => {
                    log::error!("Failed to wait install process, error: {}", e);
                    false
                }
            };
            Ok(res)
        }
        None => {
            hbb_common::bail!("Failed to get current exe as str");
        }
    }
}
*/

type GtkSettingsPtr = *mut c_void;
type GObjectPtr = *mut c_void;
#[link(name = "gtk-3")]
extern "C" {
    // fn gtk_init(argc: *mut c_int, argv: *mut *mut c_char);
    fn gtk_settings_get_default() -> GtkSettingsPtr;
}

#[link(name = "gobject-2.0")]
extern "C" {
    fn g_object_get(object: GObjectPtr, first_property_name: *const c_char, ...);
}

pub fn get_double_click_time() -> u32 {
    // GtkSettings *settings = gtk_settings_get_default ();
    // g_object_get (settings, "gtk-double-click-time", &double_click_time, NULL);
    unsafe {
        let mut double_click_time = 0u32;
        let Ok(property) = std::ffi::CString::new("gtk-double-click-time") else {
            return 0;
        };
        let settings = gtk_settings_get_default();
        g_object_get(
            settings,
            property.as_ptr(),
            &mut double_click_time as *mut u32,
            0 as *const c_void,
        );
        double_click_time
    }
}

#[inline]
fn get_width_height_from_captures<'t>(caps: &Captures<'t>) -> Option<(i32, i32)> {
    match (caps.name("width"), caps.name("height")) {
        (Some(width), Some(height)) => {
            match (
                width.as_str().parse::<i32>(),
                height.as_str().parse::<i32>(),
            ) {
                (Ok(width), Ok(height)) => {
                    return Some((width, height));
                }
                _ => {}
            }
        }
        _ => {}
    }
    None
}

#[inline]
fn get_xrandr_conn_pat(name: &str) -> String {
    format!(
        r"(?m)^{}\s+connected.+?(?P<width>\d+)x(?P<height>\d+)\+(?P<x>\d+)\+(?P<y>\d+).*?\n",
        name
    )
}

pub fn resolutions(name: &str) -> Vec<Resolution> {
    let resolutions_pat = r"(?P<resolutions>(\s*\d+x\d+\s+\d+.*\n)+)";
    let connected_pat = get_xrandr_conn_pat(name);
    let mut v = vec![];
    if let Ok(re) = Regex::new(&format!("{}{}", connected_pat, resolutions_pat)) {
        match run_cmds("xrandr --query | tr -s ' '") {
            Ok(xrandr_output) => {
                // xrandr 输出有不同种类。
                /*
                1.
                Screen 0: minimum 320 x 175, current 1920 x 1080, maximum 1920 x 1080
                default connected 1920x1080+0+0 0mm x 0mm
                 1920x1080 10.00*
                 1280x720 25.00
                 1680x1050 60.00
                Virtual2 disconnected (normal left inverted right x axis y axis)
                Virtual3 disconnected (normal left inverted right x axis y axis)

                Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 16384 x 16384
                eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 344mm x 193mm
                1920x1080     60.01*+  60.01    59.97    59.96    59.93
                1680x1050     59.95    59.88
                1600x1024     60.17

                XWAYLAND0 connected primary 1920x984+0+0 (normal left inverted right x axis y axis) 0mm x 0mm
                Virtual1 connected primary 1920x984+0+0 (normal left inverted right x axis y axis) 0mm x 0mm
                HDMI-0 connected (normal left inverted right x axis y axis)

                rdp0 connected primary 1920x1080+0+0 0mm x 0mm
                    */
                if let Some(caps) = re.captures(&xrandr_output) {
                    if let Some(resolutions) = caps.name("resolutions") {
                        let resolution_pat =
                            r"\s*(?P<width>\d+)x(?P<height>\d+)\s+(?P<rates>(\d+\.\d+\D*)+)\s*\n";
                        let Ok(resolution_re) = Regex::new(&format!(r"{}", resolution_pat)) else {
                            log::error!("Regex new failed");
                            return vec![];
                        };
                        for resolution_caps in resolution_re.captures_iter(resolutions.as_str()) {
                            if let Some((width, height)) =
                                get_width_height_from_captures(&resolution_caps)
                            {
                                let resolution = Resolution {
                                    width,
                                    height,
                                    ..Default::default()
                                };
                                if !v.contains(&resolution) {
                                    v.push(resolution);
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => log::error!("Failed to run xrandr query, {}", e),
        }
    }

    v
}

pub fn current_resolution(name: &str) -> ResultType<Resolution> {
    let xrandr_output = run_cmds("xrandr --query | tr -s ' '")?;
    let re = Regex::new(&get_xrandr_conn_pat(name))?;
    if let Some(caps) = re.captures(&xrandr_output) {
        if let Some((width, height)) = get_width_height_from_captures(&caps) {
            return Ok(Resolution {
                width,
                height,
                ..Default::default()
            });
        }
    }
    bail!("Failed to find current resolution for {}", name);
}

#[cfg(test)]
mod xrandr_tests {
    use super::*;

    #[test]
    fn output_name_does_not_match_inside_another_name() {
        let xrandr_output = "Screen 0: minimum 320 x 200, current 4480 x 1440, maximum 16384 x 16384
eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 309mm x 174mm
 1920x1080 60.01*+ 59.97
 1280x720 60.00
DP-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm
 2560x1440 59.95*+
 1920x1080 60.00
";
        let re = Regex::new(&get_xrandr_conn_pat("DP-1")).unwrap();
        let caps = re.captures(xrandr_output).unwrap();
        assert_eq!(get_width_height_from_captures(&caps), Some((2560, 1440)));
    }
}

pub fn change_resolution_directly(name: &str, width: usize, height: usize) -> ResultType<()> {
    Command::new("xrandr")
        .args(vec![
            "--output",
            name,
            "--mode",
            &format!("{}x{}", width, height),
        ])
        .spawn()?;
    Ok(())
}

/// 限定在 `uid`（正在刷新的会话所属用户）范围内：合成器以该用户身份启动 Xwayland，
/// 所以另一个用户的 Xwayland —— 被切换走的会话、第二个座位 —— 一旦响应
/// 就会把纯 Wayland 会话引到 Xwayland 探测上，而它没有可用的显示器
/// 供其查找。无法解析的 uid 会回退到无范围限制的结果。
#[inline]
pub fn is_xwayland_running(uid: &str) -> bool {
    // 与它所替换的 `pgrep -a Xwayland` 相同的检测：看进程名，而不是命令行。
    any_process(uid.parse::<u32>().ok(), "comm", |comm| {
        comm.contains("Xwayland")
    })
}

mod desktop {
    use super::*;

    pub const XFCE4_PANEL: &str = "xfce4-panel";
    pub const SDDM_GREETER: &str = "sddm-greeter";

    // xdg-desktop-portal 在所有 Wayland 桌面（GNOME、KDE、wlroots 等）上都会运行
    const XDG_DESKTOP_PORTAL: &str = "xdg-desktop-portal";
    const XWAYLAND: &str = "Xwayland";
    const IBUS_DAEMON: &str = "ibus-daemon";
    const PLASMA_KDED: &str = "kded[0-9]+";
    const GNOME_GOA_DAEMON: &str = "goa-daemon";

    const ENV_KEY_DISPLAY: &str = "DISPLAY";
    const ENV_KEY_XAUTHORITY: &str = "XAUTHORITY";
    const ENV_KEY_WAYLAND_DISPLAY: &str = "WAYLAND_DISPLAY";
    const ENV_KEY_DBUS_SESSION_BUS_ADDRESS: &str = "DBUS_SESSION_BUS_ADDRESS";

    /// 运行 Xwayland 却不导出 `XAUTHORITY` 的合成器（例如基于 wlroots 的 Hyprland）
    /// 仍然能通过 Wayland 一侧提供可用会话。在那里要求 xauth 从来
    /// 不会成功，所以每次刷新都会把重试循环跑到底。
    /// https://github.com/rustdesk/rustdesk/issues/15952
    fn is_session_env_complete(envs: &std::collections::HashMap<&str, String>) -> bool {
        let value = |key: &str| envs.get(key).map_or("", |v| v.as_str());
        !value(ENV_KEY_DISPLAY).is_empty()
            && (!value(ENV_KEY_XAUTHORITY).is_empty()
                || (!value(ENV_KEY_WAYLAND_DISPLAY).is_empty()
                    && !value(ENV_KEY_DBUS_SESSION_BUS_ADDRESS).is_empty()))
    }

    #[derive(Debug, Clone, Default)]
    pub struct Desktop {
        pub sid: String,
        pub username: String,
        pub uid: String,
        pub protocol: String,
        pub display: String,
        pub xauth: String,
        pub home: String,
        pub dbus: String,
        pub wl_display: String,
    }

    impl Desktop {
        #[inline]
        pub fn is_wayland(&self) -> bool {
            self.protocol == DISPLAY_SERVER_WAYLAND
        }

        #[inline]
        pub fn is_login_wayland(&self) -> bool {
            super::is_gdm_user(&self.username) && self.protocol == DISPLAY_SERVER_WAYLAND
        }

        fn get_display_xauth_wayland(&mut self) {
            for _ in 1..=10 {
                // 当匹配到多个 portal 进程时，优先选择与 Wayland 相关的变量。
                let mut envs = get_envs(
                    &self.uid,
                    XDG_DESKTOP_PORTAL,
                    &[
                        ENV_KEY_WAYLAND_DISPLAY,
                        ENV_KEY_DBUS_SESSION_BUS_ADDRESS,
                        ENV_KEY_DISPLAY,
                        ENV_KEY_XAUTHORITY,
                    ],
                );
                self.display = envs.remove(ENV_KEY_DISPLAY).unwrap_or_default();
                self.xauth = envs.remove(ENV_KEY_XAUTHORITY).unwrap_or_default();
                self.wl_display = envs.remove(ENV_KEY_WAYLAND_DISPLAY).unwrap_or_default();
                self.dbus = envs
                    .remove(ENV_KEY_DBUS_SESSION_BUS_ADDRESS)
                    .unwrap_or_default();
                // 对于纯 Wayland 会话，优先使用 `WAYLAND_DISPLAY`。
                // NOTE: On some systems (e.g. Ubuntu 25.10), `DISPLAY`/`XAUTHORITY` may exist even when XWayland
                // 并未运行，所以此处不要把它们当作成功条件。
                let has_wayland = !self.wl_display.is_empty();
                let has_dbus = !self.dbus.is_empty();
                if has_wayland && has_dbus {
                    return;
                }
                sleep_millis(300);
            }
        }

        fn get_display_xauth_xwayland(&mut self) {
            let tray = format!("{} +--tray", crate::get_app_name().to_lowercase());
            for _ in 1..=10 {
                let display_proc = vec![
                    XDG_DESKTOP_PORTAL,
                    XWAYLAND,
                    IBUS_DAEMON,
                    GNOME_GOA_DAEMON,
                    PLASMA_KDED,
                    tray.as_str(),
                ];
                self.display.clear();
                self.xauth.clear();
                self.wl_display.clear();
                self.dbus.clear();
                let mut kept = 0u8;
                for proc in display_proc {
                    let mut envs = get_envs_of_newest(
                        &self.uid,
                        proc,
                        &[
                            ENV_KEY_DISPLAY,
                            ENV_KEY_XAUTHORITY,
                            ENV_KEY_WAYLAND_DISPLAY,
                            ENV_KEY_DBUS_SESSION_BUS_ADDRESS,
                        ],
                    );
                    let complete = is_session_env_complete(&envs);
                    let display = envs.remove(ENV_KEY_DISPLAY).unwrap_or_default();
                    let xauth = envs.remove(ENV_KEY_XAUTHORITY).unwrap_or_default();
                    let wl_display = envs.remove(ENV_KEY_WAYLAND_DISPLAY).unwrap_or_default();
                    let dbus = envs
                        .remove(ENV_KEY_DBUS_SESSION_BUS_ADDRESS)
                        .unwrap_or_default();
                    // 完整地取一个候选。同一用户的两个图形会话各自都会回答
                    // 其中的一部分，而把某个显示器与另一个会话的 xauth 或
                    // 合成器配在一起，则是一对根本不存在的组合。所以应该对候选进行排序，而不是
                    // 把它们合并起来，并保留目前见过的最佳项：靠后的模式是兜底。
                    //
                    // 当 `is_xwayland_running` 匹配到别的
                    // 用户的 Xwayland，而当前会话没有自己的 Xwayland 时，纯 Wayland 的排序就很重要了。这时这里没有任何东西能
                    // 给出显示器，而丢掉那个候选会让
                    // 子服务器失去一个本可正常
                    // 通过它们访问的会话的合成器和总线。
                    let rank = if complete {
                        3
                    } else if !wl_display.is_empty() && !dbus.is_empty() {
                        2
                    } else if !display.is_empty() {
                        1
                    } else {
                        0
                    };
                    if rank > kept {
                        kept = rank;
                        self.display = display;
                        self.xauth = xauth;
                        self.wl_display = wl_display;
                        self.dbus = dbus;
                    }
                    if complete {
                        return;
                    }
                }
                // 仅凭这一对 Wayland 信息，本身就是一个子服务器可以
                // 据此启动的会话 —— 那正是 `get_display_xauth_wayland` 会返回的内容。重试针对的是
                // 尚未完全启动起来的会话，以及 Xwayland 按需启动的
                // 合成器 —— 它可能永远不会导出可供本次遍历查找的 `DISPLAY`，所以为一次探测多等十轮
                // ，每次刷新都要重新付出整个探测的代价。
                if kept >= 2 {
                    break;
                }
                sleep_millis(300);
            }
        }

        fn get_display_x11(&mut self) {
            for _ in 1..=10 {
                let display_proc = vec![
                    XWAYLAND,
                    IBUS_DAEMON,
                    GNOME_GOA_DAEMON,
                    PLASMA_KDED,
                    XFCE4_PANEL,
                    SDDM_GREETER,
                ];
                for proc in display_proc {
                    self.display = get_envs_of_newest(&self.uid, proc, &[ENV_KEY_DISPLAY])
                        .remove(ENV_KEY_DISPLAY)
                        .unwrap_or_default();
                    if !self.display.is_empty() {
                        break;
                    }
                }
                if !self.display.is_empty() {
                    break;
                }
                sleep_millis(300);
            }

            if self.display.is_empty() {
                // logind 保存的是会话创建时交给 pam_systemd 的值，它并不
                // 一定是本地显示器：它可能带有本机主机名限定（`myhost:0`），或者
                // 指向某个 X 转发端点（`localhost:10.0`），有些配置只记录一个裸的
                // `:`。先去掉主机名，然后要求有显示器编号。`localhost` 是刻意
                // 保留在原处的：这里非空的显示器会抑制下面所有兜底逻辑，包括
                // `get_display_by_user` 和 `:0` 默认值，所以任何非本地的值都不能通过。
                let display = Self::get_display_from_session(&self.sid)
                    .replace(&hbb_common::whoami::hostname(), "");
                if display.strip_prefix(':').map_or(false, |number| {
                    number.starts_with(|c: char| c.is_ascii_digit())
                }) {
                    self.display = display;
                }
            }
            if self.display.is_empty() {
                self.display = Self::get_display_by_user(&self.username);
            }
            if self.display.is_empty() {
                self.display = ":0".to_owned();
            }
            self.display = self
                .display
                .replace(&hbb_common::whoami::hostname(), "")
                .replace("localhost", "");
        }

        fn get_display_from_session(session: &str) -> String {
            if session.is_empty() {
                return String::new();
            }

            match Command::new(CMD_LOGINCTL.as_str())
                .args(["show-session", "-p", "Display", session])
                .output()
            {
                Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .strip_prefix("Display=")
                    .unwrap_or_default()
                    .to_owned(),
                Ok(output) => {
                    log::debug!(
                        "Failed to get display for session {session}: {}",
                        output.status
                    );
                    String::new()
                }
                Err(err) => {
                    log::debug!("Failed to get display for session {session}: {err}");
                    String::new()
                }
            }
        }

        fn get_home(&mut self) {
            self.home = "".to_string();

            let cmd = format!(
                "getent passwd '{}' | awk -F':' '{{print $6}}'",
                &self.username
            );
            self.home = run_cmds_trim_newline(&cmd).unwrap_or(format!("/home/{}", &self.username));
        }

        fn get_xauth_from_xorg(&mut self) {
            if let Ok(output) = run_cmds(&format!(
                "ps -u {} -f | grep 'Xorg' | grep -v 'grep'",
                &self.uid
            )) {
                for line in output.lines() {
                    let mut auth_found = false;

                    for v in line.split_whitespace() {
                        if v == "-auth" {
                            auth_found = true;
                        } else if auth_found {
                            if std::path::Path::new(v).is_absolute()
                                && std::path::Path::new(v).exists()
                            {
                                self.xauth = v.to_string();
                            } else {
                                if let Some(pid) = line.split_whitespace().nth(1) {
                                    let mut base_dir: String = String::from("/home"); // default pattern
                                    let home_dir = get_env_from_pid("HOME", pid);
                                    if home_dir.is_empty() {
                                        if let Some(home) = get_user_home_by_name(&self.username) {
                                            base_dir = home.as_path().to_string_lossy().to_string();
                                        };
                                    } else {
                                        base_dir = home_dir;
                                    }
                                    if Path::new(&base_dir).exists() {
                                        self.xauth = format!("{}/{}", base_dir, v);
                                    };
                                } else {
                                    // 不可达！
                                }
                            }
                            return;
                        }
                    }
                }
            }
        }

        fn get_xauth_x11(&mut self) {
            // 尝试按名称直接访问窗口管理器进程
            let tray = format!("{} +--tray", crate::get_app_name().to_lowercase());
            for _ in 1..=10 {
                let display_proc = vec![
                    XWAYLAND,
                    IBUS_DAEMON,
                    GNOME_GOA_DAEMON,
                    PLASMA_KDED,
                    XFCE4_PANEL,
                    SDDM_GREETER,
                    tray.as_str(),
                ];
                for proc in display_proc {
                    self.xauth = get_envs_of_newest(&self.uid, proc, &[ENV_KEY_XAUTHORITY])
                        .remove(ENV_KEY_XAUTHORITY)
                        .unwrap_or_default();
                    if !self.xauth.is_empty() {
                        break;
                    }
                }
                if !self.xauth.is_empty() {
                    break;
                }
                sleep_millis(300);
            }

            // 从 Xorg 进程获取参数和环境变量
            if self.xauth.is_empty() {
                self.get_xauth_from_xorg();
            }

            // 回退到默认文件名
            if self.xauth.is_empty() {
                let gdm = format!("/run/user/{}/gdm/Xauthority", self.uid);
                self.xauth = if std::path::Path::new(&gdm).exists() {
                    gdm
                } else {
                    let username = &self.username;
                    match get_user_home_by_name(username) {
                        None => {
                            if username == "root" {
                                format!("/{}/.Xauthority", username)
                            } else {
                                let tmp = format!("/home/{}/.Xauthority", username);
                                if std::path::Path::new(&tmp).exists() {
                                    tmp
                                } else {
                                    format!("/var/lib/{}/.Xauthority", username)
                                }
                            }
                        }
                        Some(home) => {
                            format!(
                                "{}/.Xauthority",
                                home.as_path().to_string_lossy().to_string()
                            )
                        }
                    }
                };
            }
        }

        fn get_display_by_user(user: &str) -> String {
            // log::debug!("w {}", &user);
            if let Ok(output) = std::process::Command::new("w").arg(&user).output() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let mut iter = line.split_whitespace();
                    let b = iter.nth(2);
                    if let Some(b) = b {
                        if b.starts_with(":") {
                            return b.to_owned();
                        }
                    }
                }
            }
            // 上面的方法对 gdm 用户无效
            // log::debug!("ls -l /tmp/.X11-unix/");
            let mut last = "".to_owned();
            if let Ok(output) = std::process::Command::new("ls")
                .args(vec!["-l", "/tmp/.X11-unix/"])
                .output()
            {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let mut iter = line.split_whitespace();
                    let user_field = iter.nth(2);
                    if let Some(x) = iter.last() {
                        if x.starts_with("X") {
                            last = x.replace("X", ":").to_owned();
                            if user_field == Some(&user) {
                                return last;
                            }
                        }
                    }
                }
            }
            last
        }

        /// 保留活动 seat0 会话的缓存身份，使服务循环只需重试
        /// 后期的 Wayland 显示器发现，而不必重复完整的 seat 查询。
        pub fn refresh(&mut self) {
            if !self.sid.is_empty() && is_active_and_seat0(&self.sid) {
                // 登录后短时间内 Xwayland 显示器和 xauth 可能还不可用。
                // 在 X11 上避免扫描进程，因为 Xwayland 发现无法提供任何
                // 有用的会话信息。
                if self.is_wayland()
                    && !self.is_login_wayland()
                    && is_xwayland_running(&self.uid)
                {
                    self.get_display_xauth_xwayland();
                } else if self.is_wayland() {
                    self.get_display_xauth_wayland();
                }
                return;
            }

            let seat0_values = get_values_of_seat0_with_gdm_wayland(&[0, 1, 2]);
            if seat0_values[0].is_empty() {
                * self = Self::default();
                return;
            }

            self.sid = seat0_values[0].clone();
            self.uid = seat0_values[1].clone();
            self.username = seat0_values[2].clone();
            self.protocol = get_display_server_of_session(&self.sid).into();
            if self.is_login_wayland() {
                self.display = "".to_owned();
                self.xauth = "".to_owned();
                // 即使走这条路径也解析 HOME。上游当时直接返回是因为那之后没有东西
                // 会消费登录 Wayland 的 Desktop，但 drm 构建会在这里以
                // 登录界面 uid 启动一个 `--server`，而没有 HOME 的子进程无处存放其配置。合成器
                // 变量（WAYLAND_DISPLAY、DBUS、DISPLAY、XAUTHORITY）被留空，
                // 这是有意为之，并非疏漏：drm 采集路径是与 root
                // 服务通过 `_drm` 以及渲染节点通信，从不与合成器或 portal 通信，
                // 这正是它能在登录界面工作的全部原因。`try_start_server_` 会跳过
                // 空条目，所以登录界面子进程根本拿不到它们。
                //
                // `is_login_wayland` 需要 `is_gdm_user(username)`，而当前的 GDM 把它的
                // 登录界面以 `gdm-greeter` 运行，该辅助函数匹配不上 —— 在测试主机上实测
                // ，因此登录界面服务器会走下面的分支，得到一个
                // 完整填充的环境变量。这适用于那些登录界面用户
                // 能够匹配的显示管理器。
                #[cfg(feature = "drm")]
                self.get_home();
                return;
            }

            self.get_home();
            if self.is_wayland() {
                if is_xwayland_running(&self.uid) {
                    self.get_display_xauth_xwayland();
                } else {
                    self.get_display_xauth_wayland();
                }
            } else {
                self.get_display_x11();
                self.get_xauth_x11();
            }
        }
    }
}

/// 会话总线上的空闲抑制接口，按顺序尝试；第一个响应的胜出。
/// 故意不包含 `org.freedesktop.ScreenSaver`：那正是 `keepawake` 已经尝试过的。
struct SessionInhibitTarget {
    dest: &'static str,
    path: &'static str,
    iface: &'static str,
    /// GNOME 接受 `(app_id, xid, reason, flags)`；PowerManagement 接受 `(app, reason)`。
    gnome_shape: bool,
    uninhibit: &'static str,
}

/// `org.gnome.SessionManager.Inhibit` 的标志 8 = 仅空闲；注销/切换用户/挂起会占用
/// 坐在机器前的人应当保留的操作。
const GNOME_INHIBIT_IDLE: u32 = 8;

const SESSION_INHIBIT_TARGETS: &[SessionInhibitTarget] = &[
    // 在 GDM 登录界面上实测：带抑制时输出维持 129.9 秒，不带时为 30.3 秒。
    SessionInhibitTarget {
        dest: "org.gnome.SessionManager",
        path: "/org/gnome/SessionManager",
        iface: "org.gnome.SessionManager",
        gnome_shape: true,
        uninhibit: "Uninhibit",
    },
    // powerdevil 和 xfce4-power-manager 实现的就是这个。此处未做测试；在接口缺失时
    // 只需付出一次失败调用的代价，而下面的日志会列出尝试过的每个接口。
    SessionInhibitTarget {
        dest: "org.freedesktop.PowerManagement",
        path: "/org/freedesktop/PowerManagement/Inhibit",
        iface: "org.freedesktop.PowerManagement.Inhibit",
        gnome_shape: false,
        uninhibit: "UnInhibit",
    },
];

/// 用于 `keepawake` 无法处理的场景的空闲抑制：它抑制 `org.freedesktop.ScreenSaver`，
/// 而 GDM 登录界面的总线既不提供也无法激活它，所以它的 `create()` 会直接失败。
/// 之所以保留该连接，是因为抑制与它绑定：丢弃连接就会释放抑制。
struct SessionIdleInhibit {
    conn: dbus::blocking::Connection,
    target: &'static SessionInhibitTarget,
    cookie: u32,
}

impl SessionIdleInhibit {
    fn new(reason: &str) -> Option<Self> {
        let conn = match dbus::blocking::Connection::new_session() {
            Ok(conn) => conn,
            Err(err) => {
                log::info!("wakelock: no session bus for the idle inhibit fallback ({err})");
                return None;
            }
        };
        let app = crate::get_app_name();
        let mut refused = Vec::new();
        for target in SESSION_INHIBIT_TARGETS {
            let res: Result<(u32,), dbus::Error> = {
                let proxy = conn.with_proxy(
                    target.dest,
                    target.path,
                    std::time::Duration::from_secs(3),
                );
                if target.gnome_shape {
                    // Inhibit(s app_id, u xid, s reason, u flags) -> u cookie；xid 0 表示无窗口。
                    proxy.method_call(
                        target.iface,
                        "Inhibit",
                        (app.clone(), 0u32, reason.to_owned(), GNOME_INHIBIT_IDLE),
                    )
                } else {
                    proxy.method_call(target.iface, "Inhibit", (app.clone(), reason.to_owned()))
                }
            };
            match res {
                Ok((cookie,)) => {
                    log::info!(
                        "wakelock: holding a {} idle inhibit (cookie {cookie})",
                        target.dest
                    );
                    return Some(Self {
                        conn,
                        target,
                        cookie,
                    });
                }
                Err(err) => refused.push(format!("{}: {err}", target.dest)),
            }
        }
        // 列出尝试过的每个接口以及失败原因：在未测试的桌面上，正是这条日志
        // 把"屏幕还是会黑屏"变成一份指出缺失接口的报告。
        log::info!(
            "wakelock: no session idle inhibitor answered, so the compositor may still blank this \
             screen ({})",
            refused.join("; ")
        );
        None
    }
}

impl Drop for SessionIdleInhibit {
    fn drop(&mut self) {
        let proxy = self.conn.with_proxy(
            self.target.dest,
            self.target.path,
            std::time::Duration::from_secs(3),
        );
        // 尽力而为：会话管理器把抑制与调用方的总线名称绑定，因此丢弃
        // 下面的 `conn` 即便这次调用没成功也会释放它。
        let res: Result<(), dbus::Error> =
            proxy.method_call(self.target.iface, self.target.uninhibit, (self.cookie,));
        if let Err(err) = res {
            log::debug!("wakelock: releasing the idle inhibit by closing the bus instead ({err})");
        }
    }
}

pub struct WakeLock(Option<keepawake::AwakeHandle>, Option<SessionIdleInhibit>);

impl WakeLock {
    pub fn new(display: bool, idle: bool, sleep: bool) -> Self {
        match keepawake::Builder::new()
            .display(display)
            .idle(idle)
            .sleep(sleep)
            .create()
        {
            Ok(handle) => WakeLock(Some(handle), None),
            Err(err) => {
                // 不用 `.ok()`：被丢弃的错误曾导致登录界面在完全没有抑制器的情况下运行，
                // 而且没有人察觉。
                log::info!("wakelock: keepawake could not take the inhibit ({err})");
                // keepawake 先请求 ScreenSaver 抑制，若失败就放弃整个请求，
                // 从而失去那些能阻止主机在会话中途挂起的 logind 空闲/睡眠抑制，
                // 。重新请求时去掉显示部分：它们位于系统总线上。
                let system = if idle || sleep {
                    match keepawake::Builder::new()
                        .display(false)
                        .idle(idle)
                        .sleep(sleep)
                        .create()
                    {
                        Ok(handle) => Some(handle),
                        Err(err) => {
                            log::info!(
                                "wakelock: the logind idle/sleep inhibit did not come back \
                                 either ({err})"
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                let session = if display {
                    SessionIdleInhibit::new("incoming session")
                } else {
                    None
                };
                WakeLock(system, session)
            }
        }
    }
}

fn has_cmd(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .status()
        .map(|x| x.success())
        .unwrap_or_default()
}

pub fn run_cmds_privileged(cmds: &str) -> bool {
    crate::platform::gtk_sudo::run(vec![cmds]).is_ok()
}

/// 延迟一段时间后派生当前可执行文件。
///
/// # 安全
/// 可执行文件路径使用 `shell_quote()` 安全加引号，以防止
/// 命令注入漏洞。`secs` 参数是 u32，因此它
/// 不可能包含恶意输入。
///
/// # 参数
/// * `secs` —— 派生前等待的秒数
pub fn run_me_with(secs: u32) {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            log::error!("Failed to get current exe: {}", e);
            return;
        }
    };

    // 安全：使用 shell_quote 安全地转义可执行文件路径，
    // 即使路径包含特殊字符也能防止命令注入。
    let exe_quoted = shell_quote(&exe.to_string_lossy());

    // 派生一个先休眠再执行的后台进程。
    // 父进程退出时子进程会自动成为孤儿进程，
    // 并被 init（PID 1）收养。
    Command::new(CMD_SH.as_str())
        .arg("-c")
        .arg(&format!("sleep {secs}; exec {exe_quoted}"))
        .spawn()
        .ok();
}

fn switch_service(stop: bool) -> String {
    // 安全：使用 getpwuid 进行可信的主目录查询，而不是 $HOME 环境变量，
    // 以防止攻击者操纵环境变量的混淆代理攻击。
    let home = get_home_dir_trusted()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    Config::set_option("stop-service".into(), if stop { "Y" } else { "" }.into());
    if !home.is_empty() && home != "/root" && !Config::get().is_empty() {
        let app_name_lower = crate::get_app_name().to_lowercase();
        let app_name0 = crate::get_app_name();
        let config_subdir = format!(".config/{}", app_name_lower);

        // 安全：对所有路径加引号，防止包含
        // 空格、分号或其他特殊字符的路径引发命令注入。
        let src1 = shell_quote(&format!("{}/{}/{}.toml", home, config_subdir, app_name0));
        let src2 = shell_quote(&format!("{}/{}/{}2.toml", home, config_subdir, app_name0));
        let dst = shell_quote(&format!("/root/{}/", config_subdir));

        format!("cp -f {} {}; cp -f {} {};", src1, dst, src2, dst)
    } else {
        "".to_owned()
    }
}

pub fn uninstall_service(show_new_window: bool, _: bool) -> bool {
    if !has_cmd("systemctl") {
        // 在已安装 + flutter run + 由 `show_new_window` 启动时失败。
        return false;
    }
    log::info!("Uninstalling service...");
    let cp = switch_service(true);
    let app_name = crate::get_app_name().to_lowercase();
    // systemctl kill rustdesk --tray，先执行 cp
    if !run_cmds_privileged(&format!(
        "{cp} systemctl disable {app_name}; systemctl stop {app_name};"
    )) {
        Config::set_option("stop-service".into(), "".into());
        return true;
    }
    // systemctl stop 会杀掉子进程，下面的代码可能不会被执行。
    if show_new_window {
        run_me_with(2);
    }
    std::process::exit(0);
}

pub fn install_service() -> bool {
    let _installing = crate::platform::InstallingService::new();
    if !has_cmd("systemctl") {
        return false;
    }
    log::info!("Installing service...");
    let cp = switch_service(false);
    let app_name = crate::get_app_name().to_lowercase();
    if !run_cmds_privileged(&format!(
        "{cp} systemctl enable {app_name}; systemctl start {app_name};"
    )) {
        Config::set_option("stop-service".into(), "Y".into());
    }
    true
}

fn check_if_stop_service() {
    if Config::get_option("stop-service".into()) == "Y" {
        let app_name = crate::get_app_name().to_lowercase();
        allow_err!(run_cmds(&format!(
            "systemctl disable {app_name}; systemctl stop {app_name}"
        )));
    }
}

pub fn check_autostart_config() -> ResultType<()> {
    // 安全：使用 getpwuid 进行可信的主目录查询，而不是 $HOME 环境变量，
    // 以防止攻击者操纵环境变量的混淆代理攻击。
    let home = match get_home_dir_trusted() {
        Some(p) => p.to_string_lossy().to_string(),
        None => {
            log::warn!("Failed to get trusted home directory for autostart config check");
            return Ok(());
        }
    };
    let app_name = crate::get_app_name().to_lowercase();
    let path = format!("{home}/.config/autostart");
    let file = format!("{path}/{app_name}.desktop");
    // https://github.com/rustdesk/rustdesk/issues/4863
    std::fs::remove_file(&file).ok();
    /*
        std::fs::create_dir_all(&path).ok();
        if !Path::new(&file).exists() {
            // 把文本写入 desktop 文件
            let mut file = std::fs::File::create(&file)?;
            file.write_all(
                format!(
                    "
    [Desktop Entry]
    Type=Application
    Exec={app_name} --tray
    NoDisplay=false
            "
                )
                .as_bytes(),
            )?;
        }
        */
    Ok(())
}

pub struct WallPaperRemover {
    old_path: String,
    old_path_dark: Option<String>, // ubuntu 22.04 light/dark theme have different uri
}

impl WallPaperRemover {
    pub fn new() -> ResultType<Self> {
        let start = std::time::Instant::now();
        let old_path = wallpaper::get().map_err(|e| anyhow!(e.to_string()))?;
        let old_path_dark = wallpaper::get_dark().ok();
        if old_path.is_empty() && old_path_dark.clone().unwrap_or_default().is_empty() {
            bail!("already solid color");
        }
        wallpaper::set_from_path("").map_err(|e| anyhow!(e.to_string()))?;
        wallpaper::set_dark_from_path("").ok();
        log::info!(
            "created wallpaper remover,  old_path: {:?}, old_path_dark: {:?}, elapsed: {:?}",
            old_path,
            old_path_dark,
            start.elapsed(),
        );
        Ok(Self {
            old_path,
            old_path_dark,
        })
    }

    pub fn support() -> bool {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        if wallpaper::gnome::is_compliant(&desktop) || desktop.as_str() == "XFCE" {
            return wallpaper::get().is_ok();
        }
        false
    }
}

impl Drop for WallPaperRemover {
    fn drop(&mut self) {
        allow_err!(wallpaper::set_from_path(&self.old_path).map_err(|e| anyhow!(e.to_string())));
        if let Some(old_path_dark) = &self.old_path_dark {
            allow_err!(wallpaper::set_dark_from_path(old_path_dark.as_str())
                .map_err(|e| anyhow!(e.to_string())));
        }
    }
}

#[inline]
pub fn is_x11() -> bool {
    *IS_X11
}

#[inline]
pub fn is_selinux_enforcing() -> bool {
    match run_cmds("getenforce") {
        Ok(output) => output.trim() == "Enforcing",
        Err(_) => match run_cmds("sestatus") {
            Ok(output) => {
                for line in output.lines() {
                    if line.contains("Current mode:") {
                        return line.contains("enforcing");
                    }
                }
                false
            }
            Err(_) => false,
        },
    }
}

/// 获取用于快捷键抑制权限的应用 ID。
/// 根据是运行在 Flatpak 还是原生环境中返回不同的 ID。
/// 该 ID 必须与已安装的 .desktop 文件名一致，因为 GNOME Shell 的
/// inhibitShortcutsDialog 使用了 `Shell.WindowTracker.get_window_app(window).get_id()`。
fn get_shortcuts_inhibitor_app_id() -> String {
    if is_flatpak() {
        // 在 Flatpak 中，FLATPAK_ID 会由运行时自动设置为应用 ID
        // （例如 "com.rustdesk.RustDesk"）。这是最可靠的来源。
        // 如果拿不到，就回退到根据应用名构造。
        match std::env::var("FLATPAK_ID") {
            Ok(id) if !id.is_empty() => format!("{}.desktop", id),
            _ => {
                let app_name = crate::get_app_name();
                format!("com.{}.{}.desktop", app_name.to_lowercase(), app_name)
            }
        }
    } else {
        format!("{}.desktop", crate::get_app_name().to_lowercase())
    }
}

const PERMISSION_STORE_DEST: &str = "org.freedesktop.impl.portal.PermissionStore";
const PERMISSION_STORE_PATH: &str = "/org/freedesktop/impl/portal/PermissionStore";
const PERMISSION_STORE_IFACE: &str = "org.freedesktop.impl.portal.PermissionStore";

/// 通过 D-Bus 清除 GNOME 快捷键抑制权限。
/// 这会让权限对话框能够再次显示。
pub fn clear_gnome_shortcuts_inhibitor_permission() -> ResultType<()> {
    let app_id = get_shortcuts_inhibitor_app_id();
    log::info!(
        "Clearing shortcuts inhibitor permission for app_id: {}, is_flatpak: {}",
        app_id,
        is_flatpak()
    );

    let conn = dbus::blocking::Connection::new_session()?;
    let proxy = conn.with_proxy(
        PERMISSION_STORE_DEST,
        PERMISSION_STORE_PATH,
        std::time::Duration::from_secs(3),
    );

    // DeletePermission(s table, s id, s app) -> ()
    let result: Result<(), dbus::Error> = proxy.method_call(
        PERMISSION_STORE_IFACE,
        "DeletePermission",
        ("gnome", "shortcuts-inhibitor", app_id.as_str()),
    );

    match result {
        Ok(()) => {
            log::info!("Successfully cleared GNOME shortcuts inhibitor permission");
            Ok(())
        }
        Err(e) => {
            let err_name = e.name().unwrap_or("");
            // 如果该权限不存在，那也没关系
            if err_name == "org.freedesktop.portal.Error.NotFound"
                || err_name == "org.freedesktop.DBus.Error.UnknownObject"
                || err_name == "org.freedesktop.DBus.Error.ServiceUnknown"
            {
                log::info!(
                    "GNOME shortcuts inhibitor permission was not set ({})",
                    err_name
                );
                Ok(())
            } else {
                bail!("Failed to clear permission: {}", e)
            }
        }
    }
}

/// 检查 GNOME 快捷键抑制权限是否存在。
pub fn has_gnome_shortcuts_inhibitor_permission() -> bool {
    let app_id = get_shortcuts_inhibitor_app_id();

    let conn = match dbus::blocking::Connection::new_session() {
        Ok(c) => c,
        Err(e) => {
            log::debug!("Failed to connect to session bus: {}", e);
            return false;
        }
    };
    let proxy = conn.with_proxy(
        PERMISSION_STORE_DEST,
        PERMISSION_STORE_PATH,
        std::time::Duration::from_secs(3),
    );

    // Lookup(s table, s id) -> (a{sas} permissions, v data)
    // 我们只需要权限字典；检查 app_id 是否为其中的键。
    let result: Result<
        (
            std::collections::HashMap<String, Vec<String>>,
            dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>,
        ),
        dbus::Error,
    > = proxy.method_call(
        PERMISSION_STORE_IFACE,
        "Lookup",
        ("gnome", "shortcuts-inhibitor"),
    );

    match result {
        Ok((permissions, _)) => {
            let found = permissions.contains_key(&app_id);
            log::debug!(
                "Shortcuts inhibitor permission lookup: app_id={}, found={}, keys={:?}",
                app_id,
                found,
                permissions.keys().collect::<Vec<_>>()
            );
            found
        }
        Err(e) => {
            log::debug!("Failed to query shortcuts inhibitor permission: {}", e);
            false
        }
    }
}
