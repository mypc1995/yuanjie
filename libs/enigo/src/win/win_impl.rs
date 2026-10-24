use self::winapi::ctypes::c_int;
use self::winapi::shared::{basetsd::ULONG_PTR, minwindef::*, windef::*};
use self::winapi::um::winbase::*;
use self::winapi::um::winuser::*;
use winapi;

use crate::win::keycodes::*;
use crate::{Key, KeyboardControllable, MouseButton, MouseControllable};
use std::mem::*;

extern "system" {
    pub fn GetLastError() -> DWORD;
}

/// 处理事件触发的主要结构体
#[derive(Default)]
pub struct Enigo;
static mut LAYOUT: HKL = std::ptr::null_mut();

/// 键盘和鼠标结构体中在 SendInput() 里使用的 dwExtraInfo 值
pub const ENIGO_INPUT_EXTRA_VALUE: ULONG_PTR = 100;

fn mouse_event(flags: u32, data: u32, dx: i32, dy: i32) -> DWORD {
    let mut u = INPUT_u::default();
    unsafe {
        * u.mi_mut() = MOUSEINPUT {
            dx,
            dy,
            mouseData: data,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: ENIGO_INPUT_EXTRA_VALUE,
        };
    }
    let mut input = INPUT {
        type_: INPUT_MOUSE,
        u,
    };
    unsafe { SendInput(1, &mut input as LPINPUT, size_of::<INPUT>() as c_int) }
}

// `v` 来自远端对端且未经校验，因此用 i64 缩放以避免溢出，并钳制到
// MOUSEEVENTF_ABSOLUTE 所要求的 0..=65535 范围内。
// 当虚拟屏幕指标不可用时，`extent` 为 0。
fn to_absolute(v: i32, origin: i32, extent: i32) -> Option<i32> {
    if extent <= 0 {
        return None;
    }
    let abs = (v as i64 - origin as i64) * 65535 / extent as i64;
    Some(abs.clamp(0, 65535) as i32)
}

fn keybd_event(mut flags: u32, vk: u16, scan: u16) -> DWORD {
    let mut scan = scan;
    unsafe {
        // https://github.com/rustdesk/rustdesk/issues/366
        if scan == 0 {
            if LAYOUT.is_null() {
                let current_window_thread_id =
                    GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
                LAYOUT = GetKeyboardLayout(current_window_thread_id);
            }
            scan = MapVirtualKeyExW(vk as _, 0, LAYOUT) as _;
        }
    }

    if flags & KEYEVENTF_UNICODE == 0 {
        if scan >> 8 == 0xE0 || scan >> 8 == 0xE1 {
            flags |= winapi::um::winuser::KEYEVENTF_EXTENDEDKEY;
        }
    }
    let mut union: INPUT_u = unsafe { std::mem::zeroed() };
    unsafe {
        * union.ki_mut() = KEYBDINPUT {
            wVk: vk,
            wScan: scan,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: ENIGO_INPUT_EXTRA_VALUE,
        };
    }
    let mut inputs = [INPUT {
        type_: INPUT_KEYBOARD,
        u: union,
    }; 1];
    unsafe {
        SendInput(
            inputs.len() as UINT,
            inputs.as_mut_ptr(),
            size_of::<INPUT>() as c_int,
        )
    }
}

fn get_error() -> String {
    unsafe {
        let buff_size = 256;
        let mut buff: Vec<u16> = Vec::with_capacity(buff_size);
        buff.resize(buff_size, 0);
        let errno = GetLastError();
        let chars_copied = FormatMessageW(
            FORMAT_MESSAGE_IGNORE_INSERTS
                | FORMAT_MESSAGE_FROM_SYSTEM
                | FORMAT_MESSAGE_ARGUMENT_ARRAY,
            std::ptr::null(),
            errno,
            0,
            buff.as_mut_ptr(),
            (buff_size + 1) as u32,
            std::ptr::null_mut(),
        );
        if chars_copied == 0 {
            return "".to_owned();
        }
        let mut curr_char: usize = chars_copied as usize;
        while curr_char > 0 {
            let ch = buff[curr_char];

            if ch >= ' ' as u16 {
                break;
            }
            curr_char -= 1;
        }
        let sl = std::slice::from_raw_parts(buff.as_ptr(), curr_char);
        let err_msg = String::from_utf16(sl);
        return err_msg.unwrap_or("".to_owned());
    }
}

