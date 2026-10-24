import 'shortcut_constants.dart';
import 'shortcut_utils.dart';

/// 标记 [KeyboardShortcutActionEntry] /
/// [KeyboardShortcutActionSubgroup] 的联合类型——即顶级
/// [KeyboardShortcutActionGroup] 可以直接包含的任何内容。设为密封类型，这样渲染器
/// 和过滤器可以对其 `switch`，而无需 default 分支。
sealed class KeyboardShortcutActionGroupChild {
  const KeyboardShortcutActionGroupChild();
}

/// 一个可配置的操作——id + 其标签的 i18n key。
class KeyboardShortcutActionEntry extends KeyboardShortcutActionGroupChild {
  final String id;
  final String labelKey;
  const KeyboardShortcutActionEntry(this.id, this.labelKey);
}

/// 一个嵌套子分组（例如 "Display" 之下的 "View Mode"）。渲染时会多出
/// 缩进，使其条目在视觉上区别于父分组的
/// 直接条目。
class KeyboardShortcutActionSubgroup extends KeyboardShortcutActionGroupChild {
  final String titleKey;
  final List<KeyboardShortcutActionEntry> entries;
  const KeyboardShortcutActionSubgroup(this.titleKey, this.entries);
}

/// 一个顶级分组（"Display"、"Keyboard"、"Chat"……）。`children` 是一个
/// 由直接条目与子分组构成的*有序*混合体，因此像
/// "子分组在前 → 直接条目 → 末尾子分组"这样的布局——正是
/// `_DisplayMenu` 所用的结构（Privacy mode 位于光标 / 显示
/// 开关直接条目之后）——这些是一等元素，无需为了插入条目而额外包一层
/// "Display Settings" 子分组。
class KeyboardShortcutActionGroup {
  final String titleKey;
  final List<KeyboardShortcutActionGroupChild> children;
  const KeyboardShortcutActionGroup(this.titleKey, this.children);
}

