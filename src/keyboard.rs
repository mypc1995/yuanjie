#[cfg(feature = "flutter")]
use crate::flutter;
#[cfg(target_os = "windows")]
use crate::platform::windows::{get_char_from_vk, get_unicode_from_vk};
#[cfg(not(feature = "flutter"))]
use crate::ui::CUR_SESSION;
use crate::ui_session_interface::{InvokeUiSession, Session};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::{client::get_key_state, common::GrabState};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::log;
use base::message_proto::*;
use hbb_common::SessionID;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use rdev::KeyCode;
use rdev::{Event, EventType, Key};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[cfg(windows)]
static mut IS_ALT_GR: bool = false;

#[allow(dead_code)]
const OS_LOWER_WINDOWS: &str = "windows";
#[allow(dead_code)]
const OS_LOWER_LINUX: &str = "linux";
#[allow(dead_code)]
const OS_LOWER_MACOS: &str = "macos";
#[allow(dead_code)]
const OS_LOWER_ANDROID: &str = "android";

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
static KEYBOARD_HOOKED: AtomicBool = AtomicBool::new(false);

// 跟踪按键按下状态，用于相对鼠标模式的退出快捷键。
// macOS：Cmd+G（跟踪 G 键）
// Windows/Linux：Ctrl+Alt（跟踪最后按下的那个修饰键）
// 这可以避免操作系统按键重复时再次触发退出。
#[cfg(all(feature = "flutter", any(target_os = "windows", target_os = "macos", target_os = "linux")))]
static EXIT_SHORTCUT_KEY_DOWN: AtomicBool = AtomicBool::new(false);

// 跟踪相对鼠标模式当前是否处于激活状态。
// 该状态由 Flutter 通过 set_relative_mouse_mode_state() 设置，并被
// rdev 抓取循环用来判断是否应处理退出快捷键。
#[cfg(all(feature = "flutter", any(target_os = "windows", target_os = "macos", target_os = "linux")))]
static RELATIVE_MOUSE_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 从 Flutter 设置相对鼠标模式状态。
/// 进入或退出相对鼠标模式时会调用此函数。
#[cfg(all(feature = "flutter", any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn set_relative_mouse_mode_state(active: bool) {
    RELATIVE_MOUSE_MODE_ACTIVE.store(active, Ordering::SeqCst);
    // 模式变化时重置退出快捷键状态，避免状态残留
    if !active {
        EXIT_SHORTCUT_KEY_DOWN.store(false, Ordering::SeqCst);
    }
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
static IS_RDEV_ENABLED: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    static ref TO_RELEASE: Arc<Mutex<HashMap<Key, Event>>> = Arc::new(Mutex::new(HashMap::new()));
    static ref MODIFIERS_STATE: Mutex<HashMap<Key, bool>> = {
        let mut m = HashMap::new();
        m.insert(Key::ShiftLeft, false);
        m.insert(Key::ShiftRight, false);
        m.insert(Key::ControlLeft, false);
        m.insert(Key::ControlRight, false);
        m.insert(Key::Alt, false);
        m.insert(Key::AltGr, false);
        m.insert(Key::MetaLeft, false);
        m.insert(Key::MetaRight, false);
        Mutex::new(m)
    };
}

pub mod shortcuts;

pub mod client {
    use super::*;

    /// 跟踪抓取归属，并在各线程之间串行化状态切换。
    ///
    /// 多个 Flutter isolate（每个会话窗口一个）会并发调用
    /// `change_grab_status(Run/Wait)`。如果不加串行化，会话 A 的
    /// 过期 `Wait` 就可能覆盖会话 B 刚刚获取的
    /// 抓取（在任何桌面操作系统上）。
    ///
    /// 在实践中 Windows 和 macOS 不太容易出现这种情况，因为 Flutter
    /// 侧只有在窗口内发生鼠标点击后才会触发 `enterView`，
    /// 但我们不能依赖这一点。在 Linux/X11 上，`XGrabKeyboard` 还可能
    /// 引发焦点变化反馈循环（约 10 Hz），因此 `last_grab` 会对
    /// `Run` 之后不久到达的虚假 `Wait` 事件做去抖。
    #[derive(Default)]
    struct GrabOwnerState {
        owner: Option<u128>,
        last_grab: Option<std::time::Instant>,
        /// 延迟释放线程在飞行过程中为 true。用于防止
        /// 在 X11 反馈循环期间派生多余的线程。
        deferred_pending: bool,
    }

    /// 获取抓取之后，我们抑制同一会话 Wait 的时长。
    /// 必须超过一个完整的 X11 反馈周期（约 100 毫秒：50 毫秒启用 + 50 毫秒禁用）。
    #[cfg(target_os = "linux")]
    const GRAB_DEBOUNCE_MS: u128 = 300;

    lazy_static::lazy_static! {
        static ref IS_GRAB_STARTED: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        static ref GRAB_STATE: Arc<Mutex<GrabOwnerState>> = Arc::new(Mutex::new(GrabOwnerState::default()));
    }

    #[cfg(target_os = "linux")]
    lazy_static::lazy_static! {
        static ref GRAB_OP_LOCK: Mutex<()> = Mutex::new(());
    }

    #[cfg(target_os = "linux")]
    fn apply_run_grab_if_owner(session_id: u128, disable_first: bool) {
        let _lock = GRAB_OP_LOCK.lock().unwrap();
        let gs = GRAB_STATE.lock().unwrap();
        if gs.owner != Some(session_id) {
            return;
        }
        drop(gs);
        if disable_first {
            log::debug!("[grab] handoff: disable_grab before re-grab");
            rdev::disable_grab();
        }
        rdev::enable_grab();
    }

    #[cfg(target_os = "linux")]
    fn disable_grab_if_released() {
        let _lock = GRAB_OP_LOCK.lock().unwrap();
        let should_disable = {
            let gs = GRAB_STATE.lock().unwrap();
            gs.owner.is_none() && gs.last_grab.is_none()
        };
        if should_disable {
            rdev::disable_grab();
        }
    }