impl MouseControllable for Enigo {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn mouse_move_to(&mut self, x: i32, y: i32) {
        let (left, top, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let (Some(dx), Some(dy)) = (to_absolute(x, left, width), to_absolute(y, top, height))
        else {
            hbb_common::throttled_log!(
                std::time::Duration::from_secs(60),
                warn,
                "mouse_move_to skipped: virtual screen size unavailable"
            );
            return;
        };
        mouse_event(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            0,
            dx,
            dy,
        );
    }

    fn mouse_move_relative(&mut self, x: i32, y: i32) {
        mouse_event(MOUSEEVENTF_MOVE, 0, x, y);
    }

    fn mouse_down(&mut self, button: MouseButton) -> crate::ResultType {
        let res = mouse_event(
            match button {
                MouseButton::Left => MOUSEEVENTF_LEFTDOWN,
                MouseButton::Middle => MOUSEEVENTF_MIDDLEDOWN,
                MouseButton::Right => MOUSEEVENTF_RIGHTDOWN,
                MouseButton::Back => MOUSEEVENTF_XDOWN,
                MouseButton::Forward => MOUSEEVENTF_XDOWN,
                _ => {
                    log::info!("Unsupported button {:?}", button);
                    return Ok(());
                }
            },
            match button {
                MouseButton::Back => XBUTTON1 as u32,
                MouseButton::Forward => XBUTTON2 as u32,
                _ => 0,
            },
            0,
            0,
        );
        if res == 0 {
            let err = get_error();
            if !err.is_empty() {
                return Err(err.into());
            }
        }
        Ok(())
    }

    fn mouse_up(&mut self, button: MouseButton) {
        mouse_event(
            match button {
                MouseButton::Left => MOUSEEVENTF_LEFTUP,
                MouseButton::Middle => MOUSEEVENTF_MIDDLEUP,
                MouseButton::Right => MOUSEEVENTF_RIGHTUP,
                MouseButton::Back => MOUSEEVENTF_XUP,
                MouseButton::Forward => MOUSEEVENTF_XUP,
                _ => {
                    log::info!("Unsupported button {:?}", button);
                    return;
                }
            },
            match button {
                MouseButton::Back => XBUTTON1 as _,
                MouseButton::Forward => XBUTTON2 as _,
                _ => 0,
            },
            0,
            0,
        );
    }

    fn mouse_click(&mut self, button: MouseButton) {
        self.mouse_down(button).ok();
        self.mouse_up(button);
    }

    fn mouse_scroll_x(&mut self, length: i32) {
        mouse_event(MOUSEEVENTF_HWHEEL, length as _, 0, 0);
    }

    fn mouse_scroll_y(&mut self, length: i32) {
        mouse_event(MOUSEEVENTF_WHEEL, length as _, 0, 0);
    }
}

impl KeyboardControllable for Enigo {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn key_sequence(&mut self, sequence: &str) {
        let mut buffer = [0; 2];

        for c in sequence.chars() {
            // Windows 使用 uft-16 编码。我们需要检查
            // 是否存在变长字符。因此某些
            // 字符可能有 32 位长，而这些字符
            // 以所谓的高位和低位代理项（surrogate）编码，
            // 每个 16 位宽，需要在
            // 另一个之后发送到 SendInput 函数，且不
            // 被 "keyup" 打断
            let result = c.encode_utf16(&mut buffer);
            if result.len() == 1 {
                self.unicode_key_click(result[0]);
            } else {
                for utf16_surrogate in result {
                    self.unicode_key_down(utf16_surrogate.clone());
                }
                // 我需要产生 keyup 吗？
                // self.unicode_key_up(0);
            }
        }
    }

    fn key_click(&mut self, key: Key) {
        let vk = self.key_to_keycode(key);
        keybd_event(0, vk, 0);
        keybd_event(KEYEVENTF_KEYUP, vk, 0);
    }

