import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import 'shortcut_constants.dart';

List<String> canonicalShortcutModsForSave(Set<String> mods) {
  return <String>[
    if (mods.contains('primary')) 'primary',
    if (mods.contains('ctrl')) 'ctrl',
    if (mods.contains('alt')) 'alt',
    if (mods.contains('shift')) 'shift',
  ];
}

List<Map<String, dynamic>> shortcutBindingMapsFrom(dynamic rawBindings) {
  if (rawBindings is! Iterable) return <Map<String, dynamic>>[];
  final bindings = <Map<String, dynamic>>[];
  for (final raw in rawBindings) {
    if (raw is! Map) continue;
    final mods = raw['mods'];
    if (raw['action'] is! String ||
        raw['key'] is! String ||
        mods is! List ||
        mods.any((mod) =>
            !const {'primary', 'ctrl', 'alt', 'shift'}.contains(mod))) {
      continue;
    }
    final binding = <String, dynamic>{};
    for (final entry in raw.entries) {
      final key = entry.key;
      if (key is String) {
        binding[key] = entry.value;
      }
    }
    if (binding.isNotEmpty) {
      bindings.add(binding);
    }
  }
  return bindings;
}

Set<String> shortcutModSetFrom(dynamic rawMods) {
  if (rawMods is! Iterable) return <String>{};
  return rawMods.whereType<String>().toSet();
}

/// 启用快捷键时是否需要填充默认绑定。只有从未
/// 持有过绑定列表的配置才会被填充；空列表意味着
/// 用户清除了所有绑定，必须保持为空。
bool shouldSeedDefaultShortcutBindings(Map<String, dynamic> config) =>
    !config.containsKey('bindings');

bool isSwitchTabShortcutAction(String? actionId) {
  return actionId == kShortcutActionSwitchTabNext ||
      actionId == kShortcutActionSwitchTabPrev;
}

/// 所有可作为快捷键的按键的 USB HID usage（键盘页 0x07），
/// 映射到保存绑定时使用的规范键名。
const Map<int, String> _kUsbHidKeyNames = {
  0x04: 'a', 0x05: 'b', 0x06: 'c', 0x07: 'd', 0x08: 'e', 0x09: 'f',
  0x0A: 'g', 0x0B: 'h', 0x0C: 'i', 0x0D: 'j', 0x0E: 'k', 0x0F: 'l',
  0x10: 'm', 0x11: 'n', 0x12: 'o', 0x13: 'p', 0x14: 'q', 0x15: 'r',
  0x16: 's', 0x17: 't', 0x18: 'u', 0x19: 'v', 0x1A: 'w', 0x1B: 'x',
  0x1C: 'y', 0x1D: 'z',
  0x1E: 'digit1', 0x1F: 'digit2', 0x20: 'digit3', 0x21: 'digit4',
  0x22: 'digit5', 0x23: 'digit6', 0x24: 'digit7', 0x25: 'digit8',
  0x26: 'digit9', 0x27: 'digit0',
  0x28: 'enter', 0x2A: 'backspace', 0x2B: 'tab', 0x2C: 'space',
  0x3A: 'f1', 0x3B: 'f2', 0x3C: 'f3', 0x3D: 'f4', 0x3E: 'f5', 0x3F: 'f6',
  0x40: 'f7', 0x41: 'f8', 0x42: 'f9', 0x43: 'f10', 0x44: 'f11', 0x45: 'f12',
  0x49: 'insert', 0x4A: 'home', 0x4B: 'page_up', 0x4C: 'delete',
  0x4D: 'end', 0x4E: 'page_down', 0x4F: 'arrow_right', 0x50: 'arrow_left',
  0x51: 'arrow_down', 0x52: 'arrow_up',
  // 数字键盘 Enter 与主 Return 键共用 "enter" 这个名称，就像
  // Rust matcher（`Return | KpReturn`）和 Web matcher（`NumpadEnter`）那样。
  0x58: 'enter',
};

/// 把 [PhysicalKeyboardKey] 映射到保存绑定时使用的规范键名，
/// 对于不作为快捷键接受的按键则返回 `null`。
///
/// 绑定使用的是物理键位名称（美式布局名称），无论当前
/// 启用的是哪种键盘布局：原生 matcher 通过 USB
/// HID usage 看到按键（`rdev::usb_hid_key_from_code` -> `event_to_key_name`，位于
/// `src/keyboard/shortcuts.rs`），而 Web matcher 则通过
/// `KeyboardEvent.code`（`flutter/web/js/src/shortcut_matcher.ts`）。一致性
/// 通过针对 `flutter/test/fixtures/shortcut_key_usb_hid.json` 的校验来保证，方式是
/// 一个 Dart 测试和 Rust 的 `usb_hid_keys_match_fixture` 测试。
String? physicalKeyName(PhysicalKeyboardKey k) {
  final usage = k.usbHidUsage;
  if (usage >> 16 != 0x07) return null;
  return _kUsbHidKeyNames[usage & 0xFFFF];
}

