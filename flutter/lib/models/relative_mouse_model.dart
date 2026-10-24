import 'dart:async';
import 'dart:convert';
import 'dart:math' as math;
import 'dart:ui' as ui;

import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_hbb/main.dart';
import 'package:flutter_hbb/utils/relative_mouse_accumulator.dart';
import 'package:get/get.dart';

import '../common.dart';
import '../consts.dart';
import 'platform_model.dart';

class RelativeMouseModel {
  final SessionID sessionId;
  final RxBool enabled;

  final bool Function() keyboardPerm;
  final bool Function() isViewCamera;
  final String Function() peerVersion;
  final String? Function() peerPlatform;

  final Map<String, dynamic> Function(Map<String, dynamic> msg) modify;

  final bool Function() getPointerInsideImage;
  final void Function(bool inside) setPointerInsideImage;

  RelativeMouseModel({
    required this.sessionId,
    required this.enabled,
    required this.keyboardPerm,
    required this.isViewCamera,
    required this.peerVersion,
    required this.peerPlatform,
    required this.modify,
    required this.getPointerInsideImage,
    required this.setPointerInsideImage,
  });

  final RelativeMouseAccumulator _accumulator = RelativeMouseAccumulator();

  // 原生相对鼠标模式支持（仅 macOS）
  // 使用 CGAssociateMouseAndMouseCursorPosition 锁定光标，并通过 NSEvent 监听获取原始增量。
  static MethodChannel? _hostChannel;
  // 当前活动的、接收原生鼠标增量事件的 model。
  // 注意：这里不存在多会话之间的竞态问题，因为
  // 相对鼠标模式激活时光标被锁定，用户无法
  // 切换到另一个会话窗口。用户必须先退出相对鼠标模式
  // （在 macOS 上按 Cmd+G，或在 Windows/Linux 上按 Ctrl+Alt）才能与
  // 其他会话交互。
  static RelativeMouseModel? _activeNativeModel;
  static bool _hostChannelInitialized = false;

  /// 初始化原生相对鼠标模式所需的宿主通道。
  /// 在 macOS 上应用启动时应调用一次。
  static void initHostChannel() {
    if (!isMacOS) return;
    if (_hostChannelInitialized) return;
    _hostChannelInitialized = true;

    _hostChannel = const MethodChannel('org.rustdesk.rustdesk/host');
    _hostChannel!.setMethodCallHandler((call) async {
      if (call.method == 'onMouseDelta') {
        final args = call.arguments as Map<dynamic, dynamic>;
        final dx = args['dx'] as int;
        final dy = args['dy'] as int;
        _activeNativeModel?._onNativeMouseDelta(dx, dy);
      }
      return null;
    });
  }

  // TODO(perf): Consider routing native delta through RelativeMouseAccumulator/throttle
  // 以免高回报率鼠标（例如 1000Hz+）在网络上造成消息洪水。
  void _onNativeMouseDelta(int dx, int dy) {
    if (!enabled.value) return;
    // 不经累加器直接发送到远端（原生已提供整数增量）
    _sendMouseMessageToSession({
      'type': 'move_relative',
      'x': '$dx',
      'y': '$dy',
    });
  }

  Future<bool> _enableNativeRelativeMouseMode() async {
    if (!isMacOS) return false;
    if (_hostChannel == null) {
      initHostChannel();
      if (_hostChannel == null) return false;
    }

    // 防御性保护：防止覆盖已经处于活动状态的原生会话。
    // 实际上这不应该发生，因为相对鼠标模式激活时，
    // 光标被锁定，用户无法切换到另一个会话窗口。
    // 用户必须先退出相对鼠标模式（在 macOS 上按 Cmd+G，或在 Windows/Linux 上按
    // Ctrl+Alt）才能与不同的会话交互。
    if (_activeNativeModel != null && _activeNativeModel != this) {
      debugPrint(
          '[RelMouse] Another model already has native relative mouse mode active');
      return false;
    }

    try {
      final result =
          await _hostChannel!.invokeMethod('enableNativeRelativeMouseMode');
      if (result == true) {
        _activeNativeModel = this;
        return true;
      }
    } catch (e) {
      debugPrint('[RelMouse] Failed to enable native relative mouse mode: $e');
    }
    return false;
  }

  Future<void> _disableNativeRelativeMouseMode() async {
    if (!isMacOS) return;
    if (_hostChannel == null) return;

    // 只有拥有者 model 才应禁用原生模式，以避免
    // 一个会话无意中破坏另一个会话的原生相对鼠标状态。
    if (_activeNativeModel != this) {
      return;
    }

    try {
      await _hostChannel!.invokeMethod('disableNativeRelativeMouseMode');
    } catch (e) {
      debugPrint('[RelMouse] Failed to disable native relative mouse mode: $e');
    } finally {
      if (_activeNativeModel == this) {
        _activeNativeModel = null;
      }
    }
  }

