import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';
import 'package:flutter/foundation.dart';
import 'dart:ui' as ui;

import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_hbb/main.dart';
import 'package:flutter_hbb/utils/multi_window_manager.dart';
import 'package:get/get.dart';

import '../../models/model.dart';
import '../../models/platform_model.dart';
import '../../models/shortcut_model.dart';
import '../../models/state_model.dart';
import '../common/widgets/keyboard_shortcuts/shortcut_utils.dart';
import 'input_modifier_utils.dart';
import 'local_flutter_shortcuts.dart';
import 'relative_mouse_model.dart';
import '../common.dart';
import '../consts.dart';

/// 鼠标按键枚举。
enum MouseButtons { left, right, wheel, back, forward }

const _kMouseEventDown = 'mousedown';
const _kMouseEventUp = 'mouseup';
const _kMouseEventMove = 'mousemove';

class CanvasCoords {
  double x = 0;
  double y = 0;
  double scale = 1.0;
  double scrollX = 0;
  double scrollY = 0;
  ScrollStyle scrollStyle = ScrollStyle.scrollauto;
  Size size = Size.zero;

  CanvasCoords();

  Map<String, dynamic> toJson() {
    return {
      'x': x,
      'y': y,
      'scale': scale,
      'scrollX': scrollX,
      'scrollY': scrollY,
      'scrollStyle': scrollStyle.toJson(),
      'size': {
        'w': size.width,
        'h': size.height,
      }
    };
  }

  static CanvasCoords fromJson(Map<String, dynamic> json) {
    final model = CanvasCoords();
    model.x = json['x'];
    model.y = json['y'];
    model.scale = json['scale'];
    model.scrollX = json['scrollX'];
    model.scrollY = json['scrollY'];
    model.scrollStyle =
        ScrollStyle.fromJson(json['scrollStyle'], ScrollStyle.scrollauto);
    model.size = Size(json['size']['w'], json['size']['h']);
    return model;
  }

  static CanvasCoords fromCanvasModel(CanvasModel model) {
    final coords = CanvasCoords();
    coords.x = model.x;
    coords.y = model.y;
    coords.scale = model.scale;
    coords.scrollX = model.scrollX;
    coords.scrollY = model.scrollY;
    coords.scrollStyle = model.scrollStyle;
    coords.size = model.size;
    return coords;
  }
}

class CursorCoords {
  Offset offset = Offset.zero;

  CursorCoords();

  Map<String, dynamic> toJson() {
    return {
      'offset_x': offset.dx,
      'offset_y': offset.dy,
    };
  }

  static CursorCoords fromJson(Map<String, dynamic> json) {
    final model = CursorCoords();
    model.offset = Offset(json['offset_x'], json['offset_y']);
    return model;
  }

  static CursorCoords fromCursorModel(CursorModel model) {
    final coords = CursorCoords();
    coords.offset = model.offset;
    return coords;
  }
}

class RemoteWindowCoords {
  RemoteWindowCoords(
      this.windowRect, this.canvas, this.cursor, this.remoteRect);
  Rect windowRect;
  CanvasCoords canvas;
  CursorCoords cursor;
  Rect remoteRect;
  Offset relativeOffset = Offset.zero;

  Map<String, dynamic> toJson() {
    return {
      'canvas': canvas.toJson(),
      'cursor': cursor.toJson(),
      'windowRect': rectToJson(windowRect),
      'remoteRect': rectToJson(remoteRect),
    };
  }

  static Map<String, dynamic> rectToJson(Rect r) {
    return {
      'l': r.left,
      't': r.top,
      'w': r.width,
      'h': r.height,
    };
  }

  static Rect rectFromJson(Map<String, dynamic> json) {
    return Rect.fromLTWH(
      json['l'],
      json['t'],
      json['w'],
      json['h'],
    );
  }

  RemoteWindowCoords.fromJson(Map<String, dynamic> json)
      : windowRect = rectFromJson(json['windowRect']),
        canvas = CanvasCoords.fromJson(json['canvas']),
        cursor = CursorCoords.fromJson(json['cursor']),
        remoteRect = rectFromJson(json['remoteRect']);
}

extension ToString on MouseButtons {
  String get value {
    switch (this) {
      case MouseButtons.left:
        return 'left';
      case MouseButtons.right:
        return 'right';
      case MouseButtons.wheel:
        return 'wheel';
      case MouseButtons.back:
        return 'back';
      case MouseButtons.forward:
        return 'forward';
    }
  }
}

class PointerEventToRust {
  final String kind;
  final String type;
  final dynamic value;

  PointerEventToRust(this.kind, this.type, this.value);

  Map<String, dynamic> toJson() {
    return {
      'k': kind,
      'v': {
        't': type,
        'v': value,
      }
    };
  }
}

class ToReleaseRawKeys {
  RawKeyEvent? lastLShiftKeyEvent;
  RawKeyEvent? lastRShiftKeyEvent;
  RawKeyEvent? lastLCtrlKeyEvent;
  RawKeyEvent? lastRCtrlKeyEvent;
  RawKeyEvent? lastLAltKeyEvent;
  RawKeyEvent? lastRAltKeyEvent;
  RawKeyEvent? lastLCommandKeyEvent;
  RawKeyEvent? lastRCommandKeyEvent;
  RawKeyEvent? lastSuperKeyEvent;

  reset() {
    lastLShiftKeyEvent = null;
    lastRShiftKeyEvent = null;
    lastLCtrlKeyEvent = null;
    lastRCtrlKeyEvent = null;
    lastLAltKeyEvent = null;
    lastRAltKeyEvent = null;
    lastLCommandKeyEvent = null;
    lastRCommandKeyEvent = null;
    lastSuperKeyEvent = null;
  }

  updateKeyDown(LogicalKeyboardKey logicKey, RawKeyDownEvent e) {
    if (e.isAltPressed) {
      if (logicKey == LogicalKeyboardKey.altLeft) {
        lastLAltKeyEvent = e;
      } else if (logicKey == LogicalKeyboardKey.altRight) {
        lastRAltKeyEvent = e;
      }
    } else if (e.isControlPressed) {
      if (logicKey == LogicalKeyboardKey.controlLeft) {
        lastLCtrlKeyEvent = e;
      } else if (logicKey == LogicalKeyboardKey.controlRight) {
        lastRCtrlKeyEvent = e;
      }
    } else if (e.isShiftPressed) {
      if (logicKey == LogicalKeyboardKey.shiftLeft) {
        lastLShiftKeyEvent = e;
      } else if (logicKey == LogicalKeyboardKey.shiftRight) {
        lastRShiftKeyEvent = e;
      }
    } else if (e.isMetaPressed) {
      if (logicKey == LogicalKeyboardKey.metaLeft) {
        lastLCommandKeyEvent = e;
      } else if (logicKey == LogicalKeyboardKey.metaRight) {
        lastRCommandKeyEvent = e;
      } else if (logicKey == LogicalKeyboardKey.superKey) {
        lastSuperKeyEvent = e;
      }
    }
  }

  updateKeyUp(LogicalKeyboardKey logicKey, RawKeyUpEvent e) {
    if (e.isAltPressed) {
      if (logicKey == LogicalKeyboardKey.altLeft) {
        lastLAltKeyEvent = null;
      } else if (logicKey == LogicalKeyboardKey.altRight) {
        lastRAltKeyEvent = null;
      }
    } else if (e.isControlPressed) {
      if (logicKey == LogicalKeyboardKey.controlLeft) {
        lastLCtrlKeyEvent = null;
      } else if (logicKey == LogicalKeyboardKey.controlRight) {
        lastRCtrlKeyEvent = null;
      }
    } else if (e.isShiftPressed) {
      if (logicKey == LogicalKeyboardKey.shiftLeft) {
        lastLShiftKeyEvent = null;
      } else if (logicKey == LogicalKeyboardKey.shiftRight) {
        lastRShiftKeyEvent = null;
      }
    } else if (e.isMetaPressed) {
      if (logicKey == LogicalKeyboardKey.metaLeft) {
        lastLCommandKeyEvent = null;
      } else if (logicKey == LogicalKeyboardKey.metaRight) {
        lastRCommandKeyEvent = null;
      } else if (logicKey == LogicalKeyboardKey.superKey) {
        lastSuperKeyEvent = null;
      }
    }
  }

  release(KeyEventResult Function(RawKeyEvent e) handleRawKeyEvent) {
    for (final key in [
      lastLShiftKeyEvent,
      lastRShiftKeyEvent,
      lastLCtrlKeyEvent,
      lastRCtrlKeyEvent,
      lastLAltKeyEvent,
      lastRAltKeyEvent,
      lastLCommandKeyEvent,
      lastRCommandKeyEvent,
      lastSuperKeyEvent,
    ]) {
      if (key != null) {
        handleRawKeyEvent(RawKeyUpEvent(
          data: key.data,
          character: key.character,
        ));
      }
    }
  }
}

class ToReleaseKeys {
  KeyEvent? lastLShiftKeyEvent;
  KeyEvent? lastRShiftKeyEvent;
  KeyEvent? lastLCtrlKeyEvent;
  KeyEvent? lastRCtrlKeyEvent;
  KeyEvent? lastLAltKeyEvent;
  KeyEvent? lastRAltKeyEvent;
  KeyEvent? lastLCommandKeyEvent;
  KeyEvent? lastRCommandKeyEvent;
  KeyEvent? lastSuperKeyEvent;

  reset() {
    lastLShiftKeyEvent = null;
    lastRShiftKeyEvent = null;
    lastLCtrlKeyEvent = null;
    lastRCtrlKeyEvent = null;
    lastLAltKeyEvent = null;
    lastRAltKeyEvent = null;
    lastLCommandKeyEvent = null;
    lastRCommandKeyEvent = null;
    lastSuperKeyEvent = null;
  }

  release(KeyEventResult Function(KeyEvent e) handleKeyEvent) {
    for (final key in [
      lastLShiftKeyEvent,
      lastRShiftKeyEvent,
      lastLCtrlKeyEvent,
      lastRCtrlKeyEvent,
      lastLAltKeyEvent,
      lastRAltKeyEvent,
      lastLCommandKeyEvent,
      lastRCommandKeyEvent,
      lastSuperKeyEvent,
    ]) {
      if (key != null) {
        handleKeyEvent(key);
      }
    }
  }
}

class InputModel {
  // Linux 上的侧键支持。
  // Flutter 的 Linux 嵌入器会丢弃 X11 的 8/9 号按键事件，所以我们通过 GDK
  // 在原生层面捕获它们，并通过平台通道转发。
  static InputModel? _activeSideButtonModel;
  // 按按键分别跟踪哪个模型收到了侧键按下事件，这样
  // 对应的抬起事件也会被路由到那里，即使指针已经离开视图
  // 或期间按下了其他按键。
  static final Map<MouseButtons, InputModel> _sideButtonDownModels = {};
  static bool _sideButtonChannelInitialized = false;

