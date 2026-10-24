/// 一个用于累加小数鼠标增量并输出整数增量的小工具。
///
/// 相对鼠标模式在线路上使用整数增量，而 Flutter 的指针增量
/// 是双精度浮点数。该累加器通过把小数余量
/// 在各个事件之间传递，来保留亚像素级移动。
class RelativeMouseDelta {
  final int x;
  final int y;

  const RelativeMouseDelta(this.x, this.y);
}

/// 累加小数鼠标增量，并在有整数增量时返回之。
class RelativeMouseAccumulator {
  double _fracX = 0.0;
  double _fracY = 0.0;

  /// 累加一个增量，当至少有一个轴的幅度达到
  /// 1px 时（向零截断之后）返回整数增量。
  ///
  /// 如果 [maxDelta] > 0，返回的整数增量在每个轴上都被钳制到
  /// [-maxDelta, maxDelta]。
  RelativeMouseDelta? add(
    double dx,
    double dy, {
    required int maxDelta,
  }) {
    // 防御误用：负的 maxDelta 会悄悄关闭钳制。
    assert(maxDelta >= 0, 'maxDelta must be non-negative');

    _fracX += dx;
    _fracY += dy;

    int intX = _fracX.truncate();
    int intY = _fracY.truncate();

    if (intX == 0 && intY == 0) {
      return null;
    }

    // 先钳制再相减，这样超出的移动量会保留在累加器中，
    // 而不是在尖峰时被永久丢弃。
    if (maxDelta > 0) {
      intX = intX.clamp(-maxDelta, maxDelta);
      intY = intY.clamp(-maxDelta, maxDelta);
    }

    _fracX -= intX;
    _fracY -= intY;

    return RelativeMouseDelta(intX, intY);
  }

  void reset() {
    _fracX = 0.0;
    _fracY = 0.0;
  }
}