    pub fn start_grab_loop() {
        let mut lock = IS_GRAB_STARTED.lock().unwrap();
        if *lock {
            return;
        }
        super::start_grab_loop();
        * lock = true;
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn change_grab_status(state: GrabState, keyboard_mode: &str, session_id: u128) {
        #[cfg(feature = "flutter")]
        if !IS_RDEV_ENABLED.load(Ordering::SeqCst) {
            return;
        }
        // 串行化状态切换，避免上一个持有者的过期 `Wait` 
        // 覆盖来自另一个会话窗口的新 `Run`。
        let mut release_after_unlock = None;
        #[cfg(target_os = "linux")]
        let mut run_grab_after_unlock = None;
        #[cfg(target_os = "linux")]
        let mut disable_after_unlock = false;
        let mut gs = GRAB_STATE.lock().unwrap();
        match state {
            GrabState::Ready => {}
            GrabState::Run => {
                #[cfg(windows)]
                update_grab_get_key_name(keyboard_mode);

                // 幂等：如果该会话已经拥有抓取，只需
                // 刷新去抖定时器（这证明该会话仍然
                // 处于活动焦点状态），并跳过真正的抓取调用。
                if gs.owner == Some(session_id) {
                    gs.last_grab = Some(std::time::Instant::now());
                    // 重置，使下一个 Wait 能够派生一个新的延迟释放
                    // 定时器，并带上最新的 last_grab 快照。
                    gs.deferred_pending = false;
                    log::debug!(
                        "[grab] Run(0x{:x}): already owner, refresh debounce",
                        session_id
                    );
                    return;
                }

                log::debug!(
                    "[grab] Run(0x{:x}): prev_owner={}, mode={}",
                    session_id,
                    gs.owner
                        .map_or("none".to_string(), |id| format!("0x{:x}", id)),
                    keyboard_mode,
                );

                #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
                KEYBOARD_HOOKED.store(true, Ordering::SeqCst);

                #[cfg(target_os = "linux")]
                let had_owner = gs.owner.is_some();
                gs.owner = Some(session_id);
                gs.last_grab = Some(std::time::Instant::now());
                // 使上一个持有者正在进行的延迟释放失效，
                // 以免它抑制新持有者的新定时器。
                gs.deferred_pending = false;
                #[cfg(target_os = "linux")]
                {
                    run_grab_after_unlock = Some(had_owner);
                }
            }
            GrabState::Wait => {
                // 丢弃与当前抓取持有者不对应的过期 `Wait` 事件。
                // 这可以防止迟到的 PointerExit 让
                // 会话 A 释放会话 B 刚刚获取的抓取。
                if gs.owner != Some(session_id) {
                    log::debug!(
                        "[grab] Wait(0x{:x}): ignored, owner={}",
                        session_id,
                        gs.owner
                            .map_or("none".to_string(), |id| format!("0x{:x}", id)),
                    );
                    return;
                }

                // 去抖：在 Linux/X11 上，XGrabKeyboard 会引发焦点变化
                // 反馈循环（抓取 -> PointerExit -> 解除抓取 -> PointerEnter ->
                // 抓取 -> ...）。如果抓取是最近才获取的，就抑制 Wait
                // 且由同一会话发起 —— 那是 X11 反馈，而不是真正的离开。
                // 会安排一次延迟释放，以免在
                // 去抖窗口内的真正离开被永久丢失。
                #[cfg(target_os = "linux")]
                if let Some(t) = gs.last_grab {
                    let elapsed = t.elapsed().as_millis();
                    if elapsed < GRAB_DEBOUNCE_MS {
                        if !gs.deferred_pending {
                            log::debug!(
                                "[grab] Wait(0x{:x}): debounced ({}ms < {}ms), scheduling deferred release",
                                session_id, elapsed, GRAB_DEBOUNCE_MS,
                            );
                            gs.deferred_pending = true;
                            let remaining = (GRAB_DEBOUNCE_MS - elapsed) as u64 + 50;
                            let snapshot = gs.last_grab;
                            let mode = keyboard_mode.to_string();
                            std::thread::spawn(move || {
                                std::thread::sleep(std::time::Duration::from_millis(remaining));
                                let release_keys = {
                                    let mut gs = GRAB_STATE.lock().unwrap();
                                    // 只有在此期间没有新的 Run 刷新过抓取时才释放。
                                    if gs.owner == Some(session_id) && gs.last_grab == snapshot {
                                        let to_release = take_remote_keys();
                                        gs.deferred_pending = false;
                                        log::debug!(
                                            "[grab] Wait(0x{:x}): deferred release",
                                            session_id
                                        );
                                        KEYBOARD_HOOKED.store(false, Ordering::SeqCst);
                                        gs.owner = None;
                                        gs.last_grab = None;
                                        Some(to_release)
                                    } else {
                                        log::debug!(
                                            "[grab] Wait(0x{:x}): deferred release cancelled (grab refreshed)",
                                            session_id,
                                        );
                                        None
                                    }
                                };
                                if let Some(to_release) = release_keys {
                                    disable_grab_if_released();
                                    release_remote_keys_for_events(&mode, to_release);
                                }
                            });
                        } else {
                            log::debug!(
                                "[grab] Wait(0x{:x}): debounced, deferred release already pending",
                                session_id,
                            );
                        }
                        return;
                    }
                }

                log::debug!("[grab] Wait(0x{:x}): releasing grab", session_id);

                #[cfg(windows)]
                rdev::set_get_key_unicode(false);

                #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
                KEYBOARD_HOOKED.store(false, Ordering::SeqCst);

                gs.owner = None;
                gs.last_grab = None;
                gs.deferred_pending = false;
                release_after_unlock = Some(take_remote_keys());
                #[cfg(target_os = "linux")]
                {
                    disable_after_unlock = true;
                }
            }
            GrabState::Exit => {}
        }
        drop(gs);
        #[cfg(target_os = "linux")]
        {
            if disable_after_unlock {
                disable_grab_if_released();
            }
            if let Some(disable_first) = run_grab_after_unlock {
                apply_run_grab_if_owner(session_id, disable_first);
            }
        }
        if let Some(to_release) = release_after_unlock {
            release_remote_keys_for_events(keyboard_mode, to_release);
        }
    }

    pub fn process_event(keyboard_mode: &str, event: &Event, lock_modes: Option<i32>) {
        // 快捷键拦截 —— 必须放在任何线路编码之前。
        // 触发快捷键的按下事件会被消费，其自动重复
        // 和松开事件也一样；其他所有事件都会继续传递到
        // 编码/转发路径。
        //
        // NOTE: Shortcut matching intentionally happens BEFORE any key swapping
        // （swap_modifier_key），使快捷键绑定到实际按下的物理按键，
        // 而不是交换后的按键。这让快捷键设置更直观：用户绑定的是
        // 他们实际按下的按键，与交换设置无关。
        // 按键交换只影响发送到远端的内容。
        //
        // 以 `feature = "flutter"` 做条件门控，因为派发目标
        // （`flutter::push_session_event`）仅 Flutter 可用。Sciter 构建从不
        // 调用 `reload_from_config`，因此缓存保持禁用状态，而
        // 匹配器本来也只会空操作 —— 但我们仍然完全跳过该调用，以免
        // 在无法呈现该动作的界面上，被手工修改的配置悄悄吞掉按键。
        // 没有途径来呈现该动作。
        //
        // session_id 为 `None` 会让辅助函数通过
        // `flutter::get_cur_session_id()` 解析 —— rdev 抓取循环是进程级的，
        // 没有可供传递的逐事件会话上下文。
        #[cfg(feature = "flutter")]
        if crate::keyboard::shortcuts::try_dispatch(
            None,
            event,
            keyboard_mode,
            || get_peer_platform().to_lowercase(),
            send_key_event,
        ) {
            return;
        }

        let keyboard_mode = get_keyboard_mode_enum(keyboard_mode);
        if is_long_press(&event) {
            return;
        }
        let peer = get_peer_platform().to_lowercase();
        for key_event in event_to_key_events(peer, &event, keyboard_mode, lock_modes) {
            send_key_event(&key_event);
        }
    }

    pub fn process_event_with_session<T: InvokeUiSession>(
        keyboard_mode: &str,
        event: &Event,
        lock_modes: Option<i32>,
        session: &Session<T>,
        session_id: SessionID,
    ) {
        // 快捷键拦截 —— 参见上面 `process_event` 中的详细注释，
        // 其中说明了消费事件/特性门控的理由。唯一的区别是
        // 这里 Flutter 的 FFI 路径会显式传递 SessionID，
        // 因此派发会准确指向按键产生的那个标签页，
        // 而不依赖全局焦点跟踪器。
        #[cfg(feature = "flutter")]
        if crate::keyboard::shortcuts::try_dispatch(
            Some(&session_id),
            event,
            keyboard_mode,
            || session.peer_platform().to_lowercase(),
            |key_event| session.send_key_event(key_event),
        ) {
            return;
        }
        #[cfg(not(feature = "flutter"))]
        let _ = session_id;

        let keyboard_mode = get_keyboard_mode_enum(keyboard_mode);
        if is_long_press(&event) {
            return;
        }
        let peer = session.peer_platform().to_lowercase();
        for key_event in event_to_key_events(peer, &event, keyboard_mode, lock_modes) {
            session.send_key_event(&key_event);
        }
    }

    pub fn get_modifiers_state(
        alt: bool,
        ctrl: bool,
        shift: bool,
        command: bool,
    ) -> (bool, bool, bool, bool) {
        let modifiers_lock = MODIFIERS_STATE.lock().unwrap();
        let ctrl = *modifiers_lock.get(&Key::ControlLeft).unwrap()
            || *modifiers_lock.get(&Key::ControlRight).unwrap()
            || ctrl;
        let shift = *modifiers_lock.get(&Key::ShiftLeft).unwrap()
            || *modifiers_lock.get(&Key::ShiftRight).unwrap()
            || shift;
        let command = *modifiers_lock.get(&Key::MetaLeft).unwrap()
            || *modifiers_lock.get(&Key::MetaRight).unwrap()
            || command;
        let alt = *modifiers_lock.get(&Key::Alt).unwrap()
            || *modifiers_lock.get(&Key::AltGr).unwrap()
            || alt;

        (alt, ctrl, shift, command)
    }

    pub fn legacy_modifiers(
        key_event: &mut KeyEvent,
        alt: bool,
        ctrl: bool,
        shift: bool,
        command: bool,
    ) {
        if alt
            && !crate::is_control_key(&key_event, &ControlKey::Alt)
            && !crate::is_control_key(&key_event, &ControlKey::RAlt)
        {
            key_event.modifiers.push(ControlKey::Alt.into());
        }
        if shift
            && !crate::is_control_key(&key_event, &ControlKey::Shift)
            && !crate::is_control_key(&key_event, &ControlKey::RShift)
        {
            key_event.modifiers.push(ControlKey::Shift.into());
        }
        if ctrl
            && !crate::is_control_key(&key_event, &ControlKey::Control)
            && !crate::is_control_key(&key_event, &ControlKey::RControl)
        {
            key_event.modifiers.push(ControlKey::Control.into());
        }
        if command
            && !crate::is_control_key(&key_event, &ControlKey::Meta)
            && !crate::is_control_key(&key_event, &ControlKey::RWin)
        {
            key_event.modifiers.push(ControlKey::Meta.into());
        }
    }

    #[cfg(target_os = "android")]
    pub fn map_key_to_control_key(key: &rdev::Key) -> Option<ControlKey> {
        match key {
            Key::Alt => Some(ControlKey::Alt),
            Key::ShiftLeft => Some(ControlKey::Shift),
            Key::ControlLeft => Some(ControlKey::Control),
            Key::MetaLeft => Some(ControlKey::Meta),
            Key::AltGr => Some(ControlKey::RAlt),
            Key::ShiftRight => Some(ControlKey::RShift),
            Key::ControlRight => Some(ControlKey::RControl),
            Key::MetaRight => Some(ControlKey::RWin),
            _ => None,
        }
    }

    pub fn event_lock_screen() -> KeyEvent {
        let mut key_event = KeyEvent::new();
        key_event.set_control_key(ControlKey::LockScreen);
        key_event.down = true;
        key_event.mode = KeyboardMode::Legacy.into();
        key_event
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn lock_screen() {
        send_key_event(&event_lock_screen());
    }

    pub fn event_ctrl_alt_del(peer_platform: &str) -> KeyEvent {
        let mut key_event = KeyEvent::new();
        if peer_platform == "Windows" {
            key_event.set_control_key(ControlKey::CtrlAltDel);
            key_event.down = true;
        } else {
            key_event.set_control_key(ControlKey::Delete);
            legacy_modifiers(&mut key_event, true, true, false, false);
            key_event.press = true;
        }
        key_event.mode = KeyboardMode::Legacy.into();
        key_event
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn ctrl_alt_del() {
        send_key_event(&event_ctrl_alt_del(&get_peer_platform()));
    }
}

#[cfg(windows)]
pub fn update_grab_get_key_name(keyboard_mode: &str) {
    match keyboard_mode {
        "map" => rdev::set_get_key_unicode(false),
        "translate" => rdev::set_get_key_unicode(true),
        "legacy" => rdev::set_get_key_unicode(true),
        _ => {}
    };
}

#[cfg(target_os = "windows")]
static mut IS_0X021D_DOWN: bool = false;

#[cfg(target_os = "macos")]
static mut IS_LEFT_OPTION_DOWN: bool = false;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn get_keyboard_mode() -> String {
    #[cfg(not(feature = "flutter"))]
    if let Some(session) = CUR_SESSION.lock().unwrap().as_ref() {
        return session.get_keyboard_mode();
    }
    #[cfg(feature = "flutter")]
    if let Some(session) = flutter::get_cur_session() {
        return session.get_keyboard_mode();
    }
    "legacy".to_string()
}

/// 检查相对鼠标模式的退出快捷键是否处于激活状态。
/// 退出快捷键（只退出，不切换）：
/// - macOS：Cmd+G
/// - Windows/Linux：Ctrl+Alt（两者同时按下时触发）
/// 注意：该快捷键仅在 Flutter 客户端可用。Sciter 客户端不支持相对鼠标模式。
#[cfg(feature = "flutter")]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn is_exit_relative_mouse_shortcut(key: Key) -> bool {
    let modifiers = MODIFIERS_STATE.lock().unwrap();

    #[cfg(target_os = "macos")]
    {
        // macOS：Cmd+G 退出
        if key != Key::KeyG {
            return false;
        }
        let meta = *modifiers.get(&Key::MetaLeft).unwrap_or(&false)
            || *modifiers.get(&Key::MetaRight).unwrap_or(&false);
        return meta;
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Windows/Linux：Ctrl+Alt 退出
        // 在 Alt 按下时按下 Ctrl，或在 Ctrl 按下时按下 Alt 时触发
        let is_ctrl_key = key == Key::ControlLeft || key == Key::ControlRight;
        let is_alt_key = key == Key::Alt || key == Key::AltGr;

        if !is_ctrl_key && !is_alt_key {
            return false;
        }

        let ctrl = *modifiers.get(&Key::ControlLeft).unwrap_or(&false)
            || *modifiers.get(&Key::ControlRight).unwrap_or(&false);
        let alt = *modifiers.get(&Key::Alt).unwrap_or(&false)
            || *modifiers.get(&Key::AltGr).unwrap_or(&false);

        // 当按下 Ctrl 而 Alt 已经按下，或反之
        (is_ctrl_key && alt) || (is_alt_key && ctrl)
    }
}

/// 通知 Flutter 退出相对鼠标模式。
/// 注意：这是 Flutter 专用的。Sciter 客户端不支持相对鼠标模式。
#[cfg(feature = "flutter")]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn notify_exit_relative_mouse_mode() {
    let session_id = flutter::get_cur_session_id();
    flutter::push_session_event(&session_id, "exit_relative_mouse_mode", vec![]);
}

/// 在 rdev 抓取循环中处理相对鼠标模式的快捷键。
/// 如果该事件应被阻止发送到对端，则返回 true。
#[cfg(feature = "flutter")]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
#[inline]
fn can_exit_relative_mouse_mode_from_grab_loop() -> bool {
    // 只在相对鼠标模式真正激活时才处理退出快捷键。
    // 这样在非相对鼠标模式下就不会屏蔽 Ctrl+Alt（或 Cmd+G）。
    if !RELATIVE_MOUSE_MODE_ACTIVE.load(Ordering::SeqCst) {
        return false;
    }

    let Some(session) = flutter::get_cur_session() else {
        return false;
    };

    // 仅用于远程桌面会话。
    if !session.is_default() {
        return false;
    }

    // 必须拥有键盘权限，且不处于仅查看模式。
    if !*session.server_keyboard_enabled.read().unwrap() {
        return false;
    }
    let lc = session.lc.read().unwrap();
    if lc.view_only.v {
        return false;
    }

    // 对端必须支持相对鼠标模式。
    crate::common::is_support_relative_mouse_mode_num(lc.version)
}

#[cfg(feature = "flutter")]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
#[inline]
fn should_block_relative_mouse_shortcut(key: Key, is_press: bool) -> bool {
    if !KEYBOARD_HOOKED.load(Ordering::SeqCst) {
        return false;
    }

    // 根据平台确定要跟踪哪个键以屏蔽 key-up
    #[cfg(target_os = "macos")]
    let is_tracked_key = key == Key::KeyG;
    #[cfg(not(target_os = "macos"))]
    let is_tracked_key = key == Key::ControlLeft
        || key == Key::ControlRight
        || key == Key::Alt
        || key == Key::AltGr;

    // 如果 key down 被屏蔽，则屏蔽 key up（避免远端出现孤立的 key up 事件）。
    // 这必须在清除下面的标志之前检查。
    if is_tracked_key && !is_press && EXIT_SHORTCUT_KEY_DOWN.swap(false, Ordering::SeqCst) {
        return true;
    }

    // 退出相对鼠标模式的快捷键：
    // - macOS：Cmd+G
    // - Windows/Linux：Ctrl+Alt
    // 加上限制，只对受支持/符合条件的会话生效，以避免意外屏蔽该组合键。
    if is_exit_relative_mouse_shortcut(key) {
        if !can_exit_relative_mouse_mode_from_grab_loop() {
            return false;
        }
        if is_press {
            // 只在从“未按下”变为“已按下”时触发退出。
            // 这可以避免操作系统按键重复时再次触发。
            if !EXIT_SHORTCUT_KEY_DOWN.swap(true, Ordering::SeqCst) {
                notify_exit_relative_mouse_mode();
            }
        }
        return true;
    }

    false
}

fn start_grab_loop() {
    std::env::set_var("KEYBOARD_ONLY", "y");
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    std::thread::spawn(move || {
        let try_handle_keyboard = move |event: Event, key: Key, is_press: bool| -> Option<Event> {
            // 修复 #2211：CAPS LOCK 不生效
            if key == Key::CapsLock || key == Key::NumLock {
                return Some(event);
            }

            let _scan_code = event.position_code;
            let _code = event.platform_code as KeyCode;

            #[cfg(feature = "flutter")]
            if should_block_relative_mouse_shortcut(key, is_press) {
                return None;
            }

            let res = if KEYBOARD_HOOKED.load(Ordering::SeqCst) {
                client::process_event(&get_keyboard_mode(), &event, None);
                if is_press {
                    None
                } else {
                    Some(event)
                }
            } else {
                Some(event)
            };

            #[cfg(target_os = "windows")]
            match _scan_code {
                0x1D | 0x021D => rdev::set_modifier(Key::ControlLeft, is_press),
                0xE01D => rdev::set_modifier(Key::ControlRight, is_press),
                0x2A => rdev::set_modifier(Key::ShiftLeft, is_press),
                0x36 => rdev::set_modifier(Key::ShiftRight, is_press),
                0x38 => rdev::set_modifier(Key::Alt, is_press),
                // 右 Alt
                0xE038 => rdev::set_modifier(Key::AltGr, is_press),
                0xE05B => rdev::set_modifier(Key::MetaLeft, is_press),
                0xE05C => rdev::set_modifier(Key::MetaRight, is_press),
                _ => {}
            }

            #[cfg(target_os = "windows")]
            unsafe {
                // AltGr
                if _scan_code == 0x021D {
                    IS_0X021D_DOWN = is_press;
                }
            }

            #[cfg(target_os = "macos")]
            unsafe {
                if _code == rdev::kVK_Option {
                    IS_LEFT_OPTION_DOWN = is_press;
                }
            }

            return res;
        };
        let func = move |event: Event| match event.event_type {
            EventType::KeyPress(key) => try_handle_keyboard(event, key, true),
            EventType::KeyRelease(key) => try_handle_keyboard(event, key, false),
            _ => Some(event),
        };
        #[cfg(target_os = "macos")]
        rdev::set_is_main_thread(false);
        #[cfg(target_os = "windows")]
        rdev::set_event_popup(false);
        if let Err(error) = rdev::grab(func) {
            log::error!("rdev Error: {:?}", error)
        }
    });

    #[cfg(target_os = "linux")]
    if let Err(err) = rdev::start_grab_listen(move |event: Event| match event.event_type {
        EventType::KeyPress(key) | EventType::KeyRelease(key) => {
            let is_press = matches!(event.event_type, EventType::KeyPress(_));
            if let Key::Unknown(keycode) = key {
                log::error!("rdev get unknown key, keycode is {:?}", keycode);
            } else {
                #[cfg(feature = "flutter")]
                if should_block_relative_mouse_shortcut(key, is_press) {
                    return None;
                }
                client::process_event(&get_keyboard_mode(), &event, None);
            }
            None
        }
        _ => Some(event),
    }) {
        log::error!("Failed to init rdev grab thread: {:?}", err);
    };
}

// 这里使用 `#[allow(dead_code)]` 没问题。无需停止抓取循环。
#[allow(dead_code)]
fn stop_grab_loop() -> Result<(), rdev::GrabError> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    rdev::exit_grab()?;
    #[cfg(target_os = "linux")]
    rdev::exit_grab_listen();
    Ok(())
}