  // 该 model 当前是否处于原生相对鼠标模式
  bool get _isNativeRelativeMouseModeActive =>
      isMacOS && _activeNativeModel == this;

  // 指针锁定中心（LOCAL 控件坐标系，用于计算增量）
  Offset? _pointerLockCenterLocal;
  // 指针锁定中心（SCREEN 坐标系，用于重新居中 OS 光标）
  Offset? _pointerLockCenterScreen;
  // 指针区域左上角（Flutter 视图坐标系）。
  // 由 PointerEvent.position - PointerEvent.localPosition 计算得出。
  Offset? _pointerRegionTopLeftGlobal;
  // 上一次指针位置（LOCAL 控件坐标系，中心点未就绪时的回退方案）。
  Offset? _lastPointerLocalPos;

  // 跟踪当前是否启用了 OS 级光标裁剪（仅 Windows）。
  // TODO(accuracy): Revisit window/client/border clipping math if users report misaligned
  // 在自定义或最大化窗口装饰上的裁剪问题。建议改用平台 API
  // （例如 Windows 上的 GetClientRect），而不是 Flutter 的窗口坐标。
  bool _cursorClipApplied = false;

  // 跟踪是否有重新居中操作正在进行，以防止调用重叠。
  bool _recenterInProgress = false;

  // 异步启用操作的请求令牌，用于防止过时的回调。
  // 每次尝试启用时递增，回调检查令牌是否仍然匹配。
  int _enableRequestId = 0;

  // 用于批量发送鼠标移动消息的节流缓冲区（减少网络消息洪水）。
  int _pendingDeltaX = 0;
  int _pendingDeltaY = 0;
  Timer? _throttleTimer;
  static const Duration _throttleInterval = Duration(milliseconds: 16);

  // 远端图像控件的尺寸（用于计算中心点）
  Size? _imageWidgetSize;

  // 相对鼠标模式切换的去抖时间戳，用于防止竞态
  // 发生在 Rust rdev 抓取循环与 Flutter 键盘处理之间。
  DateTime? _lastToggle;

  // 跟踪按键按下状态，用于退出快捷键。
  // macOS：Cmd+G —— 跟踪 G 键
  // Windows/Linux：Ctrl+Alt —— 跟踪最后按下的那个修饰键
  // 当按下事件被拦截（快捷键已触发）时，还需要拦截
  // 对应的松开事件，避免孤立的松开事件被发送到远端。
  bool _exitShortcutKeyDown = false;

  // 在相对鼠标模式被禁用时取消外部节流定时器的回调。
  VoidCallback? onDisabled;

  bool get isSupported {
    // 在 Linux/Wayland 上不支持光标位置改写，直接隐藏该选项。
    if (isDesktop && isLinux && bind.mainCurrentIsWayland()) {
      return false;
    }
    // 远端 Linux 上不支持相对鼠标模式：
    // 1. 不支持长按按键事件。
    // 2. Wayland 显示服务器缺少光标位置改写支持。
    final platform = peerPlatform();
    if (platform == kPeerPlatformLinux) {
      return false;
    }
    final v = peerVersion();
    if (v.isEmpty) return false;
    return versionCmp(v, kMinVersionForRelativeMouseMode) >= 0;
  }

  Size? get imageWidgetSize => _imageWidgetSize;

  void updateImageWidgetSize(Size size) {
    _imageWidgetSize = size;
    if (enabled.value) {
      _pointerLockCenterLocal = Offset(size.width / 2, size.height / 2);
    }
  }

  void updatePointerRegionTopLeftGlobal(PointerEvent e) {
    _pointerRegionTopLeftGlobal = e.position - e.localPosition;
  }

