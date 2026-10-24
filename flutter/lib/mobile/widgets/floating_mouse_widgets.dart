// 这些浮动鼠标组件用于模拟物理鼠标
// 即鼠标模式下从 "mobile" 到 "desktop" 的场景。
// 本文件不包含完整的鼠标组件，只包含
// 一些辅助控制的部件，例如滚轮滚动和滚轮按钮。

import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/material.dart';

import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/common/widgets/remote_input.dart';
import 'package:flutter_hbb/models/input_model.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_hbb/models/platform_model.dart';

// 供滚轮按钮和滚轮滚动组件使用
const double _kSpaceToHorizontalEdge = 25;
const double _wheelWidth = 50;
const double _wheelHeight = 162;
// 供左键/右键按钮组件使用
const double _kSpaceToVerticalEdge = 15;
const double _kSpaceBetweenLeftRightButtons = 40;
const double _kLeftRightButtonWidth = 55;
const double _kLeftRightButtonHeight = 40;
const double _kBorderWidth = 1;
final Color _kDefaultBorderColor = Colors.white.withOpacity(0.7);
final Color _kDefaultColor = Colors.black.withOpacity(0.4);
final Color _kTapDownColor = Colors.blue.withOpacity(0.7);
final Color _kWidgetHighlightColor = Colors.white.withOpacity(0.9);
const int _kInputTimerIntervalMillis = 100;

class FloatingMouseWidgets extends StatefulWidget {
  final FFI ffi;
  const FloatingMouseWidgets({
    super.key,
    required this.ffi,
  });

  @override
  State<FloatingMouseWidgets> createState() => _FloatingMouseWidgetsState();
}

class _FloatingMouseWidgetsState extends State<FloatingMouseWidgets> {
  InputModel get _inputModel => widget.ffi.inputModel;
  CursorModel get _cursorModel => widget.ffi.cursorModel;
  late final VirtualMouseMode _virtualMouseMode;

  @override
  void initState() {
    super.initState();
    _virtualMouseMode = widget.ffi.ffiModel.virtualMouseMode;
    _virtualMouseMode.addListener(_onVirtualMouseModeChanged);
    _cursorModel.blockEvents = false;
    isSpecialHoldDragActive = false;
  }

  void _onVirtualMouseModeChanged() {
    if (mounted) {
      setState(() {});
    }
  }

  @override
  void dispose() {
    _virtualMouseMode.removeListener(_onVirtualMouseModeChanged);
    super.dispose();
    _cursorModel.blockEvents = false;
    isSpecialHoldDragActive = false;
  }

  @override
  Widget build(BuildContext context) {
    final virtualMouseMode = _virtualMouseMode;
    if (!virtualMouseMode.showVirtualMouse) {
      return const Offstage();
    }
    return Stack(
      children: [
        FloatingWheel(
          inputModel: _inputModel,
          cursorModel: _cursorModel,
        ),
        if (virtualMouseMode.showVirtualJoystick)
          VirtualJoystick(
            cursorModel: _cursorModel,
            inputModel: _inputModel,
          ),
        FloatingLeftRightButton(
          isLeft: true,
          inputModel: _inputModel,
          cursorModel: _cursorModel,
        ),
        FloatingLeftRightButton(
          isLeft: false,
          inputModel: _inputModel,
          cursorModel: _cursorModel,
        ),
      ],
    );
  }
}

class FloatingWheel extends StatefulWidget {
  final InputModel inputModel;
  final CursorModel cursorModel;
  const FloatingWheel(
      {super.key, required this.inputModel, required this.cursorModel});

  @override
  State<FloatingWheel> createState() => _FloatingWheelState();
}

class _FloatingWheelState extends State<FloatingWheel> {
  Offset _position = Offset.zero;
  bool _isInitialized = false;
  Rect? _lastBlockedRect;

  bool _isUpDown = false;
  bool _isMidDown = false;
  bool _isDownDown = false;

  Orientation? _previousOrientation;

  Timer? _scrollTimer;

