//! 在本地触发会话动作的键盘快捷键。

use std::sync::{Arc, RwLock};

use hbb_common::log;
use serde::{Deserialize, Serialize};

const LOCAL_CONFIG_KEY: &str = "keyboard-shortcuts";

lazy_static::lazy_static! {
    static ref CACHE: RwLock<Arc<Bindings>> = RwLock::new(Arc::new(Bindings::default()));
}

/// 可能出现在 `Binding.action` 中的所有有效 action id 的登记表。
/// 真实来源在 Flutter 侧（`flutter/lib/consts.dart`，
/// `kShortcutAction*`）；这些常量与之对应，好让 Rust 代码无需重新拼字符串就能
/// 取到它们。
#[allow(dead_code)]
pub mod action_id {
    pub const SEND_CTRL_ALT_DEL: &str    = "send_ctrl_alt_del";
    pub const TOGGLE_FULLSCREEN: &str    = "toggle_fullscreen";
    pub const SWITCH_DISPLAY_NEXT: &str  = "switch_display_next";
    pub const SWITCH_DISPLAY_PREV: &str  = "switch_display_prev";
    pub const SWITCH_DISPLAY_ALL: &str   = "switch_display_all";
    pub const SCREENSHOT: &str           = "screenshot";
    pub const INSERT_LOCK: &str          = "insert_lock";
    pub const REFRESH: &str              = "refresh";
    pub const TOGGLE_BLOCK_INPUT: &str   = "toggle_block_input";
    pub const TOGGLE_RECORDING: &str     = "toggle_recording";
    pub const SWITCH_SIDES: &str         = "switch_sides";
    pub const CLOSE_TAB: &str            = "close_tab";
    pub const TOGGLE_TOOLBAR: &str       = "toggle_toolbar";
    pub const RESTART_REMOTE: &str       = "restart_remote";
    pub const RESET_CANVAS: &str         = "reset_canvas";
    pub const TOGGLE_MUTE: &str          = "toggle_mute";
    pub const PIN_TOOLBAR: &str          = "pin_toolbar";
    pub const VIEW_MODE_ORIGINAL: &str   = "view_mode_original";
    pub const VIEW_MODE_ADAPTIVE: &str   = "view_mode_adaptive";
    pub const TOGGLE_CHAT: &str               = "toggle_chat";
    pub const TOGGLE_QUALITY_MONITOR: &str    = "toggle_quality_monitor";
    pub const TOGGLE_SHOW_REMOTE_CURSOR: &str = "toggle_show_remote_cursor";
    pub const TOGGLE_SHOW_MY_CURSOR: &str     = "toggle_show_my_cursor";
    pub const TOGGLE_DISABLE_CLIPBOARD: &str  = "toggle_disable_clipboard";
    pub const PRIVACY_MODE_1: &str            = "privacy_mode_1";
    pub const PRIVACY_MODE_2: &str            = "privacy_mode_2";
    pub const KEYBOARD_MODE_MAP: &str         = "keyboard_mode_map";
    pub const KEYBOARD_MODE_TRANSLATE: &str   = "keyboard_mode_translate";
    pub const KEYBOARD_MODE_LEGACY: &str      = "keyboard_mode_legacy";
    pub const CODEC_AUTO: &str                = "codec_auto";
    pub const CODEC_VP8: &str                 = "codec_vp8";
    pub const CODEC_VP9: &str                 = "codec_vp9";
    pub const CODEC_AV1: &str                 = "codec_av1";
    pub const CODEC_H264: &str                = "codec_h264";
    pub const CODEC_H265: &str                = "codec_h265";
    pub const PLUG_OUT_ALL_VIRTUAL_DISPLAYS: &str = "plug_out_all_virtual_displays";
    pub const TOGGLE_RELATIVE_MOUSE_MODE: &str = "toggle_relative_mouse_mode";
    pub const TOGGLE_FOLLOW_REMOTE_CURSOR: &str = "toggle_follow_remote_cursor";
    pub const TOGGLE_FOLLOW_REMOTE_WINDOW: &str = "toggle_follow_remote_window";
    pub const TOGGLE_ZOOM_CURSOR: &str        = "toggle_zoom_cursor";
    pub const TOGGLE_REVERSE_MOUSE_WHEEL: &str = "toggle_reverse_mouse_wheel";
    pub const TOGGLE_SWAP_LEFT_RIGHT_MOUSE: &str = "toggle_swap_left_right_mouse";
    pub const TOGGLE_LOCK_AFTER_SESSION_END: &str = "toggle_lock_after_session_end";
    pub const TOGGLE_TRUE_COLOR: &str         = "toggle_true_color";
    pub const TOGGLE_SWAP_CTRL_CMD: &str      = "toggle_swap_ctrl_cmd";
    pub const TOGGLE_ENABLE_FILE_COPY_PASTE: &str = "toggle_enable_file_copy_paste";
    pub const VIEW_MODE_CUSTOM: &str          = "view_mode_custom";
    pub const IMAGE_QUALITY_BEST: &str        = "image_quality_best";
    pub const IMAGE_QUALITY_BALANCED: &str    = "image_quality_balanced";
    pub const IMAGE_QUALITY_LOW: &str         = "image_quality_low";
    pub const SEND_CLIPBOARD_KEYSTROKES: &str = "send_clipboard_keystrokes";
    pub const SWITCH_TAB_NEXT: &str           = "switch_tab_next";
    pub const SWITCH_TAB_PREV: &str           = "switch_tab_prev";
    pub const TOGGLE_VOICE_CALL: &str         = "toggle_voice_call";
    pub const TOGGLE_VIEW_ONLY: &str          = "toggle_view_only";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Primary,
    Ctrl,
    Alt,
    Shift,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub action: String,
    pub mods: Vec<Modifier>,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Bindings {
    #[serde(default)]
    pub enabled: bool,
    /// 与 `enabled` 配套的持久化标志：为 true 时，匹配器会提前返回，
    /// 每一次按键都会直通远端（即所有绑定都被
    /// 挂起”）。它与 `enabled` 和 `bindings` 一起存放，这样一次
    /// 重载就能刷新这两个标志。
    #[serde(default)]
    pub pass_through: bool,
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

pub fn default_bindings() -> Vec<Binding> {
    let prefix = || vec![Modifier::Primary, Modifier::Alt, Modifier::Shift];
    // 默认值在适用处与 AnyDesk 的 M/S/I/C/Delete/Arrow/Digit 约定保持一致，
    // 截图用 "P" 也与 AnyDesk 一致。
    vec![
        Binding { action: action_id::SEND_CTRL_ALT_DEL.into(),         mods: prefix(), key: "delete".into() },
        Binding { action: action_id::TOGGLE_FULLSCREEN.into(),         mods: prefix(), key: "enter".into() },
        Binding { action: action_id::SWITCH_DISPLAY_NEXT.into(),       mods: prefix(), key: "arrow_right".into() },
        Binding { action: action_id::SWITCH_DISPLAY_PREV.into(),       mods: prefix(), key: "arrow_left".into() },
        Binding { action: action_id::SCREENSHOT.into(),                mods: prefix(), key: "p".into() },
        Binding { action: action_id::TOGGLE_SHOW_REMOTE_CURSOR.into(), mods: prefix(), key: "m".into() },
        Binding { action: action_id::TOGGLE_MUTE.into(),               mods: prefix(), key: "s".into() },
        Binding { action: action_id::TOGGLE_BLOCK_INPUT.into(),        mods: prefix(), key: "i".into() },
        Binding { action: action_id::TOGGLE_CHAT.into(),               mods: prefix(), key: "c".into() },
    ]
}

/// 把归一化的（按键、修饰键）对与给定的绑定做匹配。
/// 返回匹配到的 action ID；当匹配器关闭
/// （`enabled == false`）、挂起（`pass_through == true`），或者没有绑定
/// 在该组合上触发时返回 None。
///
/// 纵深防御：修饰键列表为空的绑定会在这里被跳过，
/// 尽管录制对话框本来就会拒绝保存它们。一个手工编辑过的
/// 配置（或者将来写入端的 bug）如果让一个空修饰键的绑定
/// 通过，就会把该键的每次按下都变成被吞掉的
/// 快捷键，破坏远端会话中的正常输入——这比
/// 该绑定干脆不触发要糟糕得多。
pub fn match_normalized<'a>(key: &str, mods: &[Modifier], b: &'a Bindings) -> Option<&'a str> {
    if !b.enabled || b.pass_through {
        return None;
    }
    for binding in &b.bindings {
        if binding.mods.is_empty() {
            continue;
        }
        if binding.key == key && mods_equal(&binding.mods, mods) {
            return Some(binding.action.as_str());
        }
    }
    None
}