pub fn is_long_press(event: &Event) -> bool {
    let keys = MODIFIERS_STATE.lock().unwrap();
    match event.event_type {
        EventType::KeyPress(k) => {
            if let Some(&state) = keys.get(&k) {
                if state == true {
                    return true;
                }
            }
        }
        _ => {}
    };
    return false;
}

fn take_remote_keys() -> HashMap<Key, Event> {
    let mut to_release = TO_RELEASE.lock().unwrap();
    std::mem::take(&mut *to_release)
}

fn release_remote_keys_for_events(keyboard_mode: &str, to_release: HashMap<Key, Event>) {
    for (key, mut event) in to_release.into_iter() {
        #[cfg(feature = "flutter")]
        if get_peer_platform() == "Windows" && shortcuts::release_modifier_on_leave(key) {
            continue;
        }
        event.event_type = EventType::KeyRelease(key);
        client::process_event(keyboard_mode, &event, None);
        // 如果按下了 Alt 或 AltGr，我们需要再发送一次按键事件来释放它。
        // 因为受控端可能保持着 alt 状态，比如本地窗口通过 [Alt + Tab] 切换时。
        if key == Key::Alt || key == Key::AltGr {
            event.event_type = EventType::KeyPress(key);
            client::process_event(keyboard_mode, &event, None);
            event.event_type = EventType::KeyRelease(key);
            client::process_event(keyboard_mode, &event, None);
        }
    }
}