  /// 每个 Flutter 引擎（主窗口 + desktop_multi_window 的子窗口）
  /// 都运行自己的 Dart isolate，拥有各自的静态变量。该函数从 initEnv() 调用，
  /// 而 initEnv() 是按引擎运行的，因此每个 isolate 都注册自己的处理函数，
  /// 并绑定到它自己的那组 InputModel。
  static void initSideButtonChannel() {
    if (!isLinux) return;
    if (_sideButtonChannelInitialized) return;
    _sideButtonChannelInitialized = true;

    const channel = MethodChannel('org.rustdesk.rustdesk/side_buttons');
    channel.setMethodCallHandler((call) async {
      if (call.method == 'onSideMouseButton') {
        final args = call.arguments as Map<dynamic, dynamic>;
        final button = args['button'] as String;
        final type = args['type'] as String;
        final mb = button == 'back' ? MouseButtons.back : MouseButtons.forward;

        if (type == 'down') {
          final model = _activeSideButtonModel;
          if (model != null &&
              !(model.isViewOnly && !model.showMyCursor) &&
              model.keyboardPerm &&
              !model.isViewCamera) {
            _sideButtonDownModels[mb] = model;
            // 发出即忘，以免阻塞平台通道处理函数。
            unawaited(model._sendMouseUnchecked(type, mb).catchError((Object e) {
              debugPrint('[InputModel] failed to send side button $type for $mb: $e');
            }));
          }
        } else {
          // 只有记录了对应的 'down' 时才路由 'up'；
          // 丢弃可以避免把无配对的 'up' 发给无关的会话。
          // 使用 _sendMouseUnchecked 绕过权限检查，这样
          // 即使权限发生变化，抬起事件也总能发送出去。
          final model = _sideButtonDownModels.remove(mb);
          if (model != null) {
            unawaited(model._sendMouseUnchecked(type, mb).catchError((Object e) {
              debugPrint('[InputModel] failed to send side button $type for $mb: $e');
            }));
          }
        }
      }
      return null;
    });
  }

  /// 清除指向该模型的所有静态引用（防止路由到过期对象）。
  /// 在远端释放任何仍按住的侧键，这样关闭会话时
  /// 不会在按下过程中留下卡住的按键。
  void disposeSideButtonTracking() {
    if (_activeSideButtonModel == this) _activeSideButtonModel = null;
    final held = _sideButtonDownModels.entries
        .where((e) => e.value == this)
        .map((e) => e.key)
        .toList();
    for (final mb in held) {
      _sideButtonDownModels.remove(mb);
      // 尽力释放；会话可能已经在销毁过程中。
      unawaited(_sendMouseUnchecked('up', mb).catchError((Object e) {
        debugPrint('[InputModel] failed to release side button $mb: $e');
      }));
    }
  }

  final WeakReference<FFI> parent;
  String keyboardMode = '';

  // 键盘
  var shift = false;
  var ctrl = false;
  var alt = false;
  var command = false;

  final ToReleaseRawKeys toReleaseRawKeys = ToReleaseRawKeys();
  final ToReleaseKeys toReleaseKeys = ToReleaseKeys();
  late final _localFlutterShortcuts = LocalFlutterShortcutDispatcher(
    onTriggered: (action) {
      final ffi = parent.target;
      if (ffi != null && !ffi.closed) {
        ffi.shortcutModel.onTriggered(action);
      }
    },
  );

  // 触控板
  var _trackpadLastDelta = Offset.zero;
  var _stopFling = true;
  var _fling = false;
  Timer? _flingTimer;
  final _flingBaseDelay = 30;
  final _trackpadAdjustPeerLinux = 0.06;
  // 这是一个经验值。
  final _trackpadAdjustMacToWin = 2.50;
  // 当两个轴上的增量都非常小时，忽略方向锁定（包括
  // 极小的单轴移动），避免过滤掉接近零的移动。
  static const double _trackpadAxisNoiseThreshold = 0.2;
  // 只有一个轴明显更强时，才锁定到主轴。
  // 1.6 表示主轴必须比另一个轴大 60% 以上。
  static const double _trackpadAxisLockRatio = 1.6;
  int _trackpadSpeed = kDefaultTrackpadSpeed;
  double _trackpadSpeedInner = kDefaultTrackpadSpeed / 100.0;
  var _trackpadScrollUnsent = Offset.zero;

  // 移动端相对鼠标的增量累加器（用于缓慢/精细的移动）。
  double _mobileDeltaRemainderX = 0.0;
  double _mobileDeltaRemainderY = 0.0;

  var _lastScale = 1.0;

  bool _pointerMovedAfterEnter = false;
  bool _pointerInsideImage = false;

  // 鼠标
  final isPhysicalMouse = false.obs;
  int _lastButtons = 0;
  int _pressedMouseButtons = 0;
  Offset lastMousePos = Offset.zero;
  int _lastWheelTsUs = 0;

  // 滚轮加速阈值。
  static const int _wheelAccelFastThresholdUs = 40000; // 40ms
  static const int _wheelAccelMediumThresholdUs = 80000; // 80ms
  static const double _wheelBurstVelocityThreshold =
      0.002; // delta units per microsecond
  // 滚轮连击加速（经验调优）。
  // 仅对快速且不平滑的连击生效，以保留单步滚动。
  // Flutter 的 dt 使用微秒，因此速度单位是 delta/微秒。

  // 相对鼠标模式（用于游戏/3D 应用）。
  final relativeMouseMode = false.obs;
  late final RelativeMouseModel _relativeMouse;
  // 在禁用相对鼠标模式时用于取消外部节流定时器的回调。
  VoidCallback? onRelativeMouseModeDisabled;
  // relativeMouseMode 观察者的 Disposer（防止内存泄漏）。
  Worker? _relativeMouseModeDisposer;

  bool _queryOtherWindowCoords = false;
  Rect? _windowRect;
  List<RemoteWindowCoords> _remoteWindowCoords = [];

  late final SessionID sessionId;

  // 移动端 Wayland 对话框中剪贴板辅助输入流程的本地开关。
  // 它不应阻塞物理键盘事件。
  bool keyboardInputAllowed = true;

  bool get keyboardPerm => parent.target!.ffiModel.keyboard;
  String get id => parent.target?.id ?? '';
  String? get peerPlatform => parent.target?.ffiModel.pi.platform;
  String get peerVersion => parent.target?.ffiModel.pi.version ?? '';
  bool get isViewOnly => parent.target!.ffiModel.viewOnly;
  bool get showMyCursor => parent.target!.ffiModel.showMyCursor;
  double get devicePixelRatio => parent.target!.canvasModel.devicePixelRatio;
  bool get isViewCamera => parent.target!.connType == ConnType.viewCamera;
  int get trackpadSpeed => _trackpadSpeed;
  bool get useEdgeScroll =>
      parent.target!.canvasModel.scrollStyle == ScrollStyle.scrolledge;

  /// 检查已连接的服务器是否支持相对鼠标模式。
  bool get isRelativeMouseModeSupported => _relativeMouse.isSupported;

  InputModel(this.parent) {
    initSideButtonChannel();
    sessionId = parent.target!.sessionId;
    _relativeMouse = RelativeMouseModel(
      sessionId: sessionId,
      enabled: relativeMouseMode,
      keyboardPerm: () => keyboardPerm,
      isViewCamera: () => isViewCamera,
      peerVersion: () => peerVersion,
      peerPlatform: () => peerPlatform,
      modify: (msg) => modify(msg),
      getPointerInsideImage: () => _pointerInsideImage,
      setPointerInsideImage: (inside) => _pointerInsideImage = inside,
    );
    _relativeMouse.onDisabled = () => onRelativeMouseModeDisabled?.call();

    // 把相对鼠标模式状态同步到全局状态，供 UI 组件使用（例如标签栏提示）。
    _relativeMouseModeDisposer = ever(relativeMouseMode, (bool value) {
      final peerId = id;
      if (peerId.isNotEmpty) {
        stateGlobal.relativeMouseModeState[peerId] = value;
      }
    });
  }

  // https://github.com/flutter/flutter/issues/157241
  // 从字符输出推断 CapsLock 状态。
  // 之所以需要这样做，是因为 Flutter 的 HardwareKeyboard.lockModesEnabled 可能报告
  // 错误的 CapsLock 状态。
  bool _getIosCapsFromCharacter(KeyEvent e) {
    if (!isIOS) return false;
    final ch = e.character;
    return _getIosCapsFromCharacterImpl(
        ch, HardwareKeyboard.instance.isShiftPressed);
  }

  // _getIosCapsFromCharacter 的 RawKeyEvent 版本。
  bool _getIosCapsFromRawCharacter(RawKeyEvent e) {
    if (!isIOS) return false;
    final ch = e.character;
    return _getIosCapsFromCharacterImpl(ch, e.isShiftPressed);
  }

  // 从字符推断 CapsLock 状态的共享实现。
  // 使用感知 Unicode 的大小写检测，以支持非 ASCII 字母（例如 ü/Ü、é/É）。
  //
  // 限制：
  // 1. 该推断假设客户端和服务端使用相同的键盘布局。
  //    如果布局不同（例如客户端用 EN、服务端用 DE），字符输出
  //    可能与预期不符。例如，EN 布局上的 ';' 在 DE 布局上会映射为 'ö'
  //    布局，这使得无法仅凭字符
  //    正确推断 CapsLock 状态。
  // 2. 在 iOS 上，CapsLock+Shift 会产生大写字母（不同于桌面端，那里会
  //    产生小写字母）。该方法无法正确处理这种情况。
  bool _getIosCapsFromCharacterImpl(String? ch, bool shiftPressed) {
    if (ch == null || ch.length != 1) return false;
    // 使用 Dart 内置的、感知 Unicode 的大小写检测
    final upper = ch.toUpperCase();
    final lower = ch.toLowerCase();
    final isUpper = upper == ch && lower != ch;
    final isLower = lower == ch && upper != ch;
    // 跳过非字母字符（例如数字、符号、没有大小写之分的 CJK 字符）
    if (!isUpper && !isLower) return false;
    return isUpper != shiftPressed;
  }

  int _buildLockModes(bool iosCapsLock) {
    const capslock = 1;
    const numlock = 2;
    const scrolllock = 3;
    int lockModes = 0;
    if (isIOS) {
      if (iosCapsLock) {
        lockModes |= (1 << capslock);
      }
      // 暂时忽略 iOS 上的 "NumLock/ScrollLock"。
    } else {
      if (HardwareKeyboard.instance.lockModesEnabled
          .contains(KeyboardLockMode.capsLock)) {
        lockModes |= (1 << capslock);
      }
      if (HardwareKeyboard.instance.lockModesEnabled
          .contains(KeyboardLockMode.numLock)) {
        lockModes |= (1 << numlock);
      }
      if (HardwareKeyboard.instance.lockModesEnabled
          .contains(KeyboardLockMode.scrollLock)) {
        lockModes |= (1 << scrolllock);
      }
    }
    return lockModes;
  }

  // 该函数必须在收到设备信息之后调用。
  // 因为 `sessionGetKeyboardMode` 依赖于设备版本。
  updateKeyboardMode() async {
    // * 目前移动端未启用 map 模式
    if (isDesktop || isWebDesktop) {
      keyboardMode = await bind.sessionGetKeyboardMode(sessionId: sessionId) ??
          kKeyLegacyMode;
    }
  }

  /// 根据会话中的值更新触控板速度。
  ///
  /// 取到的值应为可解析为 double 的字符串。
  /// 如果解析失败或该值超出范围（小于 `kMinTrackpadSpeed` 或大于
  /// `kMaxTrackpadSpeed`），则把触控板速度重置为默认值
  /// （`kDefaultTrackpadSpeed`）。
  ///
  /// 范围：
  /// - 最小值：`kMinTrackpadSpeed`
  /// - 最大值：`kMaxTrackpadSpeed`
  /// - 默认值：`kDefaultTrackpadSpeed`
  Future<void> updateTrackpadSpeed() async {
    _trackpadSpeed =
        (await bind.sessionGetTrackpadSpeed(sessionId: sessionId) ??
            kDefaultTrackpadSpeed);
    if (_trackpadSpeed < kMinTrackpadSpeed ||
        _trackpadSpeed > kMaxTrackpadSpeed) {
      _trackpadSpeed = kDefaultTrackpadSpeed;
    }
    _trackpadSpeedInner = _trackpadSpeed / 100.0;
  }

