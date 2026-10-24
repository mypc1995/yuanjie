import 'dart:async';

import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter/scheduler.dart';
import 'package:get/get.dart';
import 'package:provider/provider.dart';
import 'package:flutter_hbb/models/state_model.dart';

import '../../consts.dart';
import '../../common/widgets/overlay.dart';
import '../../common/widgets/remote_input.dart';
import '../../common.dart';
import '../../common/widgets/dialog.dart';
import '../../common/widgets/toolbar.dart';
import '../../models/model.dart';
import '../../models/input_model.dart';
import '../../models/platform_model.dart';
import '../../models/shortcut_model.dart';
import '../../common/shared_state.dart';
import '../../utils/image.dart';
import '../widgets/remote_toolbar.dart';
import '../widgets/kb_layout_type_chooser.dart';
import '../widgets/tabbar_widget.dart';
import 'macos_full_screen_focus_recovery.dart';

import 'package:flutter_hbb/native/custom_cursor.dart'
    if (dart.library.html) 'package:flutter_hbb/web/custom_cursor.dart';

final SimpleWrapper<bool> _firstEnterImage = SimpleWrapper(false);

// 当点击"在新窗口中打开"时，用于跳过会话关闭。
final Map<String, bool> closeSessionOnDispose = {};

class RemotePage extends StatefulWidget {
  RemotePage({
    Key? key,
    required this.id,
    required this.toolbarState,
    this.sessionId,
    this.tabWindowId,
    this.password,
    this.display,
    this.displays,
    this.tabController,
    this.switchUuid,
    this.forceRelay,
    this.isSharedPassword,
  }) : super(key: key) {
    initSharedStates(id);
  }

  final String id;
  final SessionID? sessionId;
  final int? tabWindowId;
  final int? display;
  final List<int>? displays;
  final String? password;
  final ToolbarState toolbarState;
  final String? switchUuid;
  final bool? forceRelay;
  final bool? isSharedPassword;
  final SimpleWrapper<State<RemotePage>?> _lastState = SimpleWrapper(null);
  final DesktopTabController? tabController;

  FFI get ffi => (_lastState.value! as _RemotePageState)._ffi;

  void releaseMacOSInputForTabTransfer() {
    if (!isMacOS) return;
    // 在移除源标签页之前先释放。其延迟销毁不得
    // 禁用目标页面已经获取的原生键盘钩子。
    (_lastState.value! as _RemotePageState)._releaseMacOSRemoteInput();
  }

  @override
  State<RemotePage> createState() {
    final state = _RemotePageState(id);
    _lastState.value = state;
    return state;
  }
}