#[allow(dead_code)]
pub fn release_remote_keys(keyboard_mode: &str) {
    // 待办！：客户端突然退出，如何释放按键？
    release_remote_keys_for_events(keyboard_mode, take_remote_keys());
}

pub fn get_keyboard_mode_enum(keyboard_mode: &str) -> KeyboardMode {
    match keyboard_mode {
        "map" => KeyboardMode::Map,
        "translate" => KeyboardMode::Translate,
        "legacy" => KeyboardMode::Legacy,
        _ => KeyboardMode::Map,
    }
}

#[inline]
pub fn is_modifier(key: &rdev::Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::MetaLeft
            | Key::MetaRight
            | Key::Alt
            | Key::AltGr
    )
}

#[inline]
#[allow(dead_code)]
pub fn is_modifier_code(evt: &KeyEvent) -> bool {
    match evt.union {
        Some(key_event::Union::Chr(code)) => {
            let key = rdev::linux_key_from_code(code);
            is_modifier(&key)
        }
        _ => false,
    }
}

#[inline]
pub fn is_numpad_rdev_key(key: &rdev::Key) -> bool {
    matches!(
        key,
        Key::Kp0
            | Key::Kp1
            | Key::Kp2
            | Key::Kp3
            | Key::Kp4
            | Key::Kp5
            | Key::Kp6
            | Key::Kp7
            | Key::Kp8
            | Key::Kp9
            | Key::KpMinus
            | Key::KpMultiply
            | Key::KpDivide
            | Key::KpPlus
            | Key::KpDecimal
    )
}