  void handleKeyDownEventModifiers(KeyEvent e) {
    KeyUpEvent upEvent(e) => KeyUpEvent(
          physicalKey: e.physicalKey,
          logicalKey: e.logicalKey,
          timeStamp: e.timeStamp,
        );
    if (e.logicalKey == LogicalKeyboardKey.altLeft) {
      if (!alt) {
        alt = true;
      }
      toReleaseKeys.lastLAltKeyEvent = upEvent(e);
    } else if (e.logicalKey == LogicalKeyboardKey.altRight) {
      if (!alt) {
        alt = true;
      }
      toReleaseKeys.lastLAltKeyEvent = upEvent(e);
    } else if (e.logicalKey == LogicalKeyboardKey.controlLeft) {
      if (!ctrl) {
        ctrl = true;
      }
      toReleaseKeys.lastLCtrlKeyEvent = upEvent(e);
    } else if (e.logicalKey == LogicalKeyboardKey.controlRight) {
      if (!ctrl) {
        ctrl = true;
      }
      toReleaseKeys.lastRCtrlKeyEvent = upEvent(e);
    } else if (e.logicalKey == LogicalKeyboardKey.shiftLeft) {
      if (!shift) {
        shift = true;
      }
      toReleaseKeys.lastLShiftKeyEvent = upEvent(e);
    } else if (e.logicalKey == LogicalKeyboardKey.shiftRight) {
      if (!shift) {
        shift = true;
      }
      toReleaseKeys.lastRShiftKeyEvent = upEvent(e);
    } else if (e.logicalKey == LogicalKeyboardKey.metaLeft) {
      if (!command) {
        command = true;
      }
      toReleaseKeys.lastLCommandKeyEvent = upEvent(e);
    } else if (e.logicalKey == LogicalKeyboardKey.metaRight) {
      if (!command) {
        command = true;
      }
      toReleaseKeys.lastRCommandKeyEvent = upEvent(e);
    } else if (e.logicalKey == LogicalKeyboardKey.superKey) {
      if (!command) {
        command = true;
      }
      toReleaseKeys.lastSuperKeyEvent = upEvent(e);
    }
  }

  void handleKeyUpEventModifiers(KeyEvent e) {
    if (e.logicalKey == LogicalKeyboardKey.altLeft) {
      alt = false;
      toReleaseKeys.lastLAltKeyEvent = null;
    } else if (e.logicalKey == LogicalKeyboardKey.altRight) {
      alt = false;
      toReleaseKeys.lastRAltKeyEvent = null;
    } else if (e.logicalKey == LogicalKeyboardKey.controlLeft) {
      ctrl = false;
      toReleaseKeys.lastLCtrlKeyEvent = null;
    } else if (e.logicalKey == LogicalKeyboardKey.controlRight) {
      ctrl = false;
      toReleaseKeys.lastRCtrlKeyEvent = null;
    } else if (e.logicalKey == LogicalKeyboardKey.shiftLeft) {
      shift = false;
      toReleaseKeys.lastLShiftKeyEvent = null;
    } else if (e.logicalKey == LogicalKeyboardKey.shiftRight) {
      shift = false;
      toReleaseKeys.lastRShiftKeyEvent = null;
    } else if (e.logicalKey == LogicalKeyboardKey.metaLeft) {
      command = false;
      toReleaseKeys.lastLCommandKeyEvent = null;
    } else if (e.logicalKey == LogicalKeyboardKey.metaRight) {
      command = false;
      toReleaseKeys.lastRCommandKeyEvent = null;
    } else if (e.logicalKey == LogicalKeyboardKey.superKey) {
      command = false;
      toReleaseKeys.lastSuperKeyEvent = null;
    }
  }

  // 安全：这只会重新派发合成的 Shift 抬起事件。
  // 抬起路径会清除已跟踪的 Shift 状态，因此不会形成循环。
  void _releaseTrackedShiftKeyEventIfNeeded() {
    final leftShift = toReleaseKeys.lastLShiftKeyEvent;
    final rightShift = toReleaseKeys.lastRShiftKeyEvent;
    if (leftShift != null) {
      handleKeyEvent(leftShift);
    }
    if (rightShift != null) {
      handleKeyEvent(rightShift);
    }
  }

  // 安全：这只会重新派发合成的 Shift 抬起事件。
  // 原始抬起路径会清除已跟踪的 Shift 状态，因此不会形成循环。
  void _releaseTrackedRawShiftKeyEventIfNeeded() {
    final leftShift = toReleaseRawKeys.lastLShiftKeyEvent;
    final rightShift = toReleaseRawKeys.lastRShiftKeyEvent;
    if (leftShift != null) {
      handleRawKeyEvent(RawKeyUpEvent(
        data: leftShift.data,
        character: leftShift.character,
      ));
    }
    if (rightShift != null) {
      handleRawKeyEvent(RawKeyUpEvent(
        data: rightShift.data,
        character: rightShift.character,
      ));
    }
  }

  KeyEventResult handleRawKeyEvent(RawKeyEvent e) {
    if (_localFlutterShortcuts.tryDispatchRaw(e)) return KeyEventResult.handled;
    if (isViewOnly) {
      if (isWeb && !isInputSourceFlutter) return KeyEventResult.ignored;
      _tryDispatchRawFlutterShortcut(e, releaseModifiers: false);
      return KeyEventResult.handled;
    }
    if (isViewCamera) return KeyEventResult.handled;
    if (!isInputSourceFlutter) {
      if (isDesktop) {
        return KeyEventResult.handled;
      } else if (isWeb) {
        return KeyEventResult.ignored;
      }
    }
    _syncFlutterShortcutModifiersAfterViewOnly(e.physicalKey, raw: true);

    if (_relativeMouse.handleRawKeyEvent(e)) {
      return KeyEventResult.handled;
    }

    bool iosCapsLock = false;
    if (isIOS && e is RawKeyDownEvent) {
      iosCapsLock = _getIosCapsFromRawCharacter(e);
    }

    final key = e.logicalKey;
    if (e is RawKeyDownEvent) {
      if (!e.repeat) {
        if (e.isAltPressed && !alt) {
          alt = true;
        } else if (e.isControlPressed && !ctrl) {
          ctrl = true;
        } else if (e.isShiftPressed && !shift) {
          shift = true;
        } else if (e.isMetaPressed && !command) {
          command = true;
        }
      }
      toReleaseRawKeys.updateKeyDown(key, e);
    }
    if (e is RawKeyUpEvent) {
      if (key == LogicalKeyboardKey.altLeft ||
          key == LogicalKeyboardKey.altRight) {
        alt = false;
      } else if (key == LogicalKeyboardKey.controlLeft ||
          key == LogicalKeyboardKey.controlRight) {
        ctrl = false;
      } else if (key == LogicalKeyboardKey.shiftRight ||
          key == LogicalKeyboardKey.shiftLeft) {
        shift = false;
      } else if (key == LogicalKeyboardKey.metaLeft ||
          key == LogicalKeyboardKey.metaRight ||
          key == LogicalKeyboardKey.superKey) {
        command = false;
      }

      toReleaseRawKeys.updateKeyUp(key, e);
    }

    // 在某些移动端软键盘路径上，Flutter 可能会保留缓存的 Shift 状态
    // ，即使当前的原始按键事件已经不再处于 Shift 状态。
    if (e is RawKeyDownEvent &&
        shouldReleaseStaleMobileShift(
          isMobile: isMobile,
          cachedShiftPressed: shift,
          actualShiftPressed: e.isShiftPressed,
          logicalKey: e.logicalKey,
          hasTrackedShiftKeyDown: toReleaseRawKeys.lastLShiftKeyEvent != null ||
              toReleaseRawKeys.lastRShiftKeyEvent != null,
        )) {
      if (kDebugMode) {
        debugPrint(
          'input: releasing stale mobile Shift before replaying tracked raw '
          'key-up (logicalKey=${e.logicalKey.keyLabel}, '
          'actualShiftPressed=${e.isShiftPressed}, cachedShiftPressed=$shift)',
        );
      }
      _releaseTrackedRawShiftKeyEventIfNeeded();
    }

    // * 目前移动端未启用 map 模式
    if ((isDesktop || isWebDesktop) && keyboardMode == kKeyMapMode) {
      _restoreRawFlutterShortcutModifiers(e);
      mapKeyboardModeRaw(e, iosCapsLock);
    } else {
      if (_tryDispatchRawFlutterShortcut(e)) return KeyEventResult.handled;
      _restoreRawFlutterShortcutModifiers(e, mapMode: false);
      _trackFlutterShortcutKey(e.physicalKey, e.logicalKey,
          down: e is RawKeyDownEvent && !e.repeat, up: e is RawKeyUpEvent);
      legacyKeyboardModeRaw(e);
    }

    return KeyEventResult.handled;
  }