pub fn normalize_modifiers(alt: bool, ctrl: bool, shift: bool, command: bool) -> Vec<Modifier> {
    // iOS 与 macOS 共享 Apple 的键盘语义——录制对话框
    // 已经把 iOS 当作 `_isMac`，所以匹配器也必须如此。
    let mut v = Vec::new();
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        if command { v.push(Modifier::Primary); }
        if ctrl    { v.push(Modifier::Ctrl); }
    } else {
        if ctrl    { v.push(Modifier::Primary); }
    }
    if alt     { v.push(Modifier::Alt); }
    if shift   { v.push(Modifier::Shift); }
    v
}

/// 把一个 rdev::Event 映射为字符串按键名，与存储结构保持一致。
/// 对于我们不拦截的事件（仅修饰键按下、释放等）返回 None。
pub fn event_to_key_name(event: &rdev::Event) -> Option<String> {
    use rdev::{EventType, Key};
    let key = match event.event_type {
        EventType::KeyPress(k) => k,
        _ => return None,
    };
    Some(match key {
        Key::Delete => "delete".into(),
        Key::Backspace => "backspace".into(),
        Key::Tab => "tab".into(),
        Key::Space => "space".into(),
        Key::Home => "home".into(),
        Key::End => "end".into(),
        Key::PageUp => "page_up".into(),
        Key::PageDown => "page_down".into(),
        Key::Insert => "insert".into(),
        // 数字小键盘的回车（`KpReturn`）与主回车键共用 "enter" 这个名称——
        // 这与 Web 匹配器一致（`NumpadEnter` -> "enter"），也
        // 符合用户对这两个物理回车键
        // 在快捷键中可以互换的预期。
        Key::Return | Key::KpReturn => "enter".into(),
        Key::LeftArrow => "arrow_left".into(),
        Key::RightArrow => "arrow_right".into(),
        Key::UpArrow => "arrow_up".into(),
        Key::DownArrow => "arrow_down".into(),
        Key::KeyA => "a".into(),
        Key::KeyB => "b".into(),
        Key::KeyC => "c".into(),
        Key::KeyD => "d".into(),
        Key::KeyE => "e".into(),
        Key::KeyF => "f".into(),
        Key::KeyG => "g".into(),
        Key::KeyH => "h".into(),
        Key::KeyI => "i".into(),
        Key::KeyJ => "j".into(),
        Key::KeyK => "k".into(),
        Key::KeyL => "l".into(),
        Key::KeyM => "m".into(),
        Key::KeyN => "n".into(),
        Key::KeyO => "o".into(),
        Key::KeyP => "p".into(),
        Key::KeyQ => "q".into(),
        Key::KeyR => "r".into(),
        Key::KeyS => "s".into(),
        Key::KeyT => "t".into(),
        Key::KeyU => "u".into(),
        Key::KeyV => "v".into(),
        Key::KeyW => "w".into(),
        Key::KeyX => "x".into(),
        Key::KeyY => "y".into(),
        Key::KeyZ => "z".into(),
        Key::Num0 => "digit0".into(),
        Key::Num1 => "digit1".into(),
        Key::Num2 => "digit2".into(),
        Key::Num3 => "digit3".into(),
        Key::Num4 => "digit4".into(),
        Key::Num5 => "digit5".into(),
        Key::Num6 => "digit6".into(),
        Key::Num7 => "digit7".into(),
        Key::Num8 => "digit8".into(),
        Key::Num9 => "digit9".into(),
        Key::F1 => "f1".into(),
        Key::F2 => "f2".into(),
        Key::F3 => "f3".into(),
        Key::F4 => "f4".into(),
        Key::F5 => "f5".into(),
        Key::F6 => "f6".into(),
        Key::F7 => "f7".into(),
        Key::F8 => "f8".into(),
        Key::F9 => "f9".into(),
        Key::F10 => "f10".into(),
        Key::F11 => "f11".into(),
        Key::F12 => "f12".into(),
        _ => return None,
    })
}

/// 从 `LocalConfig` 读取键盘快捷键绑定并刷新缓存。
///
/// JSON 为空或无效时回退到 `Bindings::default()`（禁用、无
/// 绑定）。在启动时调用一次，并在每次配置被
/// 写入时再次调用。
pub fn reload_from_config() {
    let raw = hbb_common::config::LocalConfig::get_option(LOCAL_CONFIG_KEY);
    #[cfg(target_os = "linux")]
    let is_wayland = crate::platform::linux::current_is_wayland();
    #[cfg(not(target_os = "linux"))]
    let is_wayland = false;
    reload_from_raw(&raw, std::env::consts::OS, is_wayland);
}

fn reload_from_raw(raw: &str, platform: &str, is_wayland: bool) {
    let mut parsed: Bindings = if raw.is_empty() {
        Bindings::default()
    } else {
        match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(e) => {
                log::warn!("Failed to parse keyboard shortcut config: {}", e);
                Bindings::default()
            }
        }
    };
    // 与 ShortcutModel 中的控制端能力保持一致，同时保留
    // 存储，以便日后回到另一个平台或显示服务器。
    parsed.bindings.retain(|b| action_available_on_platform(&b.action, platform, is_wayland));
    match CACHE.write() {
        Ok(mut w) => {
            * w = Arc::new(parsed);
        }
        Err(poison) => {
            log::error!("Keyboard shortcut cache write lock is poisoned");
            * poison.into_inner() = Arc::new(parsed);
        }
    }
}

fn action_available_on_platform(action: &str, platform: &str, is_wayland: bool) -> bool {
    let mobile = matches!(platform, "android" | "ios");
    match action {
        action_id::TOGGLE_RELATIVE_MOUSE_MODE => !mobile && !is_wayland,
        action_id::TOGGLE_FULLSCREEN | action_id::SCREENSHOT | action_id::SWITCH_TAB_NEXT
        | action_id::SWITCH_TAB_PREV | action_id::CLOSE_TAB | action_id::SWITCH_SIDES
        | action_id::TOGGLE_TOOLBAR | action_id::PIN_TOOLBAR | action_id::SWITCH_DISPLAY_ALL
        | action_id::TOGGLE_ENABLE_FILE_COPY_PASTE | action_id::KEYBOARD_MODE_MAP
        | action_id::KEYBOARD_MODE_TRANSLATE | action_id::KEYBOARD_MODE_LEGACY
        | action_id::TOGGLE_SHOW_MY_CURSOR | action_id::TOGGLE_ZOOM_CURSOR => !mobile,
        action_id::RESET_CANVAS => mobile,
        action_id::TOGGLE_RECORDING | action_id::TOGGLE_VOICE_CALL => platform != "ios",
        _ => true,
    }
}

/// 当前缓存绑定的快照。开销很低（一次原子自增）——
/// 可以安全地在每次按键时调用。
pub fn current() -> Arc<Bindings> {
    match CACHE.read() {
        Ok(b) => Arc::clone(&b),
        Err(poison) => {
            log::error!("Keyboard shortcut cache read lock is poisoned");
            Arc::clone(&poison.into_inner())
        }
    }
}

/// 把一个 `rdev::Event` 与缓存的绑定做匹配。返回匹配到的
/// action id，若没有绑定触发则返回 `None`。
///
/// 已绑定的组合键属于 RustDesk：即使该动作在
/// 当前会话中不可用（例如对不支持截图的对端按下截图键），
/// 它也会被消费掉，而 Flutter 侧随后什么也不做。它绝不会被转发
/// 到远端，所以一个组合键的行为只取决于绑定本身，而不取决于
/// 用户看不到的会话状态。
pub fn match_event(event: &rdev::Event) -> Option<String> {
    let bindings = current();
    if !bindings.enabled || bindings.pass_through {
        return None;
    }
    // 注意：`match_normalized` 会再次检查下面两个标志——这个短路
    // 只是为了避免在常见的旁路情况下白做 `event_to_key_name` + `get_modifiers_state` 的开销
    // 。
    let key_name = event_to_key_name(event)?;
    let (alt, ctrl, shift, command) =
        crate::keyboard::client::get_modifiers_state(false, false, false, false);
    let mods = normalize_modifiers(alt, ctrl, shift, command);
    let action = match_normalized(&key_name, &mods, &bindings)?;
    // AltGraph 是文本输入，不是快捷键中记录的那种普通 Alt。
    // 为远端按键编码保持共享的修饰键快照不变。
    #[cfg(target_os = "windows")]
    if unsafe { super::IS_0X021D_DOWN }
        && super::MODIFIERS_STATE.lock().unwrap().get(&rdev::Key::AltGr) == Some(&true)
    {
        return None;
    }
    #[cfg(target_os = "linux")]
    if super::MODIFIERS_STATE.lock().unwrap().get(&rdev::Key::AltGr) == Some(&true) {
        match crate::platform::linux::right_alt_is_level3_shift() {
            Ok(true) => return None,
            Ok(false) => {}
            Err(err) => {
                hbb_common::throttled_log!(std::time::Duration::from_secs(5), warn,
                    "Cannot identify AltGraph for keyboard shortcuts: {}", err);
                return None;
            }
        }
    }
    Some(action.to_owned())
}