/// 桌面端和移动端配置页面共用的规范操作分组定义。
/// 这里分组、子分组和条目的顺序
/// 就是用户在 UI 中看到的顺序，并且与对应的
/// 工具栏子菜单（`_DisplayMenu` / `_KeyboardMenu`，位于
/// `desktop/widgets/remote_toolbar.dart`）的子项顺序保持一致——除了那些
/// 没有快捷键对应项的条目（例如 `_screenAdjustor.adjustWindow`、
/// `scrollStyle`、`_ResolutionsMenu`、`localKeyboardType`）。
final List<KeyboardShortcutActionGroup> kKeyboardShortcutActionGroups = [
  KeyboardShortcutActionGroup('Monitor', [
    KeyboardShortcutActionEntry(
        kShortcutActionSwitchDisplayNext, 'Switch to next display'),
    KeyboardShortcutActionEntry(
        kShortcutActionSwitchDisplayPrev, 'Switch to previous display'),
    KeyboardShortcutActionEntry(
        kShortcutActionSwitchDisplayAll, 'All monitors'),
  ]),
  KeyboardShortcutActionGroup('Control Actions', [
    KeyboardShortcutActionEntry(
        kShortcutActionSendClipboardKeystrokes, 'Send clipboard keystrokes'),
    KeyboardShortcutActionEntry(kShortcutActionResetCanvas, 'Reset canvas'),
    KeyboardShortcutActionEntry(
        kShortcutActionSendCtrlAltDel, 'Insert Ctrl + Alt + Del'),
    KeyboardShortcutActionEntry(
        kShortcutActionRestartRemote, 'Restart remote device'),
    KeyboardShortcutActionEntry(kShortcutActionInsertLock, 'Insert Lock'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleBlockInput, 'Block user input'),
    KeyboardShortcutActionEntry(kShortcutActionSwitchSides, 'Switch Sides'),
    KeyboardShortcutActionEntry(kShortcutActionRefresh, 'Refresh'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleRecording, 'Toggle session recording'),
    KeyboardShortcutActionEntry(kShortcutActionScreenshot, 'Take screenshot'),
  ]),
  // Display：子分组（View Mode → Image Quality → Codec → Virtual display）
  // 在前，然后是直接条目（光标开关 + 显示开关），最后是
  // Privacy mode 子分组在末尾——与 `_DisplayMenu.menuChildrenGetter`
  // 完全一致。调整这一顺序时也应同步调整工具栏。
  KeyboardShortcutActionGroup('Display', [
    KeyboardShortcutActionSubgroup('View Mode', [
      KeyboardShortcutActionEntry(
          kShortcutActionViewModeOriginal, 'Scale original'),
      KeyboardShortcutActionEntry(
          kShortcutActionViewModeAdaptive, 'Scale adaptive'),
      KeyboardShortcutActionEntry(
          kShortcutActionViewModeCustom, 'Scale custom'),
    ]),
    KeyboardShortcutActionSubgroup('Image Quality', [
      KeyboardShortcutActionEntry(
          kShortcutActionImageQualityBest, 'Good image quality'),
      KeyboardShortcutActionEntry(
          kShortcutActionImageQualityBalanced, 'Balanced'),
      KeyboardShortcutActionEntry(
          kShortcutActionImageQualityLow, 'Optimize reaction time'),
    ]),
    KeyboardShortcutActionSubgroup('Codec', [
      KeyboardShortcutActionEntry(kShortcutActionCodecAuto, 'Auto'),
      KeyboardShortcutActionEntry(kShortcutActionCodecVp8, 'VP8'),
      KeyboardShortcutActionEntry(kShortcutActionCodecVp9, 'VP9'),
      KeyboardShortcutActionEntry(kShortcutActionCodecAv1, 'AV1'),
      KeyboardShortcutActionEntry(kShortcutActionCodecH264, 'H264'),
      KeyboardShortcutActionEntry(kShortcutActionCodecH265, 'H265'),
    ]),
    KeyboardShortcutActionSubgroup('Virtual display', [
      KeyboardShortcutActionEntry(
          kShortcutActionPlugOutAllVirtualDisplays, 'Plug out all'),
    ]),
    // 直接条目：cursorToggles + 显示开关，按工具栏顺序排列。
    KeyboardShortcutActionEntry(
        kShortcutActionToggleShowRemoteCursor, 'Show remote cursor'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleFollowRemoteCursor, 'Follow remote cursor'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleFollowRemoteWindow, 'Follow remote window focus'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleZoomCursor, 'Zoom cursor'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleQualityMonitor, 'Show quality monitor'),
    KeyboardShortcutActionEntry(kShortcutActionToggleMute, 'Mute'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleEnableFileCopyPaste, 'Enable file copy and paste'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleDisableClipboard, 'Disable clipboard'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleLockAfterSessionEnd, 'Lock after session end'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleTrueColor, 'True color (4:4:4)'),
    // Privacy mode 位于底部——与 `_DisplayMenu` 一致，在那里它是
    // 最后添加的子菜单（remote_toolbar.dart 第 1023 行左右，在各开关之后）。
    KeyboardShortcutActionSubgroup('Privacy mode', [
      // 复用工具栏现有的 impl 名称 i18n key。位于
      // runtime 的处理函数会将 `privacy_mode_impl_mag_tip` /
      // `privacy_mode_impl_virtual_display_tip` 与对端所
      // 声明的 impl 进行匹配——与工具栏的 `toolbarPrivacyMode`
      // 子菜单使用的是同一套逻辑。
      KeyboardShortcutActionEntry(
          kShortcutActionPrivacyMode1, 'privacy_mode_impl_mag_tip'),
      KeyboardShortcutActionEntry(
          kShortcutActionPrivacyMode2, 'privacy_mode_impl_virtual_display_tip'),
    ]),
  ]),
  // Keyboard：Keyboard mode 子分组在前，然后是直接条目
  // （inputSource → viewMode → showMyCursor → toolbarKeyboardToggles），
  // 与 `_KeyboardMenu.menuChildrenGetter` 一致。
  KeyboardShortcutActionGroup('Keyboard', [
    KeyboardShortcutActionSubgroup('Keyboard mode', [
      KeyboardShortcutActionEntry(
          kShortcutActionKeyboardModeLegacy, 'Legacy mode'),
      KeyboardShortcutActionEntry(kShortcutActionKeyboardModeMap, 'Map mode'),
      KeyboardShortcutActionEntry(
          kShortcutActionKeyboardModeTranslate, 'Translate mode'),
    ]),
    KeyboardShortcutActionEntry(kShortcutActionToggleViewOnly, 'View Mode'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleShowMyCursor, 'Show my cursor'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleSwapCtrlCmd, 'Swap control-command key'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleRelativeMouseMode, 'Relative mouse mode'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleReverseMouseWheel, 'Reverse mouse wheel'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleSwapLeftRightMouse, 'swap-left-right-mouse'),
  ]),
  KeyboardShortcutActionGroup('Chat', [
    KeyboardShortcutActionEntry(kShortcutActionToggleChat, 'Text chat'),
    KeyboardShortcutActionEntry(kShortcutActionToggleVoiceCall, 'Voice call'),
  ]),
  // "Other" 收集那些没有下拉菜单的单图标工具栏按钮
  // （Pin、Close），以及完全没有工具栏条目的操作（Fullscreen——
  // 由回调驱动而非菜单；Toggle Toolbar / 标签页导航——标签页
  // 右键菜单，而不是工具栏）。它们被合并为一个分组，而不是
  // 拆成若干个只有 1 个条目的分组，以获得更清晰的视觉层次。
  KeyboardShortcutActionGroup('Other', [
    KeyboardShortcutActionEntry(kShortcutActionPinToolbar, 'Pin Toolbar'),
    KeyboardShortcutActionEntry(
        kShortcutActionToggleFullscreen, 'Toggle fullscreen'),
    KeyboardShortcutActionEntry(kShortcutActionToggleToolbar, 'Toggle toolbar'),
    KeyboardShortcutActionEntry(kShortcutActionCloseTab, 'Close tab'),
    KeyboardShortcutActionEntry(
        kShortcutActionSwitchTabNext, 'Switch to next tab'),
    KeyboardShortcutActionEntry(
        kShortcutActionSwitchTabPrev, 'Switch to previous tab'),
  ]),
];

