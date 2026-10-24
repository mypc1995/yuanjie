import 'package:flutter/widgets.dart';
import 'package:xterm/xterm.dart';

class TerminalScrollController extends ScrollController {
  TerminalScrollController(this.terminal);

  final Terminal Function() terminal;

  @override
  ScrollPosition createScrollPosition(ScrollPhysics physics,
      ScrollContext context, ScrollPosition? oldPosition) {
    return super.createScrollPosition(
      _TerminalScrollPhysics(terminal, parent: physics),
      context,
      oldPosition,
    );
  }
}

class _TerminalScrollPhysics extends ScrollPhysics {
  const _TerminalScrollPhysics(this.terminal, {super.parent});

  final Terminal Function() terminal;

  @override
  _TerminalScrollPhysics applyTo(ScrollPhysics? ancestor) =>
      _TerminalScrollPhysics(terminal, parent: buildParent(ancestor));

  @override
  bool shouldAcceptUserOffset(ScrollMetrics position) {
    // 保留的主屏偏移可能让这个内层可滚动区域赢得
    // 触控板手势，使 xterm 外层的备用屏幕滚动处理器得不到事件。
    if (terminal().isUsingAltBuffer) return false;
    return super.shouldAcceptUserOffset(position);
  }
}
