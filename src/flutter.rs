use crate::{
    client::*,
    flutter_ffi::{EventToUI, SessionID},
    ui_session_interface::{io_loop, InvokeUiSession, Session},
};
use flutter_rust_bridge::StreamSink;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::dlopen::{
    symbor::{Library, Symbol},
    Error as LibError,
};
use hbb_common::{
    anyhow::anyhow, bail, config::LocalConfig, get_version_number, log,
    rendezvous_proto::ConnType, ResultType,
};
use base::message_proto::*;
use serde::Serialize;
use serde_json::json;
#[cfg(target_os = "windows")]
use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    os::raw::{c_char, c_int, c_void},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, RwLock,
    },
};

/// 为 [Desktop Main Page] 和 [Mobile (Client and Server)] 标记 "main"（移动端不需要多窗口，只需要一条全局事件流）
/// 仅为 [Desktop CM Page] 标记 "cm"
pub(crate) const APP_TYPE_MAIN: &str = "main";
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) const APP_TYPE_CM: &str = "cm";
#[cfg(any(target_os = "android", target_os = "ios"))]
pub(crate) const APP_TYPE_CM: &str = "main";

#[cfg(target_os = "macos")]
const CORE_MAIN_EXIT_SUCCESS: i32 = 0;
#[cfg(target_os = "macos")]
const CORE_MAIN_EXIT_FAILURE: i32 = 1;

// 不要删除以下常量。
// 在用到它们时取消注释。
// pub(crate) const APP_TYPE_DESKTOP_REMOTE: &str = "remote";
// pub(crate) const APP_TYPE_DESKTOP_FILE_TRANSFER: &str = "file transfer";
// pub(crate) const APP_TYPE_DESKTOP_PORT_FORWARD: &str = "port forward";

pub type FlutterSession = Arc<Session<FlutterHandler>>;

lazy_static::lazy_static! {
    pub(crate) static ref CUR_SESSION_ID: RwLock<SessionID> = Default::default(); // For desktop only
    static ref GLOBAL_EVENT_STREAM: RwLock<HashMap<String, StreamSink<String>>> = Default::default(); // rust to dart event channel
}

#[cfg(target_os = "windows")]
lazy_static::lazy_static! {
    pub static ref TEXTURE_RGBA_RENDERER_PLUGIN: Result<Library, LibError> = load_plugin_in_app_path("texture_rgba_renderer_plugin.dll");
}

#[cfg(target_os = "linux")]
lazy_static::lazy_static! {
    pub static ref TEXTURE_RGBA_RENDERER_PLUGIN: Result<Library, LibError> = Library::open("libtexture_rgba_renderer_plugin.so");
}

#[cfg(target_os = "macos")]
lazy_static::lazy_static! {
    pub static ref TEXTURE_RGBA_RENDERER_PLUGIN: Result<Library, LibError> = Library::open_self();
}

#[cfg(target_os = "windows")]
lazy_static::lazy_static! {
    pub static ref TEXTURE_GPU_RENDERER_PLUGIN: Result<Library, LibError> = load_plugin_in_app_path("flutter_gpu_texture_renderer_plugin.dll");
}

// 如果加载插件的调用变多，把这个函数移到 `src/platform/windows.rs`。
// 用完整路径加载 dll。
#[cfg(target_os = "windows")]
fn load_plugin_in_app_path(dll_name: &str) -> Result<Library, LibError> {
    match std::env::current_exe() {
        Ok(exe_file) => {
            if let Some(cur_dir) = exe_file.parent() {
                let full_path = cur_dir.join(dll_name);
                if !full_path.exists() {
                    Err(LibError::OpeningLibraryError(IoError::new(
                        IoErrorKind::NotFound,
                        format!("{} not found", dll_name),
                    )))
                } else {
                    Library::open(full_path)
                }
            } else {
                Err(LibError::OpeningLibraryError(IoError::new(
                    IoErrorKind::Other,
                    format!(
                        "Invalid exe parent for {}",
                        exe_file.to_string_lossy().as_ref()
                    ),
                )))
            }
        }
        Err(e) => Err(LibError::OpeningLibraryError(e)),
    }
}

/// rustdesk core 主入口的 FFI。
/// 如果应用应继续带 UI（可能是 Flutter）运行则返回 true，如果应用应退出则返回 false。
/// 在 macOS 上，仅命令行的路径会在这里退出，因为 Swift 调用方会忽略
/// 返回值；因此更新失败必须使用非零的退出状态。
#[cfg(not(windows))]
#[no_mangle]
pub extern "C" fn rustdesk_core_main() -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use hbb_common::libc;

        // 原生运行器绕过了 Rust 的启动流程，而后者通常会忽略 SIGPIPE。
        if unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) } == libc::SIG_ERR {
            eprintln!("Failed to ignore SIGPIPE: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    match crate::core_main::core_main() {
        Some(crate::core_main::CoreMainAction::StartUi(_)) => return true,
        Some(crate::core_main::CoreMainAction::ExitFailure(err)) => {
            eprintln!("Command failed: {err}");
            log::error!("Command failed: {err}");
            #[cfg(target_os = "macos")]
            std::process::exit(CORE_MAIN_EXIT_FAILURE);
        }
        None => {
            #[cfg(target_os = "macos")]
            std::process::exit(CORE_MAIN_EXIT_SUCCESS);
        }
    }
    #[cfg(not(target_os = "macos"))]
    false
}

#[cfg(target_os = "macos")]
#[no_mangle]
pub extern "C" fn handle_applicationShouldOpenUntitledFile() {
    crate::platform::macos::handle_application_should_open_untitled_file();
}

#[cfg(windows)]
#[no_mangle]
pub extern "C" fn rustdesk_core_main_args(args_len: *mut c_int) -> *mut *mut c_char {
    unsafe { std::ptr::write(args_len, 0) };
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        match crate::core_main::core_main() {
            Some(crate::core_main::CoreMainAction::StartUi(args)) => {
                return rust_args_to_c_args(args, args_len);
            }
            Some(crate::core_main::CoreMainAction::ExitFailure(err)) => {
                log::error!("Command failed: {err}");
            }
            None => {}
        }
        return std::ptr::null_mut() as _;
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return std::ptr::null_mut() as _;
}

#[cfg(windows)]
#[no_mangle]
pub extern "C" fn rustdesk_is_disable_installation() -> c_int {
    hbb_common::config::is_disable_installation() as c_int
}