  /// 处理相对鼠标模式退出快捷键的共享辅助方法。
  /// 若事件已被处理且不应继续转发，则返回 true。
  ///
  /// 退出快捷键（仅在相对鼠标模式激活时生效）：
  /// - macOS：Cmd+G
  /// - Windows/Linux：Ctrl+Alt（顺序任意 —— 两个键都按下时触发）
  ///
  /// [logicalKey] —— 事件的逻辑按键
  /// [isKeyUp] —— 事件是否为按键松开事件
  /// [isKeyDown] —— 事件是否为按键按下事件
  /// [ctrlPressed]、[altPressed]、[commandPressed] —— 修饰键状态
  bool _handleExitShortcut({
    required LogicalKeyboardKey logicalKey,
    required bool isKeyUp,
    required bool isKeyDown,
    required bool ctrlPressed,
    required bool altPressed,
    required bool commandPressed,
  }) {
    if (!isDesktop || !keyboardPerm() || isViewCamera()) return false;

    // 仅在相对鼠标模式激活时处理退出快捷键
    if (!enabled.value) return false;

    // 如果按下事件被拦截，则拦截对应的松开事件（避免远端出现孤立的松开事件）。
    if (isKeyUp && _exitShortcutKeyDown) {
      _exitShortcutKeyDown = false;
      return true;
    }

    if (!isKeyDown) return false;

    // macOS：按 Cmd+G 退出
    if (isMacOS) {
      final isGKey = logicalKey == LogicalKeyboardKey.keyG;
      if (isGKey && commandPressed) {
        _exitShortcutKeyDown = true;
        setRelativeMouseMode(false);
        return true;
      }
      return false;
    }

    // Windows/Linux：按 Ctrl+Alt 退出
    // 两个修饰键都按下时触发（在 Ctrl 或 Alt 的按下事件上检查）
    final isCtrlKey = logicalKey == LogicalKeyboardKey.controlLeft ||
        logicalKey == LogicalKeyboardKey.controlRight;
    final isAltKey = logicalKey == LogicalKeyboardKey.altLeft ||
        logicalKey == LogicalKeyboardKey.altRight;

    // 当 Ctrl 被按下而 Alt 已处于按下状态时，或反之亦然
    if ((isCtrlKey && altPressed) || (isAltKey && ctrlPressed)) {
      _exitShortcutKeyDown = true;
      setRelativeMouseMode(false);
      return true;
    }

    return false;
  }

  bool handleKeyEvent(
    KeyEvent e, {
    required bool ctrlPressed,
    required bool shiftPressed,
    required bool altPressed,
    required bool commandPressed,
  }) {
    return _handleExitShortcut(
      logicalKey: e.logicalKey,
      isKeyUp: e is KeyUpEvent,
      isKeyDown: e is KeyDownEvent,
      ctrlPressed: ctrlPressed,
      altPressed: altPressed,
      commandPressed: commandPressed,
    );
  }

  /// 处理相对鼠标模式的原始按键事件。
  /// 若事件已被处理且不应继续转发，则返回 true。
  bool handleRawKeyEvent(RawKeyEvent e) {
    final modifiers = e.data;
    return _handleExitShortcut(
      logicalKey: e.logicalKey,
      isKeyUp: e is RawKeyUpEvent,
      isKeyDown: e is RawKeyDownEvent,
      ctrlPressed: modifiers.isControlPressed,
      altPressed: modifiers.isAltPressed,
      commandPressed: modifiers.isMetaPressed,
    );
  }

  void onEnterOrLeaveImage(bool enter) {
    if (!enabled.value) return;

    // 保持共享的"指针位于图像内"标志同步。
    setPointerInsideImage(enter);

    // macOS 原生模式：光标由 CGAssociateMouseAndMouseCursorPosition 锁定，
    // 无需重新居中逻辑。
    if (_isNativeRelativeMouseModeActive) {
      return;
    }

    if (!enter) {
      _releaseCursorClip();
      return;
    }

    // Windows：将光标裁剪到窗口矩形内
    // Linux：使用重新居中方法
    updatePointerLockCenter().then((_) {
      _recenterMouse();
    });
  }

  void onWindowBlur() {
    if (!enabled.value) return;

    // 指针位于窗口外时焦点也可能发生变化（例如激活任务栏）。
    // 不要跨焦点边界依赖之前的"指针在窗口内"状态。
    setPointerInsideImage(false);
    // macOS 原生模式：不要调用 _releaseCursorClip，否则会破坏 CGAssociateMouseAndMouseCursorPosition
    if (!_isNativeRelativeMouseModeActive) {
      _releaseCursorClip();
    }
  }

  void onWindowFocus() {
    if (!enabled.value) return;

    // macOS 原生模式：光标已被锁定
    if (_isNativeRelativeMouseModeActive) {
      setPointerInsideImage(false);
      return;
    }

    // 保护：必须能获取图像控件尺寸，才能正确计算中心点。
    if (_imageWidgetSize == null) {
      _disableWithCleanup();
      return;
    }

    // 容错处理：获得焦点时保持光标可用。指针锁定会在
    // 下一次进入/移动/悬停于远端图像内时重新启用。
    setPointerInsideImage(false);
    _releaseCursorClip();

    // 尽力而为：刷新中心点，使下一次启用立即生效。
    updatePointerLockCenter();
  }

  void toggleRelativeMouseMode() {
    final now = DateTime.now();
    if (_lastToggle != null &&
        now.difference(_lastToggle!).inMilliseconds <
            kRelativeMouseModeToggleDebounceMs) {
      return;
    }
    _lastToggle = now;
    setRelativeMouseMode(!enabled.value);
  }