  KeyEventResult handleKeyEvent(KeyEvent e) {
    if (_localFlutterShortcuts.tryDispatch(e)) return KeyEventResult.handled;
    if (isViewOnly) {
      if (isWeb && !isInputSourceFlutter) return KeyEventResult.ignored;
      _tryDispatchFlutterShortcut(e, releaseModifiers: false);
      return KeyEventResult.handled;
    }
    if (isViewCamera) return KeyEventResult.handled;
    if (!isInputSourceFlutter) {
      if (isDesktop) {
        return KeyEventResult.handled;
      } else if (isWeb) {
        return KeyEventResult.ignored;
      }
    }
    _syncFlutterShortcutModifiersAfterViewOnly(e.physicalKey);
    if (isWeb && _tryDispatchFlutterShortcut(e)) {
      return KeyEventResult.handled;
    }
    if (isWindows || isLinux) {
      // 忽略 meta 按键。因为按下 meta 键会让 flutter 窗口失去焦点。
      if (e.physicalKey == PhysicalKeyboardKey.metaLeft ||
          e.physicalKey == PhysicalKeyboardKey.metaRight) {
        return KeyEventResult.handled;
      }
    }

    if (_relativeMouse.handleKeyEvent(
      e,
      ctrlPressed: ctrl,
      shiftPressed: shift,
      altPressed: alt,
      commandPressed: command,
    )) {
      return KeyEventResult.handled;
    }

    bool iosCapsLock = false;
    if (isIOS && (e is KeyDownEvent || e is KeyRepeatEvent)) {
      iosCapsLock = _getIosCapsFromCharacter(e);
    }

    // 在发送事件前更新缓存的修饰键状态。移动端残留的
    // Shift 抬起检查依赖这个缓存状态。
    if (e is KeyUpEvent) {
      handleKeyUpEventModifiers(e);
    } else if (e is KeyDownEvent) {
      handleKeyDownEventModifiers(e);
    }

    bool isMobileAndMapMode = false;
    if (isMobile) {
      // 如果是从移动端连接到 Android，不要使用 map 模式。Android 目前不支持 map 模式。
      // 因为不支持模拟物理按键事件(uhid)，那需要 root 权限。
      if (peerPlatform != kPeerPlatformAndroid) {
        if (isIOS) {
          isMobileAndMapMode = true;
        } else {
          // 在 Android 上，软键盘的 physicalKey.usbHidUsage 可能不正确。
          // iOS 没有这个问题。
          // 1. 在 Android 上打开软键盘
          // 2. 切换到 zh/ko/ja 之类的输入法
          // 3. 在软键盘或物理键盘上点击 Backspace 和 Enter
          // 4. physicalKey.usbHidUsage 不正确。
          // PhysicalKeyboardKey#8ac83(usbHidUsage: "0x1100000042", debugName: "Key with ID 0x1100000042")
          // LogicalKeyboardKey#2604c(keyId: "0x10000000d", keyLabel: "Enter", debugName: "Enter")
          //
          // 正确的 PhysicalKeyboardKey 应该是
          // PhysicalKeyboardKey#e14a9(usbHidUsage: "0x00070028", debugName: "Enter")
          // https://github.com/flutter/flutter/issues/157771
          // 不能靠 debugName 来判断按键是否正确，因为 release 模式下它为 null。
          // 键盘正常的 `usbHidUsage` 应在 [0x00000010, 0x000c029f] 之间
          // https://github.com/flutter/flutter/blob/c051b69e2a2224300e20d93dbd15f4b91e8844d1/packages/flutter/lib/src/services/keyboard_key.g.dart#L5332 - 5600
          final isNormalHsbHidUsage = (e.physicalKey.usbHidUsage >> 20) == 0;
          isMobileAndMapMode = isNormalHsbHidUsage &&
              // No need to check `!['Backspace', 'Enter'].contains(e.logicalKey.keyLabel)`
              // 但我们仍然加上它以提高可靠性。
              !['Backspace', 'Enter'].contains(e.logicalKey.keyLabel);
        }
      }
    }

    // 在某些移动端软键盘路径上，Flutter 可能会保留缓存的 Shift 状态
    // ，即使当前的按键事件已经不再处于 Shift 状态。
    if (e is KeyDownEvent &&
        shouldReleaseStaleMobileShift(
          isMobile: isMobile,
          cachedShiftPressed: shift,
          actualShiftPressed: HardwareKeyboard.instance.isShiftPressed,
          logicalKey: e.logicalKey,
          hasTrackedShiftKeyDown: toReleaseKeys.lastLShiftKeyEvent != null ||
              toReleaseKeys.lastRShiftKeyEvent != null,
        )) {
      _releaseTrackedShiftKeyEventIfNeeded();
    }

    final isDesktopAndMapMode =
        isDesktop || (isWebDesktop && keyboardMode == kKeyMapMode);
    if (isMobileAndMapMode || isDesktopAndMapMode) {
      _restoreFlutterShortcutModifiers(e);
      // 原生 map 输入在转发之前由 Rust 匹配并跟踪。
      if (isWeb) {
        _trackFlutterShortcutKey(e.physicalKey, e.logicalKey,
            down: e is KeyDownEvent || e is KeyRepeatEvent,
            up: e is KeyUpEvent,
            mapMode: true);
      }
      // FIXME: e.character is wrong for dead keys, eg: ^ in de
      newKeyboardMode(
          e.character ?? '',
          e.physicalKey.usbHidUsage & 0xFFFF,
          // 把显示重复的事件转换成 "release+press" 事件？
          e is KeyDownEvent || e is KeyRepeatEvent,
          iosCapsLock);
    } else {
      if (!isWeb && _tryDispatchFlutterShortcut(e)) return KeyEventResult.handled;
      _restoreFlutterShortcutModifiers(e, mapMode: false);
      _trackFlutterShortcutKey(e.physicalKey, e.logicalKey,
          down: e is KeyDownEvent, up: e is KeyUpEvent);
      legacyKeyboardMode(e);
    }

    return KeyEventResult.handled;
  }

  void _trackFlutterShortcutKey(
      PhysicalKeyboardKey physical, LogicalKeyboardKey logical,
      {required bool down, required bool up, bool mapMode = false}) {
    if ((!down && !up) || (down && !keyboardPerm)) return;
    _localFlutterShortcuts.recordForwardedKey(physical,
        down: down,
        release: () => mapMode
            ? bind.sessionHandleFlutterKeyEvent(
                sessionId: sessionId,
                character: '',
                usbHid: physical.usbHidUsage & 0xFFFF,
                lockModes: _buildLockModes(false),
                downOrUp: false,
              )
            : bind.sessionInputKey(
                sessionId: sessionId,
                name: physicalKeyMap[physical.usbHidUsage] ??
                    logicalKeyMap[logical.keyId] ??
                    logical.keyLabel,
                down: false,
                press: false,
                alt: false,
                ctrl: false,
                shift: false,
                command: false,
              ));
  }

  bool _tryDispatchFlutterShortcut(KeyEvent e, {bool releaseModifiers = true}) {
    final keyboard = HardwareKeyboard.instance;
    return _localFlutterShortcuts.tryDispatch(
      e,
      viewOnly: isViewOnly,
      match: () {
        // AltGraph 在 Windows/Linux 上属于文本输入；Apple Option 仍可绑定。
        if (!(isMacOS || isIOS || isWebOnMacOs) &&
            keyboard.logicalKeysPressed.contains(LogicalKeyboardKey.altGraph)) {
          return null;
        }
        return _matchFlutterShortcut(e.physicalKey,
            ctrlPressed: keyboard.isControlPressed,
            altPressed: keyboard.isAltPressed,
            shiftPressed: keyboard.isShiftPressed,
            commandPressed: keyboard.isMetaPressed);
      },
      releaseModifiers:
          releaseModifiers ? _releaseFlutterShortcutModifiers : null,
    );
  }

  bool _tryDispatchRawFlutterShortcut(RawKeyEvent e,
      {bool releaseModifiers = true}) {
    return _localFlutterShortcuts.tryDispatchRaw(
      e,
      viewOnly: isViewOnly,
      match: () {
        final data = e.data;
        const gtkMod5Mask = 1 << 7;
        // GTK 的 Mod5（AltGr 使用它）不在普通修饰键标志中。
        if (data is RawKeyEventDataLinux &&
            data.keyHelper is GtkKeyHelper &&
            (data.modifiers & gtkMod5Mask) != 0) {
          return null;
        }
        return _matchFlutterShortcut(e.physicalKey,
            ctrlPressed: e.isControlPressed,
            altPressed: e.isAltPressed,
            shiftPressed: e.isShiftPressed,
            commandPressed: e.isMetaPressed);
      },
      releaseModifiers: releaseModifiers
          ? () => _releaseFlutterShortcutModifiers(raw: true)
          : null,
    );
  }

  Future<void> _releaseFlutterShortcutModifiers({bool raw = false}) async {
    final releases = <Future<void>>[];
    // 只在远端释放：本地仍然物理按住了这些修饰键。
    final pressed = raw
        ? RawKeyboard.instance.keysPressed
        : HardwareKeyboard.instance.logicalKeysPressed;
    final swapCtrlCmd = peerPlatform == kPeerPlatformWindows &&
        bind.sessionGetToggleOptionSync(
            sessionId: sessionId, arg: 'allow_swap_key');
    final neutralizeMenu = _needsShortcutMenuGuard(pressed, swapCtrlCmd);
    if (neutralizeMenu)
      await _sendShortcutMenuControl(true, pressed, swapCtrlCmd);
    for (final entry in {
      LogicalKeyboardKey.controlLeft: PhysicalKeyboardKey.controlLeft,
      LogicalKeyboardKey.controlRight: PhysicalKeyboardKey.controlRight,
      LogicalKeyboardKey.altLeft: PhysicalKeyboardKey.altLeft,
      LogicalKeyboardKey.altRight: PhysicalKeyboardKey.altRight,
      LogicalKeyboardKey.shiftLeft: PhysicalKeyboardKey.shiftLeft,
      LogicalKeyboardKey.shiftRight: PhysicalKeyboardKey.shiftRight,
      LogicalKeyboardKey.metaLeft: PhysicalKeyboardKey.metaLeft,
      LogicalKeyboardKey.metaRight: PhysicalKeyboardKey.metaRight,
    }.entries) {
      if (!pressed.contains(entry.key)) continue;
      if (isWebDesktop) {
        // 该动作可以在修饰键保持按住的情况下从 Legacy 切换到 Map。
        _localFlutterShortcuts.recordReleasedModifiers([entry.value]);
      }
      if (isWebDesktop && keyboardMode == kKeyMapMode) {
        releases.add(bind.sessionHandleFlutterKeyEvent(
          sessionId: sessionId,
          character: '',
          usbHid: entry.value.usbHidUsage & 0xFFFF,
          lockModes: _buildLockModes(false),
          downOrUp: false,
        ));
      } else {
        final label = physicalKeyMap[entry.value.usbHidUsage] ?? entry.key.keyLabel;
        releases.add(bind.sessionInputKey(
          sessionId: sessionId,
          name: label,
          down: false,
          press: false,
          alt: false,
          ctrl: false,
          shift: false,
          command: false,
        ));
      }
    }
    await Future.wait(releases).whenComplete(() => neutralizeMenu
        ? _sendShortcutMenuControl(false, pressed, swapCtrlCmd)
        : null);
  }

  bool _needsShortcutMenuGuard(
      Set<LogicalKeyboardKey> pressed, bool swapCtrlCmd) {
    if (peerPlatform != kPeerPlatformWindows) return false;
    // Web 的旧版按键名 API 不会交换按键本身。
    final swapped =
        swapCtrlCmd && (!isWebDesktop || keyboardMode == kKeyMapMode);
    return pressed.any({
          LogicalKeyboardKey.altLeft,
          LogicalKeyboardKey.altRight,
          swapped
              ? LogicalKeyboardKey.controlLeft
              : LogicalKeyboardKey.metaLeft,
          swapped
              ? LogicalKeyboardKey.controlRight
              : LogicalKeyboardKey.metaRight,
        }.contains) &&
        !pressed.any({
          swapped
              ? LogicalKeyboardKey.metaLeft
              : LogicalKeyboardKey.controlLeft,
          swapped
              ? LogicalKeyboardKey.metaRight
              : LogicalKeyboardKey.controlRight,
          LogicalKeyboardKey.shiftLeft,
          LogicalKeyboardKey.shiftRight,
        }.contains);
  }

  Future<void> _sendShortcutMenuControl(
      bool down, Set<LogicalKeyboardKey> pressed, bool swapCtrlCmd) {
    // 选择交换前那个会作为 Ctrl 到达 Windows 的按键。
    if (isWebDesktop) {
      return bind.sessionHandleFlutterKeyEvent(
        sessionId: sessionId,
        character: '',
        usbHid: (swapCtrlCmd
                    ? PhysicalKeyboardKey.metaLeft
                    : PhysicalKeyboardKey.controlLeft)
                .usbHidUsage &
            0xFFFF,
        lockModes: _buildLockModes(false),
        downOrUp: down,
      );
    }
    return bind.sessionInputKey(
      sessionId: sessionId,
      name: swapCtrlCmd ? 'Meta' : 'VK_CONTROL',
      down: down,
      press: false,
      // Legacy 会在按键按下前协调修饰键。保持 Alt/Meta 按住，直到
      // Ctrl 被按下，否则协调过程可能激活菜单或开始菜单。
      alt: down &&
          pressed.any({LogicalKeyboardKey.altLeft, LogicalKeyboardKey.altRight}
              .contains),
      ctrl: down &&
          pressed.any({
            LogicalKeyboardKey.controlLeft,
            LogicalKeyboardKey.controlRight
          }.contains),
      shift: false,
      command: down &&
          pressed.any({
            LogicalKeyboardKey.metaLeft,
            LogicalKeyboardKey.metaRight
          }.contains),
    );
  }