class _RemotePageState extends State<RemotePage>
    with
        AutomaticKeepAliveClientMixin,
        MultiWindowListener,
        WidgetsBindingObserver,
        TickerProviderStateMixin {
  Timer? _timer;
  String keyboardMode = "legacy";
  bool _isWindowBlur = false;
  // 已知的 macOS 远程输入取舍（有意保持简单）：
  // 1. 对话框依赖 FocusNode 失焦加上 middleBlocked，而不是镜像对话框
  //    状态。复现方式：激活远程输入，打开一个对话框，然后输入。
  // 2. 延迟的全屏恢复可能与本地控件的焦点切换发生竞争；不会
  //    增加归属状态。复现方式：在切换 Space 期间聚焦工具栏。
  // 3. 切换输入源时会释放原生输入，但没有更新本
  //    页面的缓存。复现方式：切换输入源，然后在点击远端图像前后分别输入；
  //    点击会重新取得输入。
  // 这些闩锁用于补偿 macOS 乱序的焦点事件。修改某个过渡或
  // _syncMacOSKeyboardGrab() 时要把它们视为相互关联。
  AppLifecycleState? _macOSLifecycleState;
  bool _macOSLocalFocusLost = false;
  bool _macOSInputActive = false;
  bool _macOSInputSuppressed = false;
  final _macOSFullScreenFocusRecovery = MacOSFullScreenFocusRecovery();
  bool _macOSExplicitFocusRequestPending = false;
  StreamSubscription<DesktopTabState>? _tabStateSubscription;
  final _cursorOverImage = false.obs;
  late RxBool _showRemoteCursor;
  late RxBool _zoomCursor;
  late RxBool _remoteCursorMoved;
  late RxBool _keyboardEnabled;
  final _uniqueKey = UniqueKey();

  var _blockableOverlayState = BlockableOverlayState();

  final FocusNode _rawKeyFocusNode = FocusNode(debugLabel: "rawkeyFocusNode");

  // 窗口事件期间用于更新指针锁定中心的防抖定时器。
  // 时长使用 consts.dart 中的 kDefaultPointerLockCenterThrottleMs。
  Timer? _pointerLockCenterDebounceTimer;

  // 我们需要 `_instanceIdOnEnterOrLeaveImage4Toolbar` 配合 `_onEnterOrLeaveImage4Toolbar`
  // 来标识工具栏实例及其回调函数。
  int? _instanceIdOnEnterOrLeaveImage4Toolbar;
  Function(bool)? _onEnterOrLeaveImage4Toolbar;

  late FFI _ffi;
  Worker? _waylandKeyboardModeWorker;
  bool _waylandKeyboardModeNormalized = false;
  bool _waylandKeyboardModeNormalizing = false;

  SessionID get sessionId => _ffi.sessionId;

  _RemotePageState(String id) {
    _initStates(id);
  }

  void _initStates(String id) {
    _zoomCursor = PeerBoolOption.find(id, kOptionZoomCursor);
    _showRemoteCursor = ShowRemoteCursorState.find(id);
    _keyboardEnabled = KeyboardEnabledState.find(id);
    _remoteCursorMoved = RemoteCursorMovedState.find(id);
  }

  @override
  void initState() {
    super.initState();
    _ffi = FFI(widget.sessionId);
    if (isMacOS) {
      // 新窗口中的第一次连接时，SchedulerBinding.instance.lifecycleState 为 null。
      _macOSLifecycleState = SchedulerBinding.instance.lifecycleState;
      WidgetsBinding.instance.addObserver(this);
      _tabStateSubscription =
          widget.tabController?.state.listen(_onMacOSTabStateChanged);
    }
    Get.put<FFI>(_ffi, tag: widget.id);
    _ffi.imageModel.addCallbackOnFirstImage((String peerId) {
      _ffi.canvasModel.activateLocalCursor();
      showKBLayoutTypeChooserIfNeeded(
          _ffi.ffiModel.pi.platform, _ffi.dialogManager);
      _ffi.recordingModel
          .updateStatus(bind.sessionGetIsRecording(sessionId: _ffi.sessionId));
      // 会话就绪后初始化快捷键动作回调，使得
      // 即使用户从未打开
      // 工具栏菜单，全局键盘快捷键也能工作。返回的列表被有意丢弃 —— 
      // 注册回调（在 toolbarControls 内部）的副作用
      // 正是我们这里需要的。
      if (mounted) {
        toolbarControls(context, widget.id, _ffi);
        registerSessionShortcutActions(_ffi,
            tabController: widget.tabController,
            toolbarState: widget.toolbarState);
        registerToolbarShortcuts(context, widget.id, _ffi);
      }
    });
    _ffi.canvasModel.initializeEdgeScrollFallback(this);
    _ffi.start(
      widget.id,
      password: widget.password,
      isSharedPassword: widget.isSharedPassword,
      switchUuid: widget.switchUuid,
      forceRelay: widget.forceRelay,
      tabWindowId: widget.tabWindowId,
      display: widget.display,
      displays: widget.displays,
    );
    WidgetsBinding.instance.addPostFrameCallback((_) {
      SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual, overlays: []);
      _ffi.dialogManager
          .showLoading(translate('Connecting...'), onCancel: closeConnection);
    });
    WakelockManager.enable(_uniqueKey);

    _ffi.ffiModel.updateEventListener(sessionId, widget.id);
    _ffi.qualityMonitorModel.checkShowQualityMonitor(sessionId);
    _ffi.dialogManager.loadMobileActionsOverlayVisible();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      // 会话选项应在 models.dart/FFI.start 之后设置
      _showRemoteCursor.value = bind.sessionGetToggleOptionSync(
          sessionId: sessionId, arg: 'show-remote-cursor');
      _zoomCursor.value = bind.sessionGetToggleOptionSync(
          sessionId: sessionId, arg: kOptionZoomCursor);
    });
    DesktopMultiWindow.addListener(this);
    // if (!_isCustomCursorInited) {
    //   customCursorController.registerNeedUpdateCursorCallback(
    //       (String? lastKey, String? currentKey) async {
    //     if (_firstEnterImage.value) {
    //       _firstEnterImage.value = false;
    //       return true;
    //     }
    //     return lastKey == null || lastKey != currentKey;
    //   });
    //   _isCustomCursorInited = true;
    // }

    _blockableOverlayState.applyFfi(_ffi);
    // 在 post frame 回调中调用 onSelected，因为无法保证该回调不会调用 setState。
    WidgetsBinding.instance.addPostFrameCallback((_) {
      widget.tabController?.onSelected?.call(widget.id);
    });

    // 注册回调，在相对鼠标模式被禁用时取消防抖定时器
    _ffi.inputModel.onRelativeMouseModeDisabled =
        _cancelPointerLockCenterDebounceTimer;

    _waylandKeyboardModeWorker = ever(_ffi.ffiModel.pi.isSet, (bool isSet) {
      if (isSet) {
        unawaited(_normalizeWaylandKeyboardModeIfNeeded());
      }
    });
    if (_ffi.ffiModel.pi.isSet.value) {
      unawaited(_normalizeWaylandKeyboardModeIfNeeded());
    }
  }

  Future<void> _normalizeWaylandKeyboardModeIfNeeded() async {
    if (!mounted ||
        _waylandKeyboardModeNormalized ||
        _waylandKeyboardModeNormalizing) {
      return;
    }
    _waylandKeyboardModeNormalizing = true;
    try {
      final pi = _ffi.ffiModel.pi;
      if (pi.platform != kPeerPlatformLinux || !pi.isWayland) return;
      final mapSupported = bind.sessionIsKeyboardModeSupported(
          sessionId: sessionId, mode: kKeyMapMode);
      if (!mapSupported) return;
      final current = await bind.sessionGetKeyboardMode(sessionId: sessionId);
      if (!mounted) return;
      if (current == kKeyMapMode) {
        _waylandKeyboardModeNormalized = true;
        return;
      }
      await bind.sessionSetKeyboardMode(
          sessionId: sessionId, value: kKeyMapMode);
      if (!mounted) return;
      await _ffi.inputModel.updateKeyboardMode();
      if (!mounted) return;
      _waylandKeyboardModeNormalized = true;
    } catch (e, st) {
      debugPrint('Failed to normalize Wayland keyboard mode: $e');
      debugPrintStack(stackTrace: st);
    } finally {
      _waylandKeyboardModeNormalizing = false;
    }
  }

  /// 取消指针锁定中心的防抖定时器
  void _cancelPointerLockCenterDebounceTimer() {
    _pointerLockCenterDebounceTimer?.cancel();
    _pointerLockCenterDebounceTimer = null;
  }

  bool get _isSelectedTab {
    final controller = widget.tabController;
    if (controller == null) return true;
    final tabState = controller.state.value;
    final selected = tabState.selected;
    return selected >= 0 &&
        selected < tabState.tabs.length &&
        tabState.tabs[selected].key == widget.id;
  }

  // Windows 上的每次 requestFocus() 都必须经过这里，否则一个阻塞式对话框或
  // 非活动标签页可能会把远程输入交给本页。
  bool get _windowsCanFocusRemoteInput =>
      _isSelectedTab && _blockableOverlayState.middleBlocked.isFalse;

  bool get _isMacOSKeyboardContextActive {
    return stateGlobal.isFocused.value && !_isWindowBlur && _isSelectedTab;
  }

  void _onMacOSTabStateChanged(DesktopTabState _) {
    if (!_isSelectedTab) {
      _macOSFullScreenFocusRecovery.cancel();
      _syncMacOSKeyboardGrab();
      return;
    }
    // 标签页监听器是同步执行的。把选中页面的处理延后，让前一个
    // 页面先释放；它迟到的离开事件可能会禁用新的会话。
    scheduleMicrotask(() {
      if (mounted) {
        _syncMacOSKeyboardGrab(reassert: true);
      }
    });
  }

  void _releaseMacOSRemoteInput() {
    _macOSFullScreenFocusRecovery.cancel();
    _macOSExplicitFocusRequestPending = false;
    _macOSInputSuppressed = true;
    _macOSLocalFocusLost = true;
    _ffi.inputModel.enterOrLeave(false);
    _macOSInputActive = false;
    _rawKeyFocusNode.unfocus();
  }

  void _onMacOSFocusChange() {
    // requestFocus() 是稍后才通知的；只有记录下来的显式请求才能清除
    // 本地失焦闩锁。
    if (_rawKeyFocusNode.hasPrimaryFocus) {
      final explicitRequest = _macOSExplicitFocusRequestPending;
      _macOSExplicitFocusRequestPending = false;
      if (explicitRequest && _isMacOSKeyboardContextActive) {
        _macOSLocalFocusLost = false;
      }
      _syncMacOSKeyboardGrab(allowInactiveLifecycle: explicitRequest);
    } else {
      if (_macOSInputActive) {
        _ffi.inputModel.enterOrLeave(false);
        _macOSInputActive = false;
      }
      if (_isMacOSKeyboardContextActive) {
        _macOSLocalFocusLost = true;
      }
    }
  }

  // 1. 将键盘捕获状态与当前上下文同步。
  // 2. 调用 enterOrLeave() 更新 FFI 层中的输入状态。
  // 3. 根据当前上下文请求或取消聚焦原始按键焦点节点。
  // Flutter 焦点与原生输入是两套机制；原生输入只有在
  // FocusNode 获得主焦点后才会激活。
  void _syncMacOSKeyboardGrab({
    bool reassert = false,
    bool allowInactiveLifecycle = false,
  }) {
    if (!isMacOS) return;
    // 副引擎在窗口可见时仍可能处于隐藏状态，因此
    // 显式的指针/全屏恢复必须绕过全局生命周期。
    final lifecycleAllowsInput = allowInactiveLifecycle ||
        _macOSLifecycleState == null ||
        _macOSLifecycleState == AppLifecycleState.resumed;
    // 除获得焦点的全屏恢复外，输入始终受指针门控，它
    // 用于补偿 macOS 在切换 Space 时不发送 PointerEnter 的情况。
    final shouldFocus = lifecycleAllowsInput &&
        _isMacOSKeyboardContextActive &&
        !_macOSInputSuppressed &&
        _blockableOverlayState.middleBlocked.isFalse &&
        _cursorOverImage.value &&
        !_macOSLocalFocusLost;
    final hasFocus = _rawKeyFocusNode.hasPrimaryFocus;
    final shouldActivateInput = shouldFocus && hasFocus;

    if (shouldActivateInput != _macOSInputActive ||
        (shouldActivateInput && reassert)) {
      _ffi.inputModel.enterOrLeave(shouldActivateInput);
    }
    _macOSInputActive = shouldActivateInput;

    if (!shouldFocus) {
      _macOSExplicitFocusRequestPending = false;
      if (hasFocus) _rawKeyFocusNode.unfocus();
    } else if (!hasFocus) {
      _macOSExplicitFocusRequestPending = allowInactiveLifecycle;
      _rawKeyFocusNode.requestFocus();
    } else {
      _macOSExplicitFocusRequestPending = false;
    }
  }

  void _restoreMacOSKeyboardAfterFullScreen({
    required int generation,
    bool allowHiddenLifecycle = false,
  }) {
    // 全屏回调在隐藏期间会保留恢复逻辑。原生窗口焦点
    // 对于刚变为可见的 Space，可能绕过过期的隐藏生命周期。
    if (!_macOSFullScreenFocusRecovery.isCurrent(generation) ||
        (!allowHiddenLifecycle &&
            _macOSLifecycleState == AppLifecycleState.hidden)) {
      return;
    }
    final contextActive =
        stateGlobal.isFocused.value && !_isWindowBlur && _isSelectedTab;
    // macOS 可能在不发送 PointerEnter 的情况下聚焦某个全屏 Space。原生
    // 窗口焦点在此处是权威依据；之后的失焦会在离屏窗口
    // 恢复输入之前取消本次生成。
    final shouldInferPointerInside = !_cursorOverImage.value &&
        allowHiddenLifecycle &&
        stateGlobal.fullscreen.isTrue &&
        contextActive;
    final canRestore = contextActive &&
        _blockableOverlayState.middleBlocked.isFalse &&
        (_cursorOverImage.value || shouldInferPointerInside);
    if (!_macOSFullScreenFocusRecovery.consume(generation)) return;
    if (!canRestore) {
      // 在此处消费恢复动作需要后续的指针/窗口/标签页事件。
      return;
    }
    if (shouldInferPointerInside) {
      _cursorOverImage.value = true;
    }
    _macOSLocalFocusLost = false;
    stateGlobal.getInputSource(force: true);
    _syncMacOSKeyboardGrab(reassert: true, allowInactiveLifecycle: true);
  }

  void _scheduleMacOSKeyboardAfterFullScreen({
    required int generation,
    bool allowHiddenLifecycle = false,
  }) {
    // 全屏可能在其回调之后才抛出 FocusNode 失焦；等待帧
    // 结束后再推进一轮事件循环，然后才恢复。
    WidgetsBinding.instance.addPostFrameCallback((_) {
      Timer.run(() {
        if (mounted) {
          _restoreMacOSKeyboardAfterFullScreen(
            generation: generation,
            allowHiddenLifecycle: allowHiddenLifecycle,
          );
        }
      });
    });
    WidgetsBinding.instance.ensureVisualUpdate();
  }

  void _queueMacOSKeyboardAfterFullScreen({
    bool allowHiddenLifecycle = false,
  }) {
    final generation = _macOSFullScreenFocusRecovery.queue();
    if (_macOSLifecycleState == AppLifecycleState.paused ||
        _macOSLifecycleState == AppLifecycleState.detached) {
      _macOSFullScreenFocusRecovery.cancel();
      return;
    }
    _scheduleMacOSKeyboardAfterFullScreen(
      generation: generation,
      allowHiddenLifecycle: allowHiddenLifecycle,
    );
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    super.didChangeAppLifecycleState(state);
    if (!isMacOS || _macOSLifecycleState == state) return;
    _macOSLifecycleState = state;
    if (state == AppLifecycleState.resumed) {
      _syncMacOSKeyboardGrab(reassert: true);
    } else if (_macOSInputActive) {
      _ffi.inputModel.enterOrLeave(false);
      _macOSInputActive = false;
    }

    final generation = _macOSFullScreenFocusRecovery.pendingGeneration;
    if (generation == null) return;
    if (state == AppLifecycleState.inactive ||
        state == AppLifecycleState.resumed) {
      _scheduleMacOSKeyboardAfterFullScreen(generation: generation);
    } else if (state == AppLifecycleState.paused ||
        state == AppLifecycleState.detached) {
      _macOSFullScreenFocusRecovery.cancel();
    }
  }

  @override
  void onWindowBlur() {
    super.onWindowBlur();
    // 在 Windows 上，我们使用 `focus` 方式更好地处理键盘。
    // 现在在 Linux 上，rdev 存在一些会破坏输入的问题。
    // 我们暂时为 Linux 禁用 `focus` 方式。
    if (isWindows || isMacOS) {
      _isWindowBlur = true;
    }
    if (isMacOS) {
      _macOSFullScreenFocusRecovery.cancel();
      // 失焦或切换 Space 可能不会发出 PointerExit，因此仅靠光标状态
      // 无法阻止旧的远端画面重新抢走键盘。
      _macOSLocalFocusLost = true;
    }
    if (isWindows) {
      // 整个窗口失去焦点时取消主焦点的聚焦，
      // 改为让操作系统处理事件。
      _rawKeyFocusNode.unfocus();
    }
    stateGlobal.isFocused.value = false;
    _syncMacOSKeyboardGrab();

    // 窗口失去焦点时，临时释放相对鼠标模式的约束
    // 以允许用户正常与其他应用交互。
    // 窗口重新获得焦点时，光标会被重新隐藏并重新居中。
    if (_ffi.inputModel.relativeMouseMode.value) {
      _ffi.inputModel.onWindowBlur();
    }
  }

  @override
  void onWindowFocus() {
    super.onWindowFocus();
    // 参见 [onWindowBlur]。
    if (isWindows || isMacOS) {
      _isWindowBlur = false;
    }
    if (isMacOS) stateGlobal.getInputSource(force: true);
    stateGlobal.isFocused.value = true;

    // 普通的 macOS 窗口会等待 PointerEnter 或 PointerDown。获得焦点的
    // 全屏 Space 会排队一次延迟恢复；如果该窗口再次失焦，
    // 待处理的恢复会在原生输入重新激活之前被取消。
    // 回归问题：在不移动也不点击的情况下，直接在多个全屏远端 Space 之间切换，
    // 只有新获得焦点的会话可以接收输入。
    if (isMacOS &&
        stateGlobal.fullscreen.isTrue &&
        !_ffi.inputModel.relativeMouseMode.value) {
      // 当副引擎在全屏 Space 变为可见后仍保留着
      // 过期的隐藏生命周期状态时，原生窗口焦点是权威依据。
      _queueMacOSKeyboardAfterFullScreen(allowHiddenLifecycle: true);
    }

    // 没有 PointerEnter 的重新聚焦：焦点返回时（Alt+Tab、任务栏）光标本就已悬停在图像上，
    // 因此 enterView() 不会再触发。
    if (isWindows &&
        _cursorOverImage.value &&
        _windowsCanFocusRemoteInput &&
        !_rawKeyFocusNode.hasFocus) {
      _rawKeyFocusNode.requestFocus();
    }

    // 窗口重新获得焦点时恢复相对鼠标模式的约束。
    if (_ffi.inputModel.relativeMouseMode.value) {
      if (isMacOS) {
        // 原生相对模式会保持指针捕获，并且不会发出
        // 窗口焦点返回后的 PointerEnter。除非
        // 本地叠加层仍持有输入，否则要同时恢复两个闩锁。
        if (_blockableOverlayState.middleBlocked.isFalse) {
          _cursorOverImage.value = true;
          _macOSLocalFocusLost = false;
        }
      } else if (!isWindows || _windowsCanFocusRemoteInput) {
        _rawKeyFocusNode.requestFocus();
      }
      _ffi.inputModel.onWindowFocus();
    }
    _syncMacOSKeyboardGrab(reassert: true, allowInactiveLifecycle: true);
  }

  @override
  void onWindowRestore() {
    super.onWindowRestore();
    // 在 Windows 上，我们使用 `onWindowRestore` 方式处理窗口从
    // 最小化状态恢复。
    if (isWindows) {
      _isWindowBlur = false;
    }
    WakelockManager.enable(_uniqueKey);
    // 窗口恢复时更新指针锁定中心
    _updatePointerLockCenterIfNeeded();
  }

  // 窗口取消最小化时，无论旧状态是否为最大化，都可能调用 onWindowMaximize 或 onWindowRestore。
  @override
  void onWindowMaximize() {
    super.onWindowMaximize();
    WakelockManager.enable(_uniqueKey);
    // 窗口最大化时更新指针锁定中心
    _updatePointerLockCenterIfNeeded();
  }

  @override
  void onWindowResize() {
    super.onWindowResize();
    // 窗口调整大小时更新指针锁定中心
    _updatePointerLockCenterIfNeeded();
  }

  @override
  void onWindowMove() {
    super.onWindowMove();
    // 窗口移动时更新指针锁定中心
    _updatePointerLockCenterIfNeeded();
  }

  /// 以防抖方式更新指针锁定中心，避免更新过于频繁
  /// （在窗口快速移动/缩放事件期间）。
  void _updatePointerLockCenterIfNeeded() {
    if (!_ffi.inputModel.relativeMouseMode.value) return;

    // 取消任何待处理的更新并安排一次新的更新（防抖模式）
    _pointerLockCenterDebounceTimer?.cancel();
    _pointerLockCenterDebounceTimer = Timer(
      const Duration(milliseconds: kDefaultPointerLockCenterThrottleMs),
      () {
        if (!mounted) return;
        if (_ffi.inputModel.relativeMouseMode.value) {
          _ffi.inputModel.updatePointerLockCenter();
        }
      },
    );
  }

  @override
  void onWindowMinimize() {
    super.onWindowMinimize();
    WakelockManager.disable(_uniqueKey);
    if (isMacOS) {
      _macOSFullScreenFocusRecovery.cancel();
      _isWindowBlur = true;
      _cursorOverImage.value = false;
      stateGlobal.isFocused.value = false;
      _syncMacOSKeyboardGrab();
    }
    // 最小化时释放光标约束
    if (_ffi.inputModel.relativeMouseMode.value) {
      _ffi.inputModel.onWindowBlur();
    }
  }

  @override
  void onWindowEnterFullScreen() {
    super.onWindowEnterFullScreen();
    if (isMacOS) {
      stateGlobal.setFullscreen(true);
      _queueMacOSKeyboardAfterFullScreen();
    }
  }

  @override
  void onWindowLeaveFullScreen() {
    super.onWindowLeaveFullScreen();
    if (isMacOS) {
      stateGlobal.setFullscreen(false);
      _queueMacOSKeyboardAfterFullScreen();
    }
  }

  @override
  Future<void> dispose() async {
    final closeSession = closeSessionOnDispose.remove(widget.id) ?? true;

    // https://github.com/flutter/flutter/issues/64935
    if (isMacOS) {
      // 标签页移动时先释放再转移，避免保留会话的迟到离开事件。
      if (closeSession) {
        _releaseMacOSRemoteInput();
      }
      _tabStateSubscription?.cancel();
      WidgetsBinding.instance.removeObserver(this);
    }
    super.dispose();
    debugPrint("REMOTE PAGE dispose session $sessionId ${widget.id}");

    // 防御性清理：确保宿主系统按键传递被重置，即使
    // MouseRegion.onExit 从未触发（例如光标在标签页内部时标签页被关闭）。
    if (!isWeb) bind.hostStopSystemKeyPropagate(stopped: true);

    _pointerLockCenterDebounceTimer?.cancel();
    _pointerLockCenterDebounceTimer = null;
    _waylandKeyboardModeWorker?.dispose();
    // 清除回调引用，防止内存泄漏和过期引用
    _ffi.inputModel.onRelativeMouseModeDisabled = null;
    // 相对鼠标模式的清理由 FFI.close(closeSession: ...) 集中处理。
    _ffi.textureModel.onRemotePageDispose(closeSession);
    if (closeSession && !isMacOS) {
      // 确保我们离开这个会话，这是一次复查
      // 在 macOS 上，enterOrLeave() 之前已在 _releaseMacOSRemoteInput() 中调用过。
      _ffi.inputModel.enterOrLeave(false);
    }
    DesktopMultiWindow.removeListener(this);
    _ffi.dialogManager.hideMobileActionsOverlay();
    _ffi.imageModel.disposeImage();
    _ffi.cursorModel.disposeImages();
    _rawKeyFocusNode.dispose();
    if (closeSession) {
      clearWaylandKeyboardPromptSuppressedForConnection(sessionId.toString());
    }
    await _ffi.close(closeSession: closeSession);
    _timer?.cancel();
    _ffi.dialogManager.dismissAll();
    if (closeSession) {
      await SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual,
          overlays: SystemUiOverlay.values);
    }
    WakelockManager.disable(_uniqueKey);
    await Get.delete<FFI>(tag: widget.id);
    removeSharedStates(widget.id);
  }

  Widget emptyOverlay() => BlockableOverlay(
        /// Overlay 键会在 BlockableOverlay 中通过 _blockableOverlayState 设置
        /// 参见 [BlockableOverlay] 中重写的 build()
        state: _blockableOverlayState,
        underlying: Container(
          color: Colors.transparent,
        ),
      );

  Widget buildBody(BuildContext context) {
    remoteToolbar(BuildContext context) => RemoteToolbar(
          id: widget.id,
          ffi: _ffi,
          state: widget.toolbarState,
          onEnterOrLeaveImageSetter: (id, func) {
            _instanceIdOnEnterOrLeaveImage4Toolbar = id;
            _onEnterOrLeaveImage4Toolbar = func;
          },
          onEnterOrLeaveImageCleaner: (id) {
            // 如果 _instanceIdOnEnterOrLeaveImage4Toolbar != id
            // 说明 `_onEnterOrLeaveImage4Toolbar` 未设置，或已被改为另一个工具栏。
            if (_instanceIdOnEnterOrLeaveImage4Toolbar == id) {
              _instanceIdOnEnterOrLeaveImage4Toolbar = null;
              _onEnterOrLeaveImage4Toolbar = null;
            }
          },
          setRemoteState: setState,
        );

    bodyWidget() {
      return Stack(
        children: [
          Container(
              color: kColorCanvas,
              child: RawKeyFocusScope(
                  focusNode: _rawKeyFocusNode,
                  onFocusChange: (bool imageFocused) {
                    debugPrint(
                        "onFocusChange(window active:${!_isWindowBlur}) $imageFocused");
                    // 参见 [onWindowBlur]。
                    if (isWindows) {
                      if (_isWindowBlur) {
                        imageFocused = false;
                        Future.delayed(Duration.zero, () {
                          _rawKeyFocusNode.unfocus();
                        });
                      }
                      if (imageFocused) {
                        _ffi.inputModel.enterOrLeave(true);
                      } else {
                        _ffi.inputModel.enterOrLeave(false);
                      }
                    } else if (isMacOS) {
                      _onMacOSFocusChange();
                    }
                  },
                  inputModel: _ffi.inputModel,
                  child: getBodyForDesktop(context))),
          Stack(
            children: [
              _ffi.ffiModel.pi.isSet.isTrue &&
                      _ffi.ffiModel.waitForFirstImage.isTrue
                  ? emptyOverlay()
                  : () {
                      if (!_ffi.ffiModel.isPeerAndroid) {
                        return Offstage();
                      } else {
                        return Obx(() => Offstage(
                              offstage: _ffi.dialogManager
                                  .mobileActionsOverlayVisible.isFalse,
                              child: Overlay(initialEntries: [
                                makeMobileActionsOverlayEntry(
                                  () => _ffi.dialogManager
                                      .setMobileActionsOverlayVisible(false),
                                  ffi: _ffi,
                                )
                              ]),
                            ));
                      }
                    }(),
              // 使用 Overlay 使每次点击菜单按钮都能重新构建。
              // 相对鼠标模式激活时隐藏工具栏，防止
              // 光标逃逸到工具栏区域。
              Obx(() => _ffi.inputModel.relativeMouseMode.value
                  ? const Offstage()
                  : _ffi.ffiModel.pi.isSet.isTrue
                      ? Overlay(initialEntries: [
                          OverlayEntry(builder: remoteToolbar)
                        ])
                      : remoteToolbar(context)),
              _ffi.ffiModel.pi.isSet.isFalse ? emptyOverlay() : Offstage(),
            ],
          ),
        ],
      );
    }

    return Scaffold(
      backgroundColor: Theme.of(context).colorScheme.background,
      body: Obx(() {
        final imageReady = _ffi.ffiModel.pi.isSet.isTrue &&
            _ffi.ffiModel.waitForFirstImage.isFalse;
        if (imageReady) {
          // 如果切换了隐私模式（禁用物理显示器），
          // 则不应立即关闭对话框。
          if (DateTime.now().difference(togglePrivacyModeTime) >
              const Duration(milliseconds: 3000)) {
            // 调用 `dismissAll()` 是为了确保状态干净。
            // 在这里调用 dismissAll() 没问题。
            _ffi.dialogManager.dismissAll();
            // 重新创建阻塞状态以刷新状态。
            _blockableOverlayState = BlockableOverlayState();
            _blockableOverlayState.applyFfi(_ffi);
          }
          // 对话框显示时阻塞整个 `bodyWidget()`。
          return BlockableOverlay(
            underlying: bodyWidget(),
            state: _blockableOverlayState,
          );
        } else {
          // 这里不会重新创建 `_blockableOverlayState`。
          // 重连时工具栏的阻塞状态无法正常工作，但也没关系。
          return bodyWidget();
        }
      }),
    );
  }

  @override
  Widget build(BuildContext context) {
    super.build(context);
    return WillPopScope(
        onWillPop: () async {
          clientClose(sessionId, _ffi);
          return false;
        },
        child: MultiProvider(providers: [
          ChangeNotifierProvider.value(value: _ffi.ffiModel),
          ChangeNotifierProvider.value(value: _ffi.imageModel),
          ChangeNotifierProvider.value(value: _ffi.cursorModel),
          ChangeNotifierProvider.value(value: _ffi.canvasModel),
          ChangeNotifierProvider.value(value: _ffi.recordingModel),
        ], child: buildBody(context)));
  }

  void enterView(PointerEnterEvent evt) {
    _ffi.canvasModel.rearmEdgeScroll();

    _cursorOverImage.value = true;
    _firstEnterImage.value = true;
    if (_onEnterOrLeaveImage4Toolbar != null) {
      try {
        _onEnterOrLeaveImage4Toolbar!(true);
      } catch (e) {
        //
      }
    }

    // 参见 [onWindowBlur]。
    if (isMacOS) {
      _macOSLocalFocusLost = false;
      stateGlobal.getInputSource(force: true);
      _syncMacOSKeyboardGrab(reassert: true, allowInactiveLifecycle: true);
    } else if (isWindows) {
      // 失焦会使该节点失去焦点且没有任何机制恢复它，因此键盘一直
      // 处于失效状态直到被点击。只在窗口真正处于活动状态时获取焦点，否则
      // 后台窗口会抢走系统按键。onFocusChange 会执行 enterOrLeave。
      if (!_isWindowBlur &&
          _windowsCanFocusRemoteInput &&
          !_rawKeyFocusNode.hasFocus) {
        _rawKeyFocusNode.requestFocus();
      }
    } else {
      if (!_rawKeyFocusNode.hasFocus) {
        _rawKeyFocusNode.requestFocus();
      }
      _ffi.inputModel.enterOrLeave(true);
    }
  }

  void leaveView(PointerExitEvent evt) {
    _ffi.canvasModel.disableEdgeScroll();

    if (_ffi.ffiModel.keyboard) {
      _ffi.inputModel.tryMoveEdgeOnExit(evt.position);
    }

    _cursorOverImage.value = false;
    _firstEnterImage.value = false;
    if (_onEnterOrLeaveImage4Toolbar != null) {
      try {
        _onEnterOrLeaveImage4Toolbar!(false);
      } catch (e) {
        //
      }
    }

    // 参见 [onWindowBlur]。
    if (isMacOS) {
      _syncMacOSKeyboardGrab();
    } else if (!isWindows) {
      _ffi.inputModel.enterOrLeave(false);
    }
  }

  Widget _buildRawTouchAndPointerRegion(
    Widget child,
    PointerEnterEventListener? onEnter,
    PointerExitEventListener? onExit,
  ) {
    return RawTouchGestureDetectorRegion(
      child: _buildRawPointerMouseRegion(child, onEnter, onExit),
      ffi: _ffi,
    );
  }

  Widget _buildRawPointerMouseRegion(
    Widget child,
    PointerEnterEventListener? onEnter,
    PointerExitEventListener? onExit,
  ) {
    return RawPointerMouseRegion(
      onEnter: onEnter,
      onExit: onExit,
      onPointerDown: (event) {
        // 在 Windows 和 macOS 上对失焦状态的再次确认。
        // 注意：如果触发了 `onPointerDown` 事件，预期 `_isWindowBlur` 为 false。
        // 有时系统不会把必要的焦点事件发送给 flutter。我们应该手动
        // 通过把 `_isWindowBlur` 设为 false 来处理这种不一致状态。这样我们就能
        // 确保用户点击远端画布时抓键线程正在运行。
        if ((isWindows || isMacOS) && _isWindowBlur) {
          debugPrint(
              "Unexpected status: onPointerDown is triggered while the remote window is in blur status");
          _isWindowBlur = false;
        }
        if (isMacOS) {
          // 没有配对的进入/离开回调的区域无法安全地持有
          // 键盘状态。
          if (onEnter == null || onExit == null) return;
          if (!stateGlobal.isFocused.value) {
            stateGlobal.isFocused.value = true;
          }
          _cursorOverImage.value = true;
          _macOSLocalFocusLost = false;
          stateGlobal.getInputSource(force: true);
          _syncMacOSKeyboardGrab(
              reassert: !isInputSourceFlutter, allowInactiveLifecycle: true);
        } else if (!_rawKeyFocusNode.hasFocus) {
          _rawKeyFocusNode.requestFocus();
        }
      },
      inputModel: _ffi.inputModel,
      child: child,
    );
  }

  Widget getBodyForDesktop(BuildContext context) {
    var paints = <Widget>[
      MouseRegion(
        onEnter: (evt) {
          if (!isWeb) bind.hostStopSystemKeyPropagate(stopped: false);
        },
        onExit: (evt) {
          if (!isWeb) bind.hostStopSystemKeyPropagate(stopped: true);
        },
        child: _ViewStyleUpdater(
          canvasModel: _ffi.canvasModel,
          inputModel: _ffi.inputModel,
          child: Builder(builder: (context) {
            final peerDisplay = CurrentDisplayState.find(widget.id);
            return Obx(
              () => _ffi.ffiModel.pi.isSet.isFalse
                  ? Container(color: Colors.transparent)
                  : Obx(() {
                      _ffi.textureModel.updateCurrentDisplay(peerDisplay.value);
                      return ImagePaint(
                        id: widget.id,
                        zoomCursor: _zoomCursor,
                        cursorOverImage: _cursorOverImage,
                        keyboardEnabled: _keyboardEnabled,
                        remoteCursorMoved: _remoteCursorMoved,
                        listenerBuilder: (child) =>
                            _buildRawTouchAndPointerRegion(
                                child, enterView, leaveView),
                        ffi: _ffi,
                      );
                    }),
            );
          }),
        ),
      )
    ];

    if (!_ffi.canvasModel.cursorEmbedded) {
      paints
          .add(Obx(() => _showRemoteCursor.isFalse || _remoteCursorMoved.isFalse
              ? Offstage()
              : CursorPaint(
                  id: widget.id,
                  zoomCursor: _zoomCursor,
                )));
    }
    paints.add(
      Positioned(
        top: 10,
        right: 10,
        child: _buildRawTouchAndPointerRegion(
            QualityMonitor(_ffi.qualityMonitorModel), null, null),
      ),
    );
    return Stack(
      children: paints,
    );
  }

  @override
  bool get wantKeepAlive => true;
}