/// 遍历（已过滤或未过滤的）分组树，并产出每一个
/// [KeyboardShortcutActionEntry]，无论它是作为顶级分组的直接
/// 子项还是位于某个子分组内。适用于名称
/// 查找、幽灵操作（ghost-action）测试，以及任何只需要扁平
/// 操作 id 列表的使用方。
Iterable<KeyboardShortcutActionEntry> allActionEntries(
  Iterable<KeyboardShortcutActionGroup> groups,
) sync* {
  for (final group in groups) {
    for (final child in group.children) {
      switch (child) {
        case KeyboardShortcutActionEntry():
          yield child;
        case KeyboardShortcutActionSubgroup():
          yield* child.entries;
      }
    }
  }
}

/// 返回 [kKeyboardShortcutActionGroups]，其中当前平台不支持的操作已被
/// 剔除。若某个子分组的所有条目都被
/// 过滤掉，则丢弃该子分组；若顶级分组的所有子项（直接条目
/// 或子分组）都被丢弃，则该顶级分组自身也被丢弃。
///
/// 与 [filterDefaultBindingsForPlatform] 使用的能力标志保持一致
/// 这样配置 UI 只显示匹配器在当前平台上真正
/// 能派发的内容。
///
/// 注意：调用方仍应遍历未过滤的
/// [kKeyboardShortcutActionGroups] 以进行名称查找（例如针对过期跨平台绑定的
/// 冲突警告），这样在桌面端
/// 绑定并延续到移动端的操作在对话框中
/// 仍然有可读的名称。
List<KeyboardShortcutActionGroup> filterKeyboardShortcutActionGroupsForPlatform(
  ShortcutPlatformCapabilities cap,
) {
  bool allowed(String id) {
    if (cap.excludedActions.contains(id)) return false;
    if (!cap.includeFullscreenShortcut &&
        id == kShortcutActionToggleFullscreen) {
      return false;
    }
    if (!cap.includeScreenshotShortcut && id == kShortcutActionScreenshot) {
      return false;
    }
    if (!cap.includeRelativeMouseModeShortcut &&
        id == kShortcutActionToggleRelativeMouseMode) {
      return false;
    }
    if (!cap.includeTabShortcuts && isSwitchTabShortcutAction(id)) return false;
    if (!cap.includeToolbarShortcut && id == kShortcutActionToggleToolbar) {
      return false;
    }
    if (!cap.includeCloseTabShortcut && id == kShortcutActionCloseTab) {
      return false;
    }
    if (!cap.includeSwitchSidesShortcut && id == kShortcutActionSwitchSides) {
      return false;
    }
    if (!cap.includeRecordingShortcut && id == kShortcutActionToggleRecording) {
      return false;
    }
    if (!cap.includeResetCanvasShortcut && id == kShortcutActionResetCanvas) {
      return false;
    }
    if (!cap.includePinToolbarShortcut && id == kShortcutActionPinToolbar) {
      return false;
    }
    if (!cap.includeViewModeShortcut &&
        (id == kShortcutActionViewModeOriginal ||
            id == kShortcutActionViewModeAdaptive ||
            id == kShortcutActionViewModeCustom)) {
      return false;
    }
    if (!cap.includeVoiceCallShortcut && id == kShortcutActionToggleVoiceCall) {
      return false;
    }
    return true;
  }

  final out = <KeyboardShortcutActionGroup>[];
  for (final group in kKeyboardShortcutActionGroups) {
    final filteredChildren = <KeyboardShortcutActionGroupChild>[];
    for (final child in group.children) {
      switch (child) {
        case KeyboardShortcutActionEntry():
          if (allowed(child.id)) filteredChildren.add(child);
        case KeyboardShortcutActionSubgroup():
          final entries =
              child.entries.where((e) => allowed(e.id)).toList();
          if (entries.isNotEmpty) {
            filteredChildren.add(
                KeyboardShortcutActionSubgroup(child.titleKey, entries));
          }
      }
    }
    if (filteredChildren.isNotEmpty) {
      out.add(KeyboardShortcutActionGroup(group.titleKey, filteredChildren));
    }
  }
  return out;
}
