use super::input_service::set_clipboard_for_paste_sync;
use crate::uinput::service::{can_input_via_keysym, char_to_keysym, map_key};
use dbus::{blocking::SyncConnection, Path};
use enigo::{Key, KeyboardControllable, MouseButton, MouseControllable};
use hbb_common::{log, ResultType};
use scrap::wayland::pipewire::{get_portal, PwStreamInfo};
use scrap::wayland::remote_desktop_portal::OrgFreedesktopPortalRemoteDesktop as remote_desktop_portal;
use std::collections::HashMap;
use std::sync::Arc;

pub mod client {
    use base::platform::linux::{DISPLAY_DESKTOP_KDE, XDG_CURRENT_DESKTOP};

    use super::*;

    const EVDEV_MOUSE_LEFT: i32 = 272;
    const EVDEV_MOUSE_RIGHT: i32 = 273;
    const EVDEV_MOUSE_MIDDLE: i32 = 274;

    const PRESSED_DOWN_STATE: u32 = 1;
    const PRESSED_UP_STATE: u32 = 0;

    /// RDP 输入的修饰键状态跟踪。
    /// Portal API 没有提供查询按键状态的方法，因此我们自行跟踪。
    #[derive(Default)]
    struct ModifierState {
        shift_left: bool,
        shift_right: bool,
        ctrl_left: bool,
        ctrl_right: bool,
        alt_left: bool,
        alt_right: bool,
        meta_left: bool,
        meta_right: bool,
    }