/// 跟踪视图尺寸和 DPR，以更新 CanvasModel.updateViewStyle()
/// 和 InputModel.updateImageWidgetSize()，且仅在二者之一发生变化时才更新。
/// 这样可以避免在每次 LayoutBuilder 重建时都调度 post-frame 回调。
class _ViewStyleUpdater extends StatefulWidget {
  final CanvasModel canvasModel;
  final InputModel inputModel;
  final Widget child;

  const _ViewStyleUpdater({
    Key? key,
    required this.canvasModel,
    required this.inputModel,
    required this.child,
  }) : super(key: key);

  @override
  State<_ViewStyleUpdater> createState() => _ViewStyleUpdaterState();
}

class _ViewStyleUpdaterState extends State<_ViewStyleUpdater> {
  Size? _lastSize;
  double? _lastDevicePixelRatio;
  bool _callbackScheduled = false;

  @override
  Widget build(BuildContext context) {
    final devicePixelRatio = MediaQuery.devicePixelRatioOf(context);
    return LayoutBuilder(
      builder: (context, constraints) {
        final maxWidth = constraints.maxWidth;
        final maxHeight = constraints.maxHeight;
        // 防止出现无限约束（例如无约束的父级）。
        if (!maxWidth.isFinite || !maxHeight.isFinite) {
          return widget.child;
        }
        final newSize = Size(maxWidth, maxHeight);
        if (_lastSize != newSize || _lastDevicePixelRatio != devicePixelRatio) {
          _lastSize = newSize;
          _lastDevicePixelRatio = devicePixelRatio;
          // 把更新安排到当前帧之后，以避免在 build 期间调用 setState。
          // 使用 _callbackScheduled 标志位，防止累积多个回调
          // （在尺寸快速变化而回调尚未执行时）。
          if (!_callbackScheduled) {
            _callbackScheduled = true;
            SchedulerBinding.instance.addPostFrameCallback((_) {
              _callbackScheduled = false;
              final currentSize = _lastSize;
              if (mounted && currentSize != null) {
                widget.canvasModel.updateViewStyle();
                widget.inputModel.updateImageWidgetSize(currentSize);
              }
            });
          }
        }
        return widget.child;
      },
    );
  }
}