#[cfg(feature = "flutter")]
lazy_static::lazy_static! {
    /// 按下时触发了快捷键、而释放尚未到达的物理按键。
    /// 这类按键的每一个事件都归 RustDesk 所有，直到它
    /// 被释放，无论期间修饰键如何变化：自动重复和
    /// 释放都会被消费掉，绝不会到达远端。
    ///
    /// 所有权是基于物理按键的，所以被所有会话共享：一个快捷键
    /// 可能在按键释放之前切换焦点或关闭它自己的会话，
    /// 而收到释放的那个会话并不是触发它的那个。
    /// 它绝不会随某个会话一起被丢弃。一个释放被漏掉的按键
    /// 会自我修复：它的下一次按下会被当作重复消费掉，而那次释放
    /// 会把它移除。
    static ref FIRED_KEYS: std::sync::Mutex<std::collections::HashSet<rdev::Key>> =
        Default::default();
    /// 等待触发它们的按键释放的动作，以及触发它们的
    /// 会话。参见 `runs_on_release`。
    static ref RELEASE_ACTIONS: std::sync::Mutex<
        std::collections::HashMap<rdev::Key, (hbb_common::SessionID, String)>,
    > = Default::default();
    static ref RELEASED_MODIFIERS: std::sync::Mutex<
        std::collections::HashMap<hbb_common::SessionID, std::collections::HashMap<rdev::Key, rdev::Event>>,
    > = Default::default();
}

/// 把键盘焦点移到另一个会话或对话框的动作。它们在
/// 触发它们的按键被释放时（而不是按下时）运行，这样
/// 消费掉那次按下的匹配器也能看到它的重复和释放。下一个
/// 会话可能会通过另一个匹配器路由它的按键（Linux 上 Flutter 的旧版
/// 路径），而那个匹配器从未见过这次按下。对应
/// `shortcut_constants.dart` 中的 `kShortcutActionsRunOnKeyUp`；两者都
/// 以 `flutter/test/fixtures/key_up_shortcut_actions.json` 为准做校验。
const RELEASE_ACTION_IDS: &[&str] = &[
    action_id::CLOSE_TAB,
    action_id::SWITCH_TAB_NEXT,
    action_id::SWITCH_TAB_PREV,
    action_id::RESTART_REMOTE,
    action_id::SWITCH_SIDES,
    action_id::TOGGLE_CHAT,
];

pub fn runs_on_release(action_id: &str) -> bool {
    // 剪贴板同意对话框可能会消费掉触发按键的释放。Web 端保持
    // 该动作为按下时触发，以满足浏览器剪贴板的用户激活要求。
    action_id == action_id::SEND_CLIPBOARD_KEYSTROKES || RELEASE_ACTION_IDS.contains(&action_id)
}

/// 忘掉某个会话在其远端释放的修饰键，以及它仍
/// 欠待在按键释放时执行的动作。已触发的按键是物理的，会一直归其所有直到
/// 释放，无论最终是哪个会话收到。
#[cfg(feature = "flutter")]
pub fn clear_session_state(session_id: &hbb_common::SessionID) {
    RELEASE_ACTIONS.lock().unwrap().retain(|_, (sid, _)| sid != session_id);
    let held = RELEASED_MODIFIERS.lock().unwrap().remove(session_id);
    if let Some(held) = held {
        {
            let mut state = super::MODIFIERS_STATE.lock().unwrap();
            for key in held.keys() {
                state.insert(*key, false);
            }
        }
        let mut to_release = super::TO_RELEASE.lock().unwrap();
        for key in held.keys() {
            to_release.remove(key);
        }
    }
}

/// 当快捷键已经释放了远端按键时，在失去焦点时清除本地状态。
#[cfg(feature = "flutter")]
pub(super) fn release_modifier_on_leave(key: rdev::Key) -> bool {
    let sid = crate::flutter::get_cur_session_id();
    let mut released = RELEASED_MODIFIERS.lock().unwrap();
    let Some(held) = released.get_mut(&sid) else { return false };
    if held.remove(&key).is_none() {
        return false;
    }
    if held.is_empty() {
        released.remove(&sid);
    }
    drop(released);
    super::MODIFIERS_STATE.lock().unwrap().insert(key, false);
    true
}

#[cfg(feature = "flutter")]
pub fn enter_view_only(session_id: &hbb_common::SessionID) {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if super::IS_RDEV_ENABLED.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // Flutter 输入源不再把按键事件路由到这里，所以已触发按键的释放
    // 再也无法到达这个匹配器。
    FIRED_KEYS.lock().unwrap().clear();
    RELEASE_ACTIONS.lock().unwrap().clear();
    clear_session_state(session_id);
}

#[cfg(feature = "flutter")]
pub fn transfer_released_modifiers(from: &hbb_common::SessionID, to: &hbb_common::SessionID) {
    if from == to {
        return;
    }
    let mut released = RELEASED_MODIFIERS.lock().unwrap();
    if let Some(held) = released.remove(from) {
        released.entry(*to).or_default().extend(held);
    }
}

/// 把 `event` 与缓存的绑定做匹配；如果匹配上了，就推送一个
/// `shortcut_triggered` Flutter 会话事件并返回 `true`，让调用方
/// 能够提前 `return`。没有快捷键触发时返回 `false`（调用方应
/// 继续正常的按键处理）。
///
/// `session_id`：
/// * `Some(&id)` — Flutter FFI 路径：分发到我们正在处理其按键
///   事件的那个确切会话。不依赖全局焦点跟踪器。
/// * `None` — rdev grab 循环：该循环是进程级的，无从知道
///   这次按键本该发给哪个 Flutter 会话 id，所以路由到
///   通过 `flutter::get_cur_session_id()` 得到的全局当前会话。
///
/// `peer` 和 `send` 必须与 `session_id` 属于同一个会话，这样
/// 远端按键的释放才会回到输入该组合键的那个会话。
#[cfg(feature = "flutter")]
pub fn try_dispatch(
    session_id: Option<&hbb_common::SessionID>,
    event: &rdev::Event,
    keyboard_mode: &str,
    peer: impl FnOnce() -> String,
    send: impl Fn(&base::message_proto::KeyEvent),
) -> bool {
    use rdev::EventType;
    let resolved;
    let sid = match session_id {
        Some(id) => id,
        None => {
            resolved = crate::flutter::get_cur_session_id();
            &resolved
        }
    };
    if let EventType::KeyRelease(key) = event.event_type {
        let mut released = RELEASED_MODIFIERS.lock().unwrap();
        if let Some(held) = released.get_mut(sid) {
            held.remove(&key);
            if held.is_empty() {
                released.remove(sid);
            }
        }
    }
    {
        let mut fired = FIRED_KEYS.lock().unwrap();
        match event.event_type {
            EventType::KeyPress(k) if fired.contains(&k) => return true,
            EventType::KeyRelease(k) if fired.remove(&k) => {
                // X11 Source 1 可能把自动重复报告成释放/按下对。
                // 在后端把它们归一化之前，持续按住的快捷键可能会再次触发。
                drop(fired);
                let pending = RELEASE_ACTIONS.lock().unwrap().remove(&k);
                if let Some((sid, action_id)) = pending {
                    push_shortcut_event(&sid, &action_id);
                }
                return true;
            }
            _ => {}
        }
    }
    // 被转发的按下在重复期间保持所有权。按住期间再加入修饰键
    // 绝不能消费掉欠给远端的 key-up。
    let action_id = match event.event_type {
        EventType::KeyPress(key) if super::TO_RELEASE.lock().unwrap().contains_key(&key) => None,
        _ => match_event(event),
    };
    let Some(action_id) = action_id else {
        restore_remote_modifiers(sid, event, keyboard_mode, peer, &send);
        return false;
    };
    let peer = peer();
    if runs_on_release(&action_id)
        || matches!(action_id.as_str(),
            action_id::TOGGLE_VIEW_ONLY | action_id::TOGGLE_SHOW_MY_CURSOR
            | action_id::INSERT_LOCK | action_id::SEND_CTRL_ALT_DEL
            | action_id::KEYBOARD_MODE_MAP | action_id::KEYBOARD_MODE_TRANSLATE
            | action_id::KEYBOARD_MODE_LEGACY)
    {
        release_remote_non_modifiers(keyboard_mode, &peer, &send);
    }
    release_remote_keys(sid, keyboard_mode, &peer, &send);
    if let EventType::KeyPress(k) = event.event_type {
        FIRED_KEYS.lock().unwrap().insert(k);
        let mut pending = RELEASE_ACTIONS.lock().unwrap();
        if runs_on_release(&action_id) {
            pending.insert(k, (*sid, action_id));
            return true;
        }
        pending.remove(&k);
    }
    push_shortcut_event(sid, &action_id);
    true
}