    fn key_down(&mut self, key: Key) -> crate::ResultType {
        match &key {
            Key::Layout(c) => {
                // 待办：重复代码
                // https://github.com/rustdesk/rustdesk/blob/1bc0dd791ed8344997024dc46626bd2ca7df73d2/src/server/input_service.rs#L1348
                let code = self.get_layoutdependent_keycode(*c);
                if code as u16 != 0xFFFF {
                    let vk = code & 0x00FF;
                    let flag = code >> 8;
                    let modifiers = [Key::Shift, Key::Control, Key::Alt];
                    let mod_len = modifiers.len();
                    for pos in 0..mod_len {
                        if flag & (0x0001 << pos) != 0 {
                            self.key_down(modifiers[pos])?;
                        }
                    }

                    let res = keybd_event(0, vk, 0);
                    let err = if res == 0 { get_error() } else { "".to_owned() };

                    for pos in 0..mod_len {
                        let rpos = mod_len - 1 - pos;
                        if flag & (0x0001 << rpos) != 0 {
                            self.key_up(modifiers[rpos]);
                        }
                    }

                    if !err.is_empty() {
                        return Err(err.into());
                    }
                } else {
                    return Err(format!("Failed to get keycode of {}", c).into());
                }
            }
            _ => {
                let code = self.key_to_keycode(key);
                if code == 0 || code == 65535 {
                    return Err("".into());
                }
                let res = keybd_event(0, code, 0);
                if res == 0 {
                    let err = get_error();
                    if !err.is_empty() {
                        return Err(err.into());
                    }
                }
            }
        }
        Ok(())
    }

    fn key_up(&mut self, key: Key) {
        match key {
            Key::Layout(c) => {
                let code = self.get_layoutdependent_keycode(c);
                if code as u16 != 0xFFFF {
                    let vk = code & 0x00FF;
                    keybd_event(KEYEVENTF_KEYUP, vk, 0);
                }
            }
            _ => {
                keybd_event(KEYEVENTF_KEYUP, self.key_to_keycode(key), 0);
            }
        }
    }

    fn get_key_state(&mut self, key: Key) -> bool {
        let keycode = self.key_to_keycode(key);
        let x = unsafe { GetKeyState(keycode as _) };
        if key == Key::CapsLock || key == Key::NumLock || key == Key::Scroll {
            return (x & 0x1) == 0x1;
        }
        return (x as u16 & 0x8000) == 0x8000;
    }
}

impl Enigo {
    /// 获取主显示器在屏幕坐标系中的 (宽, 高)
    /// (像素)。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut size = Enigo::main_display_size();
    /// ```
    pub fn main_display_size() -> (usize, usize) {
        let w = unsafe { GetSystemMetrics(SM_CXSCREEN) as usize };
        let h = unsafe { GetSystemMetrics(SM_CYSCREEN) as usize };
        (w, h)
    }

    /// 获取鼠标在屏幕坐标系中的位置（像素）。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut location = Enigo::mouse_location();
    /// ```
    pub fn mouse_location() -> (i32, i32) {
        let mut point = POINT { x: 0, y: 0 };
        let result = unsafe { GetCursorPos(&mut point) };
        if result != 0 {
            (point.x, point.y)
        } else {
            (0, 0)
        }
    }

    fn unicode_key_click(&self, unicode_char: u16) {
        self.unicode_key_down(unicode_char);
        self.unicode_key_up(unicode_char);
    }

    fn unicode_key_down(&self, unicode_char: u16) {
        keybd_event(KEYEVENTF_UNICODE, 0, unicode_char);
    }

    fn unicode_key_up(&self, unicode_char: u16) {
        keybd_event(KEYEVENTF_UNICODE | KEYEVENTF_KEYUP, 0, unicode_char);
    }