class ImagePaint extends StatefulWidget {
  final FFI ffi;
  final String id;
  final RxBool zoomCursor;
  final RxBool cursorOverImage;
  final RxBool keyboardEnabled;
  final RxBool remoteCursorMoved;
  final Widget Function(Widget)? listenerBuilder;

  ImagePaint(
      {Key? key,
      required this.ffi,
      required this.id,
      required this.zoomCursor,
      required this.cursorOverImage,
      required this.keyboardEnabled,
      required this.remoteCursorMoved,
      this.listenerBuilder})
      : super(key: key);

  @override
  State<StatefulWidget> createState() => _ImagePaintState();
}

class _ImagePaintState extends State<ImagePaint> {
  bool _lastRemoteCursorMoved = false;
  final _cursorDisplayScale = 1.0.obs;

  String get id => widget.id;
  RxBool get zoomCursor => widget.zoomCursor;
  RxBool get cursorOverImage => widget.cursorOverImage;
  RxBool get keyboardEnabled => widget.keyboardEnabled;
  RxBool get remoteCursorMoved => widget.remoteCursorMoved;
  Widget Function(Widget)? get listenerBuilder => widget.listenerBuilder;

  @override
  Widget build(BuildContext context) {
    final m = Provider.of<ImageModel>(context);
    var c = Provider.of<CanvasModel>(context);
    final s = c.scale;
    // 在画布的 post-frame 更新待处理期间读取实时 DPR。
    final dpr = MediaQuery.devicePixelRatioOf(context);

    bool isViewOriginal() => c.viewStyle.style == kRemoteViewStyleOriginal;

    mouseRegion({child}) => Obx(() {
          double getCursorScale() {
            var c = Provider.of<CanvasModel>(context);
            if (isDesktop) {
              return _getDesktopCursorScale(c, dpr);
            } else {
              return zoomCursor.value || isViewOriginal() ? s : 1.0;
            }
          }

          return MouseRegion(
              cursor: cursorOverImage.isTrue
                  ? c.cursorEmbedded
                      ? SystemMouseCursors.none
                      // 相对鼠标模式激活时隐藏光标
                      : widget.ffi.inputModel.relativeMouseMode.value
                          ? SystemMouseCursors.none
                          : keyboardEnabled.isTrue
                              ? (() {
                                  if (remoteCursorMoved.isTrue) {
                                    _lastRemoteCursorMoved = true;
                                    return SystemMouseCursors.none;
                                  } else {
                                    if (_lastRemoteCursorMoved) {
                                      _lastRemoteCursorMoved = false;
                                      _firstEnterImage.value = true;
                                    }
                                    return _buildCustomCursor(
                                        context, getCursorScale());
                                  }
                                }())
                              : _buildDisabledCursor(context, getCursorScale())
                  : MouseCursor.defer,
              onHover: (evt) {},
              child: child);
        });
    if (c.imageOverflow.isTrue && c.scrollStyle != ScrollStyle.scrollauto) {
      final paintWidth = c.getDisplayWidth() * s;
      final paintHeight = c.getDisplayHeight() * s;
      final paintSize = Size(paintWidth, paintHeight);
      final paintWidget =
          m.useTextureRender || widget.ffi.ffiModel.pi.forceTextureRender
              ? _BuildPaintTextureRender(
                  c, s, Offset.zero, paintSize, isViewOriginal())
              : _buildScrollbarNonTextureRender(m, paintSize, s);
      return NotificationListener<ScrollNotification>(
          onNotification: (notification) {
            c.updateScrollPercent();
            return false;
          },
          child: mouseRegion(
            child: Obx(() => _buildCrossScrollbarFromLayout(
                  context,
                  _buildListener(paintWidget),
                  c.size,
                  paintSize,
                  c.scrollHorizontal,
                  c.scrollVertical,
                )),
          ));
    } else {
      if (c.size.width > 0 && c.size.height > 0) {
        final paintWidget =
            m.useTextureRender || widget.ffi.ffiModel.pi.forceTextureRender
                ? _BuildPaintTextureRender(
                    c,
                    s,
                    Offset(
                      isLinux ? c.x.toInt().toDouble() : c.x,
                      isLinux ? c.y.toInt().toDouble() : c.y,
                    ),
                    c.size,
                    isViewOriginal())
                : _buildScrollAutoNonTextureRender(m, c, s);
        return mouseRegion(child: _buildListener(paintWidget));
      } else {
        return Container();
      }
    }
  }

