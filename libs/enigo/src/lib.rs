//! Enigo 让你能够模拟鼠标和键盘的输入事件，就如同它们是
//! 由真实硬件发出的。目标是让它能在不同的
//! 操作系统（如 Linux、macOS 和 Windows）上可用 —— 也可能涵盖更多，但
//! [Redox](https://redox-os.org/) 和 *BSD 已在计划中。请查看
//! [Repo](https://github.com/enigo-rs/enigo) 了解当前进展。
//!
//! 我认为这个库目前处于早期 alpha 阶段，API 将会在
//! 未来发生变化。键盘处理部分远谈不上好用。我计划
//! 构建一个简单的
//! [DSL](https://en.wikipedia.org/wiki/Domain-specific_language)
//! 它大致像这样：
//!
//! `"hello {+SHIFT}world{-SHIFT} and break line{ENTER}"`
//!
//! 目前的状态是，你只能直接打印
//! [unicode](http://unicode.org/)
//! 诸如 [emoji](http://getemoji.com/) 这样的字符，而无需 `{+SHIFT}`
//! [DSL](https://en.wikipedia.org/wiki/Domain-specific_language)
//! 或 Linux、macOS 和 Windows 操作系统上的任何其他“特殊”按键。
//!
//! 可能的用例包括：在不同的平台上测试用户界面、
//! 平台，
//! 构建远程控制应用，或仅为没有公共 API 或脚本语言的
//! 用户界面自动化任务。
//!
//! 键盘目前有两种可用模式。第一种模式
//! 由 [key_sequence]() 函数表示，
//! 其目的只是输入 unicode 字符。这与
//! 键盘布局无关。请注意，
//! 你无法使用 Control 之类的修饰键
//! 来影响结果。如果你想用修饰键来例如
//! 复制/粘贴
//! 请使用 Layout 变体。请注意，这确实是依赖布局的。

//! # 示例
//! ```no_run
//! use enigo::*;
//! let mut enigo = Enigo::new();
//! 粘贴
//! enigo.key_down(Key::Control);
//! enigo.key_click(Key::Layout('v'));
//! enigo.key_up(Key::Control);
//! ```
//!
//! ```no_run
//! use enigo::*;
//! let mut enigo = Enigo::new();
//! enigo.mouse_move_to(500, 200);
//! enigo.mouse_down(MouseButton::Left);
//! enigo.mouse_move_relative(100, 100);
//! enigo.mouse_up(MouseButton::Left);
//! enigo.key_sequence("hello world");
//! ```
#![deny(missing_docs)]

#[cfg(target_os = "macos")]
#[macro_use]
extern crate objc;

// TODO(dustin) use interior mutability not &mut self

#[cfg(target_os = "windows")]
mod win;
#[cfg(target_os = "windows")]
pub use win::Enigo;
#[cfg(target_os = "windows")]
pub use win::ENIGO_INPUT_EXTRA_VALUE;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Enigo;
#[cfg(target_os = "macos")]
pub use macos::ENIGO_INPUT_EXTRA_VALUE;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use crate::linux::Enigo;

/// DSL 解析器模块
pub mod dsl;

#[cfg(feature = "with_serde")]
#[macro_use]
extern crate serde_derive;

#[cfg(feature = "with_serde")]
extern crate serde;

///
pub type ResultType = std::result::Result<(), Box<dyn std::error::Error>>;

#[cfg_attr(feature = "with_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq)]
/// MouseButton 表示一个鼠标按键，
/// 并例如用于
/// [mouse_click](trait.MouseControllable.html#tymethod.mouse_click)。
/// WARNING: Types with the prefix Scroll
/// 并不打算被使用，且可能无法在
/// 所有操作系统上工作。
pub enum MouseButton {
    /// 鼠标左键
    Left,
    /// 鼠标中键
    Middle,
    /// 鼠标右键
    Right,
    /// 鼠标后退键
    Back,
    /// 鼠标前进键
    Forward,

    /// 滚轮上滚键
    ScrollUp,
    /// 滚轮下滚键
    ScrollDown,
    /// 滚轮左滚键
    ScrollLeft,
    /// 滚轮右滚键
    ScrollRight,
}

/// 表示一个接口以及一组每个
/// 操作系统实现都_应当_实现的鼠标函数。
pub trait MouseControllable {
    // https://stackoverflow.com/a/33687996
    /// 提供赋予具体类型的能力。
    fn as_any(&self) -> &dyn std::any::Any;

    /// 提供赋予具体类型的能力。
    fn as_mut_any(&mut self) -> &mut dyn std::any::Any;

    /// 让鼠标光标移动到指定的 x 和 y 坐标。
    ///
    /// 显示器屏幕的左上角是 x=0 y=0。向
    /// 下移动光标需要增大 y，向右移动
    /// 则需要增大 x 坐标。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// enigo.mouse_move_to(500, 200);
    /// ```
    fn mouse_move_to(&mut self, x: i32, y: i32);