  void _syncFlutterShortcutModifiersAfterViewOnly(PhysicalKeyboardKey incoming,
      {bool raw = false}) {
    if (!isInputSourceFlutter) return;
    final pressed = raw
        ? RawKeyboard.instance.physicalKeysPressed
        : HardwareKeyboard.instance.physicalKeysPressed;
    if (!_localFlutterShortcuts.resumeModifiersAfterViewOnly(incoming, pressed)) {
      return;
    }
    ctrl = pressed.contains(PhysicalKeyboardKey.controlLeft) ||
        pressed.contains(PhysicalKeyboardKey.controlRight);
    alt = pressed.contains(PhysicalKeyboardKey.altLeft) ||
        pressed.contains(PhysicalKeyboardKey.altRight);
    shift = pressed.contains(PhysicalKeyboardKey.shiftLeft) ||
        pressed.contains(PhysicalKeyboardKey.shiftRight);
    command = pressed.contains(PhysicalKeyboardKey.metaLeft) ||
        pressed.contains(PhysicalKeyboardKey.metaRight);
  }

  void _restoreFlutterShortcutModifiers(KeyEvent e, {bool mapMode = true}) {
    final keys = _localFlutterShortcuts.takeModifiersToRestore(
        e, HardwareKeyboard.instance.physicalKeysPressed);
    if (!mapMode) return;
    for (final key in keys) {
      newKeyboardMode('', key.usbHidUsage & 0xFFFF, true, false);
    }
  }

  void _restoreRawFlutterShortcutModifiers(RawKeyEvent e, {bool mapMode = true}) {
    final keys = _localFlutterShortcuts.takeRawModifiersToRestore(
        e, RawKeyboard.instance.physicalKeysPressed);
    if (!mapMode) return;
    for (final key in keys) {
      newKeyboardMode('', key.usbHidUsage & 0xFFFF, true, false);
    }
  }

  String? _matchFlutterShortcut(PhysicalKeyboardKey key,
      {required bool ctrlPressed,
      required bool altPressed,
      required bool shiftPressed,
      required bool commandPressed}) {
    if (isViewCamera) return null;
    final keyName = physicalKeyName(key);
    if (keyName == null) return null;
    final config = ShortcutModel.config(active: true);
    if (!config.enabled || config.passThrough) return null;
    final mods = <String>[];
    if (isMacOS || isIOS || isWebOnMacOs) {
      if (commandPressed) mods.add('primary');
      if (ctrlPressed) mods.add('ctrl');
    } else if (ctrlPressed) {
      mods.add('primary');
    }
    if (altPressed) mods.add('alt');
    if (shiftPressed) mods.add('shift');
    for (final binding in config.bindings) {
      final action = binding['action'];
      final key = binding['key'];
      final bindingMods =
          canonicalShortcutModsForSave(shortcutModSetFrom(binding['mods']));
      if (action is String &&
          key == keyName &&
          bindingMods.isNotEmpty &&
          listEquals(bindingMods, mods)) {
        return action;
      }
    }
    return null;
  }

  /// 发送按键事件
  void newKeyboardMode(
      String character, int usbHid, bool down, bool iosCapsLock) {
    final lockModes = _buildLockModes(iosCapsLock);
    bind.sessionHandleFlutterKeyEvent(
        sessionId: sessionId,
        character: character,
        usbHid: usbHid,
        lockModes: lockModes,
        downOrUp: down);
  }

  void mapKeyboardModeRaw(RawKeyEvent e, bool iosCapsLock) {
    int positionCode = -1;
    int platformCode = -1;
    bool down;

    if (e.data is RawKeyEventDataMacOs) {
      RawKeyEventDataMacOs newData = e.data as RawKeyEventDataMacOs;
      positionCode = newData.keyCode;
      platformCode = newData.keyCode;
    } else if (e.data is RawKeyEventDataWindows) {
      RawKeyEventDataWindows newData = e.data as RawKeyEventDataWindows;
      positionCode = newData.scanCode;
      platformCode = newData.keyCode;
    } else if (e.data is RawKeyEventDataLinux) {
      RawKeyEventDataLinux newData = e.data as RawKeyEventDataLinux;
      // RawKeyEventDataLinux 的 scanCode 和 keyCode 不正确。
      // 1. scanCode 表示 keycode
      // 2. keyCode 表示 keysym
      positionCode = newData.scanCode;
      platformCode = newData.keyCode;
    } else if (e.data is RawKeyEventDataAndroid) {
      RawKeyEventDataAndroid newData = e.data as RawKeyEventDataAndroid;
      positionCode = newData.scanCode + 8;
      platformCode = newData.keyCode;
    } else {}

    if (e is RawKeyDownEvent) {
      down = true;
    } else {
      down = false;
    }
    inputRawKey(
        e.character ?? '', platformCode, positionCode, down, iosCapsLock);
  }

  /// 发送原始按键事件
  void inputRawKey(String name, int platformCode, int positionCode, bool down,
      bool iosCapsLock) {
    final lockModes = _buildLockModes(iosCapsLock);
    bind.sessionHandleFlutterRawKeyEvent(
        sessionId: sessionId,
        name: name,
        platformCode: platformCode,
        positionCode: positionCode,
        lockModes: lockModes,
        downOrUp: down);
  }

  void legacyKeyboardModeRaw(RawKeyEvent e) {
    if (e is RawKeyDownEvent) {
      if (e.repeat) {
        sendRawKey(e, press: true);
      } else {
        sendRawKey(e, down: true);
      }
    }
    if (e is RawKeyUpEvent) {
      sendRawKey(e);
    }
  }

  void sendRawKey(RawKeyEvent e, {bool? down, bool? press}) {
    // 为了最大限度的兼容性
    final label = physicalKeyMap[e.physicalKey.usbHidUsage] ??
        logicalKeyMap[e.logicalKey.keyId] ??
        e.logicalKey.keyLabel;
    inputKey(label, down: down, press: press ?? false);
  }

  void legacyKeyboardMode(KeyEvent e) {
    if (e is KeyDownEvent) {
      sendKey(e, down: true);
    } else if (e is KeyRepeatEvent) {
      sendKey(e, press: true);
    } else if (e is KeyUpEvent) {
      sendKey(e);
    }
  }

  void sendKey(KeyEvent e, {bool? down, bool? press}) {
    // 为了最大限度的兼容性
    final label = physicalKeyMap[e.physicalKey.usbHidUsage] ??
        logicalKeyMap[e.logicalKey.keyId] ??
        e.logicalKey.keyLabel;
    inputKey(label, down: down, press: press ?? false);
  }

  /// 发送按键笔画事件。
  /// [down] 表示按键的状态（按下或抬起）。
  /// [press] 表示一次点击事件（按下并抬起）。
  void inputKey(String name, {bool? down, bool? press}) {
    if (!keyboardPerm) return;
    if (isViewCamera) return;
    bind.sessionInputKey(
        sessionId: sessionId,
        name: name,
        down: down ?? false,
        press: press ?? true,
        alt: alt,
        ctrl: ctrl,
        shift: shift,
        command: command);
  }

  static Map<String, dynamic> getMouseEventMove() => {
        'type': _kMouseEventMove,
        'buttons': 0,
      };

  Map<String, dynamic> _getMouseEvent(PointerEvent evt, String type) {
    final Map<String, dynamic> out = {};

    bool hasStaleButtonsOnMouseUp =
        type == _kMouseEventUp && evt.buttons == _lastButtons;

    // 检查更新事件类型并设置要发送的按键。
    int buttons = _lastButtons;
    if (type == _kMouseEventMove) {
      // 如果一个按键处于按下状态而另一个按键
      // 正在按下或抬起，flutter 可能会发出 move 事件。
      if (evt.buttons != _lastButtons) {
        // 为简单起见
        // 只考虑 3 - 1（(左键 + 右键) - 左键）
        // 不考虑 2 - 1（右键 - 左键）
        // 或 6 - 5（(右键 + 中键) - (左键 + 中键)）
        // 等等
        buttons = evt.buttons - _lastButtons;
        if (buttons > 0) {
          type = _kMouseEventDown;
        } else {
          type = _kMouseEventUp;
          buttons = -buttons;
        }
      }
    } else {
      if (evt.buttons != 0) {
        buttons = evt.buttons;
      }
    }
    _lastButtons = hasStaleButtonsOnMouseUp ? 0 : evt.buttons;
    if (isDesktop && _relativeMouse.enabled.value && type == _kMouseEventUp) {
      // 相对的 up 可以结束在模式切换之前发出的 press。
      _pressedMouseButtons &= ~buttons;
    }

    out['buttons'] = buttons;
    out['type'] = type;
    return out;
  }

  /// 发送鼠标点击事件（按下并抬起）。
  Future<void> tap(MouseButtons button) async {
    await sendMouse('down', button);
    await sendMouse('up', button);
  }

  Future<void> tapDown(MouseButtons button) async {
    await sendMouse('down', button);
  }

  Future<void> tapUp(MouseButtons button) async {
    await sendMouse('up', button);
  }

  /// 发送滚动事件，滚动距离为 [y]。
  Future<void> scroll(int y) async {
    if (isViewCamera) return;
    await bind.sessionSendMouse(
        sessionId: sessionId,
        msg: json
            .encode(modify({'id': id, 'type': 'wheel', 'y': y.toString()})));
  }

  /// 把按键修饰状态重置为 false，包括 [shift]、[ctrl]、[alt] 和 [command]。
  void resetModifiers() {
    shift = ctrl = alt = command = false;
    if (parent.target?.closed == true) _localFlutterShortcuts.clear();
  }

  /// 根据当前的修饰键状态修改给定的修饰键映射 [evt]。
  Map<String, dynamic> modify(Map<String, dynamic> evt) {
    if (ctrl) evt['ctrl'] = 'true';
    if (shift) evt['shift'] = 'true';
    if (alt) evt['alt'] = 'true';
    if (command) evt['command'] = 'true';
    return evt;
  }

  /// 无条件发送鼠标事件（不做权限检查）。
  /// 用于侧键的抬起事件，即使权限
  /// 在对应的按下事件发出后发生了变化，也必须发送出去。
  Future<void> _sendMouseUnchecked(String type, MouseButtons button) async {
    await bind.sessionSendMouse(
        sessionId: sessionId,
        msg: json.encode(modify({'type': type, 'buttons': button.value})));
  }

  /// 发送鼠标按下事件。
  Future<void> sendMouse(String type, MouseButtons button) async {
    if (!keyboardPerm) return;
    if (isViewCamera) return;
    await _sendMouseUnchecked(type, button);
  }

  void enterOrLeave(bool enter) {
    toReleaseKeys.release(handleKeyEvent);
    toReleaseRawKeys.release(handleRawKeyEvent);
    _pointerMovedAfterEnter = false;
    _pointerInsideImage = enter;
    _lastWheelTsUs = 0;

    // 为侧键事件跟踪活动模型（Linux）。
    if (enter) {
      _activeSideButtonModel = this;
    } else if (_activeSideButtonModel == this) {
      _activeSideButtonModel = null;
    }

    // 修复状态
    if (!enter) {
      resetModifiers();
    }
    _relativeMouse.onEnterOrLeaveImage(enter);
    _flingTimer?.cancel();
    if (!isInputSourceFlutter) {
      bind.sessionEnterOrLeave(sessionId: sessionId, enter: enter);
    }
    if (!isWeb && enter) {
      bind.setCurSessionId(sessionId: sessionId);
    }
  }