  InputModel get _inputModel => widget.inputModel;
  CursorModel get _cursorModel => widget.cursorModel;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _resetPosition();
    });
  }

  void _resetPosition() {
    final size = MediaQuery.of(context).size;
    setState(() {
      _position = Offset(
        size.width - _wheelWidth - _kSpaceToHorizontalEdge,
        (size.height - _wheelHeight) / 2,
      );
      _isInitialized = true;
    });
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _updateBlockedRect();
    });
  }

  void _updateBlockedRect() {
    if (_lastBlockedRect != null) {
      _cursorModel.removeBlockedRect(_lastBlockedRect!);
    }
    final newRect =
        Rect.fromLTWH(_position.dx, _position.dy, _wheelWidth, _wheelHeight);
    _cursorModel.addBlockedRect(newRect);
    _lastBlockedRect = newRect;
  }

  @override
  void dispose() {
    _scrollTimer?.cancel();
    if (_lastBlockedRect != null) {
      _cursorModel.removeBlockedRect(_lastBlockedRect!);
    }
    super.dispose();
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final currentOrientation = MediaQuery.of(context).orientation;
    if (_previousOrientation != null &&
        _previousOrientation != currentOrientation) {
      _resetPosition();
    }
    _previousOrientation = currentOrientation;
  }

  Widget _buildUpDownButton(
      void Function(PointerDownEvent) onPointerDown,
      void Function(PointerUpEvent) onPointerUp,
      void Function(PointerCancelEvent) onPointerCancel,
      bool Function() flagGetter,
      BorderRadiusGeometry borderRadius,
      IconData iconData) {
    return Listener(
      onPointerDown: onPointerDown,
      onPointerUp: onPointerUp,
      onPointerCancel: onPointerCancel,
      child: Container(
        width: _wheelWidth,
        height: 55,
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: _kDefaultColor,
          border: Border.all(
              color: flagGetter() ? _kTapDownColor : _kDefaultBorderColor,
              width: 1),
          borderRadius: borderRadius,
        ),
        child: Icon(iconData, color: _kDefaultBorderColor, size: 32),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    if (!_isInitialized) {
      return Positioned(child: Offstage());
    }
    return Positioned(
      left: _position.dx,
      top: _position.dy,
      child: _buildWidget(context),
    );
  }

  Widget _buildWidget(BuildContext context) {
    return Container(
      width: _wheelWidth,
      height: _wheelHeight,
      child: Column(
        children: [
          _buildUpDownButton(
            (event) {
              setState(() {
                _isUpDown = true;
              });
              _startScrollTimer(1);
            },
            (event) {
              setState(() {
                _isUpDown = false;
              });
              _stopScrollTimer();
            },
            (event) {
              setState(() {
                _isUpDown = false;
              });
              _stopScrollTimer();
            },
            () => _isUpDown,
            BorderRadius.vertical(top: Radius.circular(_wheelWidth * 0.5)),
            Icons.keyboard_arrow_up,
          ),
          Listener(
            onPointerDown: (event) {
              setState(() {
                _isMidDown = true;
              });
              _inputModel.tapDown(MouseButtons.wheel);
            },
            onPointerUp: (event) {
              setState(() {
                _isMidDown = false;
              });
              _inputModel.tapUp(MouseButtons.wheel);
            },
            onPointerCancel: (event) {
              setState(() {
                _isMidDown = false;
              });
              _inputModel.tapUp(MouseButtons.wheel);
            },
            child: Container(
              width: _wheelWidth,
              height: 52,
              decoration: BoxDecoration(
                color: _kDefaultColor,
                border: Border.symmetric(
                    vertical: BorderSide(
                        color:
                            _isMidDown ? _kTapDownColor : _kDefaultBorderColor,
                        width: _kBorderWidth)),
              ),
              child: Center(
                child: Container(
                  width: _wheelWidth - 10,
                  height: _wheelWidth - 10,
                  child: Center(
                    child: Column(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Container(
                          width: 18,
                          height: 2,
                          color: _kDefaultBorderColor,
                        ),
                        SizedBox(height: 6),
                        Container(
                          width: 24,
                          height: 2,
                          color: _kDefaultBorderColor,
                        ),
                        SizedBox(height: 6),
                        Container(
                          width: 18,
                          height: 2,
                          color: _kDefaultBorderColor,
                        ),
                      ],
                    ),
                  ),
                ),
              ),
            ),
          ),
          _buildUpDownButton(
            (event) {
              setState(() {
                _isDownDown = true;
              });
              _startScrollTimer(-1);
            },
            (event) {
              setState(() {
                _isDownDown = false;
              });
              _stopScrollTimer();
            },
            (event) {
              setState(() {
                _isDownDown = false;
              });
              _stopScrollTimer();
            },
            () => _isDownDown,
            BorderRadius.vertical(bottom: Radius.circular(_wheelWidth * 0.5)),
            Icons.keyboard_arrow_down,
          ),
        ],
      ),
    );
  }

  void _startScrollTimer(int direction) {
    _scrollTimer?.cancel();
    _inputModel.scroll(direction);
    _scrollTimer = Timer.periodic(
        Duration(milliseconds: _kInputTimerIntervalMillis), (timer) {
      _inputModel.scroll(direction);
    });
  }

  void _stopScrollTimer() {
    _scrollTimer?.cancel();
    _scrollTimer = null;
  }
}

