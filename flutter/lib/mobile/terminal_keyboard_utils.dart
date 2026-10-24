/// 取自 PR #15532 的移动端终端键盘布局。
///
/// 把按键顺序放在组件之外，可以让预期的布局一目了然
/// 并防止行为修复在不知不觉中把按键挪到其他行。
const terminalKeyboardRow1Keys = ['Esc', '/', '|', 'Home', '↑', 'End', r'\'];
const terminalKeyboardRow2Keys = ['Tab', 'Ctrl+C', '~', '←', '↓', '→'];
const terminalKeyboardRow3Keys = ['Ctrl', 'Alt', '-', 'PgUp', 'PgDn'];

const terminalKeyboardKeyWidth = 48.0;
const terminalKeyboardKeySpacing = 2.0;

/// 空的 48dp 槽位让展开后的 Row3 与上面的两行保持对齐。
const terminalKeyboardRow3TrailingPlaceholderCount = 2;