  /// 发送鼠标移动事件，距离为 [x] 和 [y]。
  Future<void> moveMouse(double x, double y) async {
    if (!keyboardPerm) return;
    if (isViewCamera) return;
    var x2 = x.toInt();
    var y2 = y.toInt();
    await bind.sessionSendMouse(
        sessionId: sessionId,
        msg: json.encode(modify({'x': '$x2', 'y': '$y2'})));
  }

  /// 为移动端客户端发送相对鼠标移动（虚拟摇杆）。
  /// 该方法供需要发送增量值的触摸控制使用。
  /// 使用 'move_relative' 类型，它会绕过绝对位置跟踪。
  ///
  /// 累加小数增量，以免丢失缓慢/精细的移动。
  /// 仅在相对鼠标模式已启用且受支持时才发送事件。
  Future<void> sendMobileRelativeMouseMove(double dx, double dy) async {
    if (!keyboardPerm) return;
    if (isViewCamera) return;
    // 仅在相对模式已启用且受支持时才发送相对鼠标事件。
    if (!isRelativeMouseModeSupported || !relativeMouseMode.value) return;
    _mobileDeltaRemainderX += dx;
    _mobileDeltaRemainderY += dy;
    final x = _mobileDeltaRemainderX.truncate();
    final y = _mobileDeltaRemainderY.truncate();
    _mobileDeltaRemainderX -= x;
    _mobileDeltaRemainderY -= y;
    if (x == 0 && y == 0) return;
    await bind.sessionSendMouse(
        sessionId: sessionId,
        msg: json.encode(modify({
          'type': 'move_relative',
          'x': '$x',
          'y': '$y',
        })));
  }

  /// 根据当前窗口边框更新指针锁定中心位置。
  Future<void> updatePointerLockCenter({Offset? localCenter}) {
    return _relativeMouse.updatePointerLockCenter(localCenter: localCenter);
  }

  /// 获取当前图像组件的尺寸（用于比较，避免不必要的更新）。
  Size? get imageWidgetSize => _relativeMouse.imageWidgetSize;

  /// 更新图像组件尺寸以计算中心。
  void updateImageWidgetSize(Size size) {
    _relativeMouse.updateImageWidgetSize(size);
  }

  void toggleRelativeMouseMode() {
    _relativeMouse.toggleRelativeMouseMode();
  }

  bool setRelativeMouseMode(bool enabled) {
    return _relativeMouse.setRelativeMouseMode(enabled);
  }

  /// 退出相对鼠标模式，并向远端释放所有修饰键。
  /// 当用户按下退出快捷键时调用（Win/Linux 上是 Ctrl+Alt，macOS 上是 Cmd+G）。
  /// 需要为所有修饰键发送抬起事件，因为该快捷键本身可能已经
  /// 屏蔽了某些按键事件，使远端处于修饰键卡住的状态。
  void exitRelativeMouseModeWithKeyRelease() {
    if (!_relativeMouse.enabled.value) return;

    // 首先，向远端发送所有修饰键的抬起事件。
    // 这可以确保退出后远端不会出现卡住的修饰键。
    // 使用 press: false, down: false 发送不带修饰键的抬起事件。
    final modifiersToRelease = [
      'Control_L',
      'Control_R',
      'Alt_L',
      'Alt_R',
      'Shift_L',
      'Shift_R',
      'Meta_L', // Command/Super left
      'Meta_R', // Command/Super right
    ];

    for (final key in modifiersToRelease) {
      bind.sessionInputKey(
        sessionId: sessionId,
        name: key,
        down: false,
        press: false,
        alt: false,
        ctrl: false,
        shift: false,
        command: false,
      );
    }

    // 重置本地修饰键状态
    resetModifiers();

    // 现在退出相对鼠标模式
    _relativeMouse.setRelativeMouseMode(false);
  }

  void disposeRelativeMouseMode() {
    _relativeMouse.dispose();
    onRelativeMouseModeDisabled = null;
    // 取消相对鼠标模式的观察者并清理全局状态。
    _relativeMouseModeDisposer?.dispose();
    _relativeMouseModeDisposer = null;
    final peerId = id;
    if (peerId.isNotEmpty) {
      stateGlobal.relativeMouseModeState.remove(peerId);
    }
  }

  void onWindowBlur() {
    _relativeMouse.onWindowBlur();
  }

  void onWindowFocus() {
    _relativeMouse.onWindowFocus();
  }

  void onPointHoverImage(PointerHoverEvent e) {
    _stopFling = true;
    if (isViewOnly && !showMyCursor) return;
    if (e.kind != ui.PointerDeviceKind.mouse) return;

    // 可能修复 https://github.com/rustdesk/rustdesk/issues/13009
    if (isIOS && e.synthesized && e.position == Offset.zero && e.buttons == 0) {
      // 鼠标断开时，iOS 可能在 (0,0) 发出一个合成的悬停事件。
      // 忽略该事件以防光标跳动。
      debugPrint('Ignored synthesized hover at (0,0) on iOS');
      return;
    }

    // 仅在启用相对鼠标模式时更新指针区域。
    // 这样在非相对模式下就无需不必要的跟踪。
    if (_relativeMouse.enabled.value) {
      _relativeMouse.updatePointerRegionTopLeftGlobal(e);
    }

    if (!isPhysicalMouse.value) {
      isPhysicalMouse.value = true;
    }
    if (isPhysicalMouse.value) {
      if (!_relativeMouse.handleRelativeMouseMove(e.localPosition)) {
        final canvasPosition = _pointerPositionForRemoteCanvas(e);
        handleMouse(_getMouseEvent(e, _kMouseEventMove), canvasPosition,
            edgeScroll: useEdgeScroll);
      }
    }
  }

  void onPointerPanZoomStart(PointerPanZoomStartEvent e) {
    _lastScale = 1.0;
    _stopFling = true;
    if (isViewOnly) return;
    if (isViewCamera) return;
    if (peerPlatform == kPeerPlatformAndroid) {
      handlePointerEvent('touch', kMouseEventTypePanStart, e.position);
    }
  }

  // https://docs.flutter.dev/release/breaking-changes/trackpad-gestures
  void onPointerPanZoomUpdate(PointerPanZoomUpdateEvent e) {
    if (isViewOnly) return;
    if (isViewCamera) return;
    if (peerPlatform != kPeerPlatformAndroid) {
      final scale = ((e.scale - _lastScale) * 1000).toInt();
      _lastScale = e.scale;

      if (scale != 0) {
        bind.sessionSendPointer(
            sessionId: sessionId,
            msg: json.encode(
                PointerEventToRust(kPointerEventKindTouch, 'scale', scale)
                    .toJson()));
        return;
      }
    }

    var delta = e.panDelta * _trackpadSpeedInner;
    if (isMacOS && peerPlatform == kPeerPlatformWindows) {
      delta *= _trackpadAdjustMacToWin;
    }
    delta = _filterTrackpadDeltaAxis(delta);
    _trackpadLastDelta = delta;

    var x = delta.dx.toInt();
    var y = delta.dy.toInt();
    if (peerPlatform == kPeerPlatformLinux) {
      _trackpadScrollUnsent += (delta * _trackpadAdjustPeerLinux);
      x = _trackpadScrollUnsent.dx.truncate();
      y = _trackpadScrollUnsent.dy.truncate();
      _trackpadScrollUnsent -= Offset(x.toDouble(), y.toDouble());
    } else {
      if (x == 0 && y == 0) {
        final thr = 0.1;
        if (delta.dx.abs() > delta.dy.abs()) {
          x = delta.dx > thr ? 1 : (delta.dx < -thr ? -1 : 0);
        } else {
          y = delta.dy > thr ? 1 : (delta.dy < -thr ? -1 : 0);
        }
      }
    }
    if (x != 0 || y != 0) {
      if (peerPlatform == kPeerPlatformAndroid) {
        handlePointerEvent('touch', kMouseEventTypePanUpdate,
            Offset(x.toDouble(), y.toDouble()));
      } else {
        if (isViewCamera) return;
        bind.sessionSendMouse(
            sessionId: sessionId,
            msg: '{"type": "trackpad", "x": "$x", "y": "$y"}');
      }
    }
  }

  Offset _filterTrackpadDeltaAxis(Offset delta) {
    final absDx = delta.dx.abs();
    final absDy = delta.dy.abs();
    // 当两个轴上的移动都很小时，保留对角方向的意图。
    if (absDx < _trackpadAxisNoiseThreshold &&
        absDy < _trackpadAxisNoiseThreshold) {
      return delta;
    }
    // 主轴锁定，以减少意外的跨轴滚动噪声。
    if (absDy >= absDx * _trackpadAxisLockRatio) {
      return Offset(0, delta.dy);
    }
    if (absDx >= absDy * _trackpadAxisLockRatio) {
      return Offset(delta.dx, 0);
    }
    return delta;
  }

  void _scheduleFling(double x, double y, int delay) {
    if (isViewCamera) return;
    if ((x == 0 && y == 0) || _stopFling) {
      _fling = false;
      return;
    }

    _flingTimer = Timer(Duration(milliseconds: delay), () {
      if (_stopFling) {
        _fling = false;
        return;
      }

      final d = 0.97;
      x *= d;
      y *= d;

      // 尝试设置增量 (x,y) 和延迟。
      var dx = x.toInt();
      var dy = y.toInt();
      if (parent.target?.ffiModel.pi.platform == kPeerPlatformLinux) {
        dx = (x * _trackpadAdjustPeerLinux).toInt();
        dy = (y * _trackpadAdjustPeerLinux).toInt();
      }

      var delay = _flingBaseDelay;

      if (dx == 0 && dy == 0) {
        _fling = false;
        return;
      }

      bind.sessionSendMouse(
          sessionId: sessionId,
          msg: '{"type": "trackpad", "x": "$dx", "y": "$dy"}');
      _scheduleFling(x, y, delay);
    });
  }

  void waitLastFlingDone() {
    if (_fling) {
      _stopFling = true;
    }
    for (var i = 0; i < 5; i++) {
      if (!_fling) {
        break;
      }
      sleep(Duration(milliseconds: 10));
    }
    _flingTimer?.cancel();
  }

  void onPointerPanZoomEnd(PointerPanZoomEndEvent e) {
    if (isViewCamera) return;
    if (peerPlatform == kPeerPlatformAndroid) {
      handlePointerEvent('touch', kMouseEventTypePanEnd, e.position);
      return;
    }

    bind.sessionSendPointer(
        sessionId: sessionId,
        msg: json.encode(
            PointerEventToRust(kPointerEventKindTouch, 'scale', 0).toJson()));

    waitLastFlingDone();
    _stopFling = false;

    // 2.0 是一个经验值
    double minFlingValue = 2.0 * _trackpadSpeedInner;
    if (isMacOS && peerPlatform == kPeerPlatformWindows) {
      minFlingValue *= _trackpadAdjustMacToWin;
    }
    if (_trackpadLastDelta.dx.abs() > minFlingValue ||
        _trackpadLastDelta.dy.abs() > minFlingValue) {
      _fling = true;
      _scheduleFling(
          _trackpadLastDelta.dx, _trackpadLastDelta.dy, _flingBaseDelay);
    }
    _trackpadLastDelta = Offset.zero;
  }

  // iOS Magic Mouse 重复事件检测。
  // 在 iPad 上使用 Magic Mouse 时，iOS 可能对同一次点击同时发出鼠标和触摸事件
  // （在某些区域，比如左上角）。
  int _lastMouseDownTimeMs = 0;
  ui.Offset _lastMouseDownPos = ui.Offset.zero;

