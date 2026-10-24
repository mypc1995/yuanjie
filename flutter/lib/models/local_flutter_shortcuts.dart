import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import '../common/widgets/keyboard_shortcuts/shortcut_constants.dart';

class LocalFlutterShortcutDispatcher {
  final void Function(String) onTriggered;
  // 已触发快捷键但其抬起事件尚未到达的按键。
  // 归属是物理层面的：由所有会话共享，因为快捷键
  // 可能在按键抬起之前切换焦点或关闭它自己的会话，而且
  // 它不会随会话一起被丢弃。抬起事件丢失的按键会自我修复：
  // 它的下一次按下会被当作重复键消费掉，而那次抬起会把它移除。
  static final _firedKeys = <PhysicalKeyboardKey>{};
  // 等待触发它们的按键抬起的动作，以及触发它们的
  // 会话。参见 [kShortcutActionsRunOnKeyUp]。
  static final _keyUpActions =
      <PhysicalKeyboardKey, (LocalFlutterShortcutDispatcher, String)>{};
  static final _clipboardModifierReleases =
      <PhysicalKeyboardKey, (LocalFlutterShortcutDispatcher, Future<void>)>{};
  final _releasedModifiers = <PhysicalKeyboardKey>{};
  final _forwardedKeyReleases =
      <PhysicalKeyboardKey, Future<void> Function()>{};
  bool _viewOnlyShortcutPending = false;
  int _generation = 0;

  LocalFlutterShortcutDispatcher({required this.onTriggered});

  /// 丢弃该会话待处理的动作和修饰符重放。已触发的按键
  /// 在抬起之前仍保持归属，无论由哪个会话接收。
  void clear() {
    _keyUpActions.removeWhere((_, pending) => identical(pending.$1, this));
    _clipboardModifierReleases
        .removeWhere((_, pending) => identical(pending.$1, this));
    _releasedModifiers.clear();
    _forwardedKeyReleases.clear();
    _viewOnlyShortcutPending = false;
    _generation++;
  }

  @visibleForTesting
  static void resetFiredKeys() {
    _firedKeys.clear();
    _keyUpActions.clear();
    _clipboardModifierReleases.clear();
  }

  void recordReleasedModifiers(Iterable<PhysicalKeyboardKey> keys) {
    _releasedModifiers.addAll(keys);
  }

  void recordForwardedKey(PhysicalKeyboardKey key,
      {required bool down, required Future<void> Function() release}) {
    if (_isModifier(key)) return;
    if (down) {
      _forwardedKeyReleases.putIfAbsent(key, () => release);
    } else {
      _forwardedKeyReleases.remove(key);
    }
  }

  bool resumeModifiersAfterViewOnly(
      PhysicalKeyboardKey incoming, Set<PhysicalKeyboardKey> pressed) {
    if (!_viewOnlyShortcutPending) return false;
    _viewOnlyShortcutPending = false;
    _releasedModifiers
        .addAll(pressed.where((key) => key != incoming && _isModifier(key)));
    return true;
  }

  List<PhysicalKeyboardKey> takeModifiersToRestore(
          KeyEvent event, Set<PhysicalKeyboardKey> pressed) =>
      _takeModifiersToRestore(event.physicalKey,
          event is KeyDownEvent || event is KeyRepeatEvent, pressed);

  List<PhysicalKeyboardKey> takeRawModifiersToRestore(
          RawKeyEvent event, Set<PhysicalKeyboardKey> pressed) =>
      _takeModifiersToRestore(
          event.physicalKey, event is RawKeyDownEvent, pressed);

  List<PhysicalKeyboardKey> _takeModifiersToRestore(
      PhysicalKeyboardKey incoming,
      bool down,
      Set<PhysicalKeyboardKey> pressed) {
    _releasedModifiers.removeWhere((key) => !pressed.contains(key));
    if (!down || _isModifier(incoming)) return [];
    final keys = _releasedModifiers.toList();
    _releasedModifiers.clear();
    return keys;
  }

  bool _isModifier(PhysicalKeyboardKey key) {
    // HID 键盘的修饰键使用码是连续的。
    return key.usbHidUsage >= PhysicalKeyboardKey.controlLeft.usbHidUsage &&
        key.usbHidUsage <= PhysicalKeyboardKey.metaRight.usbHidUsage;
  }