  /// 把桌面光标与渲染后的图像及原生像素单位进行匹配。
  ///
  /// Windows 光标像素是物理像素；NSCursor/GdkCursor 使用逻辑像素。
  /// 未缩放的自适应/自定义视图保持 macOS 光标的点尺寸。
  double _getDesktopCursorScale(CanvasModel c, double dpr) {
    final peer = widget.ffi.ffiModel;
    if (!zoomCursor.value &&
        peer.pi.platform == kPeerPlatformMacOS &&
        (c.viewStyle.style == kRemoteViewStyleAdaptive ||
            c.viewStyle.style == kRemoteViewStyleCustom)) {
      return isWindows ? dpr : 1.0;
    }
    if (peer.isPeerLinux && peer.pi.currentDisplay == kAllDisplayValue) {
      if (!zoomCursor.value || c.viewStyle.style == kRemoteViewStyleOriginal) {
        // 去掉宿主输出的缩放密度，但不应用画布缩放。
        final scale = 1.0 / _cursorDisplayScale.value;
        return isWindows ? scale : scale / dpr;
      }
      final scale = c.scale / _cursorDisplayScale.value;
      return isWindows ? scale * dpr : scale;
    }
    if (!zoomCursor.value || c.viewStyle.style == kRemoteViewStyleOriginal) {
      // 原始视图和未缩放视图不应用画布缩放。
      final scale = _getCursorScaleForDisplay(1.0);
      return isWindows ? scale : scale / dpr;
    }
    final scale = _getCursorScaleForDisplay(c.scale);
    return isWindows ? scale * dpr : scale;
  }