class FloatingLeftRightButton extends StatefulWidget {
  final bool isLeft;
  final InputModel inputModel;
  final CursorModel cursorModel;
  const FloatingLeftRightButton(
      {super.key,
      required this.isLeft,
      required this.inputModel,
      required this.cursorModel});

  @override
  State<FloatingLeftRightButton> createState() =>
      _FloatingLeftRightButtonState();
}

class _FloatingLeftRightButtonState extends State<FloatingLeftRightButton> {
  Offset _position = Offset.zero;
  bool _isInitialized = false;
  bool _isDown = false;
  Object? _activePress;
  bool _remoteButtonDown = false;
  Rect? _lastBlockedRect;

  Orientation? _previousOrientation;
  Offset _preSavedPos = Offset.zero;

  // 手势歧义处理
  Timer? _tapDownTimer;
  final Duration _pressTimeout = const Duration(milliseconds: 200);
  bool _isDragging = false;

  bool get _isLeft => widget.isLeft;
  InputModel get _inputModel => widget.inputModel;
  CursorModel get _cursorModel => widget.cursorModel;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      final currentOrientation = MediaQuery.of(context).orientation;
      _previousOrientation = currentOrientation;
      _resetPosition(currentOrientation);
    });
  }

  @override
  void dispose() {
    _activePress = null;
    if (_remoteButtonDown) {
      _remoteButtonDown = false;
      _inputModel.tapUp(_isLeft ? MouseButtons.left : MouseButtons.right);
    }
    if (_lastBlockedRect != null) {
      _cursorModel.removeBlockedRect(_lastBlockedRect!);
    }
    _tapDownTimer?.cancel();
    _trySavePosition();
    super.dispose();
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final currentOrientation = MediaQuery.of(context).orientation;
    if (_previousOrientation == null ||
        _previousOrientation != currentOrientation) {
      _resetPosition(currentOrientation);
    }
    _previousOrientation = currentOrientation;
  }

  double _getOffsetX(double w) {
    if (_isLeft) {
      return (w - _kLeftRightButtonWidth * 2 - _kSpaceBetweenLeftRightButtons) *
          0.5;
    } else {
      return (w + _kSpaceBetweenLeftRightButtons) * 0.5;
    }
  }

  String _getPositionKey(Orientation ori) {
    final strLeftRight = _isLeft ? 'l' : 'r';
    final strOri = ori == Orientation.landscape ? 'l' : 'p';
    return '$strLeftRight$strOri-mouse-btn-pos';
  }

  static Offset? _loadPositionFromString(String s) {
    if (s.isEmpty) {
      return null;
    }
    try {
      final m = jsonDecode(s);
      return Offset(m['x'], m['y']);
    } catch (e) {
      debugPrintStack(label: 'Failed to load position "$s" $e');
      return null;
    }
  }

  void _trySavePosition() {
    if (_previousOrientation == null) return;
    if (((_position - _preSavedPos)).distanceSquared < 0.1) return;
    final pos = jsonEncode({
      'x': _position.dx,
      'y': _position.dy,
    });
    bind.setLocalFlutterOption(
        k: _getPositionKey(_previousOrientation!), v: pos);
    _preSavedPos = _position;
  }

  void _restorePosition(Orientation ori) {
    final ps = bind.getLocalFlutterOption(k: _getPositionKey(ori));
    final pos = _loadPositionFromString(ps);
    if (pos == null) {
      final size = MediaQuery.of(context).size;
      _position = Offset(_getOffsetX(size.width),
          size.height - _kSpaceToVerticalEdge - _kLeftRightButtonHeight);
    } else {
      _position = pos;
      _preSavedPos = pos;
    }
  }

  void _resetPosition(Orientation ori) {
    setState(() {
      _restorePosition(ori);
      _isInitialized = true;
    });
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _updateBlockedRect();
    });
  }

  void _updateBlockedRect() {
    if (_lastBlockedRect != null) {
      _cursorModel.removeBlockedRect(_lastBlockedRect!);
    }
    final newRect = Rect.fromLTWH(_position.dx, _position.dy,
        _kLeftRightButtonWidth, _kLeftRightButtonHeight);
    _cursorModel.addBlockedRect(newRect);
    _lastBlockedRect = newRect;
  }

  void _onMoveUpdateDelta(Offset delta) {
    final context = this.context;
    final size = MediaQuery.of(context).size;
    Offset newPosition = _position + delta;
    double minX = _kSpaceToHorizontalEdge;
    double minY = _kSpaceToVerticalEdge;
    double maxX = size.width - _kLeftRightButtonWidth - _kSpaceToHorizontalEdge;
    double maxY = size.height - _kLeftRightButtonHeight - _kSpaceToVerticalEdge;
    newPosition = Offset(
      newPosition.dx.clamp(minX, maxX),
      newPosition.dy.clamp(minY, maxY),
    );
    final isPositionChanged = !(isDoubleEqual(newPosition.dx, _position.dx) &&
        isDoubleEqual(newPosition.dy, _position.dy));
    setState(() {
      _position = newPosition;
    });
    if (isPositionChanged) {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) _updateBlockedRect();
      });
    }
  }

  void _onBodyPointerMoveUpdate(PointerMoveEvent event) {
    _cursorModel.blockEvents = true;
    // 如果发生了移动，那就是拖拽，而不是点击。
    _isDragging = true;
    // 取消定时器，避免被识别为点击/长按。
    _tapDownTimer?.cancel();
    _tapDownTimer = null;
    _onMoveUpdateDelta(event.delta);
  }

  Widget _buildButtonIcon() {
    final double w = _kLeftRightButtonWidth * 0.45;
    final double h = _kLeftRightButtonHeight * 0.75;
    final double borderRadius = w * 0.5;
    final double quarterCircleRadius = borderRadius * 0.9;
    return Stack(
      children: [
        Container(
          width: w,
          height: h,
          decoration: BoxDecoration(
            borderRadius: BorderRadius.circular(_kLeftRightButtonWidth * 0.225),
            color: Colors.white,
          ),
        ),
        Positioned(
          left: _isLeft ? quarterCircleRadius * 0.25 : null,
          right: _isLeft ? null : quarterCircleRadius * 0.25,
          top: quarterCircleRadius * 0.25,
          child: CustomPaint(
            size: Size(quarterCircleRadius * 2, quarterCircleRadius * 2),
            painter: _QuarterCirclePainter(
              color: _kDefaultColor,
              isLeft: _isLeft,
              radius: quarterCircleRadius,
            ),
          ),
        ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    if (!_isInitialized) {
      return Positioned(child: Offstage());
    }
    return Positioned(
      left: _position.dx,
      top: _position.dy,
      // 这里不能用 GestureDetector，因为 `onTapDown` 可能会
      // 在拖拽时被触发。
      child: Listener(
        onPointerMove: _onBodyPointerMoveUpdate,
        onPointerDown: (event) async {
          final press = Object();
          _activePress = press;
          _isDragging = false;
          setState(() {
            _isDown = true;
          });
          // 启动一个定时器。如果它触发，就是长按。
          _tapDownTimer?.cancel();
          _tapDownTimer = Timer(_pressTimeout, () {
            isSpecialHoldDragActive = true;
            () async {
              await _cursorModel.syncCursorPosition();
              if (!mounted || !identical(_activePress, press)) return;
              _remoteButtonDown = true;
              await _inputModel
                  .tapDown(_isLeft ? MouseButtons.left : MouseButtons.right);
            }();
            _tapDownTimer = null;
          });
        },
        onPointerUp: (event) {
          if (!mounted) return;
          _activePress = null;
          _cursorModel.blockEvents = false;
          setState(() {
            _isDown = false;
          });
          // 如果定时器仍处于活动状态，就是快速点击。
          if (_tapDownTimer != null) {
            _tapDownTimer!.cancel();
            _tapDownTimer = null;
            // 快速触发按下和抬起。
            _inputModel
                .tapDown(_isLeft ? MouseButtons.left : MouseButtons.right)
                .then(
                    (_) => Future.delayed(const Duration(milliseconds: 50), () {
                          _inputModel.tapUp(
                              _isLeft ? MouseButtons.left : MouseButtons.right);
                        }));
          } else {
            // 如果不是快速点击，可能是长按或拖拽。
            if (_remoteButtonDown) {
              _remoteButtonDown = false;
              _inputModel
                  .tapUp(_isLeft ? MouseButtons.left : MouseButtons.right);
            }
          }

          if (_isDragging) {
            _trySavePosition();
          }
          isSpecialHoldDragActive = false;
        },
        onPointerCancel: (event) {
          if (!mounted) return;
          _activePress = null;
          _cursorModel.blockEvents = false;
          setState(() {
            _isDown = false;
          });
          _tapDownTimer?.cancel();
          _tapDownTimer = null;
          if (_remoteButtonDown) {
            _remoteButtonDown = false;
            _inputModel.tapUp(_isLeft ? MouseButtons.left : MouseButtons.right);
          }
          isSpecialHoldDragActive = false;
          if (_isDragging) {
            _trySavePosition();
          }
        },
        child: Container(
          width: _kLeftRightButtonWidth,
          height: _kLeftRightButtonHeight,
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: _kDefaultColor,
            border: Border.all(
                color: _isDown ? _kTapDownColor : _kDefaultBorderColor,
                width: _kBorderWidth),
            borderRadius: _isLeft
                ? BorderRadius.horizontal(
                    left: Radius.circular(_kLeftRightButtonHeight * 0.5))
                : BorderRadius.horizontal(
                    right: Radius.circular(_kLeftRightButtonHeight * 0.5)),
          ),
          child: _buildButtonIcon(),
        ),
      ),
    );
  }
}

class _QuarterCirclePainter extends CustomPainter {
  final Color color;
  final bool isLeft;
  final double radius;
  _QuarterCirclePainter(
      {required this.color, required this.isLeft, required this.radius});

  @override
  void paint(Canvas canvas, Size size) {
    final paint = Paint()
      ..color = color
      ..style = PaintingStyle.fill;
    final rect = Rect.fromLTWH(0, 0, radius * 2, radius * 2);
    if (isLeft) {
      canvas.drawArc(rect, -pi, pi / 2, true, paint);
    } else {
      canvas.drawArc(rect, -pi / 2, pi / 2, true, paint);
    }
  }

  @override
  bool shouldRepaint(CustomPainter oldDelegate) => false;
}

// 虚拟摇杆可以发送绝对移动（通过 updatePan）
// 或相对移动（通过 sendMobileRelativeMouseMove），取决于
// InputModel.relativeMouseMode 的设置。
class VirtualJoystick extends StatefulWidget {
  final CursorModel cursorModel;
  final InputModel inputModel;

  const VirtualJoystick({
    super.key,
    required this.cursorModel,
    required this.inputModel,
  });

  @override
  State<VirtualJoystick> createState() => _VirtualJoystickState();
}

class _VirtualJoystickState extends State<VirtualJoystick> {
  Offset _position = Offset.zero;
  bool _isInitialized = false;
  Offset _offset = Offset.zero;
  final double _joystickRadius = 50.0;
  final double _thumbRadius = 20.0;
  final double _moveStep = 3.0;
  final double _speed = 1.0;

  /// 相对鼠标移动灵敏度的缩放系数。
  /// 数值越大，在远程机器上光标移动越快。
  static const double _kRelativeMouseScale = 3.0;

  // 用于检测拖拽手势的一次性定时器
  Timer? _dragStartTimer;
  // 用于持续移动的周期性定时器
  Timer? _continuousMoveTimer;
  Size? _lastScreenSize;
  bool _isPressed = false;

  /// 检查是否启用了相对鼠标模式。
  bool get _useRelativeMouse => widget.inputModel.relativeMouseMode.value;

  @override
  void initState() {
    super.initState();
    widget.cursorModel.blockEvents = false;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _lastScreenSize = MediaQuery.of(context).size;
      _resetPosition();
    });
  }

  @override
  void dispose() {
    _stopSendEventTimer();
    widget.cursorModel.blockEvents = false;
    super.dispose();
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final currentScreenSize = MediaQuery.of(context).size;
    if (_lastScreenSize != null && _lastScreenSize != currentScreenSize) {
      _resetPosition();
    }
    _lastScreenSize = currentScreenSize;
  }

  void _resetPosition() {
    final size = MediaQuery.of(context).size;
    setState(() {
      _position = Offset(
        _kSpaceToHorizontalEdge + _joystickRadius,
        size.height * 0.5 + _joystickRadius * 1.5,
      );
      _isInitialized = true;
    });
  }

  Offset _offsetToPanDelta(Offset offset) {
    return Offset(
      offset.dx / _joystickRadius,
      offset.dy / _joystickRadius,
    );
  }

  /// 向远程机器发送移动增量。
  /// 如果启用了相对鼠标模式则使用它，否则使用绝对的 updatePan。
  void _sendMovement(Offset delta) {
    if (_useRelativeMouse) {
      widget.inputModel.sendMobileRelativeMouseMove(
          delta.dx * _kRelativeMouseScale, delta.dy * _kRelativeMouseScale);
    } else {
      // 在绝对模式下，使用会跟踪位置的 cursorModel.updatePan。
      widget.cursorModel.updatePan(delta, Offset.zero, false);
    }
  }

  void _stopSendEventTimer() {
    _dragStartTimer?.cancel();
    _continuousMoveTimer?.cancel();
    _dragStartTimer = null;
    _continuousMoveTimer = null;
  }

  @override
  Widget build(BuildContext context) {
    if (!_isInitialized) {
      return Positioned(child: Offstage());
    }
    return Positioned(
      left: _position.dx - _joystickRadius,
      top: _position.dy - _joystickRadius,
      child: GestureDetector(
        onPanStart: (details) {
          setState(() {
            _isPressed = true;
          });
          widget.cursorModel.blockEvents = true;
          _updateOffset(details.localPosition);

          // 1. 立即发送一次小幅度的平移事件，以保证响应速度。
          //    移动幅度很小，用于平缓起步。
          final initialDelta = _offsetToPanDelta(_offset);
          if (initialDelta.distance > 0) {
            _sendMovement(initialDelta);
          }

          // 2. 启动一次性定时器，检查用户是否长按准备拖拽。
          _dragStartTimer?.cancel();
          _dragStartTimer = Timer(const Duration(milliseconds: 120), () {
            // 3. 如果定时器触发，那就是拖拽。启动持续移动定时器。
            _continuousMoveTimer?.cancel();
            _continuousMoveTimer =
                periodic_immediate(const Duration(milliseconds: 20), () async {
              if (_offset != Offset.zero) {
                _sendMovement(_offsetToPanDelta(_offset) * _moveStep * _speed);
              }
            });
          });
        },
        onPanUpdate: (details) {
          _updateOffset(details.localPosition);
        },
        onPanEnd: (details) {
          setState(() {
            _offset = Offset.zero;
            _isPressed = false;
          });
          widget.cursorModel.blockEvents = false;

          // 4. 关键步骤：平移结束时取消所有定时器。
          //    如果这是一次快速滑动，这会在拖拽检测触发前把它取消掉。
          //    如果这是一次拖拽，这会停止持续移动。
          _stopSendEventTimer();
        },
        child: CustomPaint(
          size: Size(_joystickRadius * 2, _joystickRadius * 2),
          painter: _JoystickPainter(
              _offset, _joystickRadius, _thumbRadius, _isPressed),
        ),
      ),
    );
  }

  void _updateOffset(Offset localPosition) {
    final center = Offset(_joystickRadius, _joystickRadius);
    final offset = localPosition - center;
    final distance = offset.distance;

    if (distance <= _joystickRadius) {
      setState(() {
        _offset = offset;
      });
    } else {
      final clampedOffset = offset / distance * _joystickRadius;
      setState(() {
        _offset = clampedOffset;
      });
    }
  }
}

