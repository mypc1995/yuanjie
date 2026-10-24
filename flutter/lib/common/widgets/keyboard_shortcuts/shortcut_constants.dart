/// 键盘快捷键操作 ID —— 必须与
/// src/keyboard/shortcuts.rs::action_id 保持一致。
const kShortcutActionSendCtrlAltDel = 'send_ctrl_alt_del';
const kShortcutActionToggleFullscreen = 'toggle_fullscreen';
const kShortcutActionSwitchDisplayNext = 'switch_display_next';
const kShortcutActionSwitchDisplayPrev = 'switch_display_prev';
const kShortcutActionSwitchDisplayAll = 'switch_display_all';
const kShortcutActionScreenshot = 'screenshot';
const kShortcutActionInsertLock = 'insert_lock';
const kShortcutActionRefresh = 'refresh';
const kShortcutActionToggleBlockInput = 'toggle_block_input';
const kShortcutActionToggleRecording = 'toggle_recording';
const kShortcutActionSwitchSides = 'switch_sides';
const kShortcutActionCloseTab = 'close_tab';
const kShortcutActionToggleToolbar = 'toggle_toolbar';
const kShortcutActionRestartRemote = 'restart_remote';
const kShortcutActionResetCanvas = 'reset_canvas';
const kShortcutActionSwitchTabNext = 'switch_tab_next';
const kShortcutActionSwitchTabPrev = 'switch_tab_prev';

/// 将键盘焦点转移到其他会话或对话框的操作。它们在触发它们的按键
/// 被释放时执行，而不是在按下时执行，因此
/// 消费了这次按下的 matcher 也能看到按键的重复和释放。在 Linux 上，
/// legacy 模式的会话在 Dart 侧匹配，而 map 模式的会话在
/// Rust 侧匹配，双方都不知道对方触发的按键。对应 `runs_on_release`
/// （位于 `src/keyboard/shortcuts.rs`）；两者都以
/// `flutter/test/fixtures/key_up_shortcut_actions.json` 为准进行校验。
const kShortcutActionsRunOnKeyUp = {
  kShortcutActionCloseTab,
  kShortcutActionSwitchTabNext,
  kShortcutActionSwitchTabPrev,
  kShortcutActionRestartRemote,
  kShortcutActionSwitchSides,
  kShortcutActionToggleChat,
};
const kShortcutActionToggleMute = 'toggle_mute';
const kShortcutActionPinToolbar = 'pin_toolbar';
const kShortcutActionViewModeOriginal = 'view_mode_original';
const kShortcutActionViewModeAdaptive = 'view_mode_adaptive';
const kShortcutActionToggleChat = 'toggle_chat';
const kShortcutActionToggleQualityMonitor = 'toggle_quality_monitor';
const kShortcutActionToggleShowRemoteCursor = 'toggle_show_remote_cursor';
const kShortcutActionToggleShowMyCursor = 'toggle_show_my_cursor';
const kShortcutActionToggleDisableClipboard = 'toggle_disable_clipboard';
const kShortcutActionPrivacyMode1 = 'privacy_mode_1';
const kShortcutActionPrivacyMode2 = 'privacy_mode_2';
// 键盘模式（Map / Translate / Legacy）。
const kShortcutActionKeyboardModeMap = 'keyboard_mode_map';
const kShortcutActionKeyboardModeTranslate = 'keyboard_mode_translate';
const kShortcutActionKeyboardModeLegacy = 'keyboard_mode_legacy';
// 编解码器偏好（Auto + 工具栏提供的四个可选编解码器）。
const kShortcutActionCodecAuto = 'codec_auto';
const kShortcutActionCodecVp8 = 'codec_vp8';
const kShortcutActionCodecVp9 = 'codec_vp9';
const kShortcutActionCodecAv1 = 'codec_av1';
const kShortcutActionCodecH264 = 'codec_h264';
const kShortcutActionCodecH265 = 'codec_h265';
// 一次性拔出所有虚拟显示器——工具栏在
// 两种 IDD 模式下（RustDesk 和 Amyuni）都提供该功能。按序号切换虚拟显示器的选项
// （RustDesk IDD 的 4 个复选框）以及 +/- 数量按钮（仅 Amyuni）
// 都没有暴露为快捷键：按序号太细粒度，而 +/- 在
// RustDesk IDD 对端上没有对应的工具栏项。
const kShortcutActionPlugOutAllVirtualDisplays =
    'plug_out_all_virtual_displays';
