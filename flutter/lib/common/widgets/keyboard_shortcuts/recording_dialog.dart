// flutter/lib/common/widgets/keyboard_shortcuts/recording_dialog.dart
//
// 键盘快捷键设置页面使用的模态对话框，用于捕获新的
// 按键组合并绑定到指定操作。该对话框监听 KeyDown 事件，
// 提取修饰键集合 + 非修饰键，校验至少存在一个
// 修饰键，并报告与另一个已绑定操作之间的
// 冲突。
//
// 点击保存时，返回新的绑定映射（{action, mods, key}），以及
// 需要清除其绑定的操作的可选 id（冲突时的
// "替换"路径）。点击取消时返回 null。

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../../../common.dart';
import 'shortcut_utils.dart';

/// 录制对话框的结果。
class RecordingResult {
  /// 要写入的新绑定映射：{action, mods, key}。
  final Map<String, dynamic> binding;

  /// 如果所选组合与另一个操作冲突，说明用户选择了
  /// "替换"——调用方必须先清除该操作的绑定，再写入
  /// 新的绑定。
  final String? clearActionId;

  RecordingResult(this.binding, this.clearActionId);
}

/// 显示录制对话框。
///
/// [actionId] 是正在编辑的操作（用于标题以及检测
/// "绑定到自身"——这不算冲突）。
/// [actionLabel] 是经过翻译、面向用户的操作名称。
/// [existingBindings] 是当前的绑定列表（用于冲突检测）。
/// [actionLabelLookup] 把 actionId 解析为其翻译后的名称，用于
/// 冲突警告。
Future<RecordingResult?> showRecordingDialog({
  required BuildContext context,
  required String actionId,
  required String actionLabel,
  required List<Map<String, dynamic>> existingBindings,
  required String Function(String) actionLabelLookup,
}) {
  return showDialog<RecordingResult>(
    context: context,
    barrierDismissible: false,
    builder: (ctx) => _RecordingDialog(
      actionId: actionId,
      actionLabel: actionLabel,
      existingBindings: existingBindings,
      actionLabelLookup: actionLabelLookup,
    ),
  );
}

class _RecordingDialog extends StatefulWidget {
  final String actionId;
  final String actionLabel;
  final List<Map<String, dynamic>> existingBindings;
  final String Function(String) actionLabelLookup;

  const _RecordingDialog({
    required this.actionId,
    required this.actionLabel,
    required this.existingBindings,
    required this.actionLabelLookup,
  });

  @override
  State<_RecordingDialog> createState() => _RecordingDialogState();
}

class _RecordingDialogState extends State<_RecordingDialog> {
  final FocusNode _focusNode = FocusNode();

  // 已捕获的组合。在用户按下带有非修饰键的按键之前为 null。
  Set<String> _mods = {};
  String? _key;

  // 最近一次无法绑定按键的可读名称
  // （例如 F13、媒体键）。当上一次按键要么受支持、要么
  // 只是修饰键时为 null。只要有受支持的按键到来就会清除，因此
  // 用户在成功捕获之后又按下不受支持的按键时，会看到该警告
  // 直到按下其他按键。它与 `_key == null` 不同，这样
  // 状态行就能告诉用户*为什么*这次按键被忽略，而不是
  // 毫无反应。
  String? _unsupportedKey;