  /// 检查某个触摸点击事件是否应被忽略，因为它与最近一次鼠标事件
  /// 重复（iOS Magic Mouse 问题）。
  bool shouldIgnoreTouchTap(ui.Offset pos) {
    if (!isIOS) return false;
    final nowMs = DateTime.now().millisecondsSinceEpoch;
    final dt = nowMs - _lastMouseDownTimeMs;
    final distance = (_lastMouseDownPos - pos).distance;
    // 如果触摸点击与上一次鼠标按下相距不到 2000ms 且 80px，
    // 它很可能是同一次 Magic Mouse 点击产生的重复事件。
    if (dt >= 0 && dt < 2000 && distance < 80.0) {
      debugPrint("shouldIgnoreTouchTap: IGNORED (dt=$dt, dist=$distance)");
      return true;
    }
    return false;
  }

  /// iOS 可能在真实的鼠标点击之后发出合成的触摸事件。
  /// 该辅助方法会忽略在鼠标按下后很快到达的触摸按下事件，
  /// 即使位置相距很远（比如靠近顶部边缘）也一样。
  bool _shouldIgnoreTouchAfterMouse(int nowMs) {
    if (!isIOS) return false;
    const int kTouchAfterMouseWindowMs = 700;
    final dt = nowMs - _lastMouseDownTimeMs;
    return dt >= 0 && dt < kTouchAfterMouseWindowMs;
  }

  void onPointDownImage(PointerDownEvent e) {
    debugPrint("onPointDownImage ${e.kind}");
    _stopFling = true;
    if (isDesktop) _queryOtherWindowCoords = true;
    _remoteWindowCoords = [];
    _windowRect = null;
    if (isViewOnly && !showMyCursor) return;
    if (isViewCamera) return;

    // 跟踪鼠标按下事件，用于在 iOS 上检测重复。
    final nowMs = DateTime.now().millisecondsSinceEpoch;
    if (e.kind == ui.PointerDeviceKind.mouse) {
      if (!isPhysicalMouse.value) {
        isPhysicalMouse.value = true;
      }
      _lastMouseDownTimeMs = nowMs;
      _lastMouseDownPos = e.position;
    }

    if (_relativeMouse.enabled.value) {
      _relativeMouse.updatePointerRegionTopLeftGlobal(e);
    }

    if (e.kind != ui.PointerDeviceKind.mouse) {
      // 忽略最近一次鼠标点击之后出现的重复触摸事件（iOS Magic Mouse 问题）。
      if (isPhysicalMouse.value && _shouldIgnoreTouchAfterMouse(nowMs)) {
        return;
      }
      if (isPhysicalMouse.value) {
        isPhysicalMouse.value = false;
      }
    }
    if (isPhysicalMouse.value) {
      // 在相对鼠标模式下，发送不带位置的按键事件。
      // 与上面的判断保持一致，使用 _relativeMouse.enabled.value。
      if (_relativeMouse.enabled.value) {
        _relativeMouse
            .sendRelativeMouseButton(_getMouseEvent(e, _kMouseEventDown));
      } else {
        final canvasPosition = _pointerPositionForRemoteCanvas(e);
        handleMouse(_getMouseEvent(e, _kMouseEventDown), canvasPosition);
      }
    }
  }

  void onPointUpImage(PointerUpEvent e) {
    if (isDesktop) _queryOtherWindowCoords = false;
    if (isViewOnly && !showMyCursor) return;
    if (isViewCamera) return;

    if (_relativeMouse.enabled.value) {
      _relativeMouse.updatePointerRegionTopLeftGlobal(e);
    }

    if (e.kind != ui.PointerDeviceKind.mouse) return;
    if (isPhysicalMouse.value) {
      // 在相对鼠标模式下，发送不带位置的按键事件。
      // 与上面的判断保持一致，使用 _relativeMouse.enabled.value。
      if (_relativeMouse.enabled.value) {
        _relativeMouse
            .sendRelativeMouseButton(_getMouseEvent(e, _kMouseEventUp));
      } else {
        final canvasPosition = _pointerPositionForRemoteCanvas(e);
        handleMouse(_getMouseEvent(e, _kMouseEventUp), canvasPosition);
      }
    }
  }

  void onPointMoveImage(PointerMoveEvent e) {
    if (isViewOnly && !showMyCursor) return;
    if (isViewCamera) return;
    if (e.kind != ui.PointerDeviceKind.mouse) return;

    if (_relativeMouse.enabled.value) {
      _relativeMouse.updatePointerRegionTopLeftGlobal(e);
    }

    if (_queryOtherWindowCoords) {
      Future.delayed(Duration.zero, () async {
        _windowRect = await fillRemoteCoordsAndGetCurFrame(_remoteWindowCoords);
      });
      _queryOtherWindowCoords = false;
    }
    if (isPhysicalMouse.value) {
      if (!_relativeMouse.handleRelativeMouseMove(e.localPosition)) {
        final canvasPosition = _pointerPositionForRemoteCanvas(e);
        handleMouse(_getMouseEvent(e, _kMouseEventMove), canvasPosition,
            edgeScroll: useEdgeScroll);
      }
    }
  }

  /// 把指针坐标转换为可见的远端画布空间。
  ///
  /// 在移动端，远端页面主体被包裹在 `SafeArea` 中，但指针
  /// 事件监听器位于该子树之外。因此，
  /// `event.localPosition` 仍然包含顶部/左侧的安全区内边距。
  ///
  /// 当键盘可见的路径显示 `KeyHelpTools` 时，远端画布
  /// 还会被 `CanvasModel.getAdjustY()` 向下偏移。下游的鼠标
  /// 映射逻辑期望的是相对于可见画布区域的坐标，所以
  /// 我们在传入鼠标映射之前，减去移动端的安全区内边距以及当前的画布
  /// 偏移量。
  ///
  /// 桌面端和 web 桌面端继续直接使用全局位置
  /// 因为它们的指针映射是基于窗口的。
  Offset _pointerPositionForRemoteCanvas(PointerEvent event) {
    if (isDesktop || isWebDesktop) {
      return event.position;
    }
    final mediaData = MediaQueryData.fromView(
        WidgetsBinding.instance.platformDispatcher.views.first);
    final adjustY = parent.target?.canvasModel.getAdjustY() ?? 0.0;
    return Offset(
      event.localPosition.dx - mediaData.padding.left,
      event.localPosition.dy - mediaData.padding.top - adjustY,
    );
  }

  static Future<Rect?> fillRemoteCoordsAndGetCurFrame(
      List<RemoteWindowCoords> remoteWindowCoords) async {
    final coords =
        await rustDeskWinManager.getOtherRemoteWindowCoordsFromMain();
    final wc = WindowController.fromWindowId(kWindowId!);
    try {
      final frame = await wc.getFrame();
      for (final c in coords) {
        c.relativeOffset = Offset(
            c.windowRect.left - frame.left, c.windowRect.top - frame.top);
        remoteWindowCoords.add(c);
      }
      return frame;
    } catch (e) {
      // 不可达代码
      debugPrint("Failed to get frame of window $kWindowId, it may be hidden");
    }
    return null;
  }

  /// 处理滚动/滚轮事件。
  /// 注意：即使在相对鼠标模式下，滚动事件也刻意使用绝对定位。
  /// 这是因为滚动事件不需要相对定位——它们表示的是
  /// 与光标位置无关的滚动增量。游戏和 3D 应用
  /// 无论处于哪种鼠标模式，处理滚动事件的方式都相同。
  void onPointerSignalImage(PointerSignalEvent e) {
    if (isViewOnly) return;
    if (isViewCamera) return;
    if (e is PointerScrollEvent) {
      final rawDx = e.scrollDelta.dx;
      final rawDy = e.scrollDelta.dy;
      final dominantDelta = rawDx.abs() > rawDy.abs() ? rawDx.abs() : rawDy.abs();
      final isSmooth = dominantDelta < 1;
      final nowUs = DateTime.now().microsecondsSinceEpoch;
      final dtUs = _lastWheelTsUs == 0 ? 0 : nowUs - _lastWheelTsUs;
      _lastWheelTsUs = nowUs;
      int accel = 1;
      if (!isSmooth &&
          dtUs > 0 &&
          dtUs <= _wheelAccelMediumThresholdUs &&
          (isWindows || isLinux) &&
          peerPlatform == kPeerPlatformMacOS) {
        final velocity = dominantDelta / dtUs;
        if (velocity >= _wheelBurstVelocityThreshold) {
          if (dtUs < _wheelAccelFastThresholdUs) {
            accel = 3;
          } else {
            accel = 2;
          }
        }
      }
      var dx = rawDx.toInt();
      var dy = rawDy.toInt();
      if (rawDx.abs() > rawDy.abs()) {
        dy = 0;
      } else {
        dx = 0;
      }
      if (dx > 0) {
        dx = -accel;
      } else if (dx < 0) {
        dx = accel;
      }
      if (dy > 0) {
        dy = -accel;
      } else if (dy < 0) {
        dy = accel;
      }
      bind.sessionSendMouse(
          sessionId: sessionId,
          msg: '{"type": "wheel", "x": "$dx", "y": "$dy"}');
    }
  }

  void refreshMousePos() => handleMouse({
        'buttons': 0,
        'type': _kMouseEventMove,
      }, lastMousePos, edgeScroll: useEdgeScroll);

  void tryMoveEdgeOnExit(Offset pos) => handleMouse(
        {
          'buttons': 0,
          'type': _kMouseEventMove,
        },
        pos,
        onExit: true,
      );

  static double tryGetNearestRange(double v, double min, double max, double n) {
    if (v < min && v >= min - n) {
      v = min;
    }
    if (v > max && v <= max + n) {
      v = max;
    }
    return v;
  }

  Offset setNearestEdge(double x, double y, Rect rect) {
    double left = x - rect.left;
    double right = rect.right - 1 - x;
    double top = y - rect.top;
    double bottom = rect.bottom - 1 - y;
    if (left < right && left < top && left < bottom) {
      x = rect.left;
    }
    if (right < left && right < top && right < bottom) {
      x = rect.right - 1;
    }
    if (top < left && top < right && top < bottom) {
      y = rect.top;
    }
    if (bottom < left && bottom < right && bottom < top) {
      y = rect.bottom - 1;
    }
    return Offset(x, y);
  }

  void handlePointerEvent(String kind, String type, Offset offset) {
    double x = offset.dx;
    double y = offset.dy;
    if (_checkPeerControlProtected(x, y)) {
      return;
    }
    // 目前只处理触摸事件，因此可以直接忽略按键。
    // 待办：处理鼠标事件

    late final dynamic evtValue;
    if (type == kMouseEventTypePanUpdate) {
      evtValue = {
        'x': x.toInt(),
        'y': y.toInt(),
      };
    } else {
      final isMoveTypes = [kMouseEventTypePanStart, kMouseEventTypePanEnd];
      final pos = handlePointerDevicePos(
        kPointerEventKindTouch,
        x,
        y,
        isMoveTypes.contains(type),
        type,
      );
      if (pos == null) {
        return;
      }
      evtValue = {
        'x': pos.x.toInt(),
        'y': pos.y.toInt(),
      };
    }

    final evt = PointerEventToRust(kind, type, evtValue).toJson();
    if (isViewCamera) return;
    bind.sessionSendPointer(
        sessionId: sessionId, msg: json.encode(modify(evt)));
  }