const kShortcutActionToggleRelativeMouseMode = 'toggle_relative_mouse_mode';
const kShortcutActionToggleFollowRemoteCursor = 'toggle_follow_remote_cursor';
const kShortcutActionToggleFollowRemoteWindow = 'toggle_follow_remote_window';
const kShortcutActionToggleZoomCursor = 'toggle_zoom_cursor';
const kShortcutActionToggleReverseMouseWheel = 'toggle_reverse_mouse_wheel';
const kShortcutActionToggleSwapLeftRightMouse = 'toggle_swap_left_right_mouse';
const kShortcutActionToggleLockAfterSessionEnd = 'toggle_lock_after_session_end';
const kShortcutActionToggleTrueColor = 'toggle_true_color';
const kShortcutActionToggleSwapCtrlCmd = 'toggle_swap_ctrl_cmd';
const kShortcutActionToggleEnableFileCopyPaste = 'toggle_enable_file_copy_paste';
const kShortcutActionViewModeCustom = 'view_mode_custom';
const kShortcutActionImageQualityBest = 'image_quality_best';
const kShortcutActionImageQualityBalanced = 'image_quality_balanced';
const kShortcutActionImageQualityLow = 'image_quality_low';
const kShortcutActionSendClipboardKeystrokes = 'send_clipboard_keystrokes';
const kShortcutActionToggleVoiceCall = 'toggle_voice_call';
const kShortcutActionToggleViewOnly = 'toggle_view_only';

const kShortcutLocalConfigKey = 'keyboard-shortcuts';
const kShortcutEventName = 'shortcut_triggered';

/// 规范的默认键盘快捷键绑定，对应 Rust 的
/// `default_bindings()`（位于 `src/keyboard/shortcuts.rs`）。使用方：
///   * Web bridge（`flutter/lib/web/bridge.dart::mainGetDefaultKeyboardShortcuts`）
///     —— Web 在运行时没有 Rust，因此种子列表从这个 Dart
///     常量读取，而不是走 FFI。
///   * 配置页面在首次启用时填充默认值，此时已经过
///     [filterDefaultBindingsForPlatform] 对平台特定条目的裁剪
///     处理。
///
/// 与 Rust 的一致性在两侧都针对以下文件做了单元测试：
/// `flutter/test/fixtures/default_keyboard_shortcuts.json`——参见
/// `kDefaultShortcutBindings matches fixture` 测试位于
/// `flutter/test/keyboard_shortcuts_test.dart`，以及
/// `src/keyboard/shortcuts.rs` 中的 `default_bindings_match_fixture_json`。
/// 这里的任何改动都 MUST 同时更新该 fixture 和 Rust 源码，否则 CI
/// 会在发生偏移的一侧失败。
final List<Map<String, Object>> kDefaultShortcutBindings = [
  for (final entry in <List<Object>>[
    [kShortcutActionSendCtrlAltDel,         'delete'],
    [kShortcutActionToggleFullscreen,       'enter'],
    [kShortcutActionSwitchDisplayNext,      'arrow_right'],
    [kShortcutActionSwitchDisplayPrev,      'arrow_left'],
    [kShortcutActionScreenshot,             'p'],
    [kShortcutActionToggleShowRemoteCursor, 'm'],
    // 受测的 iPad/蓝牙键盘组合未能完整送出
    // Cmd+Option+Shift+S 组合键，在 Safari 中也一样。其他物理键盘的情况
    // 尚未验证；Mute 可以重新绑定到应用能收到的组合键上。
    [kShortcutActionToggleMute,             's'],
    [kShortcutActionToggleBlockInput,       'i'],
    [kShortcutActionToggleChat,             'c'],
  ])
    {
      'action': entry[0],
      'mods': const ['primary', 'alt', 'shift'],
      'key': entry[1],
    },
];