// https://gist.github.com/iskakaushik/1c5b8aa75c77479c33c4320913eebef6
#[cfg(windows)]
fn rust_args_to_c_args(args: Vec<String>, outlen: *mut c_int) -> *mut *mut c_char {
    let mut v = vec![];

    // 用一个以 null 结尾的字符串填充 vector
    for s in args {
        match CString::new(s) {
            Ok(s) => v.push(s),
            Err(_) => return std::ptr::null_mut() as _,
        }
    }

    // 把每个以 null 结尾的字符串转换成指针。
    // `into_raw` 取得所有权，给我们指针，并且不会释放数据。
    let mut out = v.into_iter().map(|s| s.into_raw()).collect::<Vec<_>>();

    // 确保不浪费空间。
    out.shrink_to_fit();
    debug_assert!(out.len() == out.capacity());

    // 获取指向我们 vector 的指针。
    let len = out.len();
    let ptr = out.as_mut_ptr();
    std::mem::forget(out);

    // 写回调用方可以预期的长度
    unsafe { std::ptr::write(outlen, len as c_int) };

    // 最后返回数据
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn free_c_args(ptr: *mut *mut c_char, len: c_int) {
    let len = len as usize;

    // 取回我们的 vector。
    // 之前我们做了 shrink_to_fit，所以 capacity == length。
    let v = Vec::from_raw_parts(ptr, len, len);

    // 现在逐个释放字符串。
    for elem in v {
        let s = CString::from_raw(elem);
        std::mem::drop(s);
    }

    // 之后 vector 会被 drop，从而被释放。
}

#[cfg(windows)]
#[no_mangle]
pub unsafe extern "C" fn get_rustdesk_app_name(buffer: *mut u16, length: i32) -> i32 {
    let name = crate::platform::wide_string(&crate::get_app_name());
    if length > name.len() as i32 {
        std::ptr::copy_nonoverlapping(name.as_ptr(), buffer, name.len());
        return 0;
    }
    -1
}

#[derive(Default)]
struct SessionHandler {
    event_stream: Option<StreamSink<EventToUI>>,
    // 当前会话的 displays。
    // 我们需要这个变量，在把 rgba 推送给 flutter 之前检查该 display 是否正在使用。
    displays: Vec<usize>,
    renderer: VideoRenderer,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum RenderType {
    PixelBuffer,
    #[cfg(feature = "vram")]
    Texture,
}

#[derive(Clone)]
pub struct FlutterHandler {
    // ui session id -> display handler 数据
    session_handlers: Arc<RwLock<HashMap<SessionID, SessionHandler>>>,
    display_rgbas: Arc<RwLock<HashMap<usize, RgbaData>>>,
    peer_info: Arc<RwLock<PeerInfo>>,
    use_texture_render: Arc<AtomicBool>,
}

impl Default for FlutterHandler {
    fn default() -> Self {
        Self {
            session_handlers: Default::default(),
            display_rgbas: Default::default(),
            peer_info: Default::default(),
            use_texture_render: Arc::new(
                AtomicBool::new(crate::ui_interface::use_texture_render()),
            ),
        }
    }
}

#[derive(Default, Clone)]
struct RgbaData {
    // 安全性：[rgba] 由 [rgba_valid] 保护，在 `rgba_valid == true` 时访问 [rgba] 是安全的。
    // 读取 [rgba] 前必须检查 `rgba_valid`。
    data: Vec<u8>,
    valid: bool,
}

pub type FlutterRgbaRendererPluginOnRgba = unsafe extern "C" fn(
    texture_rgba: *mut c_void,
    buffer: *const u8,
    len: c_int,
    width: c_int,
    height: c_int,
    dst_rgba_stride: c_int,
);

#[cfg(feature = "vram")]
pub type FlutterGpuTextureRendererPluginCApiSetTexture =
    unsafe extern "C" fn(output: *mut c_void, texture: *mut c_void);

#[cfg(feature = "vram")]
pub type FlutterGpuTextureRendererPluginCApiGetAdapterLuid = unsafe extern "C" fn() -> i64;

pub(super) type TextureRgbaPtr = usize;

struct DisplaySessionInfo {
    // flutter native 中的 TextureRgba 指针。
    texture_rgba_ptr: TextureRgbaPtr,
    size: (usize, usize),
    #[cfg(feature = "vram")]
    gpu_output_ptr: usize,
    notify_render_type: Option<RenderType>,
}

// Flutter 中的视频纹理渲染器
#[derive(Clone)]
struct VideoRenderer {
    is_support_multi_ui_session: bool,
    map_display_sessions: Arc<RwLock<HashMap<usize, DisplaySessionInfo>>>,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    on_rgba_func: Option<Symbol<'static, FlutterRgbaRendererPluginOnRgba>>,
    #[cfg(feature = "vram")]
    on_texture_func: Option<Symbol<'static, FlutterGpuTextureRendererPluginCApiSetTexture>>,
}

impl Default for VideoRenderer {
    fn default() -> Self {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let on_rgba_func = match &*TEXTURE_RGBA_RENDERER_PLUGIN {
            Ok(lib) => {
                let find_sym_res = unsafe {
                    lib.symbol::<FlutterRgbaRendererPluginOnRgba>("FlutterRgbaRendererPluginOnRgba")
                };
                match find_sym_res {
                    Ok(sym) => Some(sym),
                    Err(e) => {
                        log::error!("Failed to find symbol FlutterRgbaRendererPluginOnRgba, {e}");
                        None
                    }
                }
            }
            Err(e) => {
                log::error!("Failed to load texture rgba renderer plugin, {e}");
                None
            }
        };
        #[cfg(feature = "vram")]
        let on_texture_func = match &*TEXTURE_GPU_RENDERER_PLUGIN {
            Ok(lib) => {
                let find_sym_res = unsafe {
                    lib.symbol::<FlutterGpuTextureRendererPluginCApiSetTexture>(
                        "FlutterGpuTextureRendererPluginCApiSetTexture",
                    )
                };
                match find_sym_res {
                    Ok(sym) => Some(sym),
                    Err(e) => {
                        log::error!("Failed to find symbol FlutterGpuTextureRendererPluginCApiSetTexture, {e}");
                        None
                    }
                }
            }
            Err(e) => {
                log::error!("Failed to load texture gpu renderer plugin, {e}");
                None
            }
        };

        Self {
            map_display_sessions: Default::default(),
            is_support_multi_ui_session: false,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            on_rgba_func,
            #[cfg(feature = "vram")]
            on_texture_func,
        }
    }
}

impl VideoRenderer {
    #[inline]
    fn set_size(&mut self, display: usize, width: usize, height: usize) {
        let mut sessions_lock = self.map_display_sessions.write().unwrap();
        if let Some(info) = sessions_lock.get_mut(&display) {
            info.size = (width, height);
            info.notify_render_type = None;
        } else {
            sessions_lock.insert(
                display,
                DisplaySessionInfo {
                    texture_rgba_ptr: usize::default(),
                    size: (width, height),
                    #[cfg(feature = "vram")]
                    gpu_output_ptr: usize::default(),
                    notify_render_type: None,
                },
            );
        }
    }

    fn register_pixelbuffer_texture(&self, display: usize, ptr: usize) {
        let mut sessions_lock = self.map_display_sessions.write().unwrap();
        if ptr == 0 {
            if let Some(info) = sessions_lock.get_mut(&display) {
                if info.texture_rgba_ptr != usize::default() {
                    info.texture_rgba_ptr = usize::default();
                }
                #[cfg(feature = "vram")]
                if info.gpu_output_ptr != usize::default() {
                    return;
                }
            }
            sessions_lock.remove(&display);
        } else {
            if let Some(info) = sessions_lock.get_mut(&display) {
                if info.texture_rgba_ptr != usize::default()
                    && info.texture_rgba_ptr != ptr as TextureRgbaPtr
                {
                    log::warn!(
                        "texture_rgba_ptr is not null and not equal to ptr, replace {} to {}",
                        info.texture_rgba_ptr,
                        ptr
                    );
                }
                info.texture_rgba_ptr = ptr as _;
                info.notify_render_type = None;
            } else {
                if ptr != 0 {
                    sessions_lock.insert(
                        display,
                        DisplaySessionInfo {
                            texture_rgba_ptr: ptr as _,
                            size: (0, 0),
                            #[cfg(feature = "vram")]
                            gpu_output_ptr: usize::default(),
                            notify_render_type: None,
                        },
                    );
                }
            }
        }
    }

    #[cfg(feature = "vram")]
    pub fn register_gpu_output(&self, display: usize, ptr: usize) {
        let mut sessions_lock = self.map_display_sessions.write().unwrap();
        if ptr == 0 {
            if let Some(info) = sessions_lock.get_mut(&display) {
                if info.gpu_output_ptr != usize::default() {
                    info.gpu_output_ptr = usize::default();
                }
                if info.texture_rgba_ptr != usize::default() {
                    return;
                }
            }
            sessions_lock.remove(&display);
        } else {
            if let Some(info) = sessions_lock.get_mut(&display) {
                if info.gpu_output_ptr != usize::default() && info.gpu_output_ptr != ptr {
                    log::error!(
                        "gpu_output_ptr is not null and not equal to ptr, relace {} to {}",
                        info.gpu_output_ptr,
                        ptr
                    );
                }
                info.gpu_output_ptr = ptr as _;
                info.notify_render_type = None;
            } else {
                if ptr != usize::default() {
                    sessions_lock.insert(
                        display,
                        DisplaySessionInfo {
                            texture_rgba_ptr: usize::default(),
                            size: (0, 0),
                            gpu_output_ptr: ptr,
                            notify_render_type: None,
                        },
                    );
                }
            }
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn on_rgba(&self, display: usize, rgba: &scrap::ImageRgb) -> bool {
        let mut write_lock = self.map_display_sessions.write().unwrap();
        let opt_info = if !self.is_support_multi_ui_session {
            write_lock.values_mut().next()
        } else {
            write_lock.get_mut(&display)
        };
        let Some(info) = opt_info else {
            return false;
        };
        if info.texture_rgba_ptr == usize::default() {
            return false;
        }

        if info.size.0 != rgba.w || info.size.1 != rgba.h {
            log::error!(
                "width/height mismatch: ({},{}) != ({},{})",
                info.size.0,
                info.size.1,
                rgba.w,
                rgba.h
            );
            // 对端信息的处理是异步的，可能比视频帧的处理更晚
            // 允许对端信息未设置，但不允许宽/高不正确，以免本地光标位置出错
            if info.size != (0, 0) {
                return false;
            }
        }
        if let Some(func) = &self.on_rgba_func {
            unsafe {
                func(
                    info.texture_rgba_ptr as _,
                    rgba.raw.as_ptr() as _,
                    rgba.raw.len() as _,
                    rgba.w as _,
                    rgba.h as _,
                    rgba.align() as _,
                )
            };
        }
        if info.notify_render_type != Some(RenderType::PixelBuffer) {
            info.notify_render_type = Some(RenderType::PixelBuffer);
            true
        } else {
            false
        }
    }

    #[cfg(feature = "vram")]
    pub fn on_texture(&self, display: usize, texture: *mut c_void) -> bool {
        let mut write_lock = self.map_display_sessions.write().unwrap();
        let opt_info = if !self.is_support_multi_ui_session {
            write_lock.values_mut().next()
        } else {
            write_lock.get_mut(&display)
        };
        let Some(info) = opt_info else {
            return false;
        };
        if info.gpu_output_ptr == usize::default() {
            return false;
        }
        if let Some(func) = &self.on_texture_func {
            unsafe { func(info.gpu_output_ptr as _, texture) };
        }
        if info.notify_render_type != Some(RenderType::Texture) {
            info.notify_render_type = Some(RenderType::Texture);
            true
        } else {
            false
        }
    }

    pub fn reset_all_display_render_type(&self) {
        let mut write_lock = self.map_display_sessions.write().unwrap();
        write_lock
            .values_mut()
            .map(|v| v.notify_render_type = None)
            .count();
    }
}

impl SessionHandler {
    pub fn on_waiting_for_image_dialog_show(&self) {
        self.renderer.reset_all_display_render_type();
        // rgba 数组渲染每帧都会通知
    }
}

impl FlutterHandler {
    /// 向所有事件队列推送一个事件。
    /// 事件以 json 形式存储在事件队列中。
    ///
    /// # 参数
    ///
    /// * `name` - 事件名称。
    /// * `event` - 事件内容的字段。
    pub fn push_event<V>(&self, name: &str, event: &[(&str, V)], excludes: &[&SessionID])
    where
        V: Sized + Serialize + Clone,
    {
        self.push_event_(name, event, &[], excludes);
    }

    pub fn push_event_to<V>(&self, name: &str, event: &[(&str, V)], include: &[&SessionID])
    where
        V: Sized + Serialize + Clone,
    {
        self.push_event_(name, event, include, &[]);
    }

    pub fn push_event_<V>(
        &self,
        name: &str,
        event: &[(&str, V)],
        includes: &[&SessionID],
        excludes: &[&SessionID],
    ) where
        V: Sized + Serialize + Clone,
    {
        let mut h: HashMap<&str, serde_json::Value> =
            event.iter().map(|(k, v)| (*k, json!(*v))).collect();
        debug_assert!(h.get("name").is_none());
        h.insert("name", json!(name));
        let out = serde_json::ser::to_string(&h).unwrap_or("".to_owned());
        for (sid, session) in self.session_handlers.read().unwrap().iter() {
            let mut push = false;
            if includes.is_empty() {
                if !excludes.contains(&sid) {
                    push = true;
                }
            } else {
                if includes.contains(&sid) {
                    push = true;
                }
            }
            if push {
                if let Some(stream) = &session.event_stream {
                    stream.add(EventToUI::Event(out.clone()));
                }
            }
        }
    }

    pub(crate) fn close_event_stream(&self, session_id: SessionID) {
        // 待办：确认下面的逻辑是正确的。
        // 无需移除 display handler，因为连接关闭时它会被移除。
        if let Some(session) = self.session_handlers.write().unwrap().get_mut(&session_id) {
            try_send_close_event(&session.event_stream);
        }
    }

    fn make_displays_msg(displays: &Vec<DisplayInfo>) -> String {
        let mut msg_vec = Vec::new();
        for ref d in displays.iter() {
            let mut h: HashMap<&str, i32> = Default::default();
            h.insert("x", d.x);
            h.insert("y", d.y);
            h.insert("width", d.width);
            h.insert("height", d.height);
            h.insert("cursor_embedded", if d.cursor_embedded { 1 } else { 0 });
            if let Some(original_resolution) = d.original_resolution.as_ref() {
                h.insert("original_width", original_resolution.width);
                h.insert("original_height", original_resolution.height);
            }
            // 不要直接把 scale（乘 100）转换成 i32。
            // (d.scale * 100.0f64) as i32 可能产生精度误差。
            //
            // 示例：启用 Fractional Scaling 的 GNOME Wayland：
            // - 物理分辨率：2560x1600
            // - 逻辑分辨率：1074x1065
            // - 缩放因子：150%
            // 传入物理尺寸和缩放因子会导致逻辑分辨率计算不准确
            // 因为 2560/1.5 = 1706.666...（四舍五入为 1706.67），1600/1.5 = 1066.666...（四舍五入为 1066.67）
            // h.insert("scale", (d.scale * 100.0f64) as i32);

            // 发送 scaled_width 以准确计算逻辑缩放。
            if d.scale > 0.0 {
                let scaled_width = (d.width as f64 / d.scale).round() as i32;
                h.insert("scaled_width", scaled_width);
            }
            msg_vec.push(h);
        }
        serde_json::ser::to_string(&msg_vec).unwrap_or("".to_owned())
    }

    pub fn update_use_texture_render(&self) {
        self.use_texture_render
            .store(crate::ui_interface::use_texture_render(), Ordering::Relaxed);
        self.display_rgbas.write().unwrap().clear();
    }
}

impl InvokeUiSession for FlutterHandler {
    // 放在事件流上，以保持它与周围的 cursor_id 事件的顺序。
    fn set_cursor_data(&self, cd: CursorData) {
        let colors = cd.colors.to_vec();
        for session in self.session_handlers.read().unwrap().values() {
            if let Some(stream) = &session.event_stream {
                stream.add(EventToUI::Cursor {
                    id: cd.id.to_string(),
                    hotx: cd.hotx,
                    hoty: cd.hoty,
                    width: cd.width,
                    height: cd.height,
                    colors: colors.clone(),
                });
            }
        }
    }

    fn set_cursor_id(&self, id: String) {
        self.push_event("cursor_id", &[("id", &id.to_string())], &[]);
    }

    fn set_cursor_position(&self, cp: CursorPosition) {
        self.push_event(
            "cursor_position",
            &[("x", &cp.x.to_string()), ("y", &cp.y.to_string())],
            &[],
        );
    }

    /// 在 flutter 中未使用，请使用 switch_display 或 set_peer_info
    fn set_display(&self, _x: i32, _y: i32, _w: i32, _h: i32, _cursor_embedded: bool, _scale: f64) {}

    fn update_privacy_mode(&self) {
        self.push_event::<&str>("update_privacy_mode", &[], &[]);
    }

    fn set_permission(&self, name: &str, value: bool) {
        self.push_event("permission", &[(name, &value.to_string())], &[]);
    }

    // 在 flutter 中未使用
    fn close_success(&self) {}

    fn update_quality_status(&self, status: QualityStatus) {
        const NULL: String = String::new();
        self.push_event(
            "update_quality_status",
            &[
                ("speed", &status.speed.map_or(NULL, |it| it)),
                (
                    "fps",
                    &serde_json::ser::to_string(&status.fps).unwrap_or(NULL.to_owned()),
                ),
                ("delay", &status.delay.map_or(NULL, |it| it.to_string())),
                (
                    "target_bitrate",
                    &status.target_bitrate.map_or(NULL, |it| it.to_string()),
                ),
                (
                    "codec_format",
                    &status.codec_format.map_or(NULL, |it| it.to_string()),
                ),
                ("chroma", &status.chroma.map_or(NULL, |it| it.to_string())),
            ],
            &[],
        );
    }

    fn set_connection_type(&self, is_secured: bool, direct: bool, stream_type: &str) {
        self.push_event(
            "connection_ready",
            &[
                ("secure", &is_secured.to_string()),
                ("direct", &direct.to_string()),
                ("stream_type", &stream_type.to_string()),
            ],
            &[],
        );
    }

    fn set_fingerprint(&self, fingerprint: String) {
        self.push_event("fingerprint", &[("fingerprint", &fingerprint)], &[]);
    }

    fn job_error(&self, id: i32, err: String, file_num: i32) {
        self.push_event(
            "job_error",
            &[
                ("id", &id.to_string()),
                ("err", &err),
                ("file_num", &file_num.to_string()),
            ],
            &[],
        );
    }

    fn job_done(&self, id: i32, file_num: i32) {
        self.push_event(
            "job_done",
            &[("id", &id.to_string()), ("file_num", &file_num.to_string())],
            &[],
        );
    }

    // 在 flutter 中未使用
    fn clear_all_jobs(&self) {}

    fn load_last_job(&self, _cnt: i32, job_json: &str, _auto_start: bool) {
        self.push_event("load_last_job", &[("value", job_json)], &[]);
    }

    fn update_folder_files(
        &self,
        id: i32,
        entries: &Vec<FileEntry>,
        path: String,
        #[allow(unused_variables)] is_local: bool,
        only_count: bool,
    ) {
        // TODO opt
        if only_count {
            self.push_event(
                "update_folder_files",
                &[("info", &make_fd_flutter(id, entries, only_count))],
                &[],
            );
        } else {
            self.push_event(
                "file_dir",
                &[
                    ("is_local", "false"),
                    ("value", &crate::common::make_fd_to_json(id, path, entries)),
                ],
                &[],
            );
        }
    }

    fn update_empty_dirs(&self, res: ReadEmptyDirsResponse) {
        self.push_event(
            "empty_dirs",
            &[
                ("is_local", "false"),
                (
                    "value",
                    &crate::common::make_empty_dirs_response_to_json(&res),
                ),
            ],
            &[],
        );
    }

    // 在 flutter 中未使用
    fn update_transfer_list(&self) {}

    // 在 flutter 中未使用 // TEST flutter
    fn confirm_delete_files(&self, _id: i32, _i: i32, _name: String) {}

    fn override_file_confirm(
        &self,
        id: i32,
        file_num: i32,
        to: String,
        is_upload: bool,
        is_identical: bool,
    ) {
        self.push_event(
            "override_file_confirm",
            &[
                ("id", &id.to_string()),
                ("file_num", &file_num.to_string()),
                ("read_path", &to),
                ("is_upload", &is_upload.to_string()),
                ("is_identical", &is_identical.to_string()),
            ],
            &[],
        );
    }

    fn job_progress(&self, id: i32, file_num: i32, speed: f64, finished_size: f64) {
        self.push_event(
            "job_progress",
            &[
                ("id", &id.to_string()),
                ("file_num", &file_num.to_string()),
                ("speed", &speed.to_string()),
                ("finished_size", &finished_size.to_string()),
            ],
            &[],
        );
    }

    // 在 flutter 中未使用
    fn adapt_size(&self) {}

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn on_rgba(&self, display: usize, rgba: &mut scrap::ImageRgb) {
        let use_texture_render = self.use_texture_render.load(Ordering::Relaxed);
        self.on_rgba_flutter_texture_render(use_texture_render, display, rgba);
        if !use_texture_render {
            self.on_rgba_soft_render(display, rgba);
        }
    }

    #[inline]
    #[cfg(any(target_os = "android", target_os = "ios"))]
    fn on_rgba(&self, display: usize, rgba: &mut scrap::ImageRgb) {
        self.on_rgba_soft_render(display, rgba);
    }

    #[inline]
    #[cfg(feature = "vram")]
    fn on_texture(&self, display: usize, texture: *mut c_void) {
        if !self.use_texture_render.load(Ordering::Relaxed) {
            return;
        }
        for (_, session) in self.session_handlers.read().unwrap().iter() {
            if session.renderer.on_texture(display, texture) {
                if let Some(stream) = &session.event_stream {
                    stream.add(EventToUI::Texture(display, true));
                }
            }
        }
    }

    fn set_peer_info(&self, pi: &PeerInfo) {
        let displays = Self::make_displays_msg(&pi.displays);
        let mut features: HashMap<&str, bool> = Default::default();
        for ref f in pi.features.iter() {
            features.insert("privacy_mode", f.privacy_mode);
        }
        // 与 1.1.9 兼容
        if get_version_number(&pi.version) < get_version_number("1.2.0") {
            features.insert("privacy_mode", false);
        }
        let features = serde_json::ser::to_string(&features).unwrap_or("".to_owned());
        let resolutions = serialize_resolutions(&pi.resolutions.resolutions);
        * self.peer_info.write().unwrap() = pi.clone();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let is_support_multi_ui_session = crate::common::is_support_multi_ui_session(&pi.version);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let is_support_multi_ui_session = false;
        self.session_handlers
            .write()
            .unwrap()
            .values_mut()
            .for_each(|h| {
                h.renderer.is_support_multi_ui_session = is_support_multi_ui_session;
            });
        self.push_event(
            "peer_info",
            &[
                ("username", &pi.username),
                ("hostname", &pi.hostname),
                ("platform", &pi.platform),
                ("sas_enabled", &pi.sas_enabled.to_string()),
                ("displays", &displays),
                ("version", &pi.version),
                ("features", &features),
                ("current_display", &pi.current_display.to_string()),
                ("resolutions", &resolutions),
                ("platform_additions", &pi.platform_additions),
            ],
            &[],
        );
    }

    fn set_displays(&self, displays: &Vec<DisplayInfo>) {
        self.peer_info.write().unwrap().displays = displays.clone();
        self.push_event(
            "sync_peer_info",
            &[("displays", &Self::make_displays_msg(displays))],
            &[],
        );
    }

    fn set_platform_additions(&self, data: &str) {
        self.push_event(
            "sync_platform_additions",
            &[("platform_additions", &data)],
            &[],
        )
    }

    fn set_multiple_windows_session(&self, sessions: Vec<WindowsSession>) {
        let mut msg_vec = Vec::new();
        let mut sessions = sessions;
        for d in sessions.drain(..) {
            let mut h: HashMap<&str, String> = Default::default();
            h.insert("sid", d.sid.to_string());
            h.insert("name", d.name);
            msg_vec.push(h);
        }
        self.push_event(
            "set_multiple_windows_session",
            &[(
                "windows_sessions",
                &serde_json::ser::to_string(&msg_vec).unwrap_or("".to_owned()),
            )],
            &[],
        );
    }

    fn is_multi_ui_session(&self) -> bool {
        self.session_handlers.read().unwrap().len() > 1
    }

    fn set_current_display(&self, disp_idx: i32) {
        if self.is_multi_ui_session() {
            return;
        }
        self.push_event(
            "follow_current_display",
            &[("display_idx", &disp_idx.to_string())],
            &[],
        );
    }

    fn on_connected(&self, _conn_type: ConnType) {}

    fn msgbox(&self, msgtype: &str, title: &str, text: &str, link: &str, retry: bool) {
        let has_retry = if retry { "true" } else { "" };
        self.push_event(
            "msgbox",
            &[
                ("type", msgtype),
                ("title", title),
                ("text", text),
                ("link", link),
                ("hasRetry", has_retry),
            ],
            &[],
        );
    }

    fn cancel_msgbox(&self, tag: &str) {
        self.push_event("cancel_msgbox", &[("tag", tag)], &[]);
    }

    fn new_message(&self, msg: String) {
        self.push_event("chat_client_mode", &[("text", &msg)], &[]);
    }

    fn switch_display(&self, display: &SwitchDisplay) {
        let resolutions = serialize_resolutions(&display.resolutions.resolutions);
        self.push_event(
            "switch_display",
            &[
                ("display", &display.display.to_string()),
                ("x", &display.x.to_string()),
                ("y", &display.y.to_string()),
                ("width", &display.width.to_string()),
                ("height", &display.height.to_string()),
                (
                    "cursor_embedded",
                    &{
                        if display.cursor_embedded {
                            1
                        } else {
                            0
                        }
                    }
                    .to_string(),
                ),
                ("resolutions", &resolutions),
                (
                    "original_width",
                    &display.original_resolution.width.to_string(),
                ),
                (
                    "original_height",
                    &display.original_resolution.height.to_string(),
                ),
            ],
            &[],
        );
    }

    fn update_block_input_state(&self, on: bool) {
        self.push_event(
            "update_block_input_state",
            &[("input_state", if on { "on" } else { "off" })],
            &[],
        );
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    fn clipboard(&self, content: String) {
        self.push_event("clipboard", &[("content", &content)], &[]);
    }

    fn switch_back(&self, peer_id: &str) {
        self.push_event("switch_back", &[("peer_id", peer_id)], &[]);
    }

    fn portable_service_running(&self, running: bool) {
        self.push_event(
            "portable_service_running",
            &[("running", running.to_string().as_str())],
            &[],
        );
    }

    fn on_voice_call_started(&self) {
        self.push_event::<&str>("on_voice_call_started", &[], &[]);
    }

    fn on_voice_call_closed(&self, reason: &str) {
        let _res = self.push_event("on_voice_call_closed", &[("reason", reason)], &[]);
    }

    fn on_voice_call_waiting(&self) {
        self.push_event::<&str>("on_voice_call_waiting", &[], &[]);
    }

    fn on_voice_call_incoming(&self) {
        self.push_event::<&str>("on_voice_call_incoming", &[], &[]);
    }

    #[inline]
    fn get_rgba(&self, _display: usize) -> *const u8 {
        if let Some(rgba_data) = self.display_rgbas.read().unwrap().get(&_display) {
            if rgba_data.valid {
                return rgba_data.data.as_ptr();
            }
        }
        std::ptr::null_mut()
    }

    #[inline]
    fn next_rgba(&self, _display: usize) {
        if let Some(rgba_data) = self.display_rgbas.write().unwrap().get_mut(&_display) {
            rgba_data.valid = false;
        }
    }

    fn update_record_status(&self, start: bool) {
        self.push_event("record_status", &[("start", &start.to_string())], &[]);
    }

    fn printer_request(&self, id: i32, path: String) {
        self.push_event(
            "printer_request",
            &[("id", json!(id)), ("path", json!(path))],
            &[],
        );
    }

    fn handle_screenshot_resp(&self, sid: String, msg: String) {
        match SessionID::from_str(&sid) {
            Ok(sid) => self.push_event_to("screenshot", &[("msg", json!(msg))], &[&sid]),
            Err(e) => {
                // 不可达！
                log::error!("Failed to parse sid \"{}\", {}", sid, e);
            }
        }
    }

    fn handle_terminal_response(&self, response: TerminalResponse) {
        use base::message_proto::terminal_response::Union;

        match response.union {
            Some(Union::Opened(opened)) => {
                let mut event_data: Vec<(&str, serde_json::Value)> = vec![
                    ("type", json!("opened")),
                    ("terminal_id", json!(opened.terminal_id)),
                    ("success", json!(opened.success)),
                    ("message", json!(&opened.message)),
                    ("pid", json!(opened.pid)),
                    ("service_id", json!(&opened.service_id)),
                    (
                        "replay_terminal_output",
                        json!(opened.replay_terminal_output),
                    ),
                ];
                if !opened.persistent_sessions.is_empty() {
                    event_data.push(("persistent_sessions", json!(opened.persistent_sessions)));
                }
                self.push_event_("terminal_response", &event_data, &[], &[]);
            }
            Some(Union::Data(data)) => {
                // 如有必要则解压数据
                let output_data = if data.compressed {
                    hbb_common::compress::decompress(&data.data)
                } else {
                    data.data.to_vec()
                };

                let encoded = crate::encode64(&output_data);
                let event_data: Vec<(&str, serde_json::Value)> = vec![
                    ("type", json!("data")),
                    ("terminal_id", json!(data.terminal_id)),
                    ("data", json!(&encoded)),
                ];
                self.push_event_("terminal_response", &event_data, &[], &[]);
            }
            Some(Union::Closed(closed)) => {
                let event_data: Vec<(&str, serde_json::Value)> = vec![
                    ("type", json!("closed")),
                    ("terminal_id", json!(closed.terminal_id)),
                    ("exit_code", json!(closed.exit_code)),
                ];
                self.push_event_("terminal_response", &event_data, &[], &[]);
            }
            Some(Union::Error(error)) => {
                let event_data: Vec<(&str, serde_json::Value)> = vec![
                    ("type", json!("error")),
                    ("terminal_id", json!(error.terminal_id)),
                    ("message", json!(&error.message)),
                ];
                self.push_event_("terminal_response", &event_data, &[], &[]);
            }
            None => {}
            Some(_) => {
                log::warn!("Unhandled terminal response type");
            }
        }
    }
}

impl FlutterHandler {
    #[inline]
    fn on_rgba_soft_render(&self, display: usize, rgba: &mut scrap::ImageRgb) {
        // 如果当前 rgba 未被 flutter 取走，即它仍然有效。
        // 我们放弃向 flutter 发送新事件。
        let mut rgba_write_lock = self.display_rgbas.write().unwrap();
        if let Some(rgba_data) = rgba_write_lock.get_mut(&display) {
            if rgba_data.valid {
                return;
            } else {
                rgba_data.valid = true;
            }
            // 把 rgba 缓冲区归还给视频处理器，以便复用已分配的 rgba 缓冲区。
            std::mem::swap::<Vec<u8>>(&mut rgba.raw, &mut rgba_data.data);
        } else {
            let mut rgba_data = RgbaData::default();
            std::mem::swap::<Vec<u8>>(&mut rgba.raw, &mut rgba_data.data);
            rgba_data.valid = true;
            rgba_write_lock.insert(display, rgba_data);
        }
        drop(rgba_write_lock);

        let mut is_sent = false;
        let is_multi_sessions = self.is_multi_ui_session();
        for h in self.session_handlers.read().unwrap().values() {
            // 软渲染器目前还不支持多显示器会话。
            if h.displays.len() > 1 {
                continue;
            }
            // 如果有多个 ui 会话，我们只通知拥有该 display 的那个 ui 会话。
            if is_multi_sessions {
                if !h.displays.contains(&display) {
                    continue;
                }
            }
            if let Some(stream) = &h.event_stream {
                stream.add(EventToUI::Rgba(display));
                is_sent = true;
            }
        }
        // 这里需要 `is_sent`，因为多显示器会话使用纹理渲染。
        //
        // 例如：我们有两个窗口，一个是 display 1，另一个是 displays 0&1。
        // 收到 display 0 的图像时，我们不会发送该事件。
        //
        // 1. "display 1" 不会发送该事件。
        // 2. "displays 0&1" 不会发送该事件，因为它目前使用纹理渲染。
        if !is_sent {
            if let Some(rgba_data) = self.display_rgbas.write().unwrap().get_mut(&display) {
                rgba_data.valid = false;
            }
        }
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn on_rgba_flutter_texture_render(
        &self,
        use_texture_render: bool,
        display: usize,
        rgba: &mut scrap::ImageRgb,
    ) {
        for (_, session) in self.session_handlers.read().unwrap().iter() {
            if use_texture_render || session.displays.len() > 1 {
                if session.renderer.on_rgba(display, rgba) {
                    if let Some(stream) = &session.event_stream {
                        stream.add(EventToUI::Texture(display, false));
                    }
                }
            }
        }
    }
}

// 此函数仅用于默认的连接会话。
pub fn session_add_existed(
    peer_id: String,
    session_id: SessionID,
    displays: Vec<i32>,
    is_view_camera: bool,
) -> ResultType<()> {
    let conn_type = if is_view_camera {
        ConnType::VIEW_CAMERA
    } else {
        ConnType::DEFAULT_CONN
    };
    sessions::insert_peer_session_id(peer_id, conn_type, session_id, displays);
    Ok(())
}

/// 用给定的 id 创建一个新的远端会话。
///
/// # 参数
///
/// * `id` - 带前缀的远端会话标识符。正则：[\w]*[\_]*[\d]+
/// * `is_file_transfer` - 该会话是否用于文件传输。
/// * `is_view_camera` - 该会话是否用于查看摄像头。
/// * `is_port_forward` - 该会话是否用于端口转发。
pub fn session_add(
    session_id: &SessionID,
    id: &str,
    is_file_transfer: bool,
    is_view_camera: bool,
    is_port_forward: bool,
    is_rdp: bool,
    is_terminal: bool,
    switch_uuid: &str,
    force_relay: bool,
    password: String,
    is_shared_password: bool,
    conn_token: Option<String>,
) -> ResultType<FlutterSession> {
    let conn_type = if is_file_transfer {
        ConnType::FILE_TRANSFER
    } else if is_view_camera {
        ConnType::VIEW_CAMERA
    } else if is_terminal {
        ConnType::TERMINAL
    } else if is_port_forward {
        if is_rdp {
            ConnType::RDP
        } else {
            ConnType::PORT_FORWARD
        }
    } else {
        ConnType::DEFAULT_CONN
    };

    // 待办：检查同 id 的会话。
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        if session.lc.read().unwrap().conn_type != conn_type {
            bail!("same session id is found with different conn type?");
        }
        // 之前是否已添加同一个会话？
        bail!("same session id is found");
    }

    LocalConfig::set_remote_id(&id);

    let mut preset_password = password.clone();
    let shared_password = if is_shared_password {
        // 为了实现灵活的密码应用顺序，我们不把共享密码当作预设密码。
        preset_password = Default::default();
        Some(password)
    } else {
        None
    };

    let session: Session<FlutterHandler> = Session {
        password: preset_password,
        server_keyboard_enabled: Arc::new(RwLock::new(true)),
        server_file_transfer_enabled: Arc::new(RwLock::new(true)),
        server_clipboard_enabled: Arc::new(RwLock::new(true)),
        reconnect_count: Arc::new(AtomicUsize::new(0)),
        ..Default::default()
    };

    let switch_uuid = if switch_uuid.is_empty() {
        None
    } else {
        Some(switch_uuid.to_string())
    };

    session.lc.write().unwrap().initialize(
        id.to_owned(),
        conn_type,
        switch_uuid,
        force_relay,
        get_adapter_luid(),
        shared_password,
        conn_token,
    );

    let session = Arc::new(session.clone());
    sessions::insert_session(session_id.to_owned(), conn_type, session.clone());

    Ok(session)
}

/// 用给定的 id 启动一个会话。
///
/// # 参数
///
/// * `id` - 带前缀的远端会话标识符。正则：[\w]*[\_]*[\d]+
/// * `events2ui` - 通向 ui 的事件通道。
pub fn session_start_(
    session_id: &SessionID,
    id: &str,
    event_stream: StreamSink<EventToUI>,
) -> ResultType<()> {
    // is_connected 用于指示是否启动对端连接。有两种情况：
    // 1. "Move tab to new window"
    // 2. 同一个对端连接内的多个 ui 会话。
    let mut is_connected = false;
    let mut is_found = false;
    for s in sessions::get_sessions() {
        if let Some(h) = s.session_handlers.write().unwrap().get_mut(session_id) {
            is_connected = h.event_stream.is_some();
            try_send_close_event(&h.event_stream);
            h.event_stream = Some(event_stream);
            is_found = true;
            break;
        }
    }
    if !is_found {
        bail!(
            "No session with peer id {}, session id: {}",
            id,
            session_id.to_string()
        );
    }

    if let Some(session) = sessions::get_session_by_session_id(session_id) {
        let is_first_ui_session = session.session_handlers.read().unwrap().len() == 1;
        if !is_connected && is_first_ui_session {
            log::info!(
                "Session {} start, use texture render: {}",
                id,
                session.use_texture_render.load(Ordering::Relaxed)
            );
            let session = (*session).clone();
            std::thread::spawn(move || {
                let round = session.connection_round_state.lock().unwrap().new_round();
                io_loop(session, round);
            });
        }
        Ok(())
    } else {
        bail!("No session with peer id {}", id)
    }
}

#[inline]
fn try_send_close_event(event_stream: &Option<StreamSink<EventToUI>>) {
    if let Some(stream) = &event_stream {
        stream.add(EventToUI::Event("close".to_owned()));
    }
}

#[cfg(not(target_os = "ios"))]
pub fn update_text_clipboard_required() {
    let is_required = sessions::get_sessions()
        .iter()
        .filter(|s| s.connection_round_state.lock().unwrap().is_connected())
        .any(|s| s.is_default() && s.is_text_clipboard_required());
    #[cfg(target_os = "android")]
    let _ = scrap::android::ffi::call_clipboard_manager_enable_client_clipboard(is_required);
    Client::set_is_text_clipboard_required(is_required);
}

#[cfg(feature = "unix-file-copy-paste")]
pub fn update_file_clipboard_required() {
    let is_required = sessions::get_sessions()
        .iter()
        .filter(|s| s.connection_round_state.lock().unwrap().is_connected())
        .any(|s| s.is_default() && s.is_file_clipboard_required());
    Client::set_is_file_clipboard_required(is_required);
}

#[cfg(not(target_os = "ios"))]
pub fn send_clipboard_msg(msg: Message, _is_file: bool) {
    send_clipboard_msg_impl(msg, _is_file, None);
}

// `except_session_id` 是内容来源的会话，用于避免把内容回发给它。
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn send_clipboard_msg_to_other_sessions(msg: Message, except_session_id: u64) {
    send_clipboard_msg_impl(msg, false, Some(except_session_id));
}

#[cfg(not(target_os = "ios"))]
fn send_clipboard_msg_impl(msg: Message, _is_file: bool, except_session_id: Option<u64>) {
    for s in sessions::get_sessions() {
        if !s.is_default() {
            continue;
        }
        if let Some(except_session_id) = except_session_id {
            if s.lc.read().unwrap().session_id == except_session_id {
                continue;
            }
        }
        #[cfg(feature = "unix-file-copy-paste")]
        if _is_file {
            if crate::is_support_file_copy_paste_num(s.lc.read().unwrap().version)
                && s.is_file_clipboard_required()
            {
                s.send(Data::Message(msg.clone()));
            }
            continue;
        }
        if s.is_text_clipboard_required() {
            // 检查客户端是否支持多剪贴板
            if let Some(message::Union::MultiClipboards(multi_clipboards)) = &msg.union {
                let version = s.ui_handler.peer_info.read().unwrap().version.clone();
                let platform = s.ui_handler.peer_info.read().unwrap().platform.clone();
                if let Some(msg_out) = crate::clipboard::get_msg_if_not_support_multi_clip(
                    &version,
                    &platform,
                    multi_clipboards,
                ) {
                    s.send(Data::Message(msg_out));
                    continue;
                }
            }
            s.send(Data::Message(msg.clone()));
        }
    }
}

// 服务端
#[cfg(not(any(target_os = "ios")))]
pub mod connection_manager {
    use std::collections::HashMap;

    #[cfg(any(target_os = "android"))]
    use hbb_common::log;
    #[cfg(any(target_os = "android"))]
    use scrap::android::call_main_service_set_by_name;
    use serde_json::json;

    use crate::ui_cm_interface::InvokeUiCM;

    use super::GLOBAL_EVENT_STREAM;

    #[derive(Clone)]
    struct FlutterHandler {}

    impl InvokeUiCM for FlutterHandler {
        //TODO port_forward
        fn add_connection(&self, client: &crate::ui_cm_interface::Client) {
            let client_json = serde_json::to_string(&client).unwrap_or("".into());
            // 发送到 Android 服务，无论是否显示界面都激活通知。
            #[cfg(target_os = "android")]
            if let Err(e) =
                call_main_service_set_by_name("add_connection", Some(&client_json), None)
            {
                log::debug!("call_main_service_set_by_name fail,{}", e);
            }
            // 发送到 UI，刷新控件
            self.push_event("add_connection", &[("client", &client_json)]);
        }

        fn remove_connection(&self, id: i32, close: bool) {
            self.push_event(
                "on_client_remove",
                &[("id", &id.to_string()), ("close", &close.to_string())],
            );
        }

        fn new_message(&self, id: i32, text: String) {
            self.push_event(
                "chat_server_mode",
                &[("id", &id.to_string()), ("text", &text)],
            );
        }

        fn change_theme(&self, dark: String) {
            self.push_event("theme", &[("dark", &dark)]);
        }

        fn change_language(&self) {
            self.push_event::<&str>("language", &[]);
        }

        fn show_elevation(&self, show: bool) {
            self.push_event("show_elevation", &[("show", &show.to_string())]);
        }

        fn update_voice_call_state(&self, client: &crate::ui_cm_interface::Client) {
            let client_json = serde_json::to_string(&client).unwrap_or("".into());
            // 发送到 Android 服务，无论是否显示界面都激活通知。
            #[cfg(target_os = "android")]
            if let Err(e) =
                call_main_service_set_by_name("update_voice_call_state", Some(&client_json), None)
            {
                log::debug!("call_main_service_set_by_name fail,{}", e);
            }
            self.push_event("update_voice_call_state", &[("client", &client_json)]);
        }

        fn file_transfer_log(&self, action: &str, log: &str) {
            self.push_event("cm_file_transfer_log", &[(action, log)]);
        }
    }

    impl FlutterHandler {
        fn push_event<V>(&self, name: &str, event: &[(&str, V)])
        where
            V: Sized + serde::Serialize + Clone,
        {
            let mut h: HashMap<&str, serde_json::Value> =
                event.iter().map(|(k, v)| (*k, json!(*v))).collect();
            debug_assert!(h.get("name").is_none());
            h.insert("name", json!(name));

            if let Some(s) = GLOBAL_EVENT_STREAM.read().unwrap().get(super::APP_TYPE_CM) {
                s.add(serde_json::ser::to_string(&h).unwrap_or("".to_owned()));
            } else {
                println!(
                    "Push event {} failed. No {} event stream found.",
                    name,
                    super::APP_TYPE_CM
                );
            };
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn start_listen_ipc() {
        use crate::ui_cm_interface::{start_ipc, ConnectionManager};

        #[cfg(target_os = "linux")]
        std::thread::spawn(crate::ipc::start_pa);

        let cm = ConnectionManager {
            ui_handler: FlutterHandler {},
        };
        std::thread::spawn(move || start_ipc(cm));
    }

    #[inline]
    pub fn cm_init() {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        start_listen_ipc();
    }

    #[cfg(target_os = "android")]
    use hbb_common::tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

    #[cfg(target_os = "android")]
    pub fn start_channel(
        rx: UnboundedReceiver<crate::ipc::Data>,
        tx: UnboundedSender<crate::ipc::Data>,
    ) {
        use crate::ui_cm_interface::start_listen;
        let cm = crate::ui_cm_interface::ConnectionManager {
            ui_handler: FlutterHandler {},
        };
        std::thread::spawn(move || start_listen(cm, rx, tx));
    }
}

pub fn make_fd_flutter(id: i32, entries: &Vec<FileEntry>, only_count: bool) -> String {
    let mut m = serde_json::Map::new();
    m.insert("id".into(), json!(id));
    let mut a = vec![];
    let mut n: u64 = 0;
    for entry in entries {
        n += entry.size;
        if only_count {
            continue;
        }
        let mut e = serde_json::Map::new();
        e.insert("name".into(), json!(entry.name.to_owned()));
        let tmp = entry.entry_type.value();
        e.insert("type".into(), json!(if tmp == 0 { 1 } else { tmp }));
        e.insert("time".into(), json!(entry.modified_time as f64));
        e.insert("size".into(), json!(entry.size as f64));
        a.push(e);
    }
    if only_count {
        m.insert("num_entries".into(), json!(entries.len() as i32));
    } else {
        m.insert("entries".into(), json!(a));
    }
    m.insert("total_size".into(), json!(n as f64));
    serde_json::to_string(&m).unwrap_or("".into())
}

pub fn get_cur_session_id() -> SessionID {
    CUR_SESSION_ID.read().unwrap().clone()
}

pub fn get_cur_peer_id() -> String {
    sessions::get_peer_id_by_session_id(&get_cur_session_id(), ConnType::DEFAULT_CONN)
        .unwrap_or("".to_string())
}

pub fn set_cur_session_id(session_id: SessionID) {
    let mut current = CUR_SESSION_ID.write().unwrap();
    if *current != session_id {
        crate::keyboard::shortcuts::transfer_released_modifiers(&current, &session_id);
        * current = session_id;
    }
}

#[inline]
fn serialize_resolutions(resolutions: &Vec<Resolution>) -> String {
    #[derive(Debug, serde::Serialize)]
    struct ResolutionSerde {
        width: i32,
        height: i32,
    }

    let mut v = vec![];
    resolutions
        .iter()
        .map(|r| {
            v.push(ResolutionSerde {
                width: r.width,
                height: r.height,
            })
        })
        .count();
    serde_json::ser::to_string(&v).unwrap_or("".to_string())
}

fn char_to_session_id(c: *const char) -> ResultType<SessionID> {
    if c.is_null() {
        bail!("Session id ptr is null");
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(c as _) };
    let str = cstr.to_str()?;
    SessionID::from_str(str).map_err(|e| anyhow!("{:?}", e))
}

pub fn session_get_rgba_size(session_id: SessionID, display: usize) -> usize {
    if let Some(session) = sessions::get_session_by_session_id(&session_id) {
        return session
            .display_rgbas
            .read()
            .unwrap()
            .get(&display)
            .map_or(0, |rgba| rgba.data.len());
    }
    0
}

#[no_mangle]
pub extern "C" fn session_get_rgba(session_uuid_str: *const char, display: usize) -> *const u8 {
    if let Ok(session_id) = char_to_session_id(session_uuid_str) {
        if let Some(s) = sessions::get_session_by_session_id(&session_id) {
            return s.ui_handler.get_rgba(display);
        }
    }

    std::ptr::null()
}

pub fn session_next_rgba(session_id: SessionID, display: usize) {
    if let Some(s) = sessions::get_session_by_session_id(&session_id) {
        return s.ui_handler.next_rgba(display);
    }
}

#[inline]
pub fn session_set_size(session_id: SessionID, display: usize, width: usize, height: usize) {
    for s in sessions::get_sessions() {
        if let Some(h) = s
            .ui_handler
            .session_handlers
            .write()
            .unwrap()
            .get_mut(&session_id)
        {
            // 如果该会话是第一次连接，displays 尚未设置。
            // `displays` 会在切换显示器或添加新会话时设置。
            if !h.displays.contains(&display) {
                h.displays.push(display);
            }
            h.renderer.set_size(display, width, height);
            break;
        }
    }
}

#[inline]
pub fn session_register_pixelbuffer_texture(session_id: SessionID, display: usize, ptr: usize) {
    for s in sessions::get_sessions() {
        if let Some(h) = s
            .ui_handler
            .session_handlers
            .read()
            .unwrap()
            .get(&session_id)
        {
            h.renderer.register_pixelbuffer_texture(display, ptr);
            break;
        }
    }
}

#[inline]
pub fn session_register_gpu_texture(_session_id: SessionID, _display: usize, _output_ptr: usize) {
    #[cfg(feature = "vram")]
    for s in sessions::get_sessions() {
        if let Some(h) = s
            .ui_handler
            .session_handlers
            .read()
            .unwrap()
            .get(&_session_id)
        {
            h.renderer.register_gpu_output(_display, _output_ptr);
            break;
        }
    }
}

#[inline]
#[cfg(not(feature = "vram"))]
pub fn get_adapter_luid() -> Option<i64> {
    None
}

#[cfg(feature = "vram")]
pub fn get_adapter_luid() -> Option<i64> {
    if !crate::ui_interface::use_texture_render() {
        return None;
    }
    let get_adapter_luid_func = match &*TEXTURE_GPU_RENDERER_PLUGIN {
        Ok(lib) => {
            let find_sym_res = unsafe {
                lib.symbol::<FlutterGpuTextureRendererPluginCApiGetAdapterLuid>(
                    "FlutterGpuTextureRendererPluginCApiGetAdapterLuid",
                )
            };
            match find_sym_res {
                Ok(sym) => Some(sym),
                Err(e) => {
                    log::error!("Failed to find symbol FlutterGpuTextureRendererPluginCApiGetAdapterLuid, {e}");
                    None
                }
            }
        }
        Err(e) => {
            log::error!("Failed to load texture gpu renderer plugin, {e}");
            None
        }
    };
    let adapter_luid = match get_adapter_luid_func {
        Some(get_adapter_luid_func) => unsafe { Some(get_adapter_luid_func()) },
        None => Default::default(),
    };
    return adapter_luid;
}

#[inline]
pub fn push_session_event(session_id: &SessionID, name: &str, event: Vec<(&str, &str)>) {
    if let Some(s) = sessions::get_session_by_session_id(session_id) {
        s.push_event(name, &event, &[]);
    }
}

#[inline]
pub fn push_global_event(channel: &str, event: String) -> Option<bool> {
    Some(GLOBAL_EVENT_STREAM.read().unwrap().get(channel)?.add(event))
}

#[inline]
pub fn get_global_event_channels() -> Vec<String> {
    GLOBAL_EVENT_STREAM
        .read()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

pub fn start_global_event_stream(s: StreamSink<String>, app_type: String) -> ResultType<()> {
    let app_type_values = app_type.split(",").collect::<Vec<&str>>();
    let mut lock = GLOBAL_EVENT_STREAM.write().unwrap();
    if !lock.contains_key(app_type_values[0]) {
        lock.insert(app_type_values[0].to_string(), s);
    } else {
        if let Some(_) = lock.insert(app_type.clone(), s) {
            log::warn!(
                "Global event stream of type {} is started before, but now removed",
                app_type
            );
        }
    }
    Ok(())
}

pub fn stop_global_event_stream(app_type: String) {
    let _ = GLOBAL_EVENT_STREAM.write().unwrap().remove(&app_type);
}

#[inline]
fn session_send_touch_scale(
    session_id: SessionID,
    v: &serde_json::Value,
    alt: bool,
    ctrl: bool,
    shift: bool,
    command: bool,
) {
    match v.get("v").and_then(|s| s.as_i64()) {
        Some(scale) => {
            if let Some(session) = sessions::get_session_by_session_id(&session_id) {
                session.send_touch_scale(scale as _, alt, ctrl, shift, command);
            }
        }
        None => {}
    }
}

#[inline]
fn session_send_touch_pan(
    session_id: SessionID,
    v: &serde_json::Value,
    pan_event: &str,
    alt: bool,
    ctrl: bool,
    shift: bool,
    command: bool,
) {
    match v.get("v") {
        Some(v) => match (
            v.get("x").and_then(|x| x.as_i64()),
            v.get("y").and_then(|y| y.as_i64()),
        ) {
            (Some(x), Some(y)) => {
                if let Some(session) = sessions::get_session_by_session_id(&session_id) {
                    session
                        .send_touch_pan_event(pan_event, x as _, y as _, alt, ctrl, shift, command);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn session_send_touch_event(
    session_id: SessionID,
    v: &serde_json::Value,
    alt: bool,
    ctrl: bool,
    shift: bool,
    command: bool,
) {
    match v.get("t").and_then(|t| t.as_str()) {
        Some("scale") => session_send_touch_scale(session_id, v, alt, ctrl, shift, command),
        Some(pan_event) => {
            session_send_touch_pan(session_id, v, pan_event, alt, ctrl, shift, command)
        }
        _ => {}
    }
}

pub fn session_send_pointer(session_id: SessionID, msg: String) {
    if let Ok(m) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&msg) {
        let alt = m.get("alt").is_some();
        let ctrl = m.get("ctrl").is_some();
        let shift = m.get("shift").is_some();
        let command = m.get("command").is_some();
        match (m.get("k"), m.get("v")) {
            (Some(k), Some(v)) => match k.as_str() {
                Some("touch") => session_send_touch_event(session_id, v, alt, ctrl, shift, command),
                _ => {}
            },
            _ => {}
        }
    }
}

#[inline]
pub fn session_on_waiting_for_image_dialog_show(session_id: SessionID) {
    for s in sessions::get_sessions() {
        if let Some(h) = s.session_handlers.write().unwrap().get_mut(&session_id) {
            h.on_waiting_for_image_dialog_show();
        }
    }
}

#[inline]
pub fn get_cur_session() -> Option<FlutterSession> {
    sessions::get_session_by_session_id(&*CUR_SESSION_ID.read().unwrap())
}

#[inline]
pub fn try_sync_peer_option(
    session: &FlutterSession,
    cur_id: &SessionID,
    key: &str,
    _value: Option<serde_json::Value>,
) {
    let mut event = Vec::new();
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if key == "view-only" {
        event = vec![
            ("k", json!(key.to_string())),
            ("v", json!(session.lc.read().unwrap().view_only.v)),
        ];
    }
    if ["keyboard_mode", "input_source"].contains(&key) {
        event = vec![("k", json!(key.to_string())), ("v", json!(""))];
    }
    if !event.is_empty() {
        session.push_event("sync_peer_option", &event, &[cur_id]);
    }
}

pub(super) fn session_update_virtual_display(session: &FlutterSession, index: i32, on: bool) {
    let virtual_display_key = "virtual-display";
    let displays = session.get_option(virtual_display_key.to_owned());
    if !on {
        if index == -1 {
            if !displays.is_empty() {
                session.set_option(virtual_display_key.to_owned(), "".to_owned());
            }
        } else {
            let mut vdisplays = displays.split(',').collect::<Vec<_>>();
            let len = vdisplays.len();
            if index == 0 {
                // 0 表示我们无法按索引切换虚拟显示器。
                vdisplays.remove(vdisplays.len() - 1);
            } else {
                if let Some(i) = vdisplays.iter().position(|&x| x == index.to_string()) {
                    vdisplays.remove(i);
                }
            }
            if vdisplays.len() != len {
                session.set_option(
                    virtual_display_key.to_owned(),
                    vdisplays.join(",").to_owned(),
                );
            }
        }
    } else {
        let mut vdisplays = displays
            .split(',')
            .map(|x| x.to_string())
            .collect::<Vec<_>>();
        let len = vdisplays.len();
        if index == 0 {
            vdisplays.push(index.to_string());
        } else {
            if !vdisplays.iter().any(|x| *x == index.to_string()) {
                vdisplays.push(index.to_string());
            }
        }
        if vdisplays.len() != len {
            session.set_option(
                virtual_display_key.to_owned(),
                vdisplays.join(",").to_owned(),
            );
        }
    }
}

// sessions mod 用于避免 sessions map 上的大锁。
pub mod sessions {

    use super::*;

    lazy_static::lazy_static! {
        // peer -> peer session，peer session -> ui sessions
        static ref SESSIONS: RwLock<HashMap<(String, ConnType), FlutterSession>> = Default::default();
    }

    #[inline]
    pub fn get_session_count(peer_id: String, conn_type: ConnType) -> usize {
        SESSIONS
            .read()
            .unwrap()
            .get(&(peer_id, conn_type))
            .map(|s| s.ui_handler.session_handlers.read().unwrap().len())
            .unwrap_or(0)
    }

    #[inline]
    pub fn get_peer_id_by_session_id(id: &SessionID, conn_type: ConnType) -> Option<String> {
        SESSIONS
            .read()
            .unwrap()
            .iter()
            .find_map(|((peer_id, t), s)| {
                if *t == conn_type
                    && s.ui_handler
                        .session_handlers
                        .read()
                        .unwrap()
                        .contains_key(id)
                {
                    Some(peer_id.clone())
                } else {
                    None
                }
            })
    }

    #[inline]
    pub fn get_session_by_session_id(id: &SessionID) -> Option<FlutterSession> {
        SESSIONS
            .read()
            .unwrap()
            .values()
            .find(|s| {
                s.ui_handler
                    .session_handlers
                    .read()
                    .unwrap()
                    .contains_key(id)
            })
            .cloned()
    }

    #[inline]
    pub fn get_session_by_peer_id(peer_id: String, conn_type: ConnType) -> Option<FlutterSession> {
        SESSIONS.read().unwrap().get(&(peer_id, conn_type)).cloned()
    }

    #[inline]
    pub fn remove_session_by_session_id(id: &SessionID) -> Option<FlutterSession> {
        let mut remove_peer_key = None;
        for (peer_key, s) in SESSIONS.write().unwrap().iter_mut() {
            let mut write_lock = s.ui_handler.session_handlers.write().unwrap();
            let remove_ret = write_lock.remove(id);
            match remove_ret {
                Some(_) => {
                    if write_lock.is_empty() {
                        remove_peer_key = Some(peer_key.clone());
                    } else {
                        check_remove_unused_displays(None, id, s, &write_lock);
                    }
                    break;
                }
                None => {}
            }
        }
        let s = SESSIONS.write().unwrap().remove(&remove_peer_key?);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        update_session_count_to_server();
        s
    }

    /// 关闭所有客户端会话，返回关闭了多少个对端会话。
    ///
    /// 用于界面已消失但进程仍在运行的情况，例如 Android
    /// 任务被从最近任务中划掉，而前台服务仍然让
    /// 进程保持存活。否则被孤立的 `io_loop` 会继续响应
    /// `TestDelay`，这样对端永远不会触发其不活动超时，而会话
    /// 会一直保持建立状态且无从关闭。
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn close_all_sessions() -> usize {
        // 在清空前先释放按住的按键：释放路径通过
        // `get_cur_session()` 发送，而它是针对 SESSIONS 解析的，因此先清空
        // 会取走 TO_RELEASE，随后静默丢弃每个 key-up，
        // 导致按键在受控端卡住。若没有按住任何
        // 按键，则为空操作。
        crate::keyboard::release_remote_keys("map");
        // 先清空，这样在关闭每个会话之前 map 的锁已被释放。
        let sessions: Vec<FlutterSession> = SESSIONS
            .write()
            .unwrap()
            .drain()
            .map(|(_, session)| session)
            .collect();
        for session in sessions.iter() {
            let session_ids: Vec<SessionID> = session
                .ui_handler
                .session_handlers
                .read()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            for session_id in session_ids {
                session.close_event_stream(session_id);
            }
            session.close();
        }
        sessions.len()
    }

    /// 检查按 session_id 移除会话是否会导致移除整个对端。
    ///
    /// 返回：
    /// - `true`：该会话存在，且移除它之后对端将没有其他会话，
    ///           因此整个对端会被移除（等同于 `remove_session_by_session_id` 返回 `Some`）
    /// - `false`：该会话不存在，或它存在但对端还有其他会话，
    ///            因此对端不会被移除（等同于 `remove_session_by_session_id` 返回 `None`）
    #[inline]
    pub fn would_remove_peer_by_session_id(id: &SessionID) -> bool {
        for (_peer_key, s) in SESSIONS.read().unwrap().iter() {
            let read_lock = s.ui_handler.session_handlers.read().unwrap();
            if read_lock.contains_key(id) {
                // 找到该会话，检查它是否是该对端的唯一一个
                return read_lock.len() == 1;
            }
        }
        // 未找到会话
        false
    }

    fn check_remove_unused_displays(
        current: Option<usize>,
        session_id: &SessionID,
        session: &FlutterSession,
        handlers: &HashMap<SessionID, SessionHandler>,
    ) {
        // 如果某些 capture displays 不再被使用，则设置 capture displays。
        let mut remains_displays = HashSet::new();
        if let Some(current) = current {
            remains_displays.insert(current);
        }
        for (k, h) in handlers.iter() {
            if k == session_id {
                continue;
            }
            remains_displays.extend(
                h.renderer
                    .map_display_sessions
                    .read()
                    .unwrap()
                    .keys()
                    .cloned(),
            );
        }
        if !remains_displays.is_empty() {
            session.capture_displays(
                vec![],
                vec![],
                remains_displays.iter().map(|d| *d as i32).collect(),
            );
        }
    }

    pub fn session_switch_display(is_desktop: bool, session_id: SessionID, value: Vec<i32>) {
        for s in SESSIONS.read().unwrap().values() {
            let mut write_lock = s.ui_handler.session_handlers.write().unwrap();
            if let Some(h) = write_lock.get_mut(&session_id) {
                h.displays = value.iter().map(|x| *x as usize).collect::<_>();
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                let displays_refresh = value.clone();
                if value.len() == 1 {
                    // 切换显示器。
                    // 该操作也会使对端发送切换显示器消息。
                    // 切换显示器消息会包含 `SupportedResolutions`，在更换分辨率时很有用。
                    s.switch_display(value[0]);
                    // 重置该 display 的有效标志。
                    s.next_rgba(value[0] as usize);

                    if !is_desktop {
                        s.capture_displays(vec![], vec![], value);
                    } else {
                        // 检查是否需要其他显示器。
                        if value.len() == 1 {
                            check_remove_unused_displays(
                                Some(value[0] as _),
                                &session_id,
                                &s,
                                &write_lock,
                            );
                        }
                    }
                } else {
                    // 尝试捕获所有显示器。
                    s.capture_displays(vec![], vec![], value);
                }
                // 切换显示器时，还需要发送 "Refresh display" 消息。
                // 在受控端：
                // 1. 如果该 display 当前未被捕获 -> 刷新 -> 不需要 "Refresh display" 消息。
                // 由于该刷新消息，会额外发送一个关键帧（首帧）。
                // 2. 如果该 display 当前正在被捕获 -> 不刷新 -> 需要 "Refresh display" 消息。
                // 没有该消息，控制端就无法看到最新的显示画面。
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                {
                    let is_support_multi_ui_session = crate::common::is_support_multi_ui_session(
                        &s.ui_handler.peer_info.read().unwrap().version,
                    );
                    if is_support_multi_ui_session {
                        for display in displays_refresh.iter() {
                            s.refresh_video(*display);
                        }
                    }
                }
                break;
            }
        }
    }

    #[inline]
    pub fn insert_session(session_id: SessionID, conn_type: ConnType, session: FlutterSession) {
        SESSIONS
            .write()
            .unwrap()
            .entry((session.get_id(), conn_type))
            .or_insert(session)
            .ui_handler
            .session_handlers
            .write()
            .unwrap()
            .insert(session_id, Default::default());
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        update_session_count_to_server();
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn update_session_count_to_server() {
        crate::ipc::update_controlling_session_count(SESSIONS.read().unwrap().len()).ok();
    }

    #[inline]
    pub fn insert_peer_session_id(
        peer_id: String,
        conn_type: ConnType,
        session_id: SessionID,
        displays: Vec<i32>,
    ) -> bool {
        if let Some(s) = SESSIONS.read().unwrap().get(&(peer_id, conn_type)) {
            let mut h = SessionHandler::default();
            h.displays = displays.iter().map(|x| *x as usize).collect::<_>();
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let is_support_multi_ui_session = crate::common::is_support_multi_ui_session(
                &s.ui_handler.peer_info.read().unwrap().version,
            );
            #[cfg(any(target_os = "android", target_os = "ios"))]
            let is_support_multi_ui_session = false;
            h.renderer.is_support_multi_ui_session = is_support_multi_ui_session;
            let _ = s
                .ui_handler
                .session_handlers
                .write()
                .unwrap()
                .insert(session_id, h);
            // 如果该会话是单显示器会话，它可能是软件 rgba 渲染的显示器。
            // 如果这是第二次打开该 display，旧的 valid 标志可能仍为 true。
            if displays.len() == 1 {
                s.ui_handler.next_rgba(displays[0] as usize);
            }
            true
        } else {
            false
        }
    }

    #[inline]
    pub fn get_sessions() -> Vec<FlutterSession> {
        SESSIONS.read().unwrap().values().cloned().collect()
    }

    #[inline]
    #[cfg(not(target_os = "ios"))]
    pub fn has_sessions_running(conn_type: ConnType) -> bool {
        SESSIONS.read().unwrap().iter().any(|((_, r#type), s)| {
            * r#type == conn_type && s.session_handlers.read().unwrap().len() != 0
        })
    }

    #[inline]
    #[cfg(not(target_os = "ios"))]
    pub fn has_connected_sessions_running(conn_type: ConnType) -> bool {
        SESSIONS.read().unwrap().iter().any(|((_, r#type), s)| {
            * r#type == conn_type
                && s.session_handlers.read().unwrap().len() != 0
                && s.connection_round_state.lock().unwrap().is_connected()
        })
    }
}

pub(super) mod async_tasks {
    use hbb_common::{bail, tokio, ResultType};
    use std::{
        collections::HashMap,
        sync::{
            mpsc::{sync_channel, SyncSender},
            Arc, Mutex,
        },
    };

    type TxQueryOnlines = SyncSender<Vec<String>>;
    lazy_static::lazy_static! {
        static ref TX_QUERY_ONLINES: Arc<Mutex<Option<TxQueryOnlines>>> = Default::default();
    }

    #[inline]
    pub fn start_flutter_async_runner() {
        std::thread::spawn(start_flutter_async_runner_);
    }

    #[allow(dead_code)]
    pub fn stop_flutter_async_runner() {
        let _ = TX_QUERY_ONLINES.lock().unwrap().take();
    }

    #[tokio::main(flavor = "current_thread")]
    async fn start_flutter_async_runner_() {
        // 同一时刻只允许一个任务运行。
        let (tx_onlines, rx_onlines) = sync_channel::<Vec<String>>(1);
        TX_QUERY_ONLINES.lock().unwrap().replace(tx_onlines);

        loop {
            match rx_onlines.recv() {
                Ok(ids) => {
                    crate::client::peer_online::query_online_states(ids, handle_query_onlines).await
                }
                _ => {
                    // 不可达！
                    break;
                }
            }
        }
    }

    pub fn query_onlines(ids: Vec<String>) -> ResultType<()> {
        if let Some(tx) = TX_QUERY_ONLINES.lock().unwrap().as_ref() {
            // 如果通道已满则忽略。
            let _ = tx.try_send(ids)?;
        } else {
            bail!("No tx_query_onlines");
        }
        Ok(())
    }

    fn handle_query_onlines(onlines: Vec<String>, offlines: Vec<String>) {
        let data = HashMap::from([
            ("name", "callback_query_onlines".to_owned()),
            ("onlines", onlines.join(",")),
            ("offlines", offlines.join(",")),
        ]);
        let _res = super::push_global_event(
            super::APP_TYPE_MAIN,
            serde_json::ser::to_string(&data).unwrap_or("".to_owned()),
        );
    }
}