/// [KeyEvent] 记录或匹配的键名。
String? shortcutKeyNameForEvent(KeyEvent e) => physicalKeyName(e.physicalKey);

/// 一组"该快捷键在当前平台上是否可用"的标志。
///
/// 生产代码通过以下方式获得唯一的事实来源：
/// [ShortcutModel.currentPlatformCapabilities]（它把各运行时的
/// 规则集中编码在一处）；测试则直接构造一份，带上想要验证的
/// 各个标志。有两个过滤函数会使用它：
/// [filterDefaultBindingsForPlatform]（用于在写入 LocalConfig 之前裁剪默认绑定的 JSON）
/// 和 [filterKeyboardShortcutActionGroupsForPlatform]
/// （用于裁剪配置 UI 的操作列表）。两者必须在以下方面保持一致：
/// 使用相同的能力集，否则可能会为某个用户没有任何界面可管理的
/// 操作填充默认绑定。
class ShortcutPlatformCapabilities {
  final bool includeFullscreenShortcut;
  final bool includeScreenshotShortcut;
  final bool includeRelativeMouseModeShortcut;
  final bool includeTabShortcuts;
  final bool includeToolbarShortcut;
  final bool includeCloseTabShortcut;
  final bool includeSwitchSidesShortcut;
  final bool includeRecordingShortcut;
  final bool includeResetCanvasShortcut;
  final bool includePinToolbarShortcut;
  final bool includeViewModeShortcut;
  final bool includeVoiceCallShortcut;
  final Set<String> excludedActions;

  const ShortcutPlatformCapabilities({
    required this.includeFullscreenShortcut,
    required this.includeScreenshotShortcut,
    required this.includeRelativeMouseModeShortcut,
    required this.includeTabShortcuts,
    required this.includeToolbarShortcut,
    required this.includeCloseTabShortcut,
    required this.includeSwitchSidesShortcut,
    required this.includeRecordingShortcut,
    required this.includeResetCanvasShortcut,
    required this.includePinToolbarShortcut,
    required this.includeViewModeShortcut,
    required this.includeVoiceCallShortcut,
    this.excludedActions = const {},
  });
}

List<Map<String, dynamic>> filterDefaultBindingsForPlatform(
  Iterable<dynamic> bindings,
  ShortcutPlatformCapabilities cap,
) {
  final filtered = <Map<String, dynamic>>[];
  for (final binding in shortcutBindingMapsFrom(bindings)) {
    final action = binding['action'] as String?;
    if (cap.excludedActions.contains(action)) continue;
    // 紧凑的 Apple 键盘可能把 Backspace 标注为 Delete，这让
    // 默认值产生歧义。使用 Backspace 还会与 Finder 的
    // "Empty Trash without confirmation" 快捷键在 macOS 上发生冲突。
    // 保持不绑定；用户仍然可以录制自己的快捷键。
    if ((defaultTargetPlatform == TargetPlatform.macOS ||
            defaultTargetPlatform == TargetPlatform.iOS) &&
        action == kShortcutActionSendCtrlAltDel) {
      continue;
    }
    if (!cap.includeFullscreenShortcut &&
        action == kShortcutActionToggleFullscreen) {
      continue;
    }
    if (!cap.includeScreenshotShortcut && action == kShortcutActionScreenshot) {
      continue;
    }
    if (!cap.includeRelativeMouseModeShortcut &&
        action == kShortcutActionToggleRelativeMouseMode) {
      continue;
    }
    if (!cap.includeTabShortcuts && isSwitchTabShortcutAction(action)) {
      continue;
    }
    if (!cap.includeToolbarShortcut &&
        action == kShortcutActionToggleToolbar) {
      continue;
    }
    if (!cap.includeCloseTabShortcut && action == kShortcutActionCloseTab) {
      continue;
    }
    if (!cap.includeSwitchSidesShortcut &&
        action == kShortcutActionSwitchSides) {
      continue;
    }
    if (!cap.includeRecordingShortcut &&
        action == kShortcutActionToggleRecording) {
      continue;
    }
    if (!cap.includeResetCanvasShortcut &&
        action == kShortcutActionResetCanvas) {
      continue;
    }
    if (!cap.includePinToolbarShortcut && action == kShortcutActionPinToolbar) {
      continue;
    }
    if (!cap.includeViewModeShortcut &&
        (action == kShortcutActionViewModeOriginal ||
            action == kShortcutActionViewModeAdaptive ||
            action == kShortcutActionViewModeCustom)) {
      continue;
    }
    if (!cap.includeVoiceCallShortcut &&
        action == kShortcutActionToggleVoiceCall) {
      continue;
    }
    filtered.add(binding);
  }
  return filtered;
}