#[cfg(feature = "flutter")]
fn push_shortcut_event(session_id: &hbb_common::SessionID, action_id: &str) {
    if let Some(session) = crate::flutter::sessions::get_session_by_session_id(session_id) {
        session.ui_handler.push_event_to(
            "shortcut_triggered",
            &[("action", action_id)],
            &[session_id],
        );
    }
}

#[cfg(feature = "flutter")]
fn restore_remote_modifiers(
    session_id: &hbb_common::SessionID,
    event: &rdev::Event,
    keyboard_mode: &str,
    peer: impl FnOnce() -> String,
    send: &impl Fn(&base::message_proto::KeyEvent),
) {
    use super::{event_to_key_events, get_keyboard_mode_enum, is_modifier, MODIFIERS_STATE};

    let rdev::EventType::KeyPress(key) = event.event_type else { return };
    if is_modifier(&key) {
        return;
    }
    let held = RELEASED_MODIFIERS.lock().unwrap().remove(session_id);
    let Some(mut held) = held else { return };
    {
        let state = MODIFIERS_STATE.lock().unwrap();
        held.retain(|key, _| state.get(key).copied().unwrap_or(false));
    }
    if held.is_empty() {
        return;
    }
    let peer = peer();
    let mode = get_keyboard_mode_enum(keyboard_mode);
    for event in held.into_values() {
        for key_event in event_to_key_events(peer.clone(), &event, mode, None) {
            send(&key_event);
        }
    }
}

// 移动焦点、停止输入或改变输入模式的动作也必须释放普通的
// 按键：它们的物理 key-up 可能已经无法与原来的 key-down 配平。
#[cfg(feature = "flutter")]
fn release_remote_non_modifiers(
    keyboard_mode: &str,
    peer: &str,
    send: &impl Fn(&base::message_proto::KeyEvent),
) {
    use super::{event_to_key_events, get_keyboard_mode_enum, is_modifier, TO_RELEASE};
    let held: Vec<_> = TO_RELEASE.lock().unwrap().iter()
        .filter(|(key, _)| !is_modifier(key))
        .map(|(key, event)| (*key, event.clone()))
        .collect();
    let mode = get_keyboard_mode_enum(keyboard_mode);
    for (key, mut event) in held {
        event.event_type = rdev::EventType::KeyRelease(key);
        for key_event in event_to_key_events(peer.to_owned(), &event, mode, None) {
            send(&key_event);
        }
    }
}

/// 在动作之前释放远端修饰键，同时保留无关的按住键。
///
/// 与 `keyboard::release_remote_keys` 不同，它会保留本地修饰键状态：
/// 用户物理上仍按着修饰键，而该组合键的下一个按键
/// （或自动重复）仍必须能看到它们。它也通过
/// `send` 发送，而不是发给全局当前会话。
#[cfg(feature = "flutter")]
fn release_remote_keys(
    session_id: &hbb_common::SessionID,
    keyboard_mode: &str,
    peer: &str,
    send: &impl Fn(&base::message_proto::KeyEvent),
) {
    use super::{event_to_key_events, get_keyboard_mode_enum, is_modifier, MODIFIERS_STATE, TO_RELEASE};
    use rdev::{EventType, Key};

    let mode = get_keyboard_mode_enum(keyboard_mode);
    let to_release: Vec<_> = TO_RELEASE.lock().unwrap().iter()
        .filter(|(key, _)| is_modifier(key))
        .map(|(key, event)| (*key, event.clone()))
        .collect();
    let held: Vec<(Key, rdev::Event)> = {
        let state = MODIFIERS_STATE.lock().unwrap();
        to_release
            .iter()
            .filter(|(k, _)| state.get(k).copied().unwrap_or(false))
            .map(|(key, event)| (*key, event.clone()))
            .collect()
    };
    let swap_ctrl_cmd = crate::flutter::sessions::get_session_by_session_id(session_id)
        .is_some_and(|session| session.get_toggle_option("allow_swap_key".into()));
    // 被消费掉的仅含 Alt/Meta 的组合键不得激活 Windows 菜单或开始菜单。
    let neutralize_menu = peer == "windows"
        && !to_release.is_empty()
        && to_release.iter().all(|(key, _)| matches!(key, Key::Alt | Key::AltGr)
            || (swap_ctrl_cmd && matches!(key, Key::ControlLeft | Key::ControlRight))
            || (!swap_ctrl_cmd && matches!(key, Key::MetaLeft | Key::MetaRight)));
    if neutralize_menu {
        send_windows_menu_ctrl(true, swap_ctrl_cmd, send);
    }
    for (key, mut event) in to_release {
        event.event_type = EventType::KeyRelease(key);
        for key_event in event_to_key_events(peer.to_owned(), &event, mode, None) {
            send(&key_event);
        }
    }
    if neutralize_menu {
        send_windows_menu_ctrl(false, swap_ctrl_cmd, send);
    }
    {
        let mut state = MODIFIERS_STATE.lock().unwrap();
        for (key, _) in &held {
            state.insert(*key, true);
        }
    }
    RELEASED_MODIFIERS.lock().unwrap().insert(
        * session_id, held.iter().cloned().collect()
    );
    // 失去焦点的清理仍必须重置在 grab 之外释放的修饰键。
    TO_RELEASE.lock().unwrap().extend(held);
}

#[cfg(feature = "flutter")]
fn send_windows_menu_ctrl(
    down: bool,
    swap_ctrl_cmd: bool,
    send: &impl Fn(&base::message_proto::KeyEvent),
) {
    use base::message_proto::{KeyEvent, KeyboardMode};
    const LEFT_CTRL_SCAN_CODE: u32 = 0x1d;
    const LEFT_META_SCAN_CODE: u32 = 0xe05b;
    let mut event = KeyEvent::new();
    // 在 Map 模式下保留 Ctrl：旧版同步可能在 Ctrl 之前就释放 Alt/Meta。
    event.mode = KeyboardMode::Map.into();
    // 会话发送方也会替换这个数据包；掩码必须以 Ctrl 的形式送达。
    event.set_chr(if swap_ctrl_cmd {
        LEFT_META_SCAN_CODE
    } else {
        LEFT_CTRL_SCAN_CODE
    });
    event.down = down;
    send(&event);
}

fn mods_bits(m: &[Modifier]) -> u8 {
    let mut bits = 0u8;
    for x in m {
        bits |= match x {
            Modifier::Primary => 1,
            Modifier::Alt     => 2,
            Modifier::Shift   => 4,
            // macOS 用户可以绑定独立使用 Control 的快捷键，
            // 不必配合 Command。在 Win/Linux 上这个变体本不该出现在
            // 已保存的绑定中（`normalize_modifiers` 会把 Ctrl 归并进
            // Primary），但我们仍然给它一个独立的位，这样手工编辑过的
            // 配置就不会意外地与另一个修饰键冲突。
            Modifier::Ctrl    => 8,
        };
    }
    bits
}