#[inline]
pub fn is_letter_rdev_key(key: &rdev::Key) -> bool {
    matches!(
        key,
        Key::KeyA
            | Key::KeyB
            | Key::KeyC
            | Key::KeyD
            | Key::KeyE
            | Key::KeyF
            | Key::KeyG
            | Key::KeyH
            | Key::KeyI
            | Key::KeyJ
            | Key::KeyK
            | Key::KeyL
            | Key::KeyM
            | Key::KeyN
            | Key::KeyO
            | Key::KeyP
            | Key::KeyQ
            | Key::KeyR
            | Key::KeyS
            | Key::KeyT
            | Key::KeyU
            | Key::KeyV
            | Key::KeyW
            | Key::KeyX
            | Key::KeyY
            | Key::KeyZ
    )
}

// https://github.com/rustdesk/rustdesk/issues/8599
// 我们只是把这些键当作字母键添加。
#[inline]
pub fn is_letter_rdev_key_ex(key: &rdev::Key) -> bool {
    matches!(
        key,
        Key::LeftBracket | Key::RightBracket | Key::SemiColon | Key::Quote | Key::Comma | Key::Dot
    )
}

#[inline]
fn is_numpad_key(event: &Event) -> bool {
    matches!(event.event_type, EventType::KeyPress(key) | EventType::KeyRelease(key) if is_numpad_rdev_key(&key))
}

// 检查是否为字母键，以处理锁定模式。
// 只有字母键才需要检查并发送锁定键状态。
#[inline]
fn is_letter_key_4_lock_modes(event: &Event) -> bool {
    matches!(event.event_type, EventType::KeyPress(key) | EventType::KeyRelease(key) if (is_letter_rdev_key(&key) || is_letter_rdev_key_ex(&key)))
}