  bool setRelativeMouseMode(bool value) {
    // 由于 Pointer Lock API 与 Flutter 输入系统的集成复杂度较高，Web 端暂不支持
    if (isWeb) {
      return false;
    }

    if (value) {
      if (!keyboardPerm() || isViewCamera()) {
        return false;
      }

      if (isDesktop && _imageWidgetSize == null) {
        // 仅桌面端：确保能获取图像控件尺寸，以正确计算中心点。
        showToast(translate('rel-mouse-not-ready-tip'));
        return false;
      }

      if (!isSupported) {
        // 启用前检查服务端版本是否支持。
        showToast(translate('rel-mouse-not-supported-peer-tip'));
        return false;
      }
    }

    if (value) {
      try {
        if (isDesktop) {
          final requestId = ++_enableRequestId;
          if (isMacOS) {
            // macOS：使用 CGAssociateMouseAndMouseCursorPosition 的原生相对鼠标模式
            // 这会将光标锁定在原地，并通过 NSEvent 监听提供原始增量。
            _enableNativeRelativeMouseMode().then((success) {
              // 防止过时回调：用户可能已在此期间关闭相对模式
              // （异步启用正在进行中）。
              if (_enableRequestId != requestId) {
                return;
              }
              if (success) {
                _completeEnableRelativeMouseMode();
              }
              // 注意：_enableNativeRelativeMouseMode 失败时已自行处理清理
            });
          } else {
            // Windows/Linux：使用基于 Flutter 的光标重新居中方案
            if (!getPointerInsideImage()) {
              _releaseCursorClip();
            }

            updatePointerLockCenter().then((_) => _recenterMouse()).then((_) {
              if (_enableRequestId != requestId) {
                return;
              }
              _completeEnableRelativeMouseMode();
            }).catchError((e) {
              if (_enableRequestId != requestId) {
                return;
              }
              debugPrint('[RelMouse] Platform setup failed: $e');
              _resetState();
            });
          }
        } else {
          // 移动端：立即启用（无需平台特定设置）
          _completeEnableRelativeMouseMode();
        }
      } catch (e) {
        _disableWithCleanup();
        return false;
      }
    } else {
      // 给 Rust rdev 抓取循环的尽力而为标记（ESC 行为）。
      // 绕过 keyboardPerm 检查以确保 Rust 状态始终同步，
      // 即使在相对模式激活期间权限被撤销也一样。
      _sendMouseMessageToSession(
        {
          'relative_mouse_mode': '0',
        },
        disableRelativeOnError: false,
        bypassKeyboardPerm: true,
      );

      // 仅桌面端：光标操作
      if (isDesktop) {
        if (isMacOS) {
          // macOS：禁用原生相对鼠标模式
          // 它已经调用 CGAssociateMouseAndMouseCursorPosition(1) 来重新关联鼠标
          _disableNativeRelativeMouseMode();
        } else {
          _releaseCursorClip();
        }
      }
      enabled.value = false;
      _resetState();
      onDisabled?.call();
    }

    return true;
  }

  /// 在平台设置成功完成后调用，以最终完成相对鼠标模式的启用。
  void _completeEnableRelativeMouseMode() {
    enabled.value = true;

    // 显示 toast 通知，让用户知道如何退出相对鼠标模式（仅桌面端）。
    if (isDesktop) {
      showToast(
          translate('rel-mouse-exit-{${isMacOS ? "Cmd+G" : "Ctrl+Alt"}}-tip'),
          alignment: Alignment.center);
    }

    // 给 Rust rdev 抓取循环（ESC 行为）以及 peer/服务端状态的尽力而为标记。
    // 这里使用空增量，因此不会移动远端光标。
    // 有意采用"发出即不管"的方式：不因这条标记消息而阻塞启用流程。
    // 失败只记录日志，不会禁用相对鼠标模式。
    _sendMouseMessageToSession(
      {
        'relative_mouse_mode': '1',
        'type': 'move_relative',
        'x': '0',
        'y': '0',
      },
      disableRelativeOnError: false,
    ).catchError((e) {
      debugPrint('[RelMouse] Failed to send enable marker: $e');
      return false;
    });
  }

  // 用于在重新居中后跳过第一个鼠标移动事件的标志（它本身就是重新居中产生的）。
  bool _skipNextMouseMove = false;

  /// 基于当前本地指针位置处理相对鼠标移动。
  /// 若事件在相对模式下已被处理则返回 true，否则返回 false。
  bool handleRelativeMouseMove(Offset localPosition) {
    if (!enabled.value) return false;

    // macOS：原生模式通过回调处理增量，跳过基于 Flutter 的处理。
    if (_isNativeRelativeMouseModeActive) {
      return true;
    }

    // 指针移动/悬停意味着我们位于远端图像内部。
    _ensurePointerLockEngaged();

    // 跳过由重新居中操作本身触发的鼠标移动事件。
    if (_skipNextMouseMove) {
      _skipNextMouseMove = false;
      _lastPointerLocalPos = localPosition;
      return true;
    }

    final lastLocal = _lastPointerLocalPos;
    _lastPointerLocalPos = localPosition;

    // Linux 专属：处理增量之前主动检查是否需要重新居中。
    // 在 Linux 上我们没有 clip_cursor，因此如果光标移动过快
    // 可能在 _recenterIfNearEdge 反应过来之前就移出窗口。
    // 现在就检查，必要时立即重新居中。
    if (isLinux) {
      _recenterIfNearEdgeLinux(localPosition);
    }

    // 从上一次位置（而非中心点）计算增量。
    // 这样可避免 CGWarpMouseCursorPosition 整数取整带来的问题。
    if (lastLocal != null) {
      final delta = localPosition - lastLocal;
      if (delta.dx != 0 || delta.dy != 0) {
        sendRelativeMouseMove(delta.dx, delta.dy);
      }
    }

    return true;
  }