  double _getCursorScaleForDisplay(double scale) {
    final peer = widget.ffi.ffiModel;
    // 所有显示器可以混合不同的密度，不存在统一的显示缩放比例。
    if (peer.pi.currentDisplay == kAllDisplayValue) return scale;
    final displays = peer.pi.getCurDisplays();
    if (displays.isEmpty) return scale;
    if (peer.pi.platform == kPeerPlatformMacOS) {
      // macOS 以点为单位发送 NSImage.size（src/platform/macos.rs:640,682），
      // 而 HiDPI 屏幕帧使用后备像素（libs/scrap/src/quartz/display.rs:37）。
      return scale * displays.first.scale;
    }
    if (!peer.isPeerLinux) return scale;
    // 当 Wayland 宿主
    // 有多路输出并报告显示缩放 > 1 时保持渲染一致。
    // 单输出宿主保持缩放为 1.0，以保留物理 uinput
    // 坐标，即使开启了系统缩放。这里统计的是宿主输出，
    // 而不是所选的 displays.length。
    // 参见 src/server/display_service.rs:650（update_sync_displays）和
    // src/server/drm_capturer.rs:1523（DRM 的对应约定）。
    return scale / displays.first.scale;
  }

  Widget _buildScrollbarNonTextureRender(
      ImageModel m, Size imageSize, double s) {
    double sizeScale = s;
    if (widget.ffi.ffiModel.isPeerLinux) {
      final displays = widget.ffi.ffiModel.pi.getCurDisplays();
      if (displays.isNotEmpty) {
        sizeScale = s / displays[0].scale;
      }
    }
    return CustomPaint(
      size: imageSize,
      painter: ImagePainter(image: m.image, x: 0, y: 0, scale: sizeScale),
    );
  }