    /// 让鼠标光标在 x 和 y 方向移动指定
    /// 距离。
    ///
    /// x 和 y 参数中指定的量会加到
    /// 鼠标光标当前的位置上。正的 x 值会让
    /// 鼠标光标向右移动 `x` 像素。负的
    /// `x` 值会让鼠标光标向左移动。正的值
    /// of y
    /// 会让鼠标光标向下移动，负的值会让鼠标光标
    /// up.
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// enigo.mouse_move_relative(100, 100);
    /// ```
    fn mouse_move_relative(&mut self, x: i32, y: i32);

    /// 按下鼠标按键之一
    ///
    /// 按下由类型 [MouseButton](enum.MouseButton.html) 的参数 `button` 指定的
    /// 鼠标按键，
    /// 并一直保持，直到被
    /// [mouse_up](trait.MouseControllable.html#tymethod.mouse_up) 释放。
    /// 对 [mouse_move_to](trait.MouseControllable.html#tymethod.
    /// mouse_move_to) 或
    /// [mouse_move_relative](trait.MouseControllable.html#tymethod.
    /// mouse_move_relative)
    /// 的调用会按预期工作，例如拖动控件或高亮文本。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// enigo.mouse_down(MouseButton::Left);
    /// ```
    fn mouse_down(&mut self, button: MouseButton) -> ResultType;

    /// 抬起一个已按下的鼠标按键
    ///
    /// 抬起先前按下的按键（通过调用
    /// [mouse_down](trait.MouseControllable.html#tymethod.mouse_down)）。
    /// 如果该按键并未被按下，或者在没有
    /// 调用 [mouse_down](trait.MouseControllable.html#tymethod.mouse_down)
    /// 的情况下连续调用，就会发出抬起事件。这取决于
    /// 操作系统实际如何处理 —— 我猜它多半会直接
    /// 被忽略。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// enigo.mouse_up(MouseButton::Right);
    /// ```
    fn mouse_up(&mut self, button: MouseButton);

    /// 点击鼠标按键
    ///
    /// 它本质上只是连续调用
    /// [mouse_down](trait.MouseControllable.html#tymethod.mouse_down)，随后
    /// 调用 [mouse_up](trait.MouseControllable.html#tymethod.mouse_up)。只是
    /// 为了
    /// 方便。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// enigo.mouse_click(MouseButton::Right);
    /// ```
    fn mouse_click(&mut self, button: MouseButton);

    /// 让鼠标（滚轮）向左或向右滚动
    ///
    /// length 为正数会让鼠标滚轮向右滚动，
    /// 为负数则向左滚动。指定的值会换算成
    /// 操作系统所定义的 `lines`，大致相当于鼠标滚轮上 15°
    /// （一次点击）的转动。它会移动多少行取决于
    /// 操作系统当前的设置。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// enigo.mouse_scroll_x(2);
    /// ```
    fn mouse_scroll_x(&mut self, length: i32);

    /// 让鼠标（滚轮）向上或向下滚动
    ///
    /// length 为正数会让鼠标滚轮向下滚动，
    /// 为负数则向上滚动。指定的值会换算成
    /// 操作系统所定义的 `lines`，大致相当于鼠标滚轮上 15°
    /// （一次点击）的转动。它会移动多少行取决于
    /// 操作系统当前的设置。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// enigo.mouse_scroll_y(2);
    /// ```
    fn mouse_scroll_y(&mut self, length: i32);
}