  /// Linux 专属：更激进的重新居中检查，防止光标移出窗口。
  /// 在处理鼠标增量之前同步调用，确保光标始终在边界内。
  void _recenterIfNearEdgeLinux(Offset localPosition) {
    final size = _imageWidgetSize;
    if (size == null) return;

    final edgeThreshold = _calculateEdgeThreshold(size);

    final nearLeft = localPosition.dx < edgeThreshold;
    final nearRight = localPosition.dx > size.width - edgeThreshold;
    final nearTop = localPosition.dy < edgeThreshold;
    final nearBottom = localPosition.dy > size.height - edgeThreshold;

    if (nearLeft || nearRight || nearTop || nearBottom) {
      _recenterMouse();
    }
  }

  void sendRelativeMouseMove(double dx, double dy) {
    if (!isDesktop) return;

    final delta = _accumulator.add(dx, dy, maxDelta: kMaxRelativeMouseDelta);
    if (delta == null) return;

    // 将增量放入缓冲区，以便按节流节奏发送。
    _pendingDeltaX += delta.x;
    _pendingDeltaY += delta.y;

    // 启动或刷新节流定时器。
    if (_throttleTimer == null || !_throttleTimer!.isActive) {
      _throttleTimer = Timer(_throttleInterval, () => _flushPendingDelta());
    }
  }

  Future<void> _flushPendingDelta() async {
    if (!isDesktop) return;
    if (_pendingDeltaX == 0 && _pendingDeltaY == 0) return;

    final x = _pendingDeltaX;
    final y = _pendingDeltaY;
    _pendingDeltaX = 0;
    _pendingDeltaY = 0;

    final ok = await _sendMouseMessageToSession({
      'type': 'move_relative',
      'x': '$x',
      'y': '$y',
    });
    if (!ok) return;

    // 仅当鼠标接近图像控件边缘时才重新居中。
    // 这样可以实现平滑的鼠标移动，而不必频繁重新居中。
    _recenterIfNearEdge();
  }

  // 用于重新居中检测的边缘阈值参数。
  // 阈值计算方式：min(maxThreshold, min(width, height) * fraction)
  static const double _edgeThresholdFraction = 0.1; // 10% of smaller dimension
  static const double _edgeThresholdMax =
      100.0; // Maximum threshold in logical pixels
  static const double _edgeThresholdMin =
      20.0; // Minimum threshold for very small widgets

  // Linux 专属的边缘阈值参数（更激进，防止光标移出窗口）。
  // 在 Linux 上我们没有 clip_cursor 能力，因此需要更早地重新居中
  // 以防止快速移动时光标移出窗口。
  static const double _edgeThresholdFractionLinux =
      0.25; // 25% of smaller dimension
  static const double _edgeThresholdMaxLinux =
      200.0; // Larger maximum threshold for Linux
  static const double _edgeThresholdMinLinux =
      50.0; // Larger minimum threshold for Linux

  /// 根据控件尺寸计算动态边缘阈值。
  double _calculateEdgeThreshold(Size size) {
    final smallerDimension = math.min(size.width, size.height);
    if (isLinux) {
      // 在 Linux 上使用更激进的阈值，防止光标移出窗口。
      final dynamicThreshold = smallerDimension * _edgeThresholdFractionLinux;
      return dynamicThreshold.clamp(
          _edgeThresholdMinLinux, _edgeThresholdMaxLinux);
    }
    final dynamicThreshold = smallerDimension * _edgeThresholdFraction;
    // 在最小值与最大值阈值之间做钳制
    return dynamicThreshold.clamp(_edgeThresholdMin, _edgeThresholdMax);
  }

  /// 仅当光标接近图像控件边缘时才重新居中。
  void _recenterIfNearEdge() {
    final lastPos = _lastPointerLocalPos;
    final size = _imageWidgetSize;
    if (lastPos == null || size == null) return;

    // 基于控件尺寸的动态阈值
    final edgeThreshold = _calculateEdgeThreshold(size);

    final nearLeft = lastPos.dx < edgeThreshold;
    final nearRight = lastPos.dx > size.width - edgeThreshold;
    final nearTop = lastPos.dy < edgeThreshold;
    final nearBottom = lastPos.dy > size.height - edgeThreshold;

    if (nearLeft || nearRight || nearTop || nearBottom) {
      _recenterMouse();
    }
  }