  Widget _buildScrollAutoNonTextureRender(
      ImageModel m, CanvasModel c, double s) {
    double sizeScale = s;
    if (widget.ffi.ffiModel.isPeerLinux) {
      final displays = widget.ffi.ffiModel.pi.getCurDisplays();
      if (displays.isNotEmpty) {
        sizeScale = s / displays[0].scale;
      }
    }
    return CustomPaint(
      size: Size(c.size.width, c.size.height),
      painter: ImagePainter(
          image: m.image,
          x: c.x / sizeScale,
          y: c.y / sizeScale,
          scale: sizeScale),
    );
  }

  Widget _BuildPaintTextureRender(
      CanvasModel c, double s, Offset offset, Size size, bool isViewOriginal) {
    final ffiModel = c.parent.target!.ffiModel;
    final displays = ffiModel.pi.getCurDisplays();
    final children = <Widget>[];
    final rect = ffiModel.rect;
    if (rect == null) {
      return Container();
    }
    final isPeerLinux = ffiModel.isPeerLinux;
    final curDisplay = ffiModel.pi.currentDisplay;
    for (var i = 0; i < displays.length; i++) {
      final textureId = widget.ffi.textureModel
          .getTextureId(curDisplay == kAllDisplayValue ? i : curDisplay);
      if (true) {
        // "textureId.value != -1" 和 "true" 似乎都可以
        final sizeScale = isPeerLinux ? s / displays[i].scale : s;
        children.add(Positioned(
          left: (displays[i].x - rect.left) * s + offset.dx,
          top: (displays[i].y - rect.top) * s + offset.dy,
          width: displays[i].width * sizeScale,
          height: displays[i].height * sizeScale,
          child: _trackCursorDisplay(
            Obx(() => Texture(
                  textureId: textureId.value,
                  filterQuality:
                      isViewOriginal ? FilterQuality.none : FilterQuality.low,
                )),
            displays[i],
          ),
        ));
      }
    }
    return SizedBox(
      width: size.width,
      height: size.height,
      child: Stack(children: children),
    );
  }