  bool tryDispatch(
    KeyEvent event, {
    bool viewOnly = false,
    String? Function()? match,
    Future<void> Function()? releaseModifiers,
  }) =>
      _tryDispatch(
        event.physicalKey,
        down: event is KeyDownEvent,
        up: event is KeyUpEvent,
        viewOnly: viewOnly,
        match: match,
        releaseModifiers: releaseModifiers,
      );

  bool tryDispatchRaw(
    RawKeyEvent event, {
    bool viewOnly = false,
    String? Function()? match,
    Future<void> Function()? releaseModifiers,
  }) =>
      _tryDispatch(
        event.physicalKey,
        down: event is RawKeyDownEvent && !event.repeat,
        up: event is RawKeyUpEvent,
        viewOnly: viewOnly,
        match: match,
        releaseModifiers: releaseModifiers,
      );

  bool _tryDispatch(
    PhysicalKeyboardKey key, {
    required bool down,
    required bool up,
    required bool viewOnly,
    String? Function()? match,
    Future<void> Function()? releaseModifiers,
  }) {
    if (up) {
      if (!_firedKeys.remove(key)) return false;
      final pending = _keyUpActions.remove(key);
      final release = _clipboardModifierReleases.remove(key)?.$2;
      if (pending != null) {
        final (owner, action) = pending;
        unawaited(
            owner._trigger(action, release == null ? null : () => release));
      }
      return true;
    }
    // Safari 可能会省略之前按住 Cmd 时的抬起事件。新的浏览器 keydown
    // 标志着一次新的按下；KeyRepeatEvent 仍应归属于原来的组合键。
    if (kIsWeb && defaultTargetPlatform == TargetPlatform.macOS && down) {
      _firedKeys.remove(key);
    }
    if (_firedKeys.contains(key)) return true;
    if (!down) return false;
    final action = match?.call();
    if (action == null) return false;
    _firedKeys.add(key);
    if (viewOnly) _viewOnlyShortcutPending = true;
    // macOS 浏览器在按住 Cmd 时可能省略字母键的抬起事件。与 JS
    // 输入源一样，在那里于 keydown 时派发；远端清理仍然先执行。
    // Web 剪贴板读取也需要 keydown 带来的浏览器用户激活。
    final runOnKeyUp =
        !(kIsWeb && defaultTargetPlatform == TargetPlatform.macOS) &&
            (kShortcutActionsRunOnKeyUp.contains(action) ||
                (!kIsWeb && action == kShortcutActionSendClipboardKeystrokes));
    if (runOnKeyUp) {
      _keyUpActions[key] = (this, action);
    } else {
      _keyUpActions.remove(key);
    }
    if (runOnKeyUp &&
        action == kShortcutActionSendClipboardKeystrokes &&
        releaseModifiers != null) {
      // 抬起事件可能先于异步 FFI 调用完成。
      final release = Future<void>.sync(releaseModifiers);
      _clipboardModifierReleases[key] = (this, release);
      unawaited(_trigger(null, () => release));
    } else {
      unawaited(_trigger(runOnKeyUp ? null : action, releaseModifiers));
    }
    return true;
  }

  /// 先释放远端修饰键，然后执行 [action]，除非它为 null 或
  /// 期间该会话已被清除。
  Future<void> _trigger(
      String? action, Future<void> Function()? releaseModifiers) async {
    final generation = _generation;
    try {
      if (releaseModifiers != null) await releaseModifiers();
      if (action == null || generation != _generation) return;
      if ((action == kShortcutActionToggleViewOnly ||
              action == kShortcutActionToggleShowMyCursor ||
              action == kShortcutActionKeyboardModeMap ||
              action == kShortcutActionKeyboardModeTranslate ||
              action == kShortcutActionKeyboardModeLegacy ||
              kShortcutActionsRunOnKeyUp.contains(action) ||
              (!kIsWeb && action == kShortcutActionSendClipboardKeystrokes)) &&
          _forwardedKeyReleases.isNotEmpty) {
        // 焦点变化和仅查看模式可能导致来自对端的物理抬起事件收不到。
        // 模式变化可能使后续抬起事件的编码方式与按下事件不同。
        final releases = _forwardedKeyReleases.values.toList();
        _forwardedKeyReleases.clear();
        await Future.wait(releases.map((release) => Future<void>.sync(release)));
      }
      if (generation == _generation) onTriggered(action);
    } catch (e, st) {
      debugPrint('Local shortcut failed for $action: $e\n$st');
    }
  }
}