  /// 发送不带位置信息的鼠标按键事件（用于相对鼠标模式）。
  Future<void> sendRelativeMouseButton(Map<String, dynamic> evt) async {
    if (!enabled.value) return;
    _ensurePointerLockEngaged();

    final rawType = evt['type'];
    final rawButtons = evt['buttons'];
    if (rawType is! String || rawButtons is! int) return;

    final type = _mouseEventTypeToPeer(rawType);
    if (type.isEmpty) return;

    final buttons = mouseButtonsToPeer(rawButtons);
    if (buttons.isEmpty) return;

    await _sendMouseMessageToSession({
      'type': type,
      'buttons': buttons,
    });
  }

  static String _mouseEventTypeToPeer(String type) {
    switch (type) {
      case 'mousedown':
        return kMouseEventTypeDown;
      case 'mouseup':
        return kMouseEventTypeUp;
      default:
        return '';
    }
  }

  Future<bool> _sendMouseMessageToSession(
    Map<String, dynamic> msg, {
    bool disableRelativeOnError = true,
    bool bypassKeyboardPerm = false,
  }) async {
    if (!bypassKeyboardPerm && !keyboardPerm()) return false;
    if (isViewCamera()) return false;

    try {
      await bind.sessionSendMouse(
        sessionId: sessionId,
        msg: json.encode(modify(msg)),
      );
      return true;
    } catch (e) {
      debugPrint('[RelMouse] Error sending mouse message: $e');
      if (disableRelativeOnError && enabled.value) {
        _disableWithCleanup();
      }
      return false;
    }
  }

  /// 光标重新居中的重试参数。
  static const int _recenterMaxRetries = 3;
  static const Duration _recenterRetryDelay = Duration(milliseconds: 100);

  /// 将光标重新居中到指针锁定中心。
  /// "发出即不管"安全：防止调用重叠，并在内部捕获错误。
  Future<void> _recenterMouse() async {
    // 在高频鼠标移动下防止重新居中操作重叠。
    if (_recenterInProgress) return;
    _recenterInProgress = true;

    try {
      if (!enabled.value) return;
      if (!getPointerInsideImage()) return;

      final center = _pointerLockCenterScreen;
      if (center == null) {
        return;
      }

      for (int attempt = 0; attempt < _recenterMaxRetries; attempt++) {
        // 每次尝试前检查前置条件。
        if (!enabled.value || !getPointerInsideImage()) return;

        final ok = bind.mainSetCursorPosition(
          x: center.dx.toInt(),
          y: center.dy.toInt(),
        );
        if (ok) {
          // 跳过下一个鼠标移动事件 —— 它由重新居中本身触发。
          _skipNextMouseMove = true;
          return;
        }

        // 重试前先等待（最后一次尝试除外）。
        if (attempt < _recenterMaxRetries - 1) {
          await Future.delayed(_recenterRetryDelay);
        }
      }

      // 所有尝试均失败。
      _disableWithCleanup();
      showToast(translate('rel-mouse-lock-failed-tip'));
    } catch (e, st) {
      debugPrint('[RelMouse] Unexpected error in _recenterMouse: $e\n$st');
    } finally {
      _recenterInProgress = false;
    }
  }

