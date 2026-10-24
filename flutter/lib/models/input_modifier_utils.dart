import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';

/// 标识终端输入的来源，以便粘贴数据可以绕过所有
/// 纯键盘的转换。
enum TerminalInputSource {
  keyboard,
  paste,
}

/// 当移动端残留的一次性 Shift 状态应该被释放时返回 true
/// 即把已跟踪的 Shift 按下事件重放为合成的抬起事件。
///
/// 这只在移动端有效，即 Flutter 缓存的 Shift 状态仍为开启
/// （`cachedShiftPressed == true`），但当前的硬件/原始事件报告
/// Shift 为关闭（`actualShiftPressed == false`）。
///
/// 必须存在已跟踪的 Shift 按下事件，调用方才能安全地合成对应的
/// 抬起事件。`shiftLeft` 和 `shiftRight` 都被排除，因为
/// Shift 按键事件本身必须先被处理；否则我们可能在处理原本的
/// Shift 按下/抬起的同时，错误地释放已跟踪的按键。
/// 调用方应仅在缓存的修饰键状态已针对当前事件
/// 更新之后再评估这个条件。
///
/// 当它返回 true 时，调用方会记录类似下面的一行：
/// `input: releasing stale mobile Shift before replaying tracked raw key-up`
/// 紧接着调用 `_releaseTrackedRawShiftKeyEventIfNeeded()` 之前。
bool shouldReleaseStaleMobileShift({
  required bool isMobile,
  required bool cachedShiftPressed,
  required bool actualShiftPressed,
  required LogicalKeyboardKey logicalKey,
  required bool hasTrackedShiftKeyDown,
}) {
  if (!isMobile || !cachedShiftPressed || actualShiftPressed) {
    return false;
  }
  if (!hasTrackedShiftKeyDown) {
    return false;
  }
  if (logicalKey == LogicalKeyboardKey.shiftLeft ||
      logicalKey == LogicalKeyboardKey.shiftRight) {
    return false;
  }
  return true;
}

/// 把终端的一次性 Ctrl/Alt 修饰符应用到单个输入数据上。
///
String applyTerminalInputModifiers(
  String data, {
  required bool ctrlLocked,
  required bool altLocked,
}) {
  var result = data;
  if (ctrlLocked) {
    result = _applyTerminalCtrlModifier(result);
  }
  if (altLocked) {
    result = '\x1B$result';
  }
  return result;
}

/// 构建 xterm 为粘贴发送的精确数据，不应用任何修饰符。
String terminalPastePayload(String text, {required bool bracketedPasteMode}) {
  if (!bracketedPasteMode) {
    return text;
  }
  return '\x1B[200~$text\x1B[201~';
}

/// 返回一次性 Ctrl/Alt 是否可以转换并消费该输入。
///
/// xterm 会把终端控制键输出为单个控制字节或更长的
/// 转义序列。两种形式都不是普通文本输入，因此待处理的修饰符
/// 必须保留到用户输入可打印字符为止。
bool shouldApplyTerminalInputModifiers(String data) {
  if (data.characters.length != 1) return false;
  final codeUnit = data.codeUnitAt(0);
  return codeUnit >= 0x20 && codeUnit != 0x7F;
}

/// 构建发送到远端终端的键盘和粘贴输入数据。
///
/// 键盘输入会保留移动端回车变通方案以及一次性 Ctrl/Alt
/// 映射。粘贴输入刻意绕过这两种转换，因此即使只有一个
/// 字符的剪贴板数据也能被原样保留。
String prepareTerminalInputPayload(
  String data, {
  required TerminalInputSource source,
  required bool isMobileOrWebMobile,
  required bool bracketedPasteMode,
  required bool ctrlLocked,
  required bool altLocked,
}) {
  if (source == TerminalInputSource.paste) {
    return terminalPastePayload(
      data,
      bracketedPasteMode: bracketedPasteMode,
    );
  }

  var result = data;
  if (isMobileOrWebMobile && result == '\n') {
    result = '\r';
  }
  if ((ctrlLocked || altLocked) && shouldApplyTerminalInputModifiers(result)) {
    result = applyTerminalInputModifiers(
      result,
      ctrlLocked: ctrlLocked,
      altLocked: altLocked,
    );
  }
  return result;
}

/// 当硬件粘贴快捷键必须绕过键盘修饰符时返回 true。
///
/// 在常见情况下，xterm 已经处理了各平台的粘贴快捷键。
/// 只在虚拟 Ctrl/Alt 锁定处于活动状态时才拦截，因为 xterm 可能会
/// 在 bracketed paste 模式关闭时把单字符粘贴当作普通文本输出。
bool shouldHandleTerminalPasteShortcut({
  required TargetPlatform platform,
  required LogicalKeyboardKey logicalKey,
  required bool isKeyDown,
  required bool isKeyRepeat,
  required bool controlPressed,
  required bool metaPressed,
  required bool altPressed,
  required bool shiftPressed,
  required bool modifierLockActive,
}) {
  if (!modifierLockActive) return false;
  if (!isKeyDown && !isKeyRepeat) return false;
  if (logicalKey != LogicalKeyboardKey.keyV) return false;
  if (altPressed) return false;
  switch (platform) {
    case TargetPlatform.linux:
      return controlPressed && !metaPressed && shiftPressed;
    case TargetPlatform.iOS:
    case TargetPlatform.macOS:
      return !controlPressed && metaPressed && !shiftPressed;
    case TargetPlatform.android:
    case TargetPlatform.fuchsia:
    case TargetPlatform.windows:
      return controlPressed && !metaPressed && !shiftPressed;
  }
}

/// 当折叠 Row3 时还应清除隐藏的修饰键状态则返回 true。
bool shouldClearTerminalModifiersWhenRow3Collapses({
  required bool wasExpanded,
  required bool willExpand,
  required bool ctrlLocked,
  required bool altLocked,
}) {
  return wasExpanded && !willExpand && (ctrlLocked || altLocked);
}

String _applyTerminalCtrlModifier(String data) {
  // Ctrl 映射仅为 ASCII 标量定义。一个可见字符可能
  // 由多个标量组成（例如分解形式的重音字母），所以对这些
  // 字素不做处理，而不是只重写它们的 ASCII 基字母。
  final graphemes = data.characters.toList(growable: false);
  if (graphemes.length != 1) {
    return data;
  }

  final runes = graphemes.single.runes.toList(growable: false);
  if (runes.length != 1) {
    return data;
  }

  final code = runes.single;
  if (code >= 0x61 && code <= 0x7A) {
    return String.fromCharCode(code - 0x60);
  }
  if (code >= 0x41 && code <= 0x5A) {
    return String.fromCharCode(code - 0x40);
  }
  if (code == 0x20) {
    return String.fromCharCode(0);
  }
  if (code == 0x5B) {
    return String.fromCharCode(27);
  }
  if (code == 0x5C) {
    return String.fromCharCode(28);
  }
  if (code == 0x5D) {
    return String.fromCharCode(29);
  }
  if (code == 0x5E) {
    return String.fromCharCode(30);
  }
  if (code == 0x5F || code == 0x2F) {
    return String.fromCharCode(31);
  }
  return data;
}