    impl ModifierState {
        fn update(&mut self, key: &Key, down: bool) {
            match key {
                Key::Shift => self.shift_left = down,
                Key::RightShift => self.shift_right = down,
                Key::Control => self.ctrl_left = down,
                Key::RightControl => self.ctrl_right = down,
                Key::Alt => self.alt_left = down,
                Key::RightAlt => self.alt_right = down,
                Key::Meta | Key::Super | Key::Windows | Key::Command => self.meta_left = down,
                Key::RWin => self.meta_right = down,
                // 处理修饰键的原始键码（Linux evdev 编码 + 8）
                // 在 translate 模式下，修饰键可能以带原始键码的 Chr 事件形式发送。
                // +8 偏移用于将 evdev 编码转换为 X11/XKB 键码。
                Key::Raw(code) => {
                    const EVDEV_OFFSET: u16 = 8;
                    const KEY_LEFTSHIFT: u16 = evdev::Key::KEY_LEFTSHIFT.code() + EVDEV_OFFSET;
                    const KEY_RIGHTSHIFT: u16 = evdev::Key::KEY_RIGHTSHIFT.code() + EVDEV_OFFSET;
                    const KEY_LEFTCTRL: u16 = evdev::Key::KEY_LEFTCTRL.code() + EVDEV_OFFSET;
                    const KEY_RIGHTCTRL: u16 = evdev::Key::KEY_RIGHTCTRL.code() + EVDEV_OFFSET;
                    const KEY_LEFTALT: u16 = evdev::Key::KEY_LEFTALT.code() + EVDEV_OFFSET;
                    const KEY_RIGHTALT: u16 = evdev::Key::KEY_RIGHTALT.code() + EVDEV_OFFSET;
                    const KEY_LEFTMETA: u16 = evdev::Key::KEY_LEFTMETA.code() + EVDEV_OFFSET;
                    const KEY_RIGHTMETA: u16 = evdev::Key::KEY_RIGHTMETA.code() + EVDEV_OFFSET;
                    match *code {
                        KEY_LEFTSHIFT => self.shift_left = down,
                        KEY_RIGHTSHIFT => self.shift_right = down,
                        KEY_LEFTCTRL => self.ctrl_left = down,
                        KEY_RIGHTCTRL => self.ctrl_right = down,
                        KEY_LEFTALT => self.alt_left = down,
                        KEY_RIGHTALT => self.alt_right = down,
                        KEY_LEFTMETA => self.meta_left = down,
                        KEY_RIGHTMETA => self.meta_right = down,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    pub struct RdpInputKeyboard {
        conn: Arc<SyncConnection>,
        session: Path<'static>,
        modifier_state: ModifierState,
    }

    impl RdpInputKeyboard {
        pub fn new(conn: Arc<SyncConnection>, session: Path<'static>) -> ResultType<Self> {
            Ok(Self {
                conn,
                session,
                modifier_state: ModifierState::default(),
            })
        }
    }

    impl KeyboardControllable for RdpInputKeyboard {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn get_key_state(&mut self, key: Key) -> bool {
            // 对支持的按键使用已跟踪的修饰键状态
            match key {
                Key::Shift => self.modifier_state.shift_left,
                Key::RightShift => self.modifier_state.shift_right,
                Key::Control => self.modifier_state.ctrl_left,
                Key::RightControl => self.modifier_state.ctrl_right,
                Key::Alt => self.modifier_state.alt_left,
                Key::RightAlt => self.modifier_state.alt_right,
                Key::Meta | Key::Super | Key::Windows | Key::Command => {
                    self.modifier_state.meta_left
                }
                Key::RWin => self.modifier_state.meta_right,
                _ => false,
            }
        }

        fn key_sequence(&mut self, s: &str) {
            if s.is_empty() {
                return;
            }

            // 保持顺序确定：
            // - 纯 ASCII 可打印字符：通过 Portal keysym 发送
            // - 存在任何非 ASCII 字符（包括 ASCII 与非 ASCII 混合）：整条
            //   序列通过剪贴板作为一次原子粘贴发送
            let ascii_only = s.chars().all(|c| {
                let keysym = char_to_keysym(c);
                can_input_via_keysym(c, keysym)
            });
            if !ascii_only {
                input_text_via_clipboard(s, self.conn.clone(), &self.session);
                return;
            }

            for c in s.chars() {
                let keysym = char_to_keysym(c);
                // ASCII 字符：使用 keysym
                if can_input_via_keysym(c, keysym) {
                    if let Err(e) = send_keysym(keysym, true, self.conn.clone(), &self.session) {
                        log::error!("Failed to send keysym down: {:?}", e);
                    }
                    if let Err(e) = send_keysym(keysym, false, self.conn.clone(), &self.session) {
                        log::error!("Failed to send keysym up: {:?}", e);
                    }
                }
            }
        }

        fn key_down(&mut self, key: Key) -> enigo::ResultType {
            if let Key::Layout(chr) = key {
                let keysym = char_to_keysym(chr);
                // ASCII 字符：使用 keysym
                if can_input_via_keysym(chr, keysym) {
                    send_keysym(keysym, true, self.conn.clone(), &self.session)?;
                } else {
                    // 非 ASCII：使用剪贴板（在 key_down 中完成整个按键）
                    input_text_via_clipboard(&chr.to_string(), self.conn.clone(), &self.session);
                }
            } else {
                handle_key(true, key.clone(), self.conn.clone(), &self.session)?;
                // 仅在发送成功后更新修饰键状态 ——
                // 如果 handle_key 失败，我们不希望残留的 "pressed" 状态
                // 影响后续的按键事件判断。
                self.modifier_state.update(&key, true);
            }
            Ok(())
        }

        fn key_up(&mut self, key: Key) {
            // 与 key_down 有意保持不对称：先更新状态，再发送。
            // 释放时，即使下面的发送失败，我们也总是标记为已释放，
            // 以避免跟踪器中出现永久卡住的修饰键状态。这个权衡
            // （跟踪器显示 "released" 而操作系统可能仍认为其处于按下状态）是可以接受的
            // 因为这类失败很少发生，且后续事件会重新同步。
            self.modifier_state.update(&key, false);

            if let Key::Layout(chr) = key {
                // ASCII 字符：如果在 key_down 时也发送过 keysym，则发送 keysym up
                let keysym = char_to_keysym(chr);
                if can_input_via_keysym(chr, keysym) {
                    if let Err(e) = send_keysym(keysym, false, self.conn.clone(), &self.session) {
                        log::error!("Failed to send keysym up: {:?}", e);
                    }
                }
                // 非 ASCII：已在 key_down 中通过剪贴板粘贴完整处理，
                // 无需对应的释放操作（剪贴板粘贴是原子操作）
            } else {
                if let Err(e) = handle_key(false, key, self.conn.clone(), &self.session) {
                    log::error!("Failed to handle key up: {:?}", e);
                }
            }
        }

        fn key_click(&mut self, key: Key) {
            if let Key::Layout(chr) = key {
                let keysym = char_to_keysym(chr);
                // ASCII 字符：使用 keysym
                if can_input_via_keysym(chr, keysym) {
                    if let Err(e) = send_keysym(keysym, true, self.conn.clone(), &self.session) {
                        log::error!("Failed to send keysym down: {:?}", e);
                    }
                    if let Err(e) = send_keysym(keysym, false, self.conn.clone(), &self.session) {
                        log::error!("Failed to send keysym up: {:?}", e);
                    }
                } else {
                    // 非 ASCII：使用剪贴板
                    input_text_via_clipboard(&chr.to_string(), self.conn.clone(), &self.session);
                }
            } else {
                if let Err(e) = handle_key(true, key.clone(), self.conn.clone(), &self.session) {
                    log::error!("Failed to handle key down: {:?}", e);
                } else {
                    // 仅当按下事件确实被送达时才标记修饰键为按下
                    self.modifier_state.update(&key, true);
                }
                // 始终标记为已释放，以避免修饰键卡住的状态
                self.modifier_state.update(&key, false);
                if let Err(e) = handle_key(false, key, self.conn.clone(), &self.session) {
                    log::error!("Failed to handle key up: {:?}", e);
                }
            }
        }
    }

    /// 通过剪贴板 + Shift+Insert 输入文本。
    /// Shift+Insert 比 Ctrl+V 更通用，在 GUI 程序和终端中都能工作。
    ///
    /// 注意：粘贴后不会恢复剪贴板内容 —— 原因参见 `set_clipboard_for_paste_sync`。
    fn input_text_via_clipboard(text: &str, conn: Arc<SyncConnection>, session: &Path<'static>) {
        if text.is_empty() {
            return;
        }
        if !set_clipboard_for_paste_sync(text) {
            return;
        }

        let portal = get_portal(&conn);
        let shift_keycode = evdev::Key::KEY_LEFTSHIFT.code() as i32;
        let insert_keycode = evdev::Key::KEY_INSERT.code() as i32;

        // 发送 Shift+Insert（通用粘贴快捷键）
        if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
            &portal,
            session,
            HashMap::new(),
            shift_keycode,
            PRESSED_DOWN_STATE,
        ) {
            log::error!("input_text_via_clipboard: failed to press Shift: {:?}", e);
            return;
        }

        // 按下 Insert
        if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
            &portal,
            session,
            HashMap::new(),
            insert_keycode,
            PRESSED_DOWN_STATE,
        ) {
            log::error!("input_text_via_clipboard: failed to press Insert: {:?}", e);
            // 仍尝试释放 Shift。
            // 注意：剪贴板已由 set_clipboard_for_paste_sync 设置，但粘贴
            // 并未发生。我们不尝试恢复之前的剪贴板内容，
            // 因为在 Wayland 上读取剪贴板需要焦点/权限。
            let _ = remote_desktop_portal::notify_keyboard_keycode(
                &portal,
                session,
                HashMap::new(),
                shift_keycode,
                PRESSED_UP_STATE,
            );
            return;
        }

        // 释放 Insert
        if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
            &portal,
            session,
            HashMap::new(),
            insert_keycode,
            PRESSED_UP_STATE,
        ) {
            log::error!(
                "input_text_via_clipboard: failed to release Insert: {:?}",
                e
            );
        }

        // 释放 Shift
        if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
            &portal,
            session,
            HashMap::new(),
            shift_keycode,
            PRESSED_UP_STATE,
        ) {
            log::error!("input_text_via_clipboard: failed to release Shift: {:?}", e);
        }
    }