  // 当映射不到键名时，我们*不*应视为"不受支持"的
  // 修饰键 LogicalKeyboardKeys。只按下修饰键在
  // 组合捕获过程中是正常的（用户正在逐步组合按键）——只有非修饰键
  // 映射不到键名时才值得给出警告。
  static final _modifierKeys = <LogicalKeyboardKey>{
    LogicalKeyboardKey.shift,
    LogicalKeyboardKey.shiftLeft,
    LogicalKeyboardKey.shiftRight,
    LogicalKeyboardKey.control,
    LogicalKeyboardKey.controlLeft,
    LogicalKeyboardKey.controlRight,
    LogicalKeyboardKey.alt,
    LogicalKeyboardKey.altLeft,
    LogicalKeyboardKey.altRight,
    LogicalKeyboardKey.meta,
    LogicalKeyboardKey.metaLeft,
    LogicalKeyboardKey.metaRight,
    LogicalKeyboardKey.capsLock,
    LogicalKeyboardKey.numLock,
    LogicalKeyboardKey.scrollLock,
    LogicalKeyboardKey.fn,
    LogicalKeyboardKey.fnLock,
  };

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _focusNode.requestFocus();
    });
  }

  @override
  void dispose() {
    _focusNode.dispose();
    super.dispose();
  }

  bool get _isMac =>
      defaultTargetPlatform == TargetPlatform.macOS ||
      defaultTargetPlatform == TargetPlatform.iOS;

  /// 当捕获到的组合中至少包含一个修饰键时为 true。这是任何合理绑定的下限
  /// ——纯单键绑定会在快捷键启用后立即吞掉正常的
  /// 输入。超过一个修饰键则由用户自行决定；
  /// 会话内的直通（pass-through）开关是应急出口，当
  /// 所选组合与远端需要的功能冲突时可以使用。
  bool get _hasRequiredPrefix => _mods.isNotEmpty;

  /// 返回该组合当前冲突的操作 id，若无冲突则返回 null。
  /// 正在编辑的操作不与自身冲突。
  String? get _conflictActionId {
    if (_key == null || !_hasRequiredPrefix) return null;
    for (final b in widget.existingBindings) {
      final otherAction = b['action'] as String?;
      if (otherAction == null || otherAction == widget.actionId) continue;
      final otherKey = b['key'] as String?;
      final otherMods = shortcutModSetFrom(b['mods']);
      if (otherKey == _key &&
          otherMods.length == _mods.length &&
          otherMods.containsAll(_mods)) {
        return otherAction;
      }
    }
    return null;
  }

  KeyEventResult _onKeyEvent(FocusNode node, KeyEvent event) {
    if (event is KeyDownEvent &&
        event.logicalKey == LogicalKeyboardKey.escape) {
      Navigator.of(context).pop();
      return KeyEventResult.handled;
    }
    if (event is! KeyDownEvent) return KeyEventResult.handled;

    final logical = event.logicalKey;
    final keyName = shortcutKeyNameForEvent(event);

    // 对应 src/keyboard/shortcuts.rs 中 `normalize_modifiers` 的镜像实现：
    //   * macOS：Cmd → primary，Ctrl → ctrl（两者不同）。
    //   * Win/Linux：Ctrl → primary，没有单独的 Ctrl 修饰键。
    // 两端的标签必须一致，否则保存的绑定将无法
    // 匹配 matcher 在运行时看到的事件。
    final mods = <String>{};
    if (HardwareKeyboard.instance.isAltPressed) mods.add('alt');
    if (HardwareKeyboard.instance.isShiftPressed) mods.add('shift');
    if (_isMac) {
      if (HardwareKeyboard.instance.isMetaPressed) mods.add('primary');
      if (HardwareKeyboard.instance.isControlPressed) mods.add('ctrl');
    } else {
      if (HardwareKeyboard.instance.isControlPressed) mods.add('primary');
    }

    setState(() {
      _mods = mods;
      if (!_isMac &&
          HardwareKeyboard.instance.logicalKeysPressed
              .contains(LogicalKeyboardKey.altGraph)) {
        _key = null;
        _unsupportedKey = LogicalKeyboardKey.altGraph.keyLabel;
        return;
      }
      // 与运行时的派发行为保持一致：按住字母键之后才按下的修饰键不会
      // 产生新的快捷键按下事件。
      _key = keyName;
      _unsupportedKey = null;
      if (keyName == null && !_modifierKeys.contains(logical)) {
        // 我们不认识的非修饰键（例如 F13、媒体键、输入法
        // 组合键）。给出警告，而不是静默丢弃这次
        // 按键——否则对话框看起来像是没有响应。
        final label = logical.keyLabel.isNotEmpty
            ? logical.keyLabel
            : (logical.debugName ?? 'this key');
        _unsupportedKey = label;
      }
    });
    return KeyEventResult.handled;
  }

  KeyEventResult _onRawKeyEvent(FocusNode node, RawKeyEvent event) {
    final data = event.data;
    const gtkMod5Mask = 1 << 7;
    // GTK 只能通过 Mod5 暴露 AltGr，而没有逻辑上的 AltGraph 键。
    if (event is RawKeyDownEvent &&
        data is RawKeyEventDataLinux &&
        data.keyHelper is GtkKeyHelper &&
        (data.modifiers & gtkMod5Mask) != 0) {
      setState(() {
        _key = null;
        _unsupportedKey = LogicalKeyboardKey.altGraph.keyLabel;
      });
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  void _onSave() {
    if (_key == null || !_hasRequiredPrefix) return;
    final ordered = canonicalShortcutModsForSave(_mods);
    final binding = <String, dynamic>{
      'action': widget.actionId,
      'mods': ordered,
      'key': _key!,
    };
    Navigator.of(context).pop(RecordingResult(binding, _conflictActionId));
  }

  String _formatPrefix() {
    // 用于"必须包含……"校验行；列出绑定可选择的修饰键集合。
    // 这里不使用本地化的修饰键字形，这样
    // 用户搜索 "Option" / "Cmd" 时这些名称仍可被检索到。
    if (_isMac) return 'Cmd / Control / Option / Shift';
    return 'Ctrl / Alt / Shift';
  }

  String _formatCombo() {
    // 纯文本标签（同样的理由见 display.dart::_keyDisplay）。
    final parts = <String>[];
    for (final m in ['primary', 'ctrl', 'alt', 'shift']) {
      if (!_mods.contains(m)) continue;
      switch (m) {
        case 'primary':
          parts.add(_isMac ? 'Cmd' : 'Ctrl');
          break;
        case 'ctrl':
          parts.add(_isMac ? 'Control' : 'Ctrl');
          break;
        case 'alt':
          parts.add(_isMac ? 'Option' : 'Alt');
          break;
        case 'shift':
          parts.add('Shift');
          break;
      }
    }
    if (_key != null) {
      parts.add(_keyDisplay(_key!));
    }
    if (parts.isEmpty) return translate('shortcut-recording-press-keys-tip');
    return parts.join('+');
  }

  String _keyDisplay(String key) {
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
    return key.toUpperCase();
  }

  @override
  Widget build(BuildContext context) {
    final hasKey = _key != null;
    final conflictId = _conflictActionId;
    final hasConflict = conflictId != null;
    final canSave = hasKey && _hasRequiredPrefix;

    Widget statusLine;
    if (_unsupportedKey != null) {
      // 最近一次按下的是不受支持的按键。它的优先级高于
      // 已捕获组合的各种状态，这样用户能明确得知自己
      // 上一次按键被忽略了，无论之前是否
      // 仍有已捕获的组合。
      statusLine = Row(
        children: [
          const Icon(Icons.close, size: 16, color: Colors.red),
          const SizedBox(width: 6),
          Flexible(
            child: Text(
              translate('shortcut-key-not-supported')
                  .replaceAll('{}', _unsupportedKey!),
              style: const TextStyle(color: Colors.red),
            ),
          ),
        ],
      );
    } else if (!hasKey) {
      statusLine = Text(
        translate('shortcut-recording-press-keys-tip'),
        style: TextStyle(color: Theme.of(context).hintColor),
      );
    } else if (!_hasRequiredPrefix) {
      statusLine = Row(
        children: [
          Icon(Icons.close, size: 16, color: Colors.red),
          const SizedBox(width: 6),
          Flexible(
            child: Text(
              translate('shortcut-must-include-modifiers')
                  .replaceAll('{}', _formatPrefix()),
              style: const TextStyle(color: Colors.red),
            ),
          ),
        ],
      );
    } else if (hasConflict) {
      final otherLabel = widget.actionLabelLookup(conflictId);
      statusLine = Row(
        children: [
          Icon(Icons.warning_amber_outlined,
              size: 16, color: Colors.orange.shade700),
          const SizedBox(width: 6),
          Flexible(
            child: Text(
              '${translate('shortcut-already-bound-to')} "$otherLabel"',
              style: TextStyle(color: Colors.orange.shade700),
            ),
          ),
        ],
      );
    } else {
      statusLine = Row(
        children: [
          const Icon(Icons.check, size: 16, color: Colors.green),
          const SizedBox(width: 6),
          Text(translate('Valid'), style: const TextStyle(color: Colors.green)),
        ],
      );
    }

    final saveLabel = hasConflict ? 'Replace' : 'Save';

    return AlertDialog(
      title: Text(
        '${translate('Set shortcut')}: ${widget.actionLabel}',
      ),
      content: Focus(
        focusNode: _focusNode,
        autofocus: true,
        onKeyEvent: _onKeyEvent,
        onKey: _onRawKeyEvent,
        child: ConstrainedBox(
          constraints: const BoxConstraints(minWidth: 380),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(translate('shortcut-recording-instruction')),
              const SizedBox(height: 12),
              Container(
                width: double.infinity,
                padding:
                    const EdgeInsets.symmetric(vertical: 18, horizontal: 12),
                decoration: BoxDecoration(
                  border: Border.all(color: Theme.of(context).dividerColor),
                  borderRadius: BorderRadius.circular(4),
                ),
                child: Text(
                  _formatCombo(),
                  textAlign: TextAlign.center,
                  style: TextStyle(
                    fontSize: 18,
                    fontWeight: FontWeight.w600,
                    color: hasKey
                        ? Theme.of(context).textTheme.titleLarge?.color
                        : Theme.of(context).hintColor,
                  ),
                ),
              ),
              const SizedBox(height: 12),
              statusLine,
            ],
          ),
        ),
      ),
      actions: [
        dialogButton('Cancel',
            onPressed: () => Navigator.of(context).pop(), isOutline: true),
        dialogButton(saveLabel, onPressed: canSave ? _onSave : null),
      ],
    );
  }
}