fn parse_add_lock_modes_modifiers(
    key_event: &mut KeyEvent,
    lock_modes: i32,
    is_numpad_key: bool,
    is_letter_key: bool,
) {
    const CAPS_LOCK: i32 = 1;
    const NUM_LOCK: i32 = 2;
    // const SCROLL_LOCK: i32 = 3;
    if is_letter_key && (lock_modes & (1 << CAPS_LOCK) != 0) {
        key_event.modifiers.push(ControlKey::CapsLock.into());
    }
    if is_numpad_key && lock_modes & (1 << NUM_LOCK) != 0 {
        key_event.modifiers.push(ControlKey::NumLock.into());
    }
    // if lock_modes & (1 << SCROLL_LOCK) != 0 {
    //     key_event.modifiers.push(ControlKey::ScrollLock.into());
    // }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn add_lock_modes_modifiers(key_event: &mut KeyEvent, is_numpad_key: bool, is_letter_key: bool) {
    if is_letter_key && get_key_state(enigo::Key::CapsLock) {
        key_event.modifiers.push(ControlKey::CapsLock.into());
    }
    if is_numpad_key && get_key_state(enigo::Key::NumLock) {
        key_event.modifiers.push(ControlKey::NumLock.into());
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn convert_numpad_keys(key: Key) -> Key {
    if get_key_state(enigo::Key::NumLock) {
        return key;
    }
    match key {
        Key::Kp0 => Key::Insert,
        Key::KpDecimal => Key::Delete,
        Key::Kp1 => Key::End,
        Key::Kp2 => Key::DownArrow,
        Key::Kp3 => Key::PageDown,
        Key::Kp4 => Key::LeftArrow,
        Key::Kp5 => Key::Clear,
        Key::Kp6 => Key::RightArrow,
        Key::Kp7 => Key::Home,
        Key::Kp8 => Key::UpArrow,
        Key::Kp9 => Key::PageUp,
        _ => key,
    }
}

fn update_modifiers_state(event: &Event) {
    // 用于鼠标
    let mut keys = MODIFIERS_STATE.lock().unwrap();
    match event.event_type {
        EventType::KeyPress(k) => {
            if keys.contains_key(&k) {
                keys.insert(k, true);
            }
        }
        EventType::KeyRelease(k) => {
            if keys.contains_key(&k) {
                keys.insert(k, false);
            }
        }
        _ => {}
    };
}

pub fn event_to_key_events(
    mut peer: String,
    event: &Event,
    keyboard_mode: KeyboardMode,
    _lock_modes: Option<i32>,
) -> Vec<KeyEvent> {
    peer.retain(|c| !c.is_whitespace());

    update_modifiers_state(event);

    match event.event_type {
        EventType::KeyPress(key) => {
            TO_RELEASE.lock().unwrap().insert(key, event.clone());
        }
        EventType::KeyRelease(key) => {
            TO_RELEASE.lock().unwrap().remove(&key);
        }
        _ => {}
    }

    let mut key_event = KeyEvent::new();
    key_event.mode = keyboard_mode.into();

    let mut key_events = match keyboard_mode {
        KeyboardMode::Map => map_keyboard_mode(peer.as_str(), event, key_event),
        KeyboardMode::Translate => translate_keyboard_mode(peer.as_str(), event, key_event),
        _ => {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                legacy_keyboard_mode(event, key_event)
            }
            #[cfg(any(target_os = "android", target_os = "ios"))]
            {
                Vec::new()
            }
        }
    };

    let is_numpad_key = is_numpad_key(&event);
    if keyboard_mode != KeyboardMode::Translate || is_numpad_key {
        let is_letter_key = is_letter_key_4_lock_modes(&event);
        for key_event in &mut key_events {
            if let Some(lock_modes) = _lock_modes {
                parse_add_lock_modes_modifiers(key_event, lock_modes, is_numpad_key, is_letter_key);
            } else {
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                add_lock_modes_modifiers(key_event, is_numpad_key, is_letter_key);
            }
        }
    }
    key_events
}

pub fn send_key_event(key_event: &KeyEvent) {
    #[cfg(not(feature = "flutter"))]
    if let Some(session) = CUR_SESSION.lock().unwrap().as_ref() {
        session.send_key_event(key_event);
    }

    #[cfg(feature = "flutter")]
    if let Some(session) = flutter::get_cur_session() {
        session.send_key_event(key_event);
    }
}

pub fn get_peer_platform() -> String {
    #[cfg(not(feature = "flutter"))]
    if let Some(session) = CUR_SESSION.lock().unwrap().as_ref() {
        return session.peer_platform();
    }
    #[cfg(feature = "flutter")]
    if let Some(session) = flutter::get_cur_session() {
        return session.peer_platform();
    }
    "Windows".to_string()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn legacy_keyboard_mode(event: &Event, mut key_event: KeyEvent) -> Vec<KeyEvent> {
    let mut events = Vec::new();
    // legacy 模式(0)：在本地生成字符，在对端查找键码。
    let (mut key, down_or_up) = match event.event_type {
        EventType::KeyPress(key) => (key, true),
        EventType::KeyRelease(key) => (key, false),
        _ => {
            return events;
        }
    };

    let peer = get_peer_platform();
    let is_win = peer == "Windows";
    if is_win {
        key = convert_numpad_keys(key);
    }

    let alt = get_key_state(enigo::Key::Alt);
    #[cfg(windows)]
    let ctrl = {
        let mut tmp = get_key_state(enigo::Key::Control) || get_key_state(enigo::Key::RightControl);
        unsafe {
            if IS_ALT_GR {
                if alt || key == Key::AltGr {
                    if tmp {
                        tmp = false;
                    }
                } else {
                    IS_ALT_GR = false;
                }
            }
        }
        tmp
    };
    #[cfg(not(windows))]
    let ctrl = get_key_state(enigo::Key::Control) || get_key_state(enigo::Key::RightControl);
    let shift = get_key_state(enigo::Key::Shift) || get_key_state(enigo::Key::RightShift);
    #[cfg(windows)]
    let command = crate::platform::windows::get_win_key_state();
    #[cfg(not(windows))]
    let command = get_key_state(enigo::Key::Meta);
    let control_key = match key {
        Key::Alt => Some(ControlKey::Alt),
        Key::AltGr => Some(ControlKey::RAlt),
        Key::Backspace => Some(ControlKey::Backspace),
        Key::ControlLeft => {
            // 按下 AltGr 时，会额外发送一个 VK_LCONTROL，带特殊的
            // 扫描码（bit 9 置位），这里忽略它。
            #[cfg(windows)]
            if (event.position_code >> 8) == 0xE0 {
                unsafe {
                    IS_ALT_GR = true;
                }
                return events;
            }
            Some(ControlKey::Control)
        }
        Key::ControlRight => Some(ControlKey::RControl),
        Key::DownArrow => Some(ControlKey::DownArrow),
        Key::Escape => Some(ControlKey::Escape),
        Key::F1 => Some(ControlKey::F1),
        Key::F10 => Some(ControlKey::F10),
        Key::F11 => Some(ControlKey::F11),
        Key::F12 => Some(ControlKey::F12),
        Key::F2 => Some(ControlKey::F2),
        Key::F3 => Some(ControlKey::F3),
        Key::F4 => Some(ControlKey::F4),
        Key::F5 => Some(ControlKey::F5),
        Key::F6 => Some(ControlKey::F6),
        Key::F7 => Some(ControlKey::F7),
        Key::F8 => Some(ControlKey::F8),
        Key::F9 => Some(ControlKey::F9),
        Key::LeftArrow => Some(ControlKey::LeftArrow),
        Key::MetaLeft => Some(ControlKey::Meta),
        Key::MetaRight => Some(ControlKey::RWin),
        Key::Return => Some(ControlKey::Return),
        Key::RightArrow => Some(ControlKey::RightArrow),
        Key::ShiftLeft => Some(ControlKey::Shift),
        Key::ShiftRight => Some(ControlKey::RShift),
        Key::Space => Some(ControlKey::Space),
        Key::Tab => Some(ControlKey::Tab),
        Key::UpArrow => Some(ControlKey::UpArrow),
        Key::Delete => {
            if is_win && ctrl && alt {
                client::ctrl_alt_del();
                return events;
            }
            Some(ControlKey::Delete)
        }
        Key::Apps => Some(ControlKey::Apps),
        Key::Cancel => Some(ControlKey::Cancel),
        Key::Clear => Some(ControlKey::Clear),
        Key::Kana => Some(ControlKey::Kana),
        Key::Hangul => Some(ControlKey::Hangul),
        Key::Junja => Some(ControlKey::Junja),
        Key::Final => Some(ControlKey::Final),
        Key::Hanja => Some(ControlKey::Hanja),
        Key::Hanji => Some(ControlKey::Hanja),
        Key::Lang2 => Some(ControlKey::Convert),
        Key::Print => Some(ControlKey::Print),
        Key::Select => Some(ControlKey::Select),
        Key::Execute => Some(ControlKey::Execute),
        Key::PrintScreen => Some(ControlKey::Snapshot),
        Key::Help => Some(ControlKey::Help),
        Key::Sleep => Some(ControlKey::Sleep),
        Key::Separator => Some(ControlKey::Separator),
        Key::KpReturn => Some(ControlKey::NumpadEnter),
        Key::Kp0 => Some(ControlKey::Numpad0),
        Key::Kp1 => Some(ControlKey::Numpad1),
        Key::Kp2 => Some(ControlKey::Numpad2),
        Key::Kp3 => Some(ControlKey::Numpad3),
        Key::Kp4 => Some(ControlKey::Numpad4),
        Key::Kp5 => Some(ControlKey::Numpad5),
        Key::Kp6 => Some(ControlKey::Numpad6),
        Key::Kp7 => Some(ControlKey::Numpad7),
        Key::Kp8 => Some(ControlKey::Numpad8),
        Key::Kp9 => Some(ControlKey::Numpad9),
        Key::KpDivide => Some(ControlKey::Divide),
        Key::KpMultiply => Some(ControlKey::Multiply),
        Key::KpDecimal => Some(ControlKey::Decimal),
        Key::KpMinus => Some(ControlKey::Subtract),
        Key::KpPlus => Some(ControlKey::Add),
        Key::CapsLock | Key::NumLock | Key::ScrollLock => {
            return events;
        }
        Key::Home => Some(ControlKey::Home),
        Key::End => Some(ControlKey::End),
        Key::Insert => Some(ControlKey::Insert),
        Key::PageUp => Some(ControlKey::PageUp),
        Key::PageDown => Some(ControlKey::PageDown),
        Key::Pause => Some(ControlKey::Pause),
        _ => None,
    };
    if let Some(k) = control_key {
        key_event.set_control_key(k);
    } else {
        let name = event
            .unicode
            .as_ref()
            .and_then(|unicode| unicode.name.clone());
        let mut chr = match &name {
            Some(ref s) => {
                if s.len() <= 2 {
                    // 排除中文字符
                    s.chars().next().unwrap_or('\0')
                } else {
                    '\0'
                }
            }
            _ => '\0',
        };
        if chr == '·' {
            // 针对中文的特殊处理
            chr = '`';
        }
        if chr == '\0' {
            chr = match key {
                Key::Num1 => '1',
                Key::Num2 => '2',
                Key::Num3 => '3',
                Key::Num4 => '4',
                Key::Num5 => '5',
                Key::Num6 => '6',
                Key::Num7 => '7',
                Key::Num8 => '8',
                Key::Num9 => '9',
                Key::Num0 => '0',
                Key::KeyA => 'a',
                Key::KeyB => 'b',
                Key::KeyC => 'c',
                Key::KeyD => 'd',
                Key::KeyE => 'e',
                Key::KeyF => 'f',
                Key::KeyG => 'g',
                Key::KeyH => 'h',
                Key::KeyI => 'i',
                Key::KeyJ => 'j',
                Key::KeyK => 'k',
                Key::KeyL => 'l',
                Key::KeyM => 'm',
                Key::KeyN => 'n',
                Key::KeyO => 'o',
                Key::KeyP => 'p',
                Key::KeyQ => 'q',
                Key::KeyR => 'r',
                Key::KeyS => 's',
                Key::KeyT => 't',
                Key::KeyU => 'u',
                Key::KeyV => 'v',
                Key::KeyW => 'w',
                Key::KeyX => 'x',
                Key::KeyY => 'y',
                Key::KeyZ => 'z',
                Key::Comma => ',',
                Key::Dot => '.',
                Key::SemiColon => ';',
                Key::Quote => '\'',
                Key::LeftBracket => '[',
                Key::RightBracket => ']',
                Key::Slash => '/',
                Key::BackSlash => '\\',
                Key::Minus => '-',
                Key::Equal => '=',
                Key::BackQuote => '`',
                _ => '\0',
            }
        }
        if chr != '\0' {
            if chr == 'l' && is_win && command {
                client::lock_screen();
                return events;
            }
            key_event.set_chr(chr as _);
        } else {
            log::error!("Unknown key {:?}", &event);
            return events;
        }
    }
    let (alt, ctrl, shift, command) = client::get_modifiers_state(alt, ctrl, shift, command);
    client::legacy_modifiers(&mut key_event, alt, ctrl, shift, command);

    if down_or_up == true {
        key_event.down = true;
    }
    events.push(key_event);
    events
}

#[inline]
pub fn map_keyboard_mode(_peer: &str, event: &Event, key_event: KeyEvent) -> Vec<KeyEvent> {
    if let Some(evt) = windows_peer_special_key(_peer, event) {
        return vec![evt];
    }

    _map_keyboard_mode(_peer, event, key_event)
        .map(|e| vec![e])
        .unwrap_or_default()
}

fn windows_peer_special_key(peer: &str, event: &Event) -> Option<KeyEvent> {
    if peer != OS_LOWER_WINDOWS {
        return None;
    }

    let (key, down) = match event.event_type {
        EventType::KeyPress(key) => (key, true),
        EventType::KeyRelease(key) => (key, false),
        _ => return None,
    };

    // 目前对 Windows 对端只处理 `Pause`。
    // Windows 没有 `Pause` 的常规扫描码，因此把它作为旧式控制键发送。
    #[cfg(target_os = "windows")]
    let is_pause = {
        // Windows 的扫描码看起来可能像 `NumLock`；VK_PAUSE 可以区分它。
        let pause_vk_code = rdev::win_code_from_key(Key::Pause);
        key == Key::Pause || pause_vk_code == Some(event.platform_code as _)
    };
    #[cfg(not(target_os = "windows"))]
    let is_pause = key == Key::Pause;
    if !is_pause {
        return None;
    }

    let mut key_event = KeyEvent::new();
    key_event.mode = KeyboardMode::Legacy.into();
    key_event.down = down;
    key_event.set_control_key(ControlKey::Pause);
    let (alt, ctrl, shift, command) = client::get_modifiers_state(false, false, false, false);
    client::legacy_modifiers(&mut key_event, alt, ctrl, shift, command);
    Some(key_event)
}

fn _map_keyboard_mode(_peer: &str, event: &Event, mut key_event: KeyEvent) -> Option<KeyEvent> {
    match event.event_type {
        EventType::KeyPress(..) => {
            key_event.down = true;
        }
        EventType::KeyRelease(..) => {
            key_event.down = false;
        }
        _ => return None,
    };

    #[cfg(target_os = "windows")]
    let keycode = match _peer {
        OS_LOWER_WINDOWS => {
            // https://github.com/rustdesk/rustdesk/issues/1371
            // 过滤掉大于 255 且高字节不是 0xE0 的扫描码。
            if event.position_code > 255 && (event.position_code >> 8) != 0xE0 {
                return None;
            }
            event.position_code
        }
        OS_LOWER_MACOS => {
            if hbb_common::config::LocalConfig::get_kb_layout_type() == "ISO" {
                rdev::win_scancode_to_macos_iso_code(event.position_code)?
            } else {
                rdev::win_scancode_to_macos_code(event.position_code)?
            }
        }
        OS_LOWER_ANDROID => rdev::win_scancode_to_android_key_code(event.position_code)?,
        _ => rdev::win_scancode_to_linux_code(event.position_code)?,
    };
    #[cfg(target_os = "macos")]
    let keycode = match _peer {
        OS_LOWER_WINDOWS => rdev::macos_code_to_win_scancode(event.platform_code as _)?,
        OS_LOWER_MACOS => event.platform_code as _,
        OS_LOWER_ANDROID => rdev::macos_code_to_android_key_code(event.platform_code as _)?,
        _ => rdev::macos_code_to_linux_code(event.platform_code as _)?,
    };
    #[cfg(target_os = "linux")]
    let keycode = match _peer {
        OS_LOWER_WINDOWS => rdev::linux_code_to_win_scancode(event.position_code as _)?,
        OS_LOWER_MACOS => {
            if hbb_common::config::LocalConfig::get_kb_layout_type() == "ISO" {
                rdev::linux_code_to_macos_iso_code(event.position_code as _)?
            } else {
                rdev::linux_code_to_macos_code(event.position_code as _)?
            }
        }
        OS_LOWER_ANDROID => rdev::linux_code_to_android_key_code(event.position_code as _)?,
        _ => event.position_code as _,
    };
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let keycode = match _peer {
        OS_LOWER_WINDOWS => rdev::usb_hid_code_to_win_scancode(event.usb_hid as _)?,
        OS_LOWER_LINUX => rdev::usb_hid_code_to_linux_code(event.usb_hid as _)?,
        OS_LOWER_MACOS => {
            if hbb_common::config::LocalConfig::get_kb_layout_type() == "ISO" {
                rdev::usb_hid_code_to_macos_iso_code(event.usb_hid as _)?
            } else {
                rdev::usb_hid_code_to_macos_code(event.usb_hid as _)?
            }
        }
        OS_LOWER_ANDROID => rdev::usb_hid_code_to_android_key_code(event.usb_hid as _)?,
        _ => event.usb_hid as _,
    };
    key_event.set_chr(keycode as _);
    Some(key_event)
}

#[cfg(not(any(target_os = "ios")))]
fn try_fill_unicode(_peer: &str, event: &Event, key_event: &KeyEvent, events: &mut Vec<KeyEvent>) {
    match &event.unicode {
        Some(unicode_info) => {
            if let Some(name) = &unicode_info.name {
                if name.len() > 0 {
                    let mut evt = key_event.clone();
                    evt.set_seq(name.to_string());
                    evt.down = true;
                    events.push(evt);
                }
            }
        }
        None =>
        {
            #[cfg(target_os = "windows")]
            if _peer == OS_LOWER_LINUX {
                if is_hot_key_modifiers_down() && unsafe { !IS_0X021D_DOWN } {
                    if let Some(chr) = get_char_from_vk(event.platform_code as u32) {
                        let mut evt = key_event.clone();
                        evt.set_seq(chr.to_string());
                        evt.down = true;
                        events.push(evt);
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn try_fill_win2win_hotkey(
    peer: &str,
    event: &Event,
    key_event: &KeyEvent,
    events: &mut Vec<KeyEvent>,
) {
    if peer == OS_LOWER_WINDOWS && is_hot_key_modifiers_down() && unsafe { !IS_0X021D_DOWN } {
        let mut down = false;
        let win2win_hotkey = match event.event_type {
            EventType::KeyPress(..) => {
                down = true;
                if let Some(unicode) = get_unicode_from_vk(event.platform_code as u32) {
                    Some((unicode as u32 & 0x0000FFFF) | (event.platform_code << 16))
                } else {
                    None
                }
            }
            EventType::KeyRelease(..) => Some(event.platform_code << 16),
            _ => None,
        };
        if let Some(code) = win2win_hotkey {
            let mut evt = key_event.clone();
            evt.set_win2win_hotkey(code);
            evt.down = down;
            events.push(evt);
        }
    }
}

#[cfg(target_os = "windows")]
fn is_hot_key_modifiers_down() -> bool {
    if rdev::get_modifier(Key::ControlLeft) || rdev::get_modifier(Key::ControlRight) {
        return true;
    }
    if rdev::get_modifier(Key::Alt) || rdev::get_modifier(Key::AltGr) {
        return true;
    }
    if rdev::get_modifier(Key::MetaLeft) || rdev::get_modifier(Key::MetaRight) {
        return true;
    }
    return false;
}

#[inline]
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn is_altgr(event: &Event) -> bool {
    #[cfg(target_os = "linux")]
    if event.platform_code == 0xFE03 {
        true
    } else {
        false
    }

    #[cfg(target_os = "windows")]
    if unsafe { IS_0X021D_DOWN } && event.position_code == 0xE038 {
        true
    } else {
        false
    }
}

#[inline]
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn is_press(event: &Event) -> bool {
    matches!(event.event_type, EventType::KeyPress(_))
}

// https://github.com/rustdesk/rustdesk/wiki/FAQ#keyboard-translation-modes
pub fn translate_keyboard_mode(peer: &str, event: &Event, key_event: KeyEvent) -> Vec<KeyEvent> {
    let mut events: Vec<KeyEvent> = Vec::new();

    if let Some(evt) = windows_peer_special_key(peer, event) {
        events.push(evt);
        return events;
    }

    if let Some(unicode_info) = &event.unicode {
        if unicode_info.is_dead {
            #[cfg(target_os = "macos")]
            if peer != OS_LOWER_MACOS && unsafe { IS_LEFT_OPTION_DOWN } {
                // 尝试清除 dead key 状态
                // rdev::clear_dead_key_state();
            } else {
                return events;
            }
            #[cfg(not(target_os = "macos"))]
            return events;
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if is_numpad_key(&event) {
        events.append(&mut map_keyboard_mode(peer, event, key_event));
        return events;
    }

    #[cfg(target_os = "macos")]
    // 忽略右 option 键
    if event.platform_code == rdev::kVK_RightOption as u32 {
        return events;
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if is_altgr(event) {
        return events;
    }

    #[cfg(target_os = "windows")]
    if event.position_code == 0x021D {
        return events;
    }

    #[cfg(target_os = "windows")]
    try_fill_win2win_hotkey(peer, event, &key_event, &mut events);

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if events.is_empty() && is_press(event) {
        try_fill_unicode(peer, event, &key_event, &mut events);
    }

    // 如果 AltGr 处于按下状态，除 unicode 之外无需发送其他事件。
    #[cfg(target_os = "windows")]
    unsafe {
        if IS_0X021D_DOWN {
            return events;
        }
    }

    #[cfg(target_os = "macos")]
    if !unsafe { IS_LEFT_OPTION_DOWN } {
        try_fill_unicode(peer, event, &key_event, &mut events);
    }

    if events.is_empty() {
        events.append(&mut map_keyboard_mode(peer, event, key_event));
    }
    events
}

#[cfg(not(any(target_os = "ios")))]
pub fn keycode_to_rdev_key(keycode: u32) -> Key {
    #[cfg(target_os = "windows")]
    return rdev::win_key_from_scancode(keycode);
    #[cfg(any(target_os = "linux"))]
    return rdev::linux_key_from_code(keycode);
    #[cfg(any(target_os = "android"))]
    return rdev::android_key_from_code(keycode);
    #[cfg(target_os = "macos")]
    return rdev::macos_key_from_code(keycode.try_into().unwrap_or_default());
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod input_source {
    #[cfg(target_os = "macos")]
    use hbb_common::log;
    use hbb_common::SessionID;

    use crate::ui_interface::{get_local_option, set_local_option};

    pub const CONFIG_OPTION_INPUT_SOURCE: &str = "input-source";
    // rdev 抓取模式
    pub const CONFIG_INPUT_SOURCE_1: &str = "Input source 1";
    pub const CONFIG_INPUT_SOURCE_1_TIP: &str = "input_source_1_tip";
    // flutter 抓取模式
    pub const CONFIG_INPUT_SOURCE_2: &str = "Input source 2";
    pub const CONFIG_INPUT_SOURCE_2_TIP: &str = "input_source_2_tip";

    pub const CONFIG_INPUT_SOURCE_DEFAULT: &str = CONFIG_INPUT_SOURCE_1;

    pub fn init_input_source() {
        #[cfg(target_os = "linux")]
        if !crate::platform::linux::is_x11() {
            // 如果从 X11 切换到 Wayland，将不会启动抓取循环。
            // 不要在这里修改配置。
            return;
        }
        #[cfg(target_os = "macos")]
        if !crate::platform::macos::is_can_input_monitoring(false) {
            log::error!("init_input_source, is_can_input_monitoring() false");
            set_local_option(
                CONFIG_OPTION_INPUT_SOURCE.to_string(),
                CONFIG_INPUT_SOURCE_2.to_string(),
            );
            return;
        }
        let cur_input_source = get_cur_session_input_source();
        if cur_input_source == CONFIG_INPUT_SOURCE_1 {
            super::IS_RDEV_ENABLED.store(true, super::Ordering::SeqCst);
        }
        super::client::start_grab_loop();
    }

    pub fn change_input_source(session_id: SessionID, input_source: String) {
        let cur_input_source = get_cur_session_input_source();
        if cur_input_source == input_source {
            return;
        }
        if input_source == CONFIG_INPUT_SOURCE_1 {
            #[cfg(target_os = "macos")]
            if !crate::platform::macos::is_can_input_monitoring(false) {
                log::error!("change_input_source, is_can_input_monitoring() false");
                return;
            }
            // 多次启动抓取循环也没问题。
            super::client::start_grab_loop();
            super::IS_RDEV_ENABLED.store(true, super::Ordering::SeqCst);
            crate::flutter_ffi::session_enter_or_leave(session_id, true);
        } else if input_source == CONFIG_INPUT_SOURCE_2 {
            // 无需停止抓取循环。
            crate::flutter_ffi::session_enter_or_leave(session_id, false);
            super::IS_RDEV_ENABLED.store(false, super::Ordering::SeqCst);
        }
        set_local_option(CONFIG_OPTION_INPUT_SOURCE.to_string(), input_source);
    }

    #[inline]
    pub fn get_cur_session_input_source() -> String {
        #[cfg(target_os = "linux")]
        if !crate::platform::linux::is_x11() {
            return CONFIG_INPUT_SOURCE_2.to_string();
        }
        let input_source = get_local_option(CONFIG_OPTION_INPUT_SOURCE.to_string());
        if input_source.is_empty() {
            CONFIG_INPUT_SOURCE_DEFAULT.to_string()
        } else {
            input_source
        }
    }

    #[inline]
    pub fn get_supported_input_source() -> Vec<(String, String)> {
        #[cfg(target_os = "linux")]
        if !crate::platform::linux::is_x11() {
            return vec![(
                CONFIG_INPUT_SOURCE_2.to_string(),
                CONFIG_INPUT_SOURCE_2_TIP.to_string(),
            )];
        }
        vec![
            (
                CONFIG_INPUT_SOURCE_1.to_string(),
                CONFIG_INPUT_SOURCE_1_TIP.to_string(),
            ),
            (
                CONFIG_INPUT_SOURCE_2.to_string(),
                CONFIG_INPUT_SOURCE_2_TIP.to_string(),
            ),
        ]
    }
}