  bool _checkPeerControlProtected(double x, double y) {
    if (isViewOnly && showMyCursor) {
      lastMousePos = ui.Offset(x, y);
      return false;
    }

    final cursorModel = parent.target!.cursorModel;
    if (cursorModel.isPeerControlProtected) {
      lastMousePos = ui.Offset(x, y);
      return true;
    }

    if (!cursorModel.gotMouseControl) {
      bool selfGetControl =
          (x - lastMousePos.dx).abs() > kMouseControlDistance ||
              (y - lastMousePos.dy).abs() > kMouseControlDistance;
      if (selfGetControl) {
        cursorModel.gotMouseControl = true;
      } else {
        lastMousePos = ui.Offset(x, y);
        return true;
      }
    }
    lastMousePos = ui.Offset(x, y);
    return false;
  }

  Map<String, dynamic>? processEventToPeer(
    Map<String, dynamic> evt,
    Offset offset, {
    bool onExit = false,
    bool moveCanvas = true,
    bool edgeScroll = false,
  }) {
    if (isViewCamera) return null;
    final buttons = evt['buttons'];
    final isMatchingMouseUp = (isDesktop || isWebDesktop) &&
        evt['type'] == _kMouseEventUp &&
        buttons is int &&
        mouseButtonsToPeer(buttons).isNotEmpty &&
        _pressedMouseButtons & buttons != 0;
    double x = offset.dx;
    double y = max(0.0, offset.dy);
    // 在转发的按下事件与其抬起事件之间，光标归属可能发生变化。
    if (_checkPeerControlProtected(x, y) && !isMatchingMouseUp) {
      return null;
    }

    var type = kMouseEventTypeDefault;
    var isMove = false;
    switch (evt['type']) {
      case _kMouseEventDown:
        type = kMouseEventTypeDown;
        break;
      case _kMouseEventUp:
        type = kMouseEventTypeUp;
        break;
      case _kMouseEventMove:
        _pointerMovedAfterEnter = true;
        isMove = true;
        break;
      default:
        return null;
    }
    evt['type'] = type;

    if (type == kMouseEventTypeDown && !_pointerMovedAfterEnter) {
      // 先把鼠标移动到按下事件的位置。
      lastMousePos = ui.Offset(x, y);
      refreshMousePos();
    }

    final pos = handlePointerDevicePos(
      kPointerEventKindMouse,
      x,
      y,
      isMove,
      type,
      onExit: onExit,
      buttons: evt['buttons'],
      moveCanvas: moveCanvas,
      edgeScroll: edgeScroll,
    );
    if (pos == null) {
      return null;
    }
    if (isMatchingMouseUp) {
      _pressedMouseButtons &= ~buttons;
    }
    if (type != '') {
      evt['x'] = '0';
      evt['y'] = '0';
    } else {
      evt['x'] = '${pos.x.toInt()}';
      evt['y'] = '${pos.y.toInt()}';
    }

    if (buttons is int) {
      evt['buttons'] = mouseButtonsToPeer(buttons);
      if ((isDesktop || isWebDesktop) &&
          type == kMouseEventTypeDown &&
          evt['buttons'] != '') {
        _pressedMouseButtons |= buttons;
      }
    } else {
      // 如果 buttons 存在但不是 int，则记录警告（调用方异常）。
      // 对缺失的 buttons 保留空字符串回退，以保持移动/悬停行为。
      if (buttons != null) {
        debugPrint(
            '[InputModel] processEventToPeer: unexpected buttons type: ${buttons.runtimeType}, value: $buttons');
      }
      evt['buttons'] = '';
    }
    return evt;
  }

  Map<String, dynamic>? handleMouse(
    Map<String, dynamic> evt,
    Offset offset, {
    bool onExit = false,
    bool moveCanvas = true,
    bool edgeScroll = false,
  }) {
    final evtToPeer = processEventToPeer(evt, offset,
        onExit: onExit, moveCanvas: moveCanvas, edgeScroll: edgeScroll);
    if (evtToPeer != null) {
      bind.sessionSendMouse(
          sessionId: sessionId, msg: json.encode(modify(evtToPeer)));
    }
    return evtToPeer;
  }

  Point? handlePointerDevicePos(
    String kind,
    double x,
    double y,
    bool isMove,
    String evtType, {
    bool onExit = false,
    int buttons = kPrimaryMouseButton,
    bool moveCanvas = true,
    bool edgeScroll = false,
  }) {
    final ffiModel = parent.target!.ffiModel;
    CanvasCoords canvas =
        CanvasCoords.fromCanvasModel(parent.target!.canvasModel);
    Rect? rect = ffiModel.rect;

    if (isMove) {
      if (_remoteWindowCoords.isNotEmpty &&
          _windowRect != null &&
          !_isInCurrentWindow(x, y)) {
        final coords =
            findRemoteCoords(x, y, _remoteWindowCoords, devicePixelRatio);
        if (coords != null) {
          isMove = false;
          canvas = coords.canvas;
          rect = coords.remoteRect;
          x -= isWindows
              ? coords.relativeOffset.dx / devicePixelRatio
              : coords.relativeOffset.dx;
          y -= isWindows
              ? coords.relativeOffset.dy / devicePixelRatio
              : coords.relativeOffset.dy;
        }
      }
    }

    y -= CanvasModel.topToEdge;
    x -= CanvasModel.leftToEdge;
    if (isMove) {
      final canvasModel = parent.target!.canvasModel;

      if (edgeScroll) {
        canvasModel.edgeScrollMouse(x, y);
      } else if (moveCanvas) {
        canvasModel.moveDesktopMouse(x, y);
      }

      canvasModel.updateLocalCursor(x, y);
    }

    return _handlePointerDevicePos(
      kind,
      x,
      y,
      isMove,
      canvas,
      rect,
      evtType,
      onExit: onExit,
      buttons: buttons,
    );
  }

  bool _isInCurrentWindow(double x, double y) {
    var w = _windowRect!.width;
    var h = _windowRect!.height;
    if (isWindows) {
      w /= devicePixelRatio;
      h /= devicePixelRatio;
    }
    return x >= 0 && y >= 0 && x <= w && y <= h;
  }

  static RemoteWindowCoords? findRemoteCoords(double x, double y,
      List<RemoteWindowCoords> remoteWindowCoords, double devicePixelRatio) {
    if (isWindows) {
      x *= devicePixelRatio;
      y *= devicePixelRatio;
    }
    for (final c in remoteWindowCoords) {
      if (x >= c.relativeOffset.dx &&
          y >= c.relativeOffset.dy &&
          x <= c.relativeOffset.dx + c.windowRect.width &&
          y <= c.relativeOffset.dy + c.windowRect.height) {
        return c;
      }
    }
    return null;
  }

  Point? _handlePointerDevicePos(
    String kind,
    double x,
    double y,
    bool moveInCanvas,
    CanvasCoords canvas,
    Rect? rect,
    String evtType, {
    bool onExit = false,
    int buttons = kPrimaryMouseButton,
  }) {
    if (rect == null) {
      return null;
    }

    final nearThr = 3;
    var nearRight = (canvas.size.width - x) < nearThr;
    var nearBottom = (canvas.size.height - y) < nearThr;
    final imageWidth = rect.width * canvas.scale;
    final imageHeight = rect.height * canvas.scale;
    if (canvas.scrollStyle != ScrollStyle.scrollauto) {
      x += imageWidth * canvas.scrollX;
      y += imageHeight * canvas.scrollY;

      // boxed size 是一个居中组件
      if (canvas.size.width > imageWidth) {
        x -= ((canvas.size.width - imageWidth) / 2);
      }
      if (canvas.size.height > imageHeight) {
        y -= ((canvas.size.height - imageHeight) / 2);
      }
    } else {
      x -= canvas.x;
      y -= canvas.y;
    }

    x /= canvas.scale;
    y /= canvas.scale;
    if (canvas.scale > 0 && canvas.scale < 1) {
      final step = 1.0 / canvas.scale - 1;
      if (nearRight) {
        x += step;
      }
      if (nearBottom) {
        y += step;
      }
    }
    x += rect.left;
    y += rect.top;

    if (onExit) {
      final pos = setNearestEdge(x, y, rect);
      x = pos.dx;
      y = pos.dy;
    }

    return InputModel.getPointInRemoteRect(
        true, peerPlatform, kind, evtType, x, y, rect,
        buttons: buttons);
  }

  static Point<double>? getPointInRemoteRect(
      bool isLocalDesktop,
      String? peerPlatform,
      String kind,
      String evtType,
      double evtX,
      double evtY,
      Rect rect,
      {int buttons = kPrimaryMouseButton}) {
    double minX = rect.left;
    // https://github.com/rustdesk/rustdesk/issues/6678
    // 在 Windows 上，需要设置 [0,maxX]、[0,maxY] 才能启用窗口贴靠。
    double maxX = (rect.left + rect.width) -
        (peerPlatform == kPeerPlatformWindows ? 0 : 1);
    double minY = rect.top;
    double maxY = (rect.top + rect.height) -
        (peerPlatform == kPeerPlatformWindows ? 0 : 1);
    evtX = InputModel.tryGetNearestRange(evtX, minX, maxX, 5);
    evtY = InputModel.tryGetNearestRange(evtY, minY, maxY, 5);
    if (isLocalDesktop) {
      if (kind == kPointerEventKindMouse) {
        if (evtX < minX || evtY < minY || evtX > maxX || evtY > maxY) {
          // 如果是鼠标左键抬起，不提前返回。
          if (!(buttons == kPrimaryMouseButton &&
              evtType == kMouseEventTypeUp)) {
            return null;
          }
        }
      }
    } else {
      bool evtXInRange = evtX >= minX && evtX <= maxX;
      bool evtYInRange = evtY >= minY && evtY <= maxY;
      if (!(evtXInRange || evtYInRange)) {
        return null;
      }
      if (evtX < minX) {
        evtX = minX;
      } else if (evtX > maxX) {
        evtX = maxX;
      }
      if (evtY < minY) {
        evtY = minY;
      } else if (evtY > maxY) {
        evtY = maxY;
      }
    }

    return Point(evtX, evtY);
  }

  /// 仅 web
  void listenToMouse(bool yesOrNo) {
    if (yesOrNo) {
      platformFFI.startDesktopWebListener();
    } else {
      platformFFI.stopDesktopWebListener();
    }
  }

  void onMobileBack() {
    final minBackButtonVersion = "1.3.8";
    final peerVersion =
        parent.target?.ffiModel.pi.version ?? minBackButtonVersion;
    var btn = MouseButtons.back;
    // 为了兼容旧版本
    if (versionCmp(peerVersion, minBackButtonVersion) < 0) {
      btn = MouseButtons.right;
    }
    tap(btn);
  }

  void onMobileHome() => tap(MouseButtons.wheel);
  Future<void> onMobileApps() async {
    sendMouse('down', MouseButtons.wheel);
    await Future.delayed(const Duration(milliseconds: 500));
    sendMouse('up', MouseButtons.wheel);
  }

  // 模拟一次按键按下事件。
  // `usbHidUsage` 是该按键的 USB HID 使用码。
  Future<void> tapHidKey(int usbHidUsage) async {
    newKeyboardMode(kKeyFlutterKey, usbHidUsage, true, false);
    await Future.delayed(Duration(milliseconds: 100));
    newKeyboardMode(kKeyFlutterKey, usbHidUsage, false, false);
  }

  Future<void> onMobileVolumeUp() async =>
      await tapHidKey(PhysicalKeyboardKey.audioVolumeUp.usbHidUsage & 0xFFFF);
  Future<void> onMobileVolumeDown() async =>
      await tapHidKey(PhysicalKeyboardKey.audioVolumeDown.usbHidUsage & 0xFFFF);
  Future<void> onMobilePower() async =>
      await tapHidKey(PhysicalKeyboardKey.power.usbHidUsage & 0xFFFF);
}