  Future<void> updatePointerLockCenter({Offset? localCenter}) async {
    if (!isDesktop) return;

    // 对 kWindowId 进行空值安全检查。
    if (kWindowId == null) {
      if (enabled.value) {
        _disableWithCleanup();
      }
      return;
    }

    try {
      final wc = WindowController.fromWindowId(kWindowId!);
      final frame = await wc.getFrame();

      if (frame.width <= 0 || frame.height <= 0) {
        if (enabled.value) {
          _disableWithCleanup();
        }
        return;
      }

      if (localCenter != null) {
        _pointerLockCenterLocal = localCenter;
      } else if (_imageWidgetSize != null) {
        _pointerLockCenterLocal = Offset(
          _imageWidgetSize!.width / 2,
          _imageWidgetSize!.height / 2,
        );
      } else {
        if (enabled.value) {
          _disableWithCleanup();
        }
        return;
      }

      // 计算用于 OS 光标定位的屏幕坐标。
      // 使用 PlatformDispatcher 而非已废弃的 ui.window。
      final view = ui.PlatformDispatcher.instance.views.firstOrNull;
      if (view == null) {
        debugPrint('[RelMouse] No view available for coordinate calculation');
        if (enabled.value) {
          _disableWithCleanup();
        }
        return;
      }
      final scale = view.devicePixelRatio;

      if (_pointerRegionTopLeftGlobal != null && scale > 0) {
        // 在 macOS 上，窗口边框和 CGWarpMouseCursorPosition 使用点（而非像素）。
        // 在 Windows 上则使用像素。
        // 在 macOS 上，Flutter 的逻辑坐标以点为单位。
        final centerInView =
            _pointerRegionTopLeftGlobal! + _pointerLockCenterLocal!;

        // 计算客户区偏移（不含标题栏和边框）
        final clientPhysical = view.physicalSize;

        // macOS：窗口边框和 CGWarpMouseCursorPosition 都使用点（而非像素）。
        // 我们通过 `/ scale` 把 clientPhysical（像素）转换为点，以计算 titleBarHeight，
        // 即窗口总高度与 Flutter 视图高度之差。
        if (isMacOS) {
          final clientHeightPoints = clientPhysical.height / scale;
          final titleBarHeight = frame.height - clientHeightPoints;

          _pointerLockCenterScreen = Offset(
            frame.left + centerInView.dx,
            frame.top + titleBarHeight + centerInView.dy,
          );
        } else {
          // Windows/Linux：使用像素坐标。我们用启发式方法估算客户区偏移，
          // 依据是边框尺寸与客户区物理尺寸之差。
          // 这假设水平边框对称（extraW / 2），并且剩余的
          // 垂直空间（extraH - borderBottom）即标题栏高度。
          // 局限：对于最大化窗口、自定义窗口
          // 装饰，或操作系统使用不同边框样式时，该启发式可能不准确。
          // TODO: Replace this heuristic with platform API calls (e.g., GetClientRect on Windows)
          //       如果需要精确的客户区偏移。
          final extraW = frame.width - clientPhysical.width;
          final extraH = frame.height - clientPhysical.height;
          final borderX = extraW > 0 ? extraW / 2 : 0.0;
          final borderBottom = borderX;
          final borderTop = extraH > borderBottom ? extraH - borderBottom : 0.0;
          final clientTopLeftScreen =
              Offset(frame.left + borderX, frame.top + borderTop);

          // 先计算候选中心点，再校验其是否位于边框范围内。
          // 这样可以防范启发式不准确带来的问题（例如最大化窗口）。
          final tentativeCenter = Offset(
            clientTopLeftScreen.dx + centerInView.dx * scale,
            clientTopLeftScreen.dy + centerInView.dy * scale,
          );
          final withinFrame = tentativeCenter.dx >= frame.left &&
              tentativeCenter.dx <= frame.left + frame.width &&
              tentativeCenter.dy >= frame.top &&
              tentativeCenter.dy <= frame.top + frame.height;
          _pointerLockCenterScreen = withinFrame
              ? tentativeCenter
              : Offset(
                  frame.left + frame.width / 2, frame.top + frame.height / 2);
        }
      } else {
        _pointerLockCenterScreen = Offset(
          frame.left + frame.width / 2,
          frame.top + frame.height / 2,
        );
      }

      if (enabled.value && isWindows && getPointerInsideImage()) {
        _applyCursorClipForFrame(frame);
      } else if (enabled.value && isWindows && _cursorClipApplied) {
        // 仅在确实施加了裁剪时才释放，避免多余的 FFI 调用。
        _releaseCursorClip();
      }
      // macOS：没有 clip_cursor（CGAssociateMouseAndMouseCursorPosition 会停止鼠标事件）
      // 因此像其他平台一样使用重新居中方法。
    } catch (e) {
      if (enabled.value) {
        _disableWithCleanup();
      } else {
        _pointerLockCenterLocal = null;
        _pointerLockCenterScreen = null;
      }
    }
  }

  void _ensurePointerLockEngaged() {
    if (!enabled.value) return;
    if (!isDesktop) return;

    setPointerInsideImage(true);

    final needsCenter =
        _pointerLockCenterLocal == null || _pointerLockCenterScreen == null;
    // 仅 Windows：光标裁剪
    final needsClip = isWindows && !_cursorClipApplied;
    if (needsCenter || needsClip) {
      updatePointerLockCenter()
          .then((_) => _recenterMouse())
          .catchError((Object e, StackTrace st) {
        debugPrint('[RelMouse] updatePointerLockCenter failed: $e\n$st');
        _disableWithCleanup();
      });
    }
  }