  Widget _trackCursorDisplay(Widget child, Display display) {
    final peer = widget.ffi.ffiModel;
    if (!isDesktop ||
        !peer.isPeerLinux ||
        peer.pi.currentDisplay != kAllDisplayValue) {
      return child;
    }
    return MouseRegion(
      onEnter: (_) => _cursorDisplayScale.value = display.scale,
      onHover: (_) => _cursorDisplayScale.value = display.scale,
      child: child,
    );
  }

  MouseCursor _buildCustomCursor(BuildContext context, double scale) {
    final cursor = Provider.of<CursorModel>(context);
    final cache = cursor.cache ?? preDefaultCursor.cache;
    return buildCursorOfCache(cursor, scale, cache);
  }

  MouseCursor _buildDisabledCursor(BuildContext context, double scale) {
    final cursor = Provider.of<CursorModel>(context);
    final cache = preForbiddenCursor.cache;
    return buildCursorOfCache(cursor, scale, cache);
  }

  Widget _buildCrossScrollbarFromLayout(
    BuildContext context,
    Widget child,
    Size layoutSize,
    Size size,
    ScrollController horizontal,
    ScrollController vertical,
  ) {
    var widget = child;
    if (layoutSize.width < size.width) {
      widget = ScrollConfiguration(
        behavior: ScrollConfiguration.of(context).copyWith(scrollbars: false),
        child: SingleChildScrollView(
          controller: horizontal,
          scrollDirection: Axis.horizontal,
          physics: cursorOverImage.isTrue
              ? const NeverScrollableScrollPhysics()
              : null,
          child: widget,
        ),
      );
    } else {
      widget = Row(
        children: [
          Container(
            width: ((layoutSize.width - size.width) ~/ 2).toDouble(),
          ),
          widget,
        ],
      );
    }
    if (layoutSize.height < size.height) {
      widget = ScrollConfiguration(
        behavior: ScrollConfiguration.of(context).copyWith(scrollbars: false),
        child: SingleChildScrollView(
          controller: vertical,
          physics: cursorOverImage.isTrue
              ? const NeverScrollableScrollPhysics()
              : null,
          child: widget,
        ),
      );
    } else {
      widget = Column(
        children: [
          Container(
            height: ((layoutSize.height - size.height) ~/ 2).toDouble(),
          ),
          widget,
        ],
      );
    }
    if (layoutSize.width < size.width) {
      widget = RawScrollbar(
        thickness: kScrollbarThickness,
        thumbColor: Colors.grey,
        controller: horizontal,
        thumbVisibility: false,
        trackVisibility: false,
        notificationPredicate: layoutSize.height < size.height
            ? (notification) => notification.depth == 1
            : defaultScrollNotificationPredicate,
        child: widget,
      );
    }
    if (layoutSize.height < size.height) {
      widget = RawScrollbar(
        thickness: kScrollbarThickness,
        thumbColor: Colors.grey,
        controller: vertical,
        thumbVisibility: false,
        trackVisibility: false,
        child: widget,
      );
    }

    return Container(
      child: widget,
      width: layoutSize.width,
      height: layoutSize.height,
    );
  }

  Widget _buildListener(Widget child) {
    if (listenerBuilder != null) {
      return listenerBuilder!(child);
    } else {
      return child;
    }
  }
}

class CursorPaint extends StatelessWidget {
  final String id;
  final RxBool zoomCursor;

  const CursorPaint({
    Key? key,
    required this.id,
    required this.zoomCursor,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    final m = Provider.of<CursorModel>(context);
    final c = Provider.of<CanvasModel>(context);
    double hotx = m.hotx;
    double hoty = m.hoty;
    if (m.image == null) {
      if (preDefaultCursor.image != null) {
        hotx = preDefaultCursor.image!.width / 2;
        hoty = preDefaultCursor.image!.height / 2;
      }
    }

    double cx = c.x;
    double cy = c.y;
    if (c.viewStyle.style == kRemoteViewStyleOriginal &&
        c.scrollStyle == ScrollStyle.scrollbar) {
      final rect = c.parent.target!.ffiModel.rect;
      if (rect == null) {
        // 不可达！
        debugPrint('unreachable! The displays rect is null.');
        return Container();
      }
      if (cx < 0) {
        final imageWidth = rect.width * c.scale;
        cx = -imageWidth * c.scrollX;
      }
      if (cy < 0) {
        final imageHeight = rect.height * c.scale;
        cy = -imageHeight * c.scrollY;
      }
    }

    double x = (m.x - hotx) * c.scale + cx;
    double y = (m.y - hoty) * c.scale + cy;
    double scale = 1.0;
    final isViewOriginal = c.viewStyle.style == kRemoteViewStyleOriginal;
    if (zoomCursor.value || isViewOriginal) {
      x = m.x - hotx + cx / c.scale;
      y = m.y - hoty + cy / c.scale;
      scale = c.scale;
    }

    return CustomPaint(
      painter: ImagePainter(
        image: m.image ?? preDefaultCursor.image,
        x: x,
        y: y,
        scale: scale,
      ),
    );
  }
}