fn mods_equal(a: &[Modifier], b: &[Modifier]) -> bool {
    mods_bits(a) == mods_bits(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_press(k: rdev::Key) -> rdev::Event {
        let mut event = rdev::Event {
            time: std::time::SystemTime::now(),
            unicode: None,
            platform_code: 0,
            position_code: 0,
            event_type: rdev::EventType::KeyPress(k),
            usb_hid: 0,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            extra_data: 0,
        };
        #[cfg(target_os = "windows")]
        {
            event.platform_code = rdev::win_code_from_key(k).unwrap_or_default();
            event.position_code = rdev::win_scancode_from_key(k).unwrap_or_default();
        }
        #[cfg(target_os = "macos")]
        {
            event.platform_code = rdev::macos_keycode_from_key(k).unwrap_or_default() as _;
            event.position_code = event.platform_code;
        }
        #[cfg(target_os = "linux")]
        {
            event.position_code = rdev::linux_keycode_from_key(k).unwrap_or_default();
            event.platform_code = event.position_code;
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            event.usb_hid = rdev::usb_hid_keycode_from_key(k).unwrap_or_default();
        }
        event
    }

    /// 默认绑定的跨语言一致性。该 fixture 文件是
    /// 共享的真实来源——Dart 侧有一个针对同一文件的镜像测试
    /// （`kDefaultShortcutBindings matches fixture` 在
    /// `flutter/test/keyboard_shortcuts_test.dart` 中）。任何一侧出现偏差
    /// 都会让两个测试中的一个失败。
    #[test]
    fn default_bindings_match_fixture_json() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../flutter/test/fixtures/default_keyboard_shortcuts.json"
        ))
        .expect("fixture is valid JSON");
        let actual: serde_json::Value =
            serde_json::to_value(default_bindings()).expect("serialize defaults");
        assert_eq!(
            fixture, actual,
            "default_bindings() drifted from \
             flutter/test/fixtures/default_keyboard_shortcuts.json — update \
             shortcuts.rs, the fixture, and Dart kDefaultShortcutBindings together"
        );
    }

    #[test]
    fn match_returns_none_when_pass_through() {
        let bindings = Bindings {
            enabled: true,
            pass_through: true,
            bindings: default_bindings(),
        };
        let result = match_normalized(
            "p",
            &[Modifier::Primary, Modifier::Alt, Modifier::Shift],
            &bindings,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn match_returns_none_when_disabled() {
        let bindings = Bindings { enabled: false, pass_through: false, bindings: default_bindings() };
        let result = match_normalized("p", &[Modifier::Primary, Modifier::Alt, Modifier::Shift], &bindings);
        assert_eq!(result, None);
    }

    #[test]
    fn match_screenshot_when_enabled() {
        let bindings = Bindings { enabled: true, pass_through: false, bindings: default_bindings() };
        let result = match_normalized("p", &[Modifier::Primary, Modifier::Alt, Modifier::Shift], &bindings);
        assert_eq!(result, Some(action_id::SCREENSHOT));
    }

    #[test]
    fn match_returns_none_when_modifiers_partial() {
        let bindings = Bindings { enabled: true, pass_through: false, bindings: default_bindings() };
        // 缺少 Shift
        let result = match_normalized("p", &[Modifier::Primary, Modifier::Alt], &bindings);
        assert_eq!(result, None);
    }

    #[test]
    fn match_does_not_fire_on_extra_unbound_keys() {
        let bindings = Bindings { enabled: true, pass_through: false, bindings: default_bindings() };
        let result = match_normalized("z", &[Modifier::Primary, Modifier::Alt, Modifier::Shift], &bindings);
        assert_eq!(result, None);
    }

    #[test]
    fn match_handles_duplicate_modifiers_in_input() {
        // 用户编辑过的配置可能包含重复的修饰键；匹配器必须
        // 把修饰键列表当作集合而不是多重集来处理。
        let bindings = Bindings {
            enabled: true,
            pass_through: false,
            bindings: vec![Binding {
                action: "x".into(),
                mods: vec![Modifier::Primary, Modifier::Alt],
                key: "a".into(),
            }],
        };
        // 调用方传入两次 Primary——不应匹配 Primary+Alt 的绑定。
        assert_eq!(
            match_normalized("a", &[Modifier::Primary, Modifier::Primary], &bindings),
            None,
        );
        // 调用方传入 Primary+Alt 且有一个重复——仍应匹配。
        assert_eq!(
            match_normalized("a", &[Modifier::Primary, Modifier::Alt, Modifier::Alt], &bindings),
            Some("x"),
        );
    }

    #[test]
    fn modifier_normalization_primary_resolves_per_os() {
        // 在 Win/Linux 上：按下 Ctrl 即满足 Primary
        let mods = normalize_modifiers(/*alt=*/true, /*ctrl=*/true, /*shift=*/true, /*command=*/false);
        if cfg!(any(target_os = "macos", target_os = "ios")) {
            // 在 Apple 平台上 Ctrl 不是 primary
            assert!(!mods.contains(&Modifier::Primary));
            assert!(mods.contains(&Modifier::Ctrl));
        } else {
            assert!(mods.contains(&Modifier::Primary));
        }
        assert!(mods.contains(&Modifier::Alt));
        assert!(mods.contains(&Modifier::Shift));
    }

    #[test]
    fn modifier_normalization_command_is_primary_on_apple() {
        let mods = normalize_modifiers(true, false, true, /*command=*/true);
        if cfg!(any(target_os = "macos", target_os = "ios")) {
            assert!(mods.contains(&Modifier::Primary));
        } else {
            // 在 Win/Linux 上 Command/Meta 不是 primary
            assert!(!mods.contains(&Modifier::Primary));
        }
    }

    #[test]
    fn match_refuses_zero_modifier_bindings() {
        // 纵深防御：手工编辑过的、`mods` 为空的配置绝不能
        // 把普通的 "P" 按下变成截图快捷键，否则那会
        // 吞掉远端会话中的所有输入。录制对话框
        // 本来就会拒绝保存这类绑定，但匹配器必须独立地
        // 守住这条底线。
        let bindings = Bindings {
            enabled: true,
            pass_through: false,
            bindings: vec![Binding {
                action: "screenshot".into(),
                mods: vec![],
                key: "p".into(),
            }],
        };
        assert_eq!(match_normalized("p", &[], &bindings), None);
        // 即使用户还按着额外的修饰键，零修饰键的绑定
        // 仍然不会匹配（因为在空检查跳过该条目之后，按住的修饰键
        // 无论取什么组合都不可能等于空集合）。
        assert_eq!(
            match_normalized("p", &[Modifier::Primary], &bindings),
            None,
        );
    }

    /// 原生 Flutter 路径通过 USB HID
    /// usage 把按键转成名称（`rdev::usb_hid_key_from_code` -> `event_to_key_name`）；Dart
    /// 侧的录制器通过 `physicalKeyName` 做同样的事。两者都
    /// 以同一个 fixture 为准校验，所以在任意键盘布局上记录的绑定
    /// 都能匹配到实际按下的按键。
    #[test]
    fn usb_hid_keys_match_fixture() {
        let pairs: Vec<(String, u32)> = serde_json::from_str(include_str!(
            "../../flutter/test/fixtures/shortcut_key_usb_hid.json"
        ))
        .expect("parse fixture");
        for (name, usage) in pairs {
            let key = rdev::usb_hid_key_from_code(usage);
            assert_eq!(
                event_to_key_name(&make_press(key)).as_deref(),
                Some(name.as_str()),
                "USB HID {usage:#04x}"
            );
        }
    }

    /// 把写入全局 `CACHE` 的测试串行化。
    static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn reload_handles_missing_and_invalid_json() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 空（未设置值）→ 默认值
        reload_from_raw("", "windows", false);
        let b = current();
        assert!(!b.enabled);
        assert!(b.bindings.is_empty());

        // 无效 JSON → 默认值（不 panic）
        reload_from_raw("not json", "windows", false);
        let b = current();
        assert!(!b.enabled);
    }

    #[test]
    fn wayland_binding_is_inactive_and_returns_on_x11() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let raw = r#"{
            "enabled": true, "pass_through": false, "bindings": [
                {"action":"toggle_relative_mouse_mode","mods":["primary"],"key":"r"},
                {"action":"toggle_mute","mods":["primary"],"key":"m"}
            ]
        }"#;
        reload_from_raw(raw, "linux", false);
        let original = current();
        for is_wayland in [true, false] {
            reload_from_raw(raw, "linux", is_wayland);
            let active = current();
            assert!(active.enabled);
            assert!(!active.pass_through);
            assert_eq!(
                match_normalized("r", &[Modifier::Primary], &active),
                if is_wayland { None } else { Some(action_id::TOGGLE_RELATIVE_MOUSE_MODE) },
            );
            assert_eq!(match_normalized("m", &[Modifier::Primary], &active), Some(action_id::TOGGLE_MUTE));
        }
        assert_eq!(match_normalized("r", &[Modifier::Primary], &original), Some(action_id::TOGGLE_RELATIVE_MOUSE_MODE));
    }

    #[test]
    fn active_cache_omits_platform_hidden_saved_actions() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let actions = [action_id::SWITCH_DISPLAY_ALL, action_id::KEYBOARD_MODE_MAP,
            action_id::TOGGLE_SHOW_MY_CURSOR, action_id::TOGGLE_ZOOM_CURSOR,
            action_id::TOGGLE_RECORDING, action_id::TOGGLE_VOICE_CALL,
            action_id::RESET_CANVAS, action_id::TOGGLE_MUTE, action_id::SEND_CTRL_ALT_DEL];
        let saved = Bindings { enabled: true, pass_through: false,
            bindings: actions.iter().map(|action| Binding {
                action: (*action).into(), mods: vec![Modifier::Primary], key: "m".into(),
            }).collect() };
        let raw = serde_json::to_string(&saved).unwrap();
        for (platform, expected) in [
            ("android", vec![action_id::TOGGLE_RECORDING, action_id::TOGGLE_VOICE_CALL,
                action_id::RESET_CANVAS, action_id::TOGGLE_MUTE, action_id::SEND_CTRL_ALT_DEL]),
            ("ios", vec![action_id::RESET_CANVAS, action_id::TOGGLE_MUTE, action_id::SEND_CTRL_ALT_DEL]),
            ("macos", actions.iter().copied().filter(|a| *a != action_id::RESET_CANVAS).collect()),
        ] {
            reload_from_raw(&raw, platform, false);
            let active = current();
            assert_eq!(active.bindings.iter().map(|b| b.action.as_str()).collect::<Vec<_>>(), expected, "{platform}");
        }
        assert_eq!(serde_json::from_str::<Bindings>(&raw).unwrap(), saved);
    }

    #[cfg(feature = "flutter")]
    fn make_release(k: rdev::Key) -> rdev::Event {
        let mut e = make_press(k);
        e.event_type = rdev::EventType::KeyRelease(k);
        e
    }

    /// 按住组合键的修饰键并按下两个已绑定的按键，必须各自触发一次
    /// 对应的动作：在远端释放修饰键绝不能清除
    /// 本地的修饰键，而且相互重叠的已触发按键各自拥有自己的重复和
    /// 释放。
    #[cfg(feature = "flutter")]
    #[test]
    fn dispatch_consumes_overlapping_fired_keys() {
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        // 记录每一次真正分发出去的按下的动作：某个按键的按下
        // 在这次调用之前尚未被触发过。
        let fired = std::cell::RefCell::new(Vec::new());
        let dispatch = |e: &rdev::Event| {
            let before = fired_keys();
            let action = match_event(e);
            let hit = try_dispatch(Some(&SID_A), e, "map", || "windows".into(), |_| {});
            if let rdev::EventType::KeyPress(k) = e.event_type {
                if hit && !before.contains(&k) {
                    fired.borrow_mut().push(action.unwrap_or_default());
                }
            }
            hit
        };

        assert!(dispatch(&make_press(Key::KeyP)));
        {
            let state = super::super::MODIFIERS_STATE.lock().unwrap();
            for k in chord {
                assert_eq!(state.get(&k), Some(&true), "{k:?} must stay held locally");
            }
        }
        assert!(dispatch(&make_press(Key::KeyC)), "second chord key fires");
        assert!(dispatch(&make_press(Key::KeyP)), "P repeat is consumed");
        assert!(dispatch(&make_release(Key::KeyP)), "P release is consumed");
        assert!(dispatch(&make_release(Key::KeyC)), "C release is consumed");
        assert!(fired_keys().is_empty());
        assert_eq!(
            * fired.borrow(),
            vec![action_id::SCREENSHOT.to_owned(), action_id::TOGGLE_CHAT.to_owned()]
        );

        release_chord(chord);
    }

    /// 已触发的按键会一直归其所有直到释放，即使修饰键已经松开。
    #[cfg(feature = "flutter")]
    #[test]
    fn dispatch_consumes_fired_key_after_modifiers_release() {
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        let dispatch =
            |e: &rdev::Event| try_dispatch(Some(&SID_A), e, "map", || "windows".into(), |_| {});

        assert!(dispatch(&make_press(Key::KeyP)));
        release_chord(chord);
        assert!(dispatch(&make_press(Key::KeyP)), "repeat without modifiers is consumed");
        assert!(dispatch(&make_release(Key::KeyP)), "release without modifiers is consumed");
        assert!(!dispatch(&make_press(Key::KeyP)), "a new press without the chord passes");
        assert!(!dispatch(&make_release(Key::KeyP)));
    }

    /// 关闭标签页本身就是一个快捷键：触发该按键的会话在按键
    /// 释放之前就已经消失，而收到该按键其余
    /// 事件的那个会话仍然拥有它们。
    #[cfg(feature = "flutter")]
    #[test]
    fn dispatch_keeps_fired_key_after_session_close() {
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        let dispatch = |sid: &hbb_common::SessionID, e: &rdev::Event| {
            try_dispatch(Some(sid), e, "map", || "windows".into(), |_| {})
        };

        assert!(dispatch(&SID_A, &make_press(Key::KeyP)));
        clear_session_state(&SID_A);
        assert!(dispatch(&SID_B, &make_press(Key::KeyP)), "repeat stays consumed after A closed");
        release_chord(chord);
        assert!(dispatch(&SID_B, &make_press(Key::KeyP)), "repeat without the chord too");
        assert!(dispatch(&SID_B, &make_release(Key::KeyP)), "release stays consumed after A closed");
        assert!(fired_keys().is_empty());
        assert!(!dispatch(&SID_B, &make_press(Key::KeyP)), "a new press without the chord passes");
        assert!(!dispatch(&SID_B, &make_release(Key::KeyP)));
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn dispatch_consumes_fired_key_after_session_switch() {
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        let previous_session = crate::flutter::get_cur_session_id();
        crate::flutter::set_cur_session_id(SID_A);
        let dispatch = |e: &rdev::Event| {
            try_dispatch(None, e, "map", || "windows".into(), |_| {})
        };

        assert!(dispatch(&make_press(Key::KeyP)));
        crate::flutter::set_cur_session_id(SID_B);
        release_chord(chord);
        assert!(dispatch(&make_press(Key::KeyP)), "repeat stays consumed in the new tab");
        clear_session_state(&SID_A);
        crate::flutter::set_cur_session_id(SID_A);
        assert!(dispatch(&make_press(Key::KeyP)), "repeat stays consumed after switching back");
        crate::flutter::set_cur_session_id(SID_B);
        assert!(dispatch(&make_release(Key::KeyP)), "release stays consumed in the new tab");
        crate::flutter::set_cur_session_id(SID_A);
        assert!(!dispatch(&make_press(Key::KeyP)), "new press is not a stale repeat");
        crate::flutter::set_cur_session_id(previous_session);
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn dispatch_preserves_modifier_cleanup_on_leave() {
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        assert!(try_dispatch(
            Some(&SID_A), &make_press(Key::KeyP), "map", || "windows".into(), |_| {}
        ));
        super::super::release_remote_keys("map");
        assert_eq!(
            super::super::client::get_modifiers_state(false, false, false, false),
            (false, false, false, false),
            "leaving the remote image must clear modifiers even after a shortcut"
        );
        release_chord(chord);
        reset_fired_keys();
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn repeated_shortcuts_leave_remote_modifiers_released() {
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        let remote_held = std::cell::RefCell::new(
            chord.into_iter().collect::<std::collections::HashSet<_>>()
        );
        for key in [Key::KeyP, Key::KeyC] {
            assert!(try_dispatch(
                Some(&SID_A),
                &make_press(key),
                "map",
                || "windows".into(),
                |event| {
                    let key = rdev::win_key_from_scancode(event.chr());
                    assert!(chord.contains(&key), "only held modifiers are released");
                    assert!(
                        key != Key::Alt || !event.down,
                        "pressing Alt again activates the Windows system menu and eats the next character"
                    );
                    if event.down {
                        remote_held.borrow_mut().insert(key);
                    } else {
                        remote_held.borrow_mut().remove(&key);
                    }
                },
            ));
            assert!(remote_held.borrow().is_empty());
        }
        release_chord(chord);
        reset_fired_keys();
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn shortcut_cleanup_keeps_unrelated_keys_held() {
        use base::message_proto::KeyboardMode;
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for action in [action_id::TOGGLE_MUTE, action_id::TOGGLE_CHAT,
            action_id::TOGGLE_VIEW_ONLY, action_id::TOGGLE_SHOW_MY_CURSOR,
            action_id::INSERT_LOCK, action_id::SEND_CTRL_ALT_DEL,
            action_id::KEYBOARD_MODE_MAP, action_id::KEYBOARD_MODE_TRANSLATE,
            action_id::KEYBOARD_MODE_LEGACY]
        {
            let chord = enable_defaults_and_hold_chord();
            let mut bindings = default_bindings();
            bindings.iter_mut().find(|b| b.key == "s").unwrap().action = action.into();
            * CACHE.write().unwrap() = Arc::new(Bindings { enabled: true, pass_through: false, bindings });
            release_chord(chord);
            super::super::event_to_key_events(
                "windows".into(), &make_press(Key::KeyW), KeyboardMode::Map, Some(0),
            );
            for key in chord {
                super::super::event_to_key_events(
                    "windows".into(), &make_press(key), KeyboardMode::Map, Some(0),
                );
            }
            let sent = std::cell::RefCell::new(Vec::new());
            let consumed = try_dispatch(
                Some(&SID_A), &make_press(Key::KeyS), "map", || "windows".into(),
                |event| sent.borrow_mut().push((rdev::win_key_from_scancode(event.chr()), event.down)),
            );
            let tracked = super::super::TO_RELEASE.lock().unwrap().contains_key(&Key::KeyW);
            super::super::event_to_key_events(
                "windows".into(), &make_release(Key::KeyW), KeyboardMode::Map, Some(0),
            );
            release_chord(chord);
            reset_fired_keys();
            clear_session_state(&SID_A);

            assert!(consumed);
            let keep_held = action == action_id::TOGGLE_MUTE;
            assert_eq!(tracked, keep_held, "held key tracking after {action}");
            let w_events: Vec<_> = sent.borrow().iter()
                .filter(|(key, _)| *key == Key::KeyW).map(|(_, down)| *down).collect();
            assert_eq!(w_events, if keep_held { vec![] } else { vec![false] }, "{action}");
        }
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn repeated_forwarded_key_does_not_start_shortcut() {
        use base::message_proto::KeyboardMode;
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        release_chord(chord);
        let press = make_press(Key::KeyS);
        let release = make_release(Key::KeyS);
        assert!(!try_dispatch(Some(&SID_A), &press, "map", || "windows".into(), |_| {}));
        super::super::event_to_key_events("windows".into(), &press, KeyboardMode::Map, Some(0));
        for key in chord {
            super::super::event_to_key_events("windows".into(), &make_press(key), KeyboardMode::Map, Some(0));
        }
        let repeat_consumed = try_dispatch(Some(&SID_A), &press, "map", || "windows".into(), |_| {});
        let release_consumed = try_dispatch(Some(&SID_A), &release, "map", || "windows".into(), |_| {});
        let release_events = if release_consumed { vec![] } else {
            super::super::event_to_key_events("windows".into(), &release, KeyboardMode::Map, Some(0))
        };
        // 先做清理再做断言，这样一个失败的回归测试不会污染其他测试。
        super::super::event_to_key_events("windows".into(), &release, KeyboardMode::Map, Some(0));
        release_chord(chord);
        reset_fired_keys();
        clear_session_state(&SID_A);

        assert!(!repeat_consumed, "a repeated forwarded key must not become a shortcut");
        assert!(!release_consumed, "the original keyup must still reach the peer");
        assert_eq!(release_events.len(), 1);
        assert!(!release_events[0].down);
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn alt_only_shortcut_neutralizes_windows_menu() {
        use base::message_proto::KeyEvent;
        #[cfg(windows)]
        use rdev::EventType;
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut results = Vec::new();
        for (mode, alt) in ["map", "translate", "legacy"].into_iter()
            .flat_map(|mode| [Key::Alt, Key::AltGr].map(|alt| (mode, alt)))
        {
            #[cfg(any(target_os = "android", target_os = "ios"))]
            if mode == "legacy" { continue; } // Mobile does not encode Legacy events.
            #[cfg(target_os = "macos")]
            if mode == "translate" && alt == Key::AltGr { continue; } // Right Option is filtered.
            reload_from_raw(r#"{"enabled":true,"bindings":[{"action":"toggle_mute","mods":["alt"],"key":"m"}]}"#, "windows", false);
            reset_fired_keys();
            clear_session_state(&SID_A);
            let sent = std::cell::RefCell::new(Vec::new());
            let send = |event: &KeyEvent| {
                sent.borrow_mut().push(windows_shortcut_modifier(event));
            };
            let handle = |event: &rdev::Event| {
                #[cfg(windows)]
                if matches!(event.event_type, EventType::KeyPress(k) | EventType::KeyRelease(k) if k == alt) {
                    rdev::set_modifier(alt, matches!(event.event_type, EventType::KeyPress(_)));
                }
                if !try_dispatch(Some(&SID_A), event, mode, || "windows".into(), &send) {
                    for key in super::super::event_to_key_events(
                        "windows".into(), event, super::super::get_keyboard_mode_enum(mode), Some(0),
                    ) {
                        send(&key);
                    }
                }
            };
            handle(&make_press(alt));
            handle(&make_press(Key::KeyM));
            handle(&make_press(Key::KeyM));
            handle(&make_release(Key::KeyM));
            handle(&make_release(alt));
            reset_fired_keys();
            clear_session_state(&SID_A);
            results.push((mode, alt, sent.into_inner()));
        }
        assert!(results.iter().all(|(_, alt, sent)| *sent == vec![
            (*alt, true), (Key::ControlLeft, true), (*alt, false),
            (Key::ControlLeft, false), (*alt, false),
        ]), "the consumed M must not leave a bare Alt tap on Windows: {results:?}");
    }

    #[cfg(feature = "flutter")]
    fn windows_shortcut_modifier(event: &base::message_proto::KeyEvent) -> (rdev::Key, bool) {
        use base::message_proto::{key_event::Union, ControlKey};
        use rdev::Key;
        let key = match event.union.as_ref() {
            Some(Union::Chr(code)) => rdev::win_key_from_scancode(*code),
            Some(Union::Win2winHotkey(code)) => rdev::win_key_from_keycode(*code >> 16),
            Some(Union::ControlKey(key)) => match key.enum_value().unwrap() {
                ControlKey::Alt => Key::Alt,
                ControlKey::RAlt => Key::AltGr,
                ControlKey::Control => Key::ControlLeft,
                _ => panic!("unexpected modifier: {event:?}"),
            },
            _ => panic!("unexpected shortcut output: {event:?}"),
        };
        (key, event.down)
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn shortcut_cleanup_does_not_tap_alt() {
        use base::message_proto::KeyboardMode;
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for peer in ["windows", "linux", "macos"] {
            let chord = enable_defaults_and_hold_chord();
            let alt = super::super::event_to_key_events(
                peer.into(), &make_press(Key::Alt), KeyboardMode::Map, Some(0),
            );
            assert_eq!(alt.len(), 1);
            let sent = std::cell::RefCell::new(Vec::new());
            let consumed = try_dispatch(
                Some(&SID_A), &make_press(Key::KeyS), "map", || peer.into(),
                |event| {
                    if event.chr() == alt[0].chr() { sent.borrow_mut().push(event.down); }
                },
            );
            release_chord(chord);
            reset_fired_keys();
            clear_session_state(&SID_A);

            assert!(consumed);
            assert_eq!(*sent.borrow(), vec![false], "shortcut cleanup must only release Alt on {peer}");
        }
    }

    #[cfg(feature = "flutter")]
    fn shortcut_followed_by_c(release_primary: bool) -> (rdev::Key, Vec<(rdev::Key, bool)>) {
        use base::message_proto::KeyboardMode;
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_fired_keys();
        * CACHE.write().unwrap() = Arc::new(Bindings {
            enabled: true,
            pass_through: false,
            bindings: vec![Binding {
                action: action_id::SCREENSHOT.into(),
                mods: vec![Modifier::Primary],
                key: "p".into(),
            }],
        });
        let primary = if cfg!(any(target_os = "macos", target_os = "ios")) {
            Key::MetaLeft
        } else {
            Key::ControlLeft
        };
        let sent = std::cell::RefCell::new(Vec::new());
        let send = |event: &base::message_proto::KeyEvent| {
            sent.borrow_mut().push((rdev::win_key_from_scancode(event.chr()), event.down));
        };
        let handle = |event: &rdev::Event| {
            if !try_dispatch(Some(&SID_A), event, "map", || "windows".into(), &send) {
                for key_event in super::super::event_to_key_events(
                    "windows".into(), event, KeyboardMode::Map, Some(0)
                ) {
                    send(&key_event);
                }
            }
        };
        handle(&make_press(primary));
        handle(&make_press(Key::KeyP));
        handle(&make_release(Key::KeyP));
        if release_primary {
            handle(&make_release(primary));
        }
        sent.borrow_mut().clear();
        handle(&make_press(Key::KeyC));
        let result = sent.borrow().clone();
        handle(&make_release(Key::KeyC));
        handle(&make_release(primary));
        reset_fired_keys();
        (primary, result)
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn unbound_key_restores_modifiers_held_after_shortcut() {
        let (primary, sent) = shortcut_followed_by_c(false);
        assert_eq!(sent, vec![(primary, true), (rdev::Key::KeyC, true)]);
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn unbound_key_does_not_restore_released_modifiers() {
        let (_, sent) = shortcut_followed_by_c(true);
        assert_eq!(sent, vec![(rdev::Key::KeyC, true)]);
    }

    #[cfg(all(feature = "flutter", not(any(target_os = "android", target_os = "ios"))))]
    #[test]
    fn view_only_preserves_rdev_shortcut_keys() {
        use rdev::Key;
        use std::sync::atomic::Ordering;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = super::super::IS_RDEV_ENABLED.swap(true, Ordering::SeqCst);
        let chord = enable_defaults_and_hold_chord();
        assert!(try_dispatch(
            Some(&SID_A), &make_press(Key::KeyP), "map", || "windows".into(), |_| {}
        ));
        enter_view_only(&SID_A);
        let action = match_event(&make_press(Key::KeyC));
        let fired = fired_keys().contains(&Key::KeyP);
        super::super::IS_RDEV_ENABLED.store(previous, Ordering::SeqCst);
        release_chord(chord);
        clear_session_state(&SID_A);
        assert_eq!(action.as_deref(), Some(action_id::TOGGLE_CHAT));
        assert!(fired, "rdev still receives the held shortcut key's release");
    }

    #[cfg(all(feature = "flutter", not(any(target_os = "android", target_os = "ios"))))]
    #[test]
    fn view_only_clears_flutter_shortcut_keys_and_modifiers() {
        use rdev::Key;
        use std::sync::atomic::Ordering;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = super::super::IS_RDEV_ENABLED.swap(false, Ordering::SeqCst);
        let chord = enable_defaults_and_hold_chord();
        assert!(try_dispatch(
            Some(&SID_A), &make_press(Key::KeyP), "map", || "windows".into(), |_| {}
        ));
        clear_session_state(&SID_B);
        assert_eq!(match_event(&make_press(Key::KeyC)).as_deref(), Some(action_id::TOGGLE_CHAT));
        enter_view_only(&SID_A);
        super::super::IS_RDEV_ENABLED.store(previous, Ordering::SeqCst);
        assert!(fired_keys().is_empty());
        assert_eq!(
            super::super::client::get_modifiers_state(false, false, false, false),
            (false, false, false, false)
        );
        assert!(!try_dispatch(
            Some(&SID_A), &make_press(Key::KeyP), "map", || "windows".into(),
            |_| panic!("view-only reset must not replay old modifiers")
        ));
        release_chord(chord);
    }

    /// Dart 匹配器保留着同样的列表（`kShortcutActionsRunOnKeyUp`）；
    /// 该 fixture 是共享的真实来源。
    #[test]
    fn key_up_actions_match_fixture() {
        use std::collections::BTreeSet;

        let fixture: Vec<String> = serde_json::from_str(include_str!(
            "../../flutter/test/fixtures/key_up_shortcut_actions.json"
        ))
        .expect("parse fixture");
        let expected: BTreeSet<&str> = fixture.iter().map(String::as_str).collect();
        let actual: BTreeSet<&str> = RELEASE_ACTION_IDS.iter().copied().collect();
        assert_eq!(
            actual, expected,
            "runs_on_release drifted from key_up_shortcut_actions.json — update \
             shortcuts.rs, the fixture, and Dart kShortcutActionsRunOnKeyUp together"
        );
        for id in &fixture {
            assert!(runs_on_release(id));
        }
        assert!(!runs_on_release(action_id::SCREENSHOT));
    }

    /// 关闭标签页和标签切换会在按键
    /// 释放之前就把焦点移走；它们在释放时运行，好让同一个匹配器看到整个
    /// 按下过程。
    #[cfg(feature = "flutter")]
    #[test]
    fn focus_changing_actions_run_on_release() {
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        let prefix = || vec![Modifier::Primary, Modifier::Alt, Modifier::Shift];
        * CACHE.write().unwrap() = Arc::new(Bindings {
            enabled: true,
            pass_through: false,
            bindings: vec![
                Binding { action: action_id::CLOSE_TAB.into(), mods: prefix(), key: "w".into() },
                Binding { action: action_id::SCREENSHOT.into(), mods: prefix(), key: "p".into() },
            ],
        });
        let dispatch = |sid: &hbb_common::SessionID, e: &rdev::Event| {
            try_dispatch(Some(sid), e, "map", || "windows".into(), |_| {})
        };
        let pending_for = |k: Key| RELEASE_ACTIONS.lock().unwrap().get(&k).cloned();

        assert!(dispatch(&SID_A, &make_press(Key::KeyW)));
        assert_eq!(pending_for(Key::KeyW), Some((SID_A, action_id::CLOSE_TAB.to_owned())));
        assert!(dispatch(&SID_A, &make_press(Key::KeyW)), "repeat is consumed");
        assert_eq!(pending_for(Key::KeyW), Some((SID_A, action_id::CLOSE_TAB.to_owned())));
        assert!(dispatch(&SID_B, &make_release(Key::KeyW)), "release is consumed wherever it lands");
        assert_eq!(pending_for(Key::KeyW), None, "the action ran on the release");

        assert!(dispatch(&SID_A, &make_press(Key::KeyP)));
        assert_eq!(pending_for(Key::KeyP), None, "other actions still run on the press");
        assert!(dispatch(&SID_A, &make_release(Key::KeyP)));

        assert!(dispatch(&SID_A, &make_press(Key::KeyW)));
        clear_session_state(&SID_A);
        assert_eq!(pending_for(Key::KeyW), None, "a closed session owes nothing");
        assert!(dispatch(&SID_A, &make_release(Key::KeyW)), "but its key stays owned");
        release_chord(chord);
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn clipboard_prompt_waits_for_release_before_taking_focus() {
        use rdev::Key;

        let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let chord = enable_defaults_and_hold_chord();
        * CACHE.write().unwrap() = Arc::new(Bindings {
            enabled: true,
            pass_through: false,
            bindings: vec![Binding {
                action: action_id::SEND_CLIPBOARD_KEYSTROKES.into(),
                mods: vec![Modifier::Primary, Modifier::Alt, Modifier::Shift],
                key: "v".into(),
            }],
        });
        let dispatch = |event: &rdev::Event| {
            try_dispatch(Some(&SID_A), event, "map", || "linux".into(), |_| {})
        };
        for _ in 0..2 {
            assert!(dispatch(&make_press(Key::KeyV)));
            assert_eq!(
                RELEASE_ACTIONS.lock().unwrap().get(&Key::KeyV).cloned(),
                Some((SID_A, action_id::SEND_CLIPBOARD_KEYSTROKES.to_owned())),
                "the prompt must not take focus before the trigger release"
            );
            assert!(dispatch(&make_release(Key::KeyV)));
            assert!(!fired_keys().contains(&Key::KeyV));
            assert!(!RELEASE_ACTIONS.lock().unwrap().contains_key(&Key::KeyV));
        }
        release_chord(chord);
    }

    #[cfg(feature = "flutter")]
    const SID_A: hbb_common::SessionID = hbb_common::SessionID::from_u128(0xA);
    #[cfg(feature = "flutter")]
    const SID_B: hbb_common::SessionID = hbb_common::SessionID::from_u128(0xB);

    #[cfg(feature = "flutter")]
    fn fired_keys() -> std::collections::HashSet<rdev::Key> {
        FIRED_KEYS.lock().unwrap().clone()
    }

    #[cfg(feature = "flutter")]
    fn reset_fired_keys() {
        FIRED_KEYS.lock().unwrap().clear();
        RELEASE_ACTIONS.lock().unwrap().clear();
    }

    #[cfg(feature = "flutter")]
    fn enable_defaults_and_hold_chord() -> [rdev::Key; 3] {
        use base::message_proto::KeyboardMode;
        use rdev::Key;

        * CACHE.write().unwrap() = Arc::new(Bindings {
            enabled: true,
            pass_through: false,
            bindings: default_bindings(),
        });
        reset_fired_keys();
        clear_session_state(&SID_A);
        clear_session_state(&SID_B);
        let primary = if cfg!(any(target_os = "macos", target_os = "ios")) {
            Key::MetaLeft
        } else {
            Key::ControlLeft
        };
        let chord = [primary, Key::Alt, Key::ShiftLeft];
        for k in chord {
            super::super::event_to_key_events("windows".into(), &make_press(k), KeyboardMode::Map, None);
        }
        chord
    }

    #[cfg(feature = "flutter")]
    fn release_chord(chord: [rdev::Key; 3]) {
        use base::message_proto::KeyboardMode;
        for k in chord {
            super::super::event_to_key_events("windows".into(), &make_release(k), KeyboardMode::Map, None);
        }
    }
}