  void _applyCursorClipForFrame(Rect frame) {
    if (!isWindows) return;

    // 使用 PlatformDispatcher 获取设备像素比，以正确缩放。
    final view = ui.PlatformDispatcher.instance.views.firstOrNull;
    final scale = view?.devicePixelRatio ?? 1.0;

    // 获取 Flutter 视图的物理尺寸（以像素计的客户区）。
    final clientPhysical = view?.physicalSize ?? ui.Size.zero;

    // 计算非客户区（操作系统窗口标题栏、边框）。
    // frame 包含整个窗口（标题栏 + 边框 + 客户区）。
    final extraW = frame.width - clientPhysical.width;
    final extraH = frame.height - clientPhysical.height;

    // 假设水平边框对称。
    final borderX = extraW > 0 ? extraW / 2 : 0.0;
    // 底部边框通常与侧边边框相同。
    final borderBottom = borderX;
    // 操作系统窗口标题栏高度就是剩余的垂直非客户区空间。
    final borderTop = extraH > borderBottom ? extraH - borderBottom : 0.0;

    // 计算客户区左上角的屏幕坐标。
    final clientTopLeftScreen =
        Offset(frame.left + borderX, frame.top + borderTop);

    int left, top, right, bottom;

    // 如果掌握了精确的图像控件信息，则裁剪到远端图像区域。
    // 这样可排除 Flutter 应用内部的标题栏和工具栏。
    if (_pointerRegionTopLeftGlobal != null &&
        _imageWidgetSize != null &&
        scale > 0) {
      // _pointerRegionTopLeftGlobal 采用 Flutter 逻辑坐标（相对于客户区）。
      // 转换为屏幕物理坐标。
      left = (clientTopLeftScreen.dx + _pointerRegionTopLeftGlobal!.dx * scale)
          .toInt();
      top = (clientTopLeftScreen.dy + _pointerRegionTopLeftGlobal!.dy * scale)
          .toInt();
      right = (left + _imageWidgetSize!.width * scale).toInt();
      bottom = (top + _imageWidgetSize!.height * scale).toInt();
    } else {
      // 回退方案：裁剪到客户区（不含操作系统窗口装饰）。
      left = clientTopLeftScreen.dx.toInt();
      top = clientTopLeftScreen.dy.toInt();
      right = (frame.left + frame.width - borderX).toInt();
      bottom = (frame.top + frame.height - borderBottom).toInt();
    }

    _cursorClipApplied = bind.mainClipCursor(
      left: left,
      top: top,
      right: right,
      bottom: bottom,
      enable: true,
    );
  }

  void _releaseCursorClip() {
    if (!_cursorClipApplied) return;
    _cursorClipApplied = false;
    if (!isWindows) return;

    bind.mainClipCursor(
      left: 0,
      top: 0,
      right: 0,
      bottom: 0,
      enable: false,
    );
  }

  void _resetState() {
    // 在清除状态前先把待处理的增量发送出去。
    // 确保在数值被清零前，最后一批缓冲的移动已发送。
    // "发出即不管"：不等待异步发送完成。
    if (_throttleTimer != null || _pendingDeltaX != 0 || _pendingDeltaY != 0) {
      _throttleTimer?.cancel();
      _throttleTimer = null;
      if (_pendingDeltaX != 0 || _pendingDeltaY != 0) {
        final x = _pendingDeltaX;
        final y = _pendingDeltaY;
        _pendingDeltaX = 0;
        _pendingDeltaY = 0;
        // 发送但不等待；由于正在禁用，跳过重新居中。
        _sendMouseMessageToSession({
          'type': 'move_relative',
          'x': '$x',
          'y': '$y',
        }, disableRelativeOnError: false);
      }
    }
    _accumulator.reset();
    _pointerLockCenterLocal = null;
    _pointerLockCenterScreen = null;
    _pointerRegionTopLeftGlobal = null;
    _lastPointerLocalPos = null;
    _skipNextMouseMove = false;
    setPointerInsideImage(false);
    _cursorClipApplied = false;
    _exitShortcutKeyDown = false;
  }

  /// [_disableWithCleanup] 与 [dispose] 共用的核心清理逻辑。
  /// 向 Rust 发送禁用消息，释放平台资源并重置状态。
  void _performCleanupCore() {
    // 给 Rust rdev 抓取循环的尽力而为标记（ESC 行为）。
    // 绕过 keyboardPerm 检查以确保 Rust 状态始终同步。
    _sendMouseMessageToSession(
      {
        'relative_mouse_mode': '0',
      },
      disableRelativeOnError: false,
      bypassKeyboardPerm: true,
    );

    // macOS：禁用原生相对鼠标模式
    // 它已经调用 CGAssociateMouseAndMouseCursorPosition(1) 来重新关联鼠标
    if (isMacOS) {
      _disableNativeRelativeMouseMode();
    } else {
      _releaseCursorClip();
    }

    _resetState();
  }

  void _disableWithCleanup() {
    _performCleanupCore();
    enabled.value = false;
    onDisabled?.call();
  }

  bool _disposed = false;

  void dispose() {
    if (_disposed) return;
    _disposed = true;

    _performCleanupCore();
    _imageWidgetSize = null;
    _lastToggle = null;
    // 在调用 onDisabled 之前将 enabled 设为 false，与 _disableWithCleanup() 保持一致。
    enabled.value = false;
    // 先触发回调再清空，以便外部清理得以执行。
    onDisabled?.call();
    onDisabled = null;
  }
}