/// 键盘上的一个按键。
/// 对于字母按键，使用 Key::Layout 以获得与系统无关的按键。
/// 如果某个按键缺失，可以用 Key::Raw 使用原始键码。
#[cfg_attr(feature = "with_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// Linux 和 Windows 上的 alt 键（macOS 上是 option 键）
    Alt,
    /// backspace 键
    Backspace,
    /// caps lock 键
    CapsLock,
    // #[deprecated(since = "0.0.12", note = "now renamed to Meta")]
    /// macOS 上的 command 键（Linux 上是 super 键，Windows 上是 windows 键）
    Command,
    /// control 键
    Control,
    /// delete 键
    Delete,
    /// 下方向键
    DownArrow,
    /// end 键
    End,
    /// escape 键（esc）
    Escape,
    /// F1 键
    F1,
    /// F10 键
    F10,
    /// F11 键
    F11,
    /// F12 键
    F12,
    /// F2 键
    F2,
    /// F3 键
    F3,
    /// F4 键
    F4,
    /// F5 键
    F5,
    /// F6 键
    F6,
    /// F7 键
    F7,
    /// F8 键
    F8,
    /// F9 键
    F9,
    /// home 键
    Home,
    /// 左方向键
    LeftArrow,
    /// meta 键（也称为 "windows"、"super" 和 "command"）
    Meta,
    /// macOS 上的 option 键（Linux 和 Windows 上是 alt 键）
    Option, // deprecated, use Alt instead
    /// page down 键
    PageDown,
    /// page up 键
    PageUp,
    /// return 键
    Return,
    /// 右方向键
    RightArrow,
    /// shift 键
    Shift,
    /// space 键
    Space,
    // #[deprecated(since = "0.0.12", note = "now renamed to Meta")]
    /// linux 上的 super 键（macOS 上是 command 键，Windows 上是 windows 键）
    Super,
    /// tab 键（制表符）
    Tab,
    /// 上方向键
    UpArrow,
    // #[deprecated(since = "0.0.12", note = "now renamed to Meta")]
    /// Windows 上的 windows 键（Linux 上是 super 键，macOS 上是 command 键）
    Windows,
    ///
    Numpad0,
    ///
    Numpad1,
    ///
    Numpad2,
    ///
    Numpad3,
    ///
    Numpad4,
    ///
    Numpad5,
    ///
    Numpad6,
    ///
    Numpad7,
    ///
    Numpad8,
    ///
    Numpad9,
    ///
    Cancel,
    ///
    Clear,
    ///
    Pause,
    ///
    Kana,
    ///
    Hangul,
    ///
    Junja,
    ///
    Final,
    ///
    Hanja,
    ///
    Kanji,
    ///
    Convert,
    ///
    Select,
    ///
    Print,
    ///
    Execute,
    ///
    Snapshot,
    ///
    Insert,
    ///
    Help,
    ///
    Sleep,
    ///
    Separator,
    ///
    VolumeUp,
    ///
    VolumeDown,
    ///
    Mute,
    ///
    Scroll,
    /// scroll lock
    NumLock,
    ///
    RWin,
    ///
    Apps,
    ///
    Multiply,
    ///
    Add,
    ///
    Subtract,
    ///
    Decimal,
    ///
    Divide,
    ///
    Equals,
    ///
    NumpadEnter,
    ///
    RightShift,
    ///
    RightControl,
    ///
    RightAlt,
    ///
    /// 功能键，/// mac
    /// 依赖键盘布局的按键
    Layout(char),
    /// 原始键码，例如 0x38
    Raw(u16),
}

/// 表示一个接口以及一组每个
/// 操作系统实现都_应当_实现的键盘函数。
pub trait KeyboardControllable {
    // https://stackoverflow.com/a/33687996
    /// 提供赋予具体类型的能力。
    fn as_any(&self) -> &dyn std::any::Any;

    /// 提供赋予具体类型的能力。
    fn as_mut_any(&mut self) -> &mut dyn std::any::Any;

    /// 输入由 DSL 解析出的字符串。
    ///
    /// 输入 {+SHIFT}hello{-SHIFT} 会变成 HELLO。
    /// TODO: Full documentation
    fn key_sequence_parse(&mut self, sequence: &str)
    where
        Self: Sized,
    {
        if let Err(..) = self.key_sequence_parse_try(sequence) {
            println!("Could not parse sequence");
        }
    }
    /// 与 key_sequence_parse 相同，只是会返回错误
    fn key_sequence_parse_try(&mut self, sequence: &str) -> Result<(), dsl::ParseError>
    where
        Self: Sized,
    {
        dsl::eval(self, sequence)
    }

    /// 输入该字符串
    ///
    /// 发出按键序列，使给定的字符串被输入。
    ///
    /// 这里可以使用很多 unicode 字符，例如：❤️。这样是有效的，
    /// 与当前的键盘布局无关。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// enigo.key_sequence("hello world ❤️");
    /// ```
    fn key_sequence(&mut self, sequence: &str);

    /// 按下指定的按键
    fn key_down(&mut self, key: Key) -> ResultType;

    /// 释放此前由
    /// [key_down](trait.KeyboardControllable.html#tymethod.key_down)
    fn key_up(&mut self, key: Key);

    /// 与
    /// [key_down](trait.KeyboardControllable.html#tymethod.key_down) 和
    /// [key_up](trait.KeyboardControllable.html#tymethod.key_up)
    /// 函数类似，它们只是被连续调用
    fn key_click(&mut self, key: Key);

    ///
    fn get_key_state(&mut self, key: Key) -> bool;
}

#[cfg(any(target_os = "android", target_os = "ios"))]
struct Enigo;

impl Enigo {
    /// 构造一个新的 `Enigo` 实例。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use enigo::*;
    /// let mut enigo = Enigo::new();
    /// ```
    pub fn new() -> Self {
        #[cfg(any(target_os = "android", target_os = "ios"))]
        return Enigo {};
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Self::default()
    }
}

use std::fmt;

impl fmt::Debug for Enigo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Enigo")
    }
}
