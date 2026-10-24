// flutter/lib/common/widgets/keyboard_shortcuts/display.dart
import 'dart:convert';
import 'package:flutter/foundation.dart';
import '../../../consts.dart';
import '../../../models/platform_model.dart';
import 'shortcut_utils.dart';

/// 读取绑定 JSON，为指定的动作生成易于阅读的快捷键字符串，
/// `actionId` 按当前操作系统格式化。若未绑定则返回 null，或者——
/// 当 [requireEnabled] 为 true（默认值）时——当主开关
/// 处于关闭状态时。配置页面传入 `requireEnabled: false`，这样用户在功能禁用时
/// 仍能看到自己绑定的按键。
class ShortcutDisplay {
  // 以原始字符串为键缓存解析后的 JSON —— 每次菜单重建时
  // 都会针对每个可见动作调用，因此 jsonDecode 才是真正的开销。失效处理是
  // 自动的：写入会改变原始字符串，我们就重新解析。
  static String? _cachedRaw;
  static Map<String, dynamic>? _cachedParsed;

  @visibleForTesting
  static void resetCache() {
    _cachedRaw = null;
    _cachedParsed = null;
  }

  static String? formatFor(String actionId, {bool requireEnabled = true}) {
    final raw = bind.mainGetLocalOption(key: kShortcutLocalConfigKey);
    if (raw.isEmpty) return null;
    Map<String, dynamic>? parsed;
    if (raw == _cachedRaw) {
      parsed = _cachedParsed;
    } else {
      try {
        parsed = jsonDecode(raw) as Map<String, dynamic>;
      } catch (_) {
        parsed = null;
      }
      _cachedRaw = raw;
      _cachedParsed = parsed;
    }
    if (parsed == null) return null;
    if (requireEnabled && parsed['enabled'] != true) return null;
    // 当透传开启时，匹配器会在每次按键时提前返回。
    // 在菜单项旁显示已绑定的组合键会误导用户——他们
    // 按下后会以为会触发本地动作，结果按键却会被发送到
    // 远程端。显示时按未绑定处理。
    if (requireEnabled && parsed['pass_through'] == true) return null;
    final list = shortcutBindingMapsFrom(parsed['bindings']);
    final found = list.firstWhere(
      (b) => b['action'] == actionId,
      orElse: () => {},
    );
    if (found.isEmpty) return null;

    // 防止手工编辑或损坏的配置中 `key` 缺失
    // 或不是字符串 —— 静默地把该绑定视为未绑定，而不是
    // 导致工具栏渲染崩溃。
    final keyValue = found['key'];
    if (keyValue is! String) return null;

    final isMac = defaultTargetPlatform == TargetPlatform.macOS ||
                  defaultTargetPlatform == TargetPlatform.iOS;
    // `mods` 同样可能格式错误；把非列表视为没有修饰键。
    final modsRaw = found['mods'];
    final mods = modsRaw is List
        ? modsRaw.whereType<String>().toList()
        : const <String>[];
    // 使用纯文本标签（Cmd / Ctrl / Alt / Shift）而不是 Unicode 符号
    // （⌘ ⌃ ⌥ ⇧）。Flutter Web 的 CanvasKit 内置字体并不总是包含
    // macOS 的修饰键符号，在 Mac 浏览器上会显示成乱码方块；
    // 文本在所有平台上都通用且易读。
    //
    // 顺序与 macOS 的标准顺序一致（Cmd, Control, Option, Shift），
    // 这样渲染出的提示读起来才自然。`ctrl` 只出现在
    // macOS 保存的绑定中 —— Win/Linux 会把 Ctrl 合并为 `primary`。
    final parts = <String>[];
    for (final m in ['primary', 'ctrl', 'alt', 'shift']) {
      if (!mods.contains(m)) continue;
      switch (m) {
        case 'primary': parts.add(isMac ? 'Cmd' : 'Ctrl'); break;
        case 'ctrl':    parts.add(isMac ? 'Control' : 'Ctrl'); break;
        case 'alt':     parts.add(isMac ? 'Option' : 'Alt'); break;
        case 'shift':   parts.add('Shift'); break;
      }
    }
    parts.add(_keyDisplay(keyValue));
    return parts.join('+');
  }

  static String _keyDisplay(String key) {
    switch (key) {
      case 'delete':     return 'Del';
      case 'backspace':  return 'Backspace';
      case 'enter':      return 'Enter';
      case 'tab':        return 'Tab';
      case 'space':      return 'Space';
      case 'arrow_left': return 'Left';
      case 'arrow_right':return 'Right';
      case 'arrow_up':   return 'Up';
      case 'arrow_down': return 'Down';
      case 'home':       return 'Home';
      case 'end':        return 'End';
      case 'page_up':    return 'PgUp';
      case 'page_down':  return 'PgDn';
      case 'insert':     return 'Ins';
    }
    if (key.startsWith('digit')) return key.substring(5);
    // F 键（"f1".."f12"）和单个字母直接转为大写。
    return key.toUpperCase();
  }
}