    fn desktop_is_niri(desktop: &str) -> bool {
        desktop
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("niri"))
    }

    lazy_static::lazy_static! {
        static ref SHOULD_SCALE_POINTER_COORDINATES: bool =
            std::env::var(XDG_CURRENT_DESKTOP)
                .map(|desktop| desktop == DISPLAY_DESKTOP_KDE || desktop_is_niri(&desktop))
                .unwrap_or(false);
    }

    pub struct RdpInputMouse {
        conn: Arc<SyncConnection>,
        session: Path<'static>,
        stream: PwStreamInfo,
        resolution: (usize, usize),
        scale: Option<f64>,
        position: (f64, f64),
    }

    impl RdpInputMouse {
        pub fn new(
            conn: Arc<SyncConnection>,
            session: Path<'static>,
            stream: PwStreamInfo,
            resolution: (usize, usize),
        ) -> ResultType<Self> {
            // https://github.com/rustdesk/rustdesk/pull/9019#issuecomment-2295252388
            // Ubuntu 24.04（Gnome 46）上的 Gnome 中 Rdp 输入可能存在 bug
            //
            // 例如：分辨率 800x600，分数缩放 200%（逻辑尺寸：400x300）
            // https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.impl.portal.RemoteDesktop.html#:~:text=new%20pointer%20position-,in%20the%20streams%20logical%20coordinate%20space,-.
            // 那么 `mouse_move_to()` 和 `mouse_move_relative()` 中的 (x,y) 应按逻辑尺寸(stream.get_size())缩放，其范围从 (0,0) 到 (400,300)。
            // 在 Ubuntu 24.04（Gnome 46）中，(x,y) 被限制在 (0,0) 到 (400,300) 之间，但屏幕上实际的范围是：
            // 逻辑坐标从 (0,0) 到 (200x150)。
            // 即物理坐标从 (0,0) 到 (400,300)。
            let scale = if *SHOULD_SCALE_POINTER_COORDINATES {
                if resolution.0 == 0 || stream.get_size().0 == 0 {
                    Some(1.0f64)
                } else {
                    Some(resolution.0 as f64 / stream.get_size().0 as f64)
                }
            } else {
                None
            };
            let pos = stream.get_position();
            Ok(Self {
                conn,
                session,
                stream,
                resolution,
                scale,
                position: (pos.0 as f64, pos.1 as f64),
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::desktop_is_niri;

        #[test]
        fn detects_niri_in_desktop_list() {
            assert!(desktop_is_niri("niri"));
            assert!(desktop_is_niri("NIRI"));
            assert!(desktop_is_niri("GNOME:niri"));
            assert!(!desktop_is_niri("GNOME"));
        }
    }

    impl MouseControllable for RdpInputMouse {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn mouse_move_to(&mut self, x: i32, y: i32) {
            let x = if let Some(s) = self.scale {
                x as f64 / s
            } else {
                x as f64
            };
            let y = if let Some(s) = self.scale {
                y as f64 / s
            } else {
                y as f64
            };
            let x = x - self.position.0;
            let y = y - self.position.1;
            let portal = get_portal(&self.conn);
            let _ = remote_desktop_portal::notify_pointer_motion_absolute(
                &portal,
                &self.session,
                HashMap::new(),
                self.stream.path as u32,
                x,
                y,
            );
        }
        fn mouse_move_relative(&mut self, x: i32, y: i32) {
            let x = if let Some(s) = self.scale {
                x as f64 / s
            } else {
                x as f64
            };
            let y = if let Some(s) = self.scale {
                y as f64 / s
            } else {
                y as f64
            };
            let portal = get_portal(&self.conn);
            let _ = remote_desktop_portal::notify_pointer_motion(
                &portal,
                &self.session,
                HashMap::new(),
                x,
                y,
            );
        }
        fn mouse_down(&mut self, button: MouseButton) -> enigo::ResultType {
            handle_mouse(true, button, self.conn.clone(), &self.session);
            Ok(())
        }
        fn mouse_up(&mut self, button: MouseButton) {
            handle_mouse(false, button, self.conn.clone(), &self.session);
        }
        fn mouse_click(&mut self, button: MouseButton) {
            handle_mouse(true, button, self.conn.clone(), &self.session);
            handle_mouse(false, button, self.conn.clone(), &self.session);
        }
        fn mouse_scroll_x(&mut self, length: i32) {
            let portal = get_portal(&self.conn);
            let _ = remote_desktop_portal::notify_pointer_axis(
                &portal,
                &self.session,
                HashMap::new(),
                length as f64,
                0 as f64,
            );
        }
        fn mouse_scroll_y(&mut self, length: i32) {
            let portal = get_portal(&self.conn);
            let _ = remote_desktop_portal::notify_pointer_axis(
                &portal,
                &self.session,
                HashMap::new(),
                0 as f64,
                length as f64,
            );
        }
    }

    /// 通过 RemoteDesktop portal 发送 keysym。
    fn send_keysym(
        keysym: i32,
        down: bool,
        conn: Arc<SyncConnection>,
        session: &Path<'static>,
    ) -> ResultType<()> {
        let state: u32 = if down {
            PRESSED_DOWN_STATE
        } else {
            PRESSED_UP_STATE
        };
        let portal = get_portal(&conn);
        log::trace!(
            "send_keysym: calling notify_keyboard_keysym, keysym={:#x}, state={}",
            keysym,
            state
        );
        match remote_desktop_portal::notify_keyboard_keysym(
            &portal,
            session,
            HashMap::new(),
            keysym,
            state,
        ) {
            Ok(_) => {
                log::trace!("send_keysym: notify_keyboard_keysym succeeded");
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn get_raw_evdev_keycode(key: u16) -> i32 {
        // 8 是 xkb 与 evdev 之间的偏移量
        let mut key = key as i32 - 8;
        // 修复 right_meta 按键
        if key == 126 {
            key = 125;
        }
        key
    }

    fn handle_key(
        down: bool,
        key: Key,
        conn: Arc<SyncConnection>,
        session: &Path<'static>,
    ) -> ResultType<()> {
        let state: u32 = if down {
            PRESSED_DOWN_STATE
        } else {
            PRESSED_UP_STATE
        };
        let portal = get_portal(&conn);
        match key {
            Key::Raw(key) => {
                let key = get_raw_evdev_keycode(key);
                remote_desktop_portal::notify_keyboard_keycode(
                    &portal,
                    &session,
                    HashMap::new(),
                    key,
                    state,
                )?;
            }
            _ => {
                if let Ok((key, is_shift)) = map_key(&key) {
                    let shift_keycode = evdev::Key::KEY_LEFTSHIFT.code() as i32;
                    if down {
                        // 按下：先按下 Shift，再按下按键
                        if is_shift {
                            if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
                                &portal,
                                &session,
                                HashMap::new(),
                                shift_keycode,
                                state,
                            ) {
                                log::error!("handle_key: failed to press Shift: {:?}", e);
                                return Err(e.into());
                            }
                        }
                        if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
                            &portal,
                            &session,
                            HashMap::new(),
                            key.code() as i32,
                            state,
                        ) {
                            log::error!("handle_key: failed to press key: {:?}", e);
                            // 尽力而为：如果 Shift 处于按下状态则释放它
                            if is_shift {
                                if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
                                    &portal,
                                    &session,
                                    HashMap::new(),
                                    shift_keycode,
                                    PRESSED_UP_STATE,
                                ) {
                                    log::warn!(
                                        "handle_key: best-effort Shift release also failed: {:?}",
                                        e
                                    );
                                }
                            }
                            return Err(e.into());
                        }
                    } else {
                        // 释放：先释放按键，再释放 Shift
                        if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
                            &portal,
                            &session,
                            HashMap::new(),
                            key.code() as i32,
                            PRESSED_UP_STATE,
                        ) {
                            log::error!("handle_key: failed to release key: {:?}", e);
                            // 尽力而为：仍尝试释放 Shift
                            if is_shift {
                                if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
                                    &portal,
                                    &session,
                                    HashMap::new(),
                                    shift_keycode,
                                    PRESSED_UP_STATE,
                                ) {
                                    log::warn!(
                                        "handle_key: best-effort Shift release also failed: {:?}",
                                        e
                                    );
                                }
                            }
                            return Err(e.into());
                        }
                        if is_shift {
                            if let Err(e) = remote_desktop_portal::notify_keyboard_keycode(
                                &portal,
                                &session,
                                HashMap::new(),
                                shift_keycode,
                                PRESSED_UP_STATE,
                            ) {
                                log::error!("handle_key: failed to release Shift: {:?}", e);
                                return Err(e.into());
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_mouse(
        down: bool,
        button: MouseButton,
        conn: Arc<SyncConnection>,
        session: &Path<'static>,
    ) {
        let portal = get_portal(&conn);
        let but_key = match button {
            MouseButton::Left => EVDEV_MOUSE_LEFT,
            MouseButton::Right => EVDEV_MOUSE_RIGHT,
            MouseButton::Middle => EVDEV_MOUSE_MIDDLE,
            _ => {
                return;
            }
        };
        let state: u32 = if down {
            PRESSED_DOWN_STATE
        } else {
            PRESSED_UP_STATE
        };
        let _ = remote_desktop_portal::notify_pointer_button(
            &portal,
            &session,
            HashMap::new(),
            but_key,
            state,
        );
    }
}