    fn key_to_keycode(&self, key: Key) -> u16 {
        // 不要使用 crate winapi 中的编码，它们
        // 被错误地标注为 i32 而不是 i16，请使用
        // win/keycodes.rs 中提供的那些，其前缀为
        // 在原名之前带有一个 'E'
        #[allow(deprecated)]
        // 我的意思是，显然，在废弃按键被移除之前我们仍需支持它们
        match key {
            Key::Alt => EVK_MENU,
            Key::Backspace => EVK_BACK,
            Key::CapsLock => EVK_CAPITAL,
            Key::Control => EVK_LCONTROL,
            Key::Delete => EVK_DELETE,
            Key::DownArrow => EVK_DOWN,
            Key::End => EVK_END,
            Key::Escape => EVK_ESCAPE,
            Key::F1 => EVK_F1,
            Key::F10 => EVK_F10,
            Key::F11 => EVK_F11,
            Key::F12 => EVK_F12,
            Key::F2 => EVK_F2,
            Key::F3 => EVK_F3,
            Key::F4 => EVK_F4,
            Key::F5 => EVK_F5,
            Key::F6 => EVK_F6,
            Key::F7 => EVK_F7,
            Key::F8 => EVK_F8,
            Key::F9 => EVK_F9,
            Key::Home => EVK_HOME,
            Key::LeftArrow => EVK_LEFT,
            Key::Option => EVK_MENU,
            Key::PageDown => EVK_NEXT,
            Key::PageUp => EVK_PRIOR,
            Key::Return => EVK_RETURN,
            Key::RightArrow => EVK_RIGHT,
            Key::Shift => EVK_SHIFT,
            Key::Space => EVK_SPACE,
            Key::Tab => EVK_TAB,
            Key::UpArrow => EVK_UP,
            Key::Numpad0 => EVK_NUMPAD0,
            Key::Numpad1 => EVK_NUMPAD1,
            Key::Numpad2 => EVK_NUMPAD2,
            Key::Numpad3 => EVK_NUMPAD3,
            Key::Numpad4 => EVK_NUMPAD4,
            Key::Numpad5 => EVK_NUMPAD5,
            Key::Numpad6 => EVK_NUMPAD6,
            Key::Numpad7 => EVK_NUMPAD7,
            Key::Numpad8 => EVK_NUMPAD8,
            Key::Numpad9 => EVK_NUMPAD9,
            Key::Cancel => EVK_CANCEL,
            Key::Clear => EVK_CLEAR,
            Key::Pause => EVK_PAUSE,
            Key::Kana => EVK_KANA,
            Key::Hangul => EVK_HANGUL,
            Key::Junja => EVK_JUNJA,
            Key::Final => EVK_FINAL,
            Key::Hanja => EVK_HANJA,
            Key::Kanji => EVK_KANJI,
            Key::Convert => EVK_CONVERT,
            Key::Select => EVK_SELECT,
            Key::Print => EVK_PRINT,
            Key::Execute => EVK_EXECUTE,
            Key::Snapshot => EVK_SNAPSHOT,
            Key::Insert => EVK_INSERT,
            Key::Help => EVK_HELP,
            Key::Sleep => EVK_SLEEP,
            Key::Separator => EVK_SEPARATOR,
            Key::Mute => EVK_VOLUME_MUTE,
            Key::VolumeDown => EVK_VOLUME_DOWN,
            Key::VolumeUp => EVK_VOLUME_UP,
            Key::Scroll => EVK_SCROLL,
            Key::NumLock => EVK_NUMLOCK,
            Key::RWin => EVK_RWIN,
            Key::Apps => EVK_APPS,
            Key::Add => EVK_ADD,
            Key::Multiply => EVK_MULTIPLY,
            Key::Decimal => EVK_DECIMAL,
            Key::Subtract => EVK_SUBTRACT,
            Key::Divide => EVK_DIVIDE,
            Key::NumpadEnter => EVK_RETURN,
            Key::Equals => '=' as _,
            Key::RightShift => EVK_RSHIFT,
            Key::RightControl => EVK_RCONTROL,
            Key::RightAlt => EVK_RMENU,

            Key::Raw(raw_keycode) => raw_keycode,
            Key::Super | Key::Command | Key::Windows | Key::Meta => EVK_LWIN,
            Key::Layout(..) => {
                // 不可达
                0
            }
        }
    }

    fn get_layoutdependent_keycode(&self, chr: char) -> u16 {
        unsafe {
            LAYOUT = std::ptr::null_mut();
        }
        // NOTE VkKeyScanW uses the current keyboard LAYOUT
        // 要指定 LAYOUT，请使用 VkKeyScanExW 和 GetKeyboardLayout
        // 或用 LoadKeyboardLayoutW 加载一个
        let current_window_thread_id =
            unsafe { GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut()) };
        unsafe { LAYOUT = GetKeyboardLayout(current_window_thread_id) };
        unsafe { VkKeyScanExW(chr as _, LAYOUT) as _ }
    }
}