class _JoystickPainter extends CustomPainter {
  final Offset _offset;
  final double _joystickRadius;
  final double _thumbRadius;
  final bool _isPressed;

  _JoystickPainter(
      this._offset, this._joystickRadius, this._thumbRadius, this._isPressed);

  @override
  void paint(Canvas canvas, Size size) {
    final center = Offset(size.width / 2, size.height / 2);
    final joystickColor = _kDefaultColor;
    final borderColor = _isPressed ? _kTapDownColor : _kDefaultBorderColor;
    final thumbColor = _kWidgetHighlightColor;

    final joystickPaint = Paint()
      ..color = joystickColor
      ..style = PaintingStyle.fill;

    final borderPaint = Paint()
      ..color = borderColor
      ..style = PaintingStyle.stroke
      ..strokeWidth = 1.5;

    final thumbPaint = Paint()
      ..color = thumbColor
      ..style = PaintingStyle.fill;

    // 绘制摇杆底座和边框
    canvas.drawCircle(center, _joystickRadius, joystickPaint);
    canvas.drawCircle(center, _joystickRadius, borderPaint);

    // 绘制摇杆头
    final thumbCenter = center + _offset;
    canvas.drawCircle(thumbCenter, _thumbRadius, thumbPaint);
  }

  @override
  bool shouldRepaint(covariant _JoystickPainter oldDelegate) {
    return oldDelegate._offset != _offset ||
        oldDelegate._isPressed != _isPressed;
  }
}
