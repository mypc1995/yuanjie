import 'dart:convert';
import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common/widgets/audio_input.dart';
import 'package:flutter_hbb/common/widgets/dialog.dart';
import 'package:flutter_hbb/common/widgets/keyboard_shortcuts/display.dart';
import 'package:flutter_hbb/common/widgets/toolbar.dart';
import 'package:flutter_hbb/models/chat_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:get/get.dart';
import 'package:provider/provider.dart';
import 'package:debounce_throttle/debounce_throttle.dart';
import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:window_size/window_size.dart' as window_size;

import '../../common.dart';
import '../../models/model.dart';
import '../../models/platform_model.dart';
import '../../common/shared_state.dart';
import './popup_menu.dart';
import './kb_layout_type_chooser.dart';
import 'package:flutter_hbb/utils/scale.dart';
import 'package:flutter_hbb/common/widgets/custom_scale_base.dart';

enum _ToolbarEdge { top, right, bottom, left }

_ToolbarEdge _parseToolbarEdge(String? s) {
  switch (s) {
    case 'right':
      return _ToolbarEdge.right;
    case 'bottom':
      return _ToolbarEdge.bottom;
    case 'left':
      return _ToolbarEdge.left;
    default:
      return _ToolbarEdge.top;
  }
}

String _toolbarEdgeToString(_ToolbarEdge e) {
  switch (e) {
    case _ToolbarEdge.top:
      return 'top';
    case _ToolbarEdge.right:
      return 'right';
    case _ToolbarEdge.bottom:
      return 'bottom';
    case _ToolbarEdge.left:
      return 'left';
  }
}

bool _isHorizontalEdge(_ToolbarEdge e) =>
    e == _ToolbarEdge.top || e == _ToolbarEdge.bottom;

const _legacyRemoteMenubarDragX = 'remote-menubar-drag-x';

double _clampToolbarFraction(double fraction, double left, double right) {
  if (fraction < left) fraction = left;
  if (fraction > right) fraction = right;
  return fraction;
}

Size _toolbarSizeForEdge(_ToolbarEdge edge, Size? measured) {
  final isHorizontal = _isHorizontalEdge(edge);
  final fallback = isHorizontal ? const Size(360, 40) : const Size(40, 360);
  final size = measured ?? fallback;
  final long = size.longestSide;
  final short = size.shortestSide;
  return Size(isHorizontal ? long : short, isHorizontal ? short : long);
}

Offset _toolbarOffsetForEdge({
  required _ToolbarEdge edge,
  required double fraction,
  required Size parentSize,
  required Size toolbarSize,
}) {
  final xTravel = parentSize.width - toolbarSize.width;
  final yTravel = parentSize.height - toolbarSize.height;
  switch (edge) {
    case _ToolbarEdge.top:
      return Offset(xTravel * fraction, 0);
    case _ToolbarEdge.bottom:
      return Offset(xTravel * fraction, yTravel);
    case _ToolbarEdge.left:
      return Offset(0, yTravel * fraction);
    case _ToolbarEdge.right:
      return Offset(xTravel, yTravel * fraction);
  }
}

double _fractionForAlignedDrag({
  required double cursor,
  required double grabOffset,
  required double parentExtent,
  required double toolbarExtent,
  required double left,
  required double right,
}) {
  final travelExtent = parentExtent - toolbarExtent;
  if (travelExtent <= 0) {
    return _clampToolbarFraction(0.5, left, right);
  }
  return _clampToolbarFraction(
      (cursor - grabOffset) / travelExtent, left, right);
}

({double left, double right}) _fractionBoundsForEdge(
  _ToolbarEdge edge,
  double left,
  double right,
) {
  return _isHorizontalEdge(edge)
      ? (left: left, right: right)
      : (left: 0, right: 1);
}

String _toolbarRawFraction({
  required bool multiEdgeEnabled,
  required _ToolbarEdge edge,
  required String? savedFraction,
  required String? legacyFraction,
}) {
  if (!multiEdgeEnabled) {
    return (legacyFraction != null && legacyFraction.isNotEmpty)
        ? legacyFraction
        : '0.5';
  }
  if (savedFraction != null && savedFraction.isNotEmpty) {
    return savedFraction;
  }
  if (edge == _ToolbarEdge.top &&
      legacyFraction != null &&
      legacyFraction.isNotEmpty) {
    return legacyFraction;
  }
  return '0.5';
}

// 返回用于包裹的 Align 的对齐方式，使整个
// 工具栏按给定的比例贴靠在给定的边缘上。
// Alignment 使用 [-1, 1] 坐标（0 = 居中）。
Alignment _alignmentForEdge(_ToolbarEdge edge, double fraction) {
  final f = fraction * 2 - 1;
  switch (edge) {
    case _ToolbarEdge.top:
      return Alignment(f, -1);
    case _ToolbarEdge.bottom:
      return Alignment(f, 1);
    case _ToolbarEdge.left:
      return Alignment(-1, f);
    case _ToolbarEdge.right:
      return Alignment(1, f);
  }
}

// 拖拽手柄悬挂在工具栏背离
// 停靠边缘的一侧，因此图标本身紧贴该边缘。
BorderRadius _collapseHandleBorderRadius(_ToolbarEdge edge) {
  const r = Radius.circular(5);
  switch (edge) {
    case _ToolbarEdge.top:
      return const BorderRadius.vertical(bottom: r);
    case _ToolbarEdge.bottom:
      return const BorderRadius.vertical(top: r);
    case _ToolbarEdge.left:
      return const BorderRadius.horizontal(right: r);
    case _ToolbarEdge.right:
      return const BorderRadius.horizontal(left: r);
  }
}

int _monitorMenuQuarterTurns(_ToolbarEdge edge) {
  switch (edge) {
    case _ToolbarEdge.left:
      return 1;
    case _ToolbarEdge.right:
      return 3;
    case _ToolbarEdge.top:
    case _ToolbarEdge.bottom:
      return 0;
  }
}

IconData _toolbarCollapseIcon(_ToolbarEdge edge, bool isCollapsed) {
  switch (edge) {
    case _ToolbarEdge.top:
      return isCollapsed ? Icons.expand_more : Icons.expand_less;
    case _ToolbarEdge.bottom:
      return isCollapsed ? Icons.expand_less : Icons.expand_more;
    case _ToolbarEdge.left:
      return isCollapsed ? Icons.chevron_right : Icons.chevron_left;
    case _ToolbarEdge.right:
      return isCollapsed ? Icons.chevron_left : Icons.chevron_right;
  }
}

class _ToolbarDockingOptions {
  _ToolbarDockingOptions({
    required this.edge,
    required this.fraction,
    required this.multiEdgeEnabled,
  });

  _ToolbarEdge edge;
  double fraction;
  bool multiEdgeEnabled;
}

final _toolbarDockingOptionsBySession = <String, _ToolbarDockingOptions>{};

String _toolbarDockingCacheKey(SessionID sessionId) => sessionId.toString();

_ToolbarDockingOptions? _cachedToolbarDockingOptions(SessionID sessionId) =>
    _toolbarDockingOptionsBySession[_toolbarDockingCacheKey(sessionId)];

void _cacheToolbarDockingOptions({
  required SessionID sessionId,
  required _ToolbarEdge edge,
  required double fraction,
  required bool multiEdgeEnabled,
}) {
  final key = _toolbarDockingCacheKey(sessionId);
  final cached = _toolbarDockingOptionsBySession[key];
  if (cached == null) {
    _toolbarDockingOptionsBySession[key] = _ToolbarDockingOptions(
      edge: edge,
      fraction: fraction,
      multiEdgeEnabled: multiEdgeEnabled,
    );
    return;
  }
  cached.edge = edge;
  cached.fraction = fraction;
  cached.multiEdgeEnabled = multiEdgeEnabled;
}

class ToolbarState {
  late RxBool _pin;

  RxBool collapse = false.obs;
  RxBool hide = false.obs;

  // 跟踪初始化状态以防止闪烁
  final RxBool initialized = false.obs;
  bool _isInitializing = false;

  ToolbarState() {
    _pin = RxBool(false);
    final s = bind.getLocalFlutterOption(k: kOptionRemoteMenubarState);
    if (s.isEmpty) {
      return;
    }

    try {
      final m = jsonDecode(s);
      if (m != null) {
        _pin = RxBool(m['pin'] ?? false);
      }
    } catch (e) {
      debugPrint('Failed to decode toolbar state ${e.toString()}');
    }
  }

  bool get pin => _pin.value;

  /// 从会话选项初始化所有工具栏状态。
  /// 这应在工具栏首次创建时调用一次。
  Future<void> init(SessionID sessionId) async {
    if (initialized.value || _isInitializing) return;
    _isInitializing = true;

    try {
      // 并行加载两种状态以提升性能
      final results = await Future.wait([
        bind.sessionGetToggleOption(
            sessionId: sessionId, arg: kOptionCollapseToolbar),
        bind.sessionGetToggleOption(
            sessionId: sessionId, arg: kOptionHideToolbar),
      ]);

      collapse.value = results[0] ?? false;
      hide.value = results[1] ?? false;
    } finally {
      _isInitializing = false;
      initialized.value = true;
    }
  }

  switchCollapse(SessionID sessionId) async {
    bind.sessionToggleOption(
        sessionId: sessionId, value: kOptionCollapseToolbar);
    collapse.value = !collapse.value;
  }

  // 切换隐藏状态以控制整个工具栏的可见性
  switchHide(SessionID sessionId) async {
    bind.sessionToggleOption(sessionId: sessionId, value: kOptionHideToolbar);
    hide.value = !hide.value;
  }

  switchPin() async {
    _pin.value = !_pin.value;
    // 每次变化都保存，因为此函数不会被频繁调用
    await _savePin();
  }

  setPin(bool v) async {
    if (_pin.value != v) {
      _pin.value = v;
      // 每次变化都保存，因为此函数不会被频繁调用
      await _savePin();
    }
  }

  _savePin() async {
    bind.setLocalFlutterOption(
        k: kOptionRemoteMenubarState, v: jsonEncode({'pin': _pin.value}));
  }
}

class _ToolbarTheme {
  static const Color blueColor = MyTheme.button;
  static const Color hoverBlueColor = MyTheme.accent;
  static Color inactiveColor = Colors.grey[800]!;
  static Color hoverInactiveColor = Colors.grey[850]!;

  static const Color redColor = Colors.redAccent;
  static const Color hoverRedColor = Colors.red;
  // kMinInteractiveDimension
  static const double height = 20.0;
  static const double dividerHeight = 12.0;

  static const double buttonSize = 32;
  static const double buttonHMargin = 2;
  static const double buttonVMargin = 6;
  static const double iconRadius = 8;
  static const double elevation = 3;

  static double dividerSpaceToAction = isWindows ? 8 : 14;

  static double menuBorderRadius = isWindows ? 5.0 : 7.0;
  static EdgeInsets menuPadding = isWindows
      ? EdgeInsets.fromLTRB(4, 12, 4, 12)
      : EdgeInsets.fromLTRB(6, 14, 6, 14);
  static const double menuButtonBorderRadius = 3.0;

  static Color borderColor(BuildContext context) =>
      MyTheme.color(context).border3 ?? MyTheme.border;

  static Color? dividerColor(BuildContext context) =>
      MyTheme.color(context).divider;

  static MenuStyle defaultMenuStyle(BuildContext context) => MenuStyle(
        side: MaterialStateProperty.all(BorderSide(
          width: 1,
          color: borderColor(context),
        )),
        shape: MaterialStatePropertyAll(RoundedRectangleBorder(
            borderRadius:
                BorderRadius.circular(_ToolbarTheme.menuBorderRadius))),
        padding: MaterialStateProperty.all(_ToolbarTheme.menuPadding),
      );
  static final defaultMenuButtonStyle = ButtonStyle(
    backgroundColor: MaterialStatePropertyAll(Colors.transparent),
    padding: MaterialStatePropertyAll(EdgeInsets.zero),
    overlayColor: MaterialStatePropertyAll(Colors.transparent),
  );

  static Widget borderWrapper(
      BuildContext context, Widget child, BorderRadius borderRadius) {
    return Container(
      decoration: BoxDecoration(
        border: Border.all(
          color: borderColor(context),
          width: 1,
        ),
        borderRadius: borderRadius,
      ),
      child: child,
    );
  }
}

typedef DismissFunc = void Function();

class RemoteMenuEntry {
  static MenuEntryButton<String> insertLock(
    SessionID sessionId,
    EdgeInsets? padding, {
    DismissFunc? dismissFunc,
    DismissCallback? dismissCallback,
  }) {
    return MenuEntryButton<String>(
      childBuilder: (TextStyle? style) => Text(
        translate('Insert Lock'),
        style: style,
      ),
      proc: () {
        bind.sessionLockScreen(sessionId: sessionId);
        if (dismissFunc != null) {
          dismissFunc();
        }
      },
      padding: padding,
      dismissOnClicked: true,
      dismissCallback: dismissCallback,
    );
  }

  static insertCtrlAltDel(
    SessionID sessionId,
    EdgeInsets? padding, {
    DismissFunc? dismissFunc,
    DismissCallback? dismissCallback,
  }) {
    return MenuEntryButton<String>(
      childBuilder: (TextStyle? style) => Text(
        translate("Insert Ctrl + Alt + Del"),
        style: style,
      ),
      proc: () {
        bind.sessionCtrlAltDel(sessionId: sessionId);
        if (dismissFunc != null) {
          dismissFunc();
        }
      },
      padding: padding,
      dismissOnClicked: true,
      dismissCallback: dismissCallback,
    );
  }
}

class RemoteToolbar extends StatefulWidget {
  final String id;
  final FFI ffi;
  final ToolbarState state;
  final Function(int, Function(bool)) onEnterOrLeaveImageSetter;
  final Function(int) onEnterOrLeaveImageCleaner;
  final Function(VoidCallback) setRemoteState;

  RemoteToolbar({
    Key? key,
    required this.id,
    required this.ffi,
    required this.state,
    required this.onEnterOrLeaveImageSetter,
    required this.onEnterOrLeaveImageCleaner,
    required this.setRemoteState,
  }) : super(key: key);

  @override
  State<RemoteToolbar> createState() => _RemoteToolbarState();
}

class _RemoteToolbarState extends State<RemoteToolbar> {
  late Debouncer<int> _debouncerHide;
  bool _isCursorOverImage = false;
  final _fraction = 0.5.obs;
  final _edge = _ToolbarEdge.top.obs;
  final _dragging = false.obs;
  // 实时拖拽预览：如果用户现在松手，工具栏会停靠在哪里。
  final _previewEdge = Rxn<_ToolbarEdge>();
  final _previewFraction = Rxn<double>();
  // 实时工具栏的测量尺寸，使预览虚影与实际情况一致
  // （折叠手柄与展开工具栏）。每次布局后更新。
  final _toolbarSize = Rxn<Size>();
  final _toolbarKey = GlobalKey(debugLabel: 'remote_toolbar_root');
  // 为 false（默认）时，工具栏停留在顶部边缘，拖拽
  // 手柄只是让它水平滑动——保持长期以来的使用体验，同时
  // 也修复了拖拽只移动手柄的 bug。为 true 时，
  // 用户选择了启用多边缘停靠，并吸附到最近的边缘。
  // 在设置引发的重建后保持同步。
  final _multiEdgeEnabled = false.obs;
  final _dockingOptionsInitialized = false.obs;
  bool _pendingDockingOptionSync = false;
  int _dockingOptionSyncSerial = 0;
  int _dragEpoch = 0;

  int get windowId => stateGlobal.windowId;

  void _setFullscreen(bool v) {
    stateGlobal.setFullscreen(v);
    // stateGlobal.fullscreen 现在是 RxBool，无需调用 setState。
    // setState(() {});
  }

  RxBool get collapse => widget.state.collapse;
  RxBool get hide => widget.state.hide;
  bool get pin => widget.state.pin;

  PeerInfo get pi => widget.ffi.ffiModel.pi;
  FfiModel get ffiModel => widget.ffi.ffiModel;

  triggerAutoHide() => _debouncerHide.value = _debouncerHide.value + 1;

  void _minimize() async =>
      await WindowController.fromWindowId(windowId).minimize();

  Future<void> _syncDockingOptions({required bool force}) async {
    final syncSerial = ++_dockingOptionSyncSerial;
    if (_dragging.isTrue) {
      _deferDockingOptionsSync();
      return;
    }
    final dragEpoch = _dragEpoch;

    // 使用规范的辅助方法，以便选项文档中说明的默认语义
    // 得以生效（allow-* 前缀 => 默认为 false）。若保持为原始字符串就会
    // 与 _OptionCheckBox 显示同一个 key 的方式不一致。
    final multiEdgeEnabled =
        mainGetLocalBoolOptionSync(kOptionAllowMultiEdgeToolbarDock);
    final cached = _cachedToolbarDockingOptions(widget.ffi.sessionId);
    if (cached == null && pi.isSet.isFalse) {
      return;
    }
    final hadDockingOptions = cached != null;
    final wasMultiEdgeEnabled =
        cached?.multiEdgeEnabled ?? _multiEdgeEnabled.value;
    if (!force &&
        hadDockingOptions &&
        wasMultiEdgeEnabled == multiEdgeEnabled) {
      _pendingDockingOptionSync = false;
      return;
    }

    final savedFraction = await bind.sessionGetOption(
        sessionId: widget.ffi.sessionId, arg: kOptionRemoteMenubarFraction);
    // 向后兼容：旧版仅水平定位。
    final legacyFraction = await bind.sessionGetOption(
        sessionId: widget.ffi.sessionId, arg: _legacyRemoteMenubarDragX);
    if (!mounted || syncSerial != _dockingOptionSyncSerial) return;

    var nextEdge = _edge.value;
    var savedFractionForNextEdge = savedFraction;
    var keepCurrentPosition = false;
    if (!multiEdgeEnabled) {
      nextEdge = _ToolbarEdge.top;
    } else if (force || wasMultiEdgeEnabled || cached == null) {
      final edgeStr = await bind.sessionGetOption(
          sessionId: widget.ffi.sessionId, arg: kOptionRemoteMenubarEdge);
      if (!mounted || syncSerial != _dockingOptionSyncSerial) return;
      nextEdge = _parseToolbarEdge(edgeStr);
    } else {
      // 该设置从仅顶部改为多边缘时，这个工具栏已经
      // 可见。保持它当前的位置，而不是跳到
      // 上次保存的多边缘停靠位置。
      nextEdge = cached.edge;
      savedFractionForNextEdge = cached.fraction.toString();
      keepCurrentPosition = true;
    }

    final rawFraction = _toolbarRawFraction(
      multiEdgeEnabled: multiEdgeEnabled,
      edge: nextEdge,
      savedFraction: savedFractionForNextEdge,
      legacyFraction: legacyFraction,
    );
    // 按已保存的拖拽边界约定做钳制，使损坏或超出范围的
    // 保存值无法绕过它，直到用户再次拖拽。
    final dragLeft = double.tryParse(
            bind.mainGetLocalOption(key: kOptionRemoteMenubarDragLeft)) ??
        0.0;
    final dragRight = double.tryParse(
            bind.mainGetLocalOption(key: kOptionRemoteMenubarDragRight)) ??
        1.0;
    final fractionBounds =
        _fractionBoundsForEdge(nextEdge, dragLeft, dragRight);
    final nextFraction = (double.tryParse(rawFraction) ?? 0.5)
        .clamp(fractionBounds.left, fractionBounds.right)
        .toDouble();
    if (!mounted || syncSerial != _dockingOptionSyncSerial) return;
    if (_dragging.isTrue || dragEpoch != _dragEpoch) {
      _deferDockingOptionsSync();
      return;
    }
    _edge.value = nextEdge;
    _fraction.value = nextFraction;
    _multiEdgeEnabled.value = multiEdgeEnabled;
    _dockingOptionsInitialized.value = true;
    _cacheToolbarDockingOptions(
      sessionId: widget.ffi.sessionId,
      edge: nextEdge,
      fraction: nextFraction,
      multiEdgeEnabled: multiEdgeEnabled,
    );
    _pendingDockingOptionSync = false;
    if (!multiEdgeEnabled || keepCurrentPosition) {
      bind.sessionPeerOption(
        sessionId: widget.ffi.sessionId,
        name: kOptionRemoteMenubarEdge,
        value: _toolbarEdgeToString(nextEdge),
      );
      bind.sessionPeerOption(
        sessionId: widget.ffi.sessionId,
        name: kOptionRemoteMenubarFraction,
        value: nextFraction.toString(),
      );
    }
  }

  void _deferDockingOptionsSync() {
    _pendingDockingOptionSync = true;
    if (_dragging.isFalse) {
      _syncDockingOptionsAfterDragIfNeeded();
    }
  }

  void _markToolbarDragEpoch() {
    ++_dragEpoch;
  }

  void _syncDockingOptionsAfterDragIfNeeded() {
    if (!_pendingDockingOptionSync) return;
    WidgetsBinding.instance.addPostFrameCallback((_) async {
      await _syncDockingOptions(force: false);
    });
  }

  @override
  initState() {
    super.initState();

    final cached = _cachedToolbarDockingOptions(widget.ffi.sessionId);
    final multiEdgeEnabled =
        mainGetLocalBoolOptionSync(kOptionAllowMultiEdgeToolbarDock);
    final shouldResetToTop =
        cached != null && cached.multiEdgeEnabled && !multiEdgeEnabled;
    if (cached != null && !shouldResetToTop) {
      _edge.value = cached.edge;
      _fraction.value = cached.fraction;
      _multiEdgeEnabled.value = multiEdgeEnabled;
      _dockingOptionsInitialized.value = true;
    }

    WidgetsBinding.instance.addPostFrameCallback((_) async {
      await _syncDockingOptions(force: cached == null || shouldResetToTop);
      // 从会话选项初始化工具栏状态（折叠、隐藏）
      widget.state.init(widget.ffi.sessionId);
    });

    _debouncerHide = Debouncer<int>(
      Duration(milliseconds: 5000),
      onChanged: _debouncerHideProc,
      initialValue: 0,
    );

    widget.onEnterOrLeaveImageSetter(identityHashCode(this), (enter) {
      if (enter) {
        triggerAutoHide();
        _isCursorOverImage = true;
      } else {
        _isCursorOverImage = false;
      }
    });
  }

  @override
  void didUpdateWidget(covariant RemoteToolbar oldWidget) {
    super.didUpdateWidget(oldWidget);
    WidgetsBinding.instance.addPostFrameCallback((_) async {
      await _syncDockingOptions(force: false);
    });
  }

  _debouncerHideProc(int v) {
    if (!pin && collapse.isFalse && _isCursorOverImage && _dragging.isFalse) {
      collapse.value = true;
    }
  }

  @override
  dispose() {
    ++_dockingOptionSyncSerial;
    widget.onEnterOrLeaveImageCleaner(identityHashCode(this));
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Obx(() {
      // 等待初始化完成以防止闪烁
      if (!widget.state.initialized.value ||
          !_dockingOptionsInitialized.value) {
        return const SizedBox.shrink();
      }
      // 如果工具栏已隐藏，返回空组件
      if (hide.value) {
        return const SizedBox.shrink();
      }
      final edge = _edge.value;
      final isHorizontal = _isHorizontalEdge(edge);

      // 每次布局后测量实时工具栏，使预览虚影能够
      // 与其实际占位一致（折叠手柄与展开工具栏）。
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (_dragging.isTrue) return;
        final ro = _toolbarKey.currentContext?.findRenderObject();
        if (ro is RenderBox && ro.hasSize) {
          final s = ro.size;
          if (_toolbarSize.value != s) _toolbarSize.value = s;
        }
      });

      final toolbar = Align(
        alignment: _alignmentForEdge(edge, _fraction.value),
        child: KeyedSubtree(
          key: _toolbarKey,
          child: collapse.isFalse
              ? _buildToolbar(context, edge, isHorizontal)
              : _buildDraggableCollapse(context, edge, isHorizontal),
        ),
      );

      // 始终返回 Stack——即使没有在拖拽——使工具栏的
      // 在 Element 树中的位置保持稳定。拖拽过程中对其
      // 进行包裹/解包会破坏 Draggable 的手势状态。
      return Stack(
        fit: StackFit.expand,
        children: [
          IgnorePointer(
            child: Obx(() {
              final pe = _previewEdge.value;
              final pf = _previewFraction.value;
              if (!_dragging.isTrue || pe == null || pf == null) {
                return const SizedBox.shrink();
              }
              return _buildDragPreview(context, pe, pf, _toolbarSize.value);
            }),
          ),
          toolbar,
        ],
      );
    });
  }

  Widget _buildDragPreview(BuildContext context, _ToolbarEdge edge,
      double fraction, Size? measured) {
    final color = Theme.of(context).colorScheme.primary;
    // 使用测量得到的实时工具栏尺寸，使折叠与展开的效果
    // 正确。当前方向可能与预览方向不同
    // （例如把停靠在顶部的工具栏拖向左边缘），因此在预览其他方向时
    // 需要交换长轴/短轴。
    final previewSize = _toolbarSizeForEdge(edge, measured);
    return Align(
      alignment: _alignmentForEdge(edge, fraction),
      child: Container(
        width: previewSize.width,
        height: previewSize.height,
        decoration: BoxDecoration(
          color: color.withOpacity(0.10),
          borderRadius: BorderRadius.circular(6),
          border: Border.all(color: color.withOpacity(0.55), width: 1.5),
        ),
      ),
    );
  }

  Widget _buildDraggableCollapse(
      BuildContext context, _ToolbarEdge edge, bool isHorizontal) {
    return Obx(() {
      if (collapse.isFalse && _dragging.isFalse) {
        triggerAutoHide();
      }
      final borderRadius = _collapseHandleBorderRadius(edge);
      return Offstage(
        offstage: _dragging.isTrue,
        child: Material(
          elevation: _ToolbarTheme.elevation,
          shadowColor: MyTheme.color(context).shadow,
          borderRadius: borderRadius,
          child: _DraggableShowHide(
            id: widget.id,
            ffi: widget.ffi,
            sessionId: widget.ffi.sessionId,
            dragging: _dragging,
            fraction: _fraction,
            edge: _edge,
            previewEdge: _previewEdge,
            previewFraction: _previewFraction,
            toolbarSize: _toolbarSize,
            markDragEpoch: _markToolbarDragEpoch,
            syncDockingOptionsAfterDragIfNeeded:
                _syncDockingOptionsAfterDragIfNeeded,
            isHorizontal: isHorizontal,
            multiEdgeEnabled: _multiEdgeEnabled.value,
            toolbarState: widget.state,
            setFullscreen: _setFullscreen,
            setMinimize: _minimize,
            borderRadius: borderRadius,
          ),
        ),
      );
    });
  }

  Widget _buildToolbar(
      BuildContext context, _ToolbarEdge edge, bool isHorizontal) {
    final List<Widget> toolbarItems = [];
    toolbarItems.add(_PinMenu(state: widget.state));
    toolbarItems.add(Obx(() {
      final privacyModeState = PrivacyModeState.find(widget.id);
      if ((privacyModeState.isEmpty ||
              allowDisplaySwitchInPrivacyMode(pi, privacyModeState.value)) &&
          pi.displaysCount.value > 1 &&
          mainGetLocalBoolOptionSync(kOptionAllowMonitorSwitchMainToolbar)) {
        return _MainMonitorSwitchButton(id: widget.id, ffi: widget.ffi);
      } else {
        return const Offstage();
      }
    }));
    if (!isWebDesktop) {
      toolbarItems.add(_MobileActionMenu(ffi: widget.ffi));
    }

    toolbarItems.add(Obx(() {
      final privacyModeState = PrivacyModeState.find(widget.id);
      if ((privacyModeState.isEmpty ||
              allowDisplaySwitchInPrivacyMode(pi, privacyModeState.value)) &&
          pi.displaysCount.value > 1) {
        return _MonitorMenu(
            id: widget.id,
            ffi: widget.ffi,
            edge: edge,
            setRemoteState: widget.setRemoteState);
      } else {
        return Offstage();
      }
    }));

    toolbarItems
        .add(_ControlMenu(id: widget.id, ffi: widget.ffi, state: widget.state));
    toolbarItems.add(_DisplayMenu(
      id: widget.id,
      ffi: widget.ffi,
      state: widget.state,
      setFullscreen: _setFullscreen,
    ));
    // 摄像头连接类型不显示键盘。
    if (widget.ffi.connType == ConnType.defaultConn) {
      toolbarItems.add(_KeyboardMenu(id: widget.id, ffi: widget.ffi));
    }
    toolbarItems.add(_ChatMenu(id: widget.id, ffi: widget.ffi));
    if (!isWeb) {
      toolbarItems.add(_VoiceCallMenu(id: widget.id, ffi: widget.ffi));
    }
    if (!isWeb) toolbarItems.add(_RecordMenu());
    toolbarItems.add(_CloseMenu(id: widget.id, ffi: widget.ffi));
    final toolbarBorderRadius = BorderRadius.all(Radius.circular(4.0));
    // innerAxis：工具栏图标自身的排列方向。
    // outerAxis：工具栏整体与手柄相互堆叠的方式
    // （与停靠边缘垂直，因此手柄悬挂在内侧面上）。
    final innerAxis = isHorizontal ? Axis.horizontal : Axis.vertical;
    final outerAxis = isHorizontal ? Axis.vertical : Axis.horizontal;
    final spacer = isHorizontal
        ? SizedBox(width: _ToolbarTheme.buttonHMargin * 2)
        : SizedBox(height: _ToolbarTheme.buttonHMargin * 2);
    final toolbarMaterial = Material(
      elevation: _ToolbarTheme.elevation,
      shadowColor: MyTheme.color(context).shadow,
      borderRadius: toolbarBorderRadius,
      color: Theme.of(context)
          .menuBarTheme
          .style
          ?.backgroundColor
          ?.resolve(MaterialState.values.toSet()),
      child: SingleChildScrollView(
        scrollDirection: innerAxis,
        child: Theme(
          data: themeData(),
          child: _ToolbarTheme.borderWrapper(
              context,
              Flex(
                direction: innerAxis,
                mainAxisSize: MainAxisSize.min,
                children: [
                  spacer,
                  ...toolbarItems,
                  spacer,
                ],
              ),
              toolbarBorderRadius),
        ),
      ),
    );
    final handle = _buildDraggableCollapse(context, edge, isHorizontal);
    // 手柄悬挂在工具栏的内侧面上（背离
    // 停靠边缘的一侧），并由 Flex 默认的交叉轴对齐
    // 沿该面居中，因此图标本身紧贴停靠边缘。
    final children = (edge == _ToolbarEdge.top || edge == _ToolbarEdge.left)
        ? [toolbarMaterial, handle]
        : [handle, toolbarMaterial];
    return Flex(
      direction: outerAxis,
      mainAxisSize: MainAxisSize.min,
      children: children,
    );
  }

  ThemeData themeData() {
    return Theme.of(context).copyWith(
      menuButtonTheme: MenuButtonThemeData(
        style: ButtonStyle(
          minimumSize: MaterialStatePropertyAll(Size(64, 32)),
          textStyle: MaterialStatePropertyAll(
            TextStyle(fontWeight: FontWeight.normal),
          ),
          shape: MaterialStatePropertyAll(RoundedRectangleBorder(
              borderRadius:
                  BorderRadius.circular(_ToolbarTheme.menuButtonBorderRadius))),
        ),
      ),
      dividerTheme: DividerThemeData(
        space: _ToolbarTheme.dividerSpaceToAction,
        color: _ToolbarTheme.dividerColor(context),
      ),
      menuBarTheme: MenuBarThemeData(
          style: MenuStyle(
        padding: MaterialStatePropertyAll(EdgeInsets.zero),
        elevation: MaterialStatePropertyAll(0),
        shape: MaterialStatePropertyAll(BeveledRectangleBorder()),
      ).copyWith(
              backgroundColor:
                  Theme.of(context).menuBarTheme.style?.backgroundColor)),
    );
  }
}

class _PinMenu extends StatelessWidget {
  final ToolbarState state;
  const _PinMenu({Key? key, required this.state}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return Obx(
      () => _IconMenuButton(
        assetName: state.pin ? "assets/pinned.svg" : "assets/unpinned.svg",
        tooltip: state.pin ? 'Unpin Toolbar' : 'Pin Toolbar',
        onPressed: state.switchPin,
        color:
            state.pin ? _ToolbarTheme.blueColor : _ToolbarTheme.inactiveColor,
        hoverColor: state.pin
            ? _ToolbarTheme.hoverBlueColor
            : _ToolbarTheme.hoverInactiveColor,
      ),
    );
  }
}

class _MobileActionMenu extends StatelessWidget {
  final FFI ffi;
  const _MobileActionMenu({Key? key, required this.ffi}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    if (!ffi.ffiModel.isPeerAndroid) return Offstage();
    return Obx(() => _IconMenuButton(
          assetName: 'assets/actions_mobile.svg',
          tooltip: 'Mobile Actions',
          onPressed: () => ffi.dialogManager.setMobileActionsOverlayVisible(
              !ffi.dialogManager.mobileActionsOverlayVisible.value),
          color: ffi.dialogManager.mobileActionsOverlayVisible.isTrue
              ? _ToolbarTheme.blueColor
              : _ToolbarTheme.inactiveColor,
          hoverColor: ffi.dialogManager.mobileActionsOverlayVisible.isTrue
              ? _ToolbarTheme.hoverBlueColor
              : _ToolbarTheme.hoverInactiveColor,
        ));
  }
}

class _MonitorCycle {
  final String id;
  final FFI ffi;
  const _MonitorCycle(this.id, this.ffi);

  PeerInfo get _pi => ffi.ffiModel.pi;
  int get total => _pi.displays.length;
  int get _current => CurrentDisplayState.find(id).value;
  bool get _inRange => _current >= 0 && _current < total;

  String get label => _inRange ? '${_current + 1}' : '*';
  String get tooltip => '${translate('Switch display')} ($label/$total)';

  void next() {
    final t = total;
    if (t < 2) return;
    final from = _inRange ? _current : -1;
    final target = (from + 1) % t;
    final isChooseDisplayToOpenInNewWindow = _pi.isSupportMultiDisplay &&
        bind.sessionGetDisplaysAsIndividualWindows(sessionId: ffi.sessionId) ==
            'Y';
    if (isChooseDisplayToOpenInNewWindow) {
      openMonitorInNewTabOrWindow(target, ffi.id, _pi);
    } else {
      openMonitorInTheSameTab(target, ffi, _pi, updateCursorPos: false);
    }
  }
}

class _MainMonitorSwitchButton extends StatelessWidget {
  final String id;
  final FFI ffi;

  const _MainMonitorSwitchButton({
    Key? key,
    required this.id,
    required this.ffi,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    final cycle = _MonitorCycle(id, ffi);
    return Obx(() {
      if (cycle.total < 2) return const Offstage();
      final label = cycle.label;

      return _IconMenuButton(
        tooltip: cycle.tooltip,
        color: _ToolbarTheme.blueColor,
        hoverColor: _ToolbarTheme.hoverBlueColor,
        onPressed: cycle.next,
        icon: SizedBox(
          width: _ToolbarTheme.buttonSize,
          height: _ToolbarTheme.buttonSize,
          child: Stack(
            alignment: const Alignment(0, -0.125),
            children: [
              SvgPicture.asset(
                'assets/display_switcher.svg',
                colorFilter:
                    const ColorFilter.mode(Colors.white, BlendMode.srcIn),
                width: _ToolbarTheme.buttonSize,
                height: _ToolbarTheme.buttonSize,
              ),
              Text(
                label,
                textAlign: TextAlign.center,
                style: const TextStyle(
                  color: Colors.black,
                  fontSize: 11,
                  height: 1,
                  fontWeight: FontWeight.bold,
                ),
              ),
            ],
          ),
        ),
      );
    });
  }
}

class _MonitorMenu extends StatelessWidget {
  final String id;
  final FFI ffi;
  final _ToolbarEdge edge;
  final Function(VoidCallback) setRemoteState;
  const _MonitorMenu({
    Key? key,
    required this.id,
    required this.ffi,
    required this.edge,
    required this.setRemoteState,
  }) : super(key: key);

  bool get showMonitorsToolbar =>
      bind.mainGetUserDefaultOption(key: kKeyShowMonitorsToolbar) == 'Y';

  bool get supportIndividualWindows =>
      !isWeb && ffi.ffiModel.pi.isSupportMultiDisplay;

  @override
  Widget build(BuildContext context) {
    final child = showMonitorsToolbar
        ? buildMultiMonitorMenu(context)
        : Obx(() => buildMonitorMenu(context));
    final quarterTurns = _monitorMenuQuarterTurns(edge);
    if (quarterTurns == 0) return child;
    return RotatedBox(
      quarterTurns: quarterTurns,
      child: child,
    );
  }

  Widget buildMonitorMenu(BuildContext context) {
    final width = SimpleWrapper<double>(0);
    final monitorsIcon =
        globalMonitorsWidget(width, Colors.white, Colors.black38);
    return _IconSubmenuButton(
        tooltip: 'Select Monitor',
        icon: monitorsIcon,
        ffi: ffi,
        width: width.value,
        color: _ToolbarTheme.blueColor,
        hoverColor: _ToolbarTheme.hoverBlueColor,
        menuStyle: MenuStyle(
            padding:
                MaterialStatePropertyAll(EdgeInsets.symmetric(horizontal: 6))),
        menuChildrenGetter: (_) => [buildMonitorSubmenuWidget(context)]);
  }

  Widget buildMultiMonitorMenu(BuildContext context) {
    return Row(children: buildMonitorList(context, true));
  }

  Widget buildMonitorSubmenuWidget(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Row(children: buildMonitorList(context, false)),
        supportIndividualWindows ? Divider() : Offstage(),
        supportIndividualWindows ? chooseDisplayBehavior() : Offstage(),
      ],
    );
  }

  Widget chooseDisplayBehavior() {
    final value =
        bind.sessionGetDisplaysAsIndividualWindows(sessionId: ffi.sessionId) ==
            'Y';
    return CkbMenuButton(
        value: value,
        onChanged: (value) async {
          if (value == null) return;
          await bind.sessionSetDisplaysAsIndividualWindows(
              sessionId: ffi.sessionId, value: value ? 'Y' : 'N');
        },
        ffi: ffi,
        child: Text(translate('Show displays as individual windows')));
  }

  buildOneMonitorButton(i, curDisplay) => Text(
        '${i + 1}',
        style: TextStyle(
          color: i == curDisplay
              ? _ToolbarTheme.blueColor
              : _ToolbarTheme.inactiveColor,
          fontSize: 12,
          fontWeight: FontWeight.bold,
        ),
      );

  List<Widget> buildMonitorList(BuildContext context, bool isMulti) {
    final List<Widget> monitorList = [];
    final pi = ffi.ffiModel.pi;

    buildMonitorButton(int i) => Obx(() {
          RxInt display = CurrentDisplayState.find(id);

          final isAllMonitors = i == kAllDisplayValue;
          final width = SimpleWrapper<double>(0);
          Widget? monitorsIcon;
          if (isAllMonitors) {
            monitorsIcon = globalMonitorsWidget(
                width, Colors.white, _ToolbarTheme.blueColor);
          }
          return _IconMenuButton(
            tooltip: isMulti
                ? ''
                : isAllMonitors
                    ? 'All monitors'
                    : '#{${i + 1}} monitor',
            hMargin: isMulti ? null : 6,
            vMargin: isMulti ? null : 12,
            topLevel: false,
            color: i == display.value
                ? _ToolbarTheme.blueColor
                : _ToolbarTheme.inactiveColor,
            hoverColor: i == display.value
                ? _ToolbarTheme.hoverBlueColor
                : _ToolbarTheme.hoverInactiveColor,
            width: isAllMonitors ? width.value : null,
            icon: isAllMonitors
                ? monitorsIcon
                : Container(
                    alignment: AlignmentDirectional.center,
                    constraints:
                        const BoxConstraints(minHeight: _ToolbarTheme.height),
                    child: Stack(
                      alignment: Alignment.center,
                      children: [
                        SvgPicture.asset(
                          "assets/screen.svg",
                          colorFilter:
                              ColorFilter.mode(Colors.white, BlendMode.srcIn),
                        ),
                        Obx(() => buildOneMonitorButton(i, display.value)),
                      ],
                    ),
                  ),
            onPressed: () => onPressed(i, pi, isMulti),
          );
        });

    for (int i = 0; i < pi.displays.length; i++) {
      monitorList.add(buildMonitorButton(i));
    }
    if (supportIndividualWindows && pi.displays.length > 1) {
      monitorList.add(buildMonitorButton(kAllDisplayValue));
    }
    return monitorList;
  }

  globalMonitorsWidget(
      SimpleWrapper<double> width, Color activeTextColor, Color activeBgColor) {
    getMonitors() {
      final pi = ffi.ffiModel.pi;
      RxInt display = CurrentDisplayState.find(id);
      final rect = ffi.ffiModel.globalDisplaysRect();
      if (rect == null) {
        return Offstage();
      }

      final scale = _ToolbarTheme.buttonSize / rect.height * 0.75;
      final height = rect.height * scale;
      final startY = (_ToolbarTheme.buttonSize - height) * 0.5;
      final startX = startY;

      final children = <Widget>[];
      for (var i = 0; i < pi.displays.length; i++) {
        final d = pi.displays[i];
        double s = d.scale;
        int dWidth = d.width.toDouble() ~/ s;
        int dHeight = d.height.toDouble() ~/ s;
        final fontSize = (dWidth * scale < dHeight * scale
                ? dWidth * scale
                : dHeight * scale) *
            0.65;
        children.add(Positioned(
          left: (d.x - rect.left) * scale + startX,
          top: (d.y - rect.top) * scale + startY,
          width: dWidth * scale,
          height: dHeight * scale,
          child: Container(
            decoration: BoxDecoration(
              border: Border.all(
                color: Colors.grey,
                width: 1.0,
              ),
              color: display.value == i ? activeBgColor : Colors.white,
            ),
            child: Center(
                child: Text(
              '${i + 1}',
              style: TextStyle(
                color: display.value == i
                    ? activeTextColor
                    : _ToolbarTheme.inactiveColor,
                fontSize: fontSize,
                fontWeight: FontWeight.bold,
              ),
            )),
          ),
        ));
      }
      width.value = rect.width * scale + startX * 2;
      return SizedBox(
        width: width.value,
        height: height + startY * 2,
        child: Stack(
          children: children,
        ),
      );
    }

    return Stack(
      alignment: Alignment.center,
      children: [
        SizedBox(height: _ToolbarTheme.buttonSize),
        getMonitors(),
      ],
    );
  }

  onPressed(int i, PeerInfo pi, bool isMulti) {
    if (!isMulti) {
      // 如果在工具栏中显示显示器（`buildMultiMonitorMenu()`），菜单会自动关闭。
      _menuDismissCallback(ffi);
    }
    RxInt display = CurrentDisplayState.find(id);
    if (display.value != i) {
      final isChooseDisplayToOpenInNewWindow = pi.isSupportMultiDisplay &&
          bind.sessionGetDisplaysAsIndividualWindows(
                  sessionId: ffi.sessionId) ==
              'Y';
      if (isChooseDisplayToOpenInNewWindow) {
        openMonitorInNewTabOrWindow(i, ffi.id, pi);
      } else {
        openMonitorInTheSameTab(i, ffi, pi, updateCursorPos: !isMulti);
      }
    }
  }
}

class _ControlMenu extends StatelessWidget {
  final String id;
  final FFI ffi;
  final ToolbarState state;
  _ControlMenu(
      {Key? key, required this.id, required this.ffi, required this.state})
      : super(key: key);

  @override
  Widget build(BuildContext context) {
    return _IconSubmenuButton(
        tooltip: 'Control Actions',
        svg: "assets/actions.svg",
        color: _ToolbarTheme.blueColor,
        hoverColor: _ToolbarTheme.hoverBlueColor,
        ffi: ffi,
        menuChildrenGetter: (_) => toolbarControls(context, id, ffi).map((e) {
              if (e.divider) {
                return Divider();
              } else {
                final hint = e.actionId == null
                    ? null
                    : ShortcutDisplay.formatFor(e.actionId!);
                final child = hint == null
                    ? e.child
                    : Row(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          Flexible(child: e.child),
                          Flexible(
                            child: Padding(
                              padding: const EdgeInsets.only(left: 16),
                              child: Text(
                                hint,
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: Theme.of(context)
                                    .textTheme
                                    .bodySmall
                                    ?.copyWith(
                                      color: Theme.of(context).hintColor,
                                    ),
                              ),
                            ),
                          ),
                        ],
                      );
                return MenuButton(
                    child: child,
                    onPressed: e.onPressed,
                    ffi: ffi,
                    trailingIcon: e.trailingIcon);
              }
            }).toList());
  }
}

class ScreenAdjustor {
  final String id;
  final FFI ffi;
  final VoidCallback cbExitFullscreen;
  window_size.Screen? _screen;
  Size? _waylandMaximizedWorkAreaSize;
  Rect? _waylandWorkAreaScreenFrame;
  double? _waylandWorkAreaScaleFactor;
  Rect? _x11WorkArea;
  Rect? _x11WorkAreaScreenFrame;
  double? _x11WorkAreaScaleFactor;

  ScreenAdjustor({
    required this.id,
    required this.ffi,
    required this.cbExitFullscreen,
  });

  bool get isFullscreen => stateGlobal.fullscreen.isTrue;
  int get windowId => stateGlobal.windowId;

  Future<bool?> isWindowMaximized() async {
    try {
      return await WindowController.fromWindowId(windowId).isMaximized();
    } catch (_) {
      // 延迟的分辨率回调可能在窗口销毁后才执行。
      return null;
    }
  }

  adjustWindow(BuildContext context) {
    return futureBuilder(
        future: isWindowCanBeAdjusted(context),
        hasData: (data) {
          final visible = data as bool;
          if (!visible) return Offstage();
          return Column(
            children: [
              MenuButton(
                  child: Text(translate('Adjust Window')),
                  onPressed: () => doAdjustWindow(context),
                  ffi: ffi),
              Divider(),
            ],
          );
        });
  }

  // Linux 的屏幕坐标和工作区坐标可能使用不同的单位，或者在
  // Wayland/X11 状态变化时变得不可靠，因此要对报告的边框
  // 进行归一化，并在调整窗口尺寸前缓存可用的工作区测量值。

  Future<void> _updateLinuxWorkAreaCache({
    required window_size.Screen screen,
    required Rect wndRect,
    required bool isWayland,
    required bool isX11,
    required bool forMenu,
  }) async {
    if (isWayland &&
        (_waylandWorkAreaScreenFrame != screen.frame ||
            _waylandWorkAreaScaleFactor != screen.scaleFactor)) {
      _waylandMaximizedWorkAreaSize = null;
      _waylandWorkAreaScreenFrame = screen.frame;
      _waylandWorkAreaScaleFactor = screen.scaleFactor;
    }
    if (isWayland &&
        forMenu &&
        !isFullscreen &&
        await isWindowMaximized() == true) {
      _waylandMaximizedWorkAreaSize = wndRect.size;
    }
    if (isX11 &&
        (_x11WorkAreaScreenFrame != screen.frame ||
            _x11WorkAreaScaleFactor != screen.scaleFactor)) {
      _x11WorkArea = null;
      _x11WorkAreaScreenFrame = screen.frame;
      _x11WorkAreaScaleFactor = screen.scaleFactor;
    }
    if (isX11 && forMenu && !isFullscreen) {
      _x11WorkArea = screen.visibleFrame;
    }
  }

  Future<Rect?> _getEffectiveScreenFrame({
    required window_size.Screen screen,
    required bool isWayland,
    required bool isX11,
    required bool forMenu,
  }) async {
    Rect frameRect = screen.visibleFrame;
    if (isMacOS && forMenu && isFullscreen) {
      List<double>? workArea;
      try {
        workArea = await kMacOSPermChannel
            .invokeListMethod<double>('getMacOSWorkAreaSize');
      } catch (_) {
        return null;
      }
      if (workArea == null || workArea.length != 2) {
        return null;
      }
      frameRect = Rect.fromLTWH(
        frameRect.left,
        frameRect.top,
        workArea[0] < frameRect.width ? workArea[0] : frameRect.width,
        workArea[1] < frameRect.height ? workArea[1] : frameRect.height,
      );
    }
    final x11WorkArea = _x11WorkArea;
    if (isX11 &&
        forMenu &&
        isFullscreen &&
        x11WorkArea != null &&
        (x11WorkArea.width < frameRect.width ||
            x11WorkArea.height < frameRect.height)) {
      frameRect = x11WorkArea;
    }
    final screenScale = screen.scaleFactor;
    if (isWayland && screenScale > 1.01) {
      String monitorLayoutMode;
      try {
        monitorLayoutMode =
            await bind.mainGetCommon(key: 'gnome-monitor-layout-mode');
      } catch (_) {
        monitorLayoutMode = '';
      }
      if (monitorLayoutMode == 'physical') {
        frameRect = Rect.fromLTRB(
          frameRect.left / screenScale,
          frameRect.top / screenScale,
          frameRect.right / screenScale,
          frameRect.bottom / screenScale,
        );
      }
    }
    return frameRect;
  }

  Future<Rect?> _getAdjustedWindowFrame(Size mediaSize,
      {bool forMenu = false}) async {
    final screen = _screen;
    if (screen != null) {
      // Windows 的窗口边框使用物理像素，而 Flutter 的视图尺寸是
      // 逻辑像素。macOS 和 Linux 的窗口边框与 Flutter 使用相同单位。
      double scale = isWindows ? screen.scaleFactor : 1.0;
      final Rect wndRect;
      try {
        wndRect = await WindowController.fromWindowId(windowId).getFrame();
      } catch (e) {
        debugPrint("Failed to get frame of window $windowId, it may be hidden");
        return null;
      }
      // 在 Windows 上，wndRect 是 GetWindowRect，而 mediaSize 是 GetClientRect。
      // https://stackoverflow.com/a/7561083
      double magicWidth =
          wndRect.right - wndRect.left - mediaSize.width * scale;
      double magicHeight =
          wndRect.bottom - wndRect.top - mediaSize.height * scale;
      final canvasModel = ffi.canvasModel;
      // canvasModel.scale 是渲染缩放，已经应用了 kIgnoreDpi。
      // 用它代替远端源分辨率。
      final isWayland = isLinux && bind.mainCurrentIsWayland();
      final isX11 = isLinux && !isWayland;
      await _updateLinuxWorkAreaCache(
        screen: screen,
        wndRect: wndRect,
        isWayland: isWayland,
        isX11: isX11,
        forMenu: forMenu,
      );
      if (isWindows && forMenu && isFullscreen) {
        // desktop_multi_window 的隐藏标题栏会在每条水平边缘和底部
        // 各保留 8 个物理像素，顶部最多保留 1px。
        // 全屏时会在 WM_NCCALCSIZE 中移除这些，因此在决定是否显示菜单时，
        // 要预估还原后边框的最大内边距。
        magicWidth = 16.0;
        magicHeight = 9.0;
      }
      double horizontalEdges;
      double verticalEdges;
      if (forMenu && (isLinux || ((isMacOS || isWindows) && isFullscreen))) {
        // Linux 的 Adjust Window 会取消最大化；macOS 和 Windows 会退出全屏
        // 后再调整尺寸。在决定是否显示菜单项时，
        // 要预估还原后的普通窗口边缘。
        final resizePadding = isLinux && !kUseCompatibleUiMode
            ? kDragToResizeAreaPaddingSize
            : 0.0;
        final windowEdge = kWindowBorderWidth + resizePadding;
        horizontalEdges = windowEdge * 2;
        verticalEdges = kDesktopRemoteTabBarHeight + windowEdge * 2;
      } else {
        horizontalEdges = CanvasModel.leftToEdge + CanvasModel.rightToEdge;
        verticalEdges = CanvasModel.topToEdge + CanvasModel.bottomToEdge;
      }
      final width = (canvasModel.getDisplayWidth() * canvasModel.scale +
                  horizontalEdges) *
              scale +
          magicWidth;
      final height =
          (canvasModel.getDisplayHeight() * canvasModel.scale + verticalEdges) *
                  scale +
              magicHeight;
      double left = wndRect.left + (wndRect.width - width) / 2;
      double top = wndRect.top + (wndRect.height - height) / 2;

      final frameRect = await _getEffectiveScreenFrame(
        screen: screen,
        isWayland: isWayland,
        isX11: isX11,
        forMenu: forMenu,
      );
      if (frameRect == null) {
        return null;
      }
      var availableSize = frameRect.size;
      if (isWayland && forMenu && _waylandMaximizedWorkAreaSize != null) {
        final cachedSize = _waylandMaximizedWorkAreaSize!;
        availableSize = Size(
          cachedSize.width < availableSize.width
              ? cachedSize.width
              : availableSize.width,
          cachedSize.height < availableSize.height
              ? cachedSize.height
              : availableSize.height,
        );
      }
      // 窗口边框不可能小于其客户区。要容忍小的
      // 浮点误差；出现较大的负值说明原生
      // 边框与 Flutter 视图度量没有同步。
      if (magicWidth < -0.1 || magicHeight < -0.1) {
        return null;
      }
      // 拒绝小得不合理的目标尺寸，以免窗口被隐藏。
      if (width < 300 || height < 300) {
        return null;
      }
      // 远端尺寸可能在菜单构建后发生变化。拒绝
      // 超出可用区域的目标尺寸。
      final exceedsScreen =
          width > availableSize.width || height > availableSize.height;
      if (exceedsScreen) {
        return null;
      }
      if (left < frameRect.left) {
        left = frameRect.left;
      }
      if (top < frameRect.top) {
        top = frameRect.top;
      }
      if ((left + width) > frameRect.right) {
        left = frameRect.right - width;
      }
      if ((top + height) > frameRect.bottom) {
        top = frameRect.bottom - height;
      }
      return Rect.fromLTWH(left, top, width, height);
    }
    return null;
  }

  doAdjustWindow([BuildContext? context]) async {
    // 分辨率变化会在延迟后调整，此时菜单上下文可能
    // 已经被销毁。每个 desktop_multi_window 窗口都有自己的引擎，
    // 因此该引擎的第一个视图就是当前窗口。
    final views = WidgetsBinding.instance.platformDispatcher.views;
    if (context == null && views.isEmpty) {
      return;
    }
    final view = context != null ? View.of(context) : views.first;
    await updateScreen();
    if (_screen != null) {
      final wc = WindowController.fromWindowId(windowId);
      final wasFullscreen = isFullscreen;
      cbExitFullscreen();
      if (wasFullscreen) {
        // 等待原生全屏退出后再更新窗口边框。
        await Future.delayed(Duration(milliseconds: 700));
        await updateScreen();
      }
      if (isLinux) {
        final isMaximized = await isWindowMaximized();
        if (isMaximized == null) {
          return;
        }
        if (isMaximized == true) {
          // 原生窗口最大化时 setFrame 可能被忽略。
          try {
            await wc.unmaximize();
          } catch (_) {
            return;
          }
          stateGlobal.setMaximized(false);
          // 等待窗口管理器和 Flutter 视图度量反映
          // 还原后的窗口，再计算并设置其边框。
          await Future.delayed(Duration(milliseconds: 300));
          await updateScreen();
        }
      }
      final mediaSize = MediaQueryData.fromView(view).size;
      final frame = await _getAdjustedWindowFrame(mediaSize);
      if (frame == null) {
        return;
      }
      try {
        await wc.setFrame(frame);
      } catch (_) {
        return;
      }
      stateGlobal.setMaximized(false);
    }
  }

  updateScreen() async {
    _screen = await _getCurrentScreen();
  }

  Future<window_size.Screen?> _getCurrentScreen() async {
    try {
      return (await window_size.getWindowInfo()).screen;
    } catch (e) {
      debugPrint('Failed to get current window screen: $e');
      return null;
    }
  }

  Future<bool> isWindowCanBeAdjusted([BuildContext? context]) async {
    if (isWeb) {
      return false;
    }
    // 在 await 之前先获取视图，因为菜单上下文可能已被销毁。
    final views = WidgetsBinding.instance.platformDispatcher.views;
    if (context == null && views.isEmpty) {
      return false;
    }
    final view = context != null ? View.of(context) : views.first;
    final mediaSize = MediaQueryData.fromView(view).size;
    final viewStyle =
        await bind.sessionGetViewStyle(sessionId: ffi.sessionId) ?? '';
    if (viewStyle != kRemoteViewStyleOriginal) {
      return false;
    }
    final remoteCount = RemoteCountState.find().value;
    if (remoteCount != 1) {
      return false;
    }
    await updateScreen();
    if (_screen == null) {
      return false;
    }
    return await _getAdjustedWindowFrame(mediaSize, forMenu: true) != null;
  }
}

class _DisplayMenu extends StatefulWidget {
  final String id;
  final FFI ffi;
  final ToolbarState state;
  final Function(bool) setFullscreen;
  const _DisplayMenu(
      {required this.id,
      required this.ffi,
      required this.state,
      required this.setFullscreen});

  @override
  State<_DisplayMenu> createState() => _DisplayMenuState();
}

class _DisplayMenuState extends State<_DisplayMenu> {
  final RxInt _customPercent = 100.obs;
  late final ScreenAdjustor _screenAdjustor = ScreenAdjustor(
    id: widget.id,
    ffi: widget.ffi,
    cbExitFullscreen: () => widget.setFullscreen(false),
  );

  int get windowId => stateGlobal.windowId;
  Map<String, bool> get perms => widget.ffi.ffiModel.permissions;
  PeerInfo get pi => widget.ffi.ffiModel.pi;
  FfiModel get ffiModel => widget.ffi.ffiModel;
  FFI get ffi => widget.ffi;
  String get id => widget.id;

  @override
  void initState() {
    super.initState();
    // 从已保存的选项一次性初始化自定义百分比
    WidgetsBinding.instance.addPostFrameCallback((_) async {
      try {
        final v = await getSessionCustomScalePercent(widget.ffi.sessionId);
        if (_customPercent.value != v) {
          _customPercent.value = v;
        }
      } catch (_) {}
    });
  }

  @override
  Widget build(BuildContext context) {
    final colorScheme = Theme.of(context).colorScheme;
    menuChildrenGetter(_IconSubmenuButtonState state) {
      final menuChildren = <Widget>[
        _screenAdjustor.adjustWindow(context),
        viewStyle(customPercent: _customPercent),
        scrollStyle(state, colorScheme),
        imageQuality(),
        codec(),
        if (ffi.connType == ConnType.defaultConn)
          _ResolutionsMenu(
            id: widget.id,
            ffi: widget.ffi,
            screenAdjustor: _screenAdjustor,
          ),
        if (showVirtualDisplayMenu(ffi) && ffi.connType == ConnType.defaultConn)
          _SubmenuButton(
            ffi: widget.ffi,
            menuChildren: getVirtualDisplayMenuChildren(ffi, id, null),
            child: Text(translate("Virtual display")),
          ),
        if (ffi.connType == ConnType.defaultConn) cursorToggles(),
        Divider(),
        toggles(),
      ];
      // 隐私模式
      final privacyModeState = PrivacyModeState.find(id);
      if (ffi.connType == ConnType.defaultConn &&
          (pi.features.privacyMode || privacyModeState.isNotEmpty) &&
          (ffiModel.keyboard || privacyModeState.isNotEmpty)) {
        final privacyModeList =
            toolbarPrivacyMode(privacyModeState, context, id, ffi);
        if (privacyModeList.length == 1) {
          menuChildren.add(CkbMenuButton(
              value: privacyModeList[0].value,
              onChanged: privacyModeList[0].onChanged,
              child: privacyModeList[0].child,
              ffi: ffi));
        } else if (privacyModeList.length > 1) {
          menuChildren.addAll([
            Divider(),
            _SubmenuButton(
                ffi: widget.ffi,
                child: Text(translate('Privacy mode')),
                menuChildren: privacyModeList
                    .map((e) => CkbMenuButton(
                        value: e.value,
                        onChanged: e.onChanged,
                        child: e.child,
                        ffi: ffi))
                    .toList()),
          ]);
        }
      }
      return menuChildren;
    }

    return _IconSubmenuButton(
      tooltip: 'Display Settings',
      svg: "assets/display.svg",
      ffi: widget.ffi,
      color: _ToolbarTheme.blueColor,
      hoverColor: _ToolbarTheme.hoverBlueColor,
      menuChildrenGetter: menuChildrenGetter,
    );
  }

  viewStyle({required RxInt customPercent}) {
    return futureBuilder(
        future: toolbarViewStyle(context, widget.id, widget.ffi),
        hasData: (data) {
          final v = data as List<TRadioMenu<String>>;
          final bool isCustomSelected = v.isNotEmpty
              ? v.first.groupValue == kRemoteViewStyleCustom
              : false;
          return Column(children: [
            ...v.map((e) {
              final isCustom = e.value == kRemoteViewStyleCustom;
              final child =
                  isCustom ? Text(translate('Scale custom')) : e.child;
              // 当前选中项是否已经是自定义
              final bool isGroupCustomSelected =
                  e.groupValue == kRemoteViewStyleCustom;
              // 切换到自定义时保持菜单打开，以便立即看到滑块
              final bool keepOpenForThisItem =
                  isCustom && !isGroupCustomSelected;
              return RdoMenuButton<String>(
                  value: e.value,
                  groupValue: e.groupValue,
                  onChanged: (value) {
                    // 执行原有的变更
                    e.onChanged?.call(value);
                    // 只有在保持菜单打开以显示滑块时才强制重建
                    if (keepOpenForThisItem) {
                      setState(() {});
                    }
                  },
                  child: child,
                  ffi: ffi,
                  // 进入自定义时保持子菜单打开，以显示滑块控件
                  closeOnActivate: !keepOpenForThisItem);
            }).toList(),
            // 仅在未选中自定义时才显示分隔线
            if (!isCustomSelected) Divider(),
            _customControlsIfCustomSelected(
                onChanged: (v) => customPercent.value = v),
          ]);
        });
  }

  Widget _customControlsIfCustomSelected({ValueChanged<int>? onChanged}) {
    return futureBuilder(future: () async {
      final current = await bind.sessionGetViewStyle(sessionId: ffi.sessionId);
      return current == kRemoteViewStyleCustom;
    }(), hasData: (data) {
      final isCustom = data as bool;
      return AnimatedSwitcher(
        duration: Duration(milliseconds: 220),
        switchInCurve: Curves.easeOut,
        switchOutCurve: Curves.easeIn,
        child: isCustom
            ? _CustomScaleMenuControls(ffi: ffi, onChanged: onChanged)
            : SizedBox.shrink(),
      );
    });
  }

  scrollStyle(_IconSubmenuButtonState state, ColorScheme colorScheme) {
    return futureBuilder(future: () async {
      final viewStyle =
          await bind.sessionGetViewStyle(sessionId: ffi.sessionId) ?? '';
      final visible = viewStyle == kRemoteViewStyleOriginal ||
          viewStyle == kRemoteViewStyleCustom;
      final scrollStyle =
          await bind.sessionGetScrollStyle(sessionId: ffi.sessionId) ?? '';
      final edgeScrollEdgeThickness = await bind
          .sessionGetEdgeScrollEdgeThickness(sessionId: ffi.sessionId);
      return {
        'visible': visible,
        'scrollStyle': scrollStyle,
        'edgeScrollEdgeThickness': edgeScrollEdgeThickness,
      };
    }(), hasData: (data) {
      final visible = data['visible'] as bool;
      if (!visible) return Offstage();
      final groupValue = data['scrollStyle'] as String;
      final edgeScrollEdgeThickness = data['edgeScrollEdgeThickness'] as int;

      onChangeScrollStyle(String? value) async {
        if (value == null) return;
        await bind.sessionSetScrollStyle(
            sessionId: ffi.sessionId, value: value);
        widget.ffi.canvasModel.updateScrollStyle();
        state.setState(() {});
      }

      onChangeEdgeScrollEdgeThickness(double? value) async {
        if (value == null) return;
        final newThickness = value.round();
        await bind.sessionSetEdgeScrollEdgeThickness(
            sessionId: ffi.sessionId, value: newThickness);
        widget.ffi.canvasModel.updateEdgeScrollEdgeThickness(newThickness);
        state.setState(() {});
      }

      return Obx(() => Column(children: [
            RdoMenuButton<String>(
              child: Text(translate('ScrollAuto')),
              value: kRemoteScrollStyleAuto,
              groupValue: groupValue,
              onChanged: widget.ffi.canvasModel.imageOverflow.value
                  ? (value) => onChangeScrollStyle(value)
                  : null,
              closeOnActivate: groupValue != kRemoteScrollStyleEdge,
              ffi: widget.ffi,
            ),
            RdoMenuButton<String>(
              child: Text(translate('Scrollbar')),
              value: kRemoteScrollStyleBar,
              groupValue: groupValue,
              onChanged: widget.ffi.canvasModel.imageOverflow.value
                  ? (value) => onChangeScrollStyle(value)
                  : null,
              closeOnActivate: groupValue != kRemoteScrollStyleEdge,
              ffi: widget.ffi,
            ),
            if (!isWeb) ...[
              RdoMenuButton<String>(
                child: Text(translate('ScrollEdge')),
                value: kRemoteScrollStyleEdge,
                groupValue: groupValue,
                closeOnActivate: false,
                onChanged: widget.ffi.canvasModel.imageOverflow.value
                    ? (value) => onChangeScrollStyle(value)
                    : null,
                ffi: widget.ffi,
              ),
              Offstage(
                  offstage: groupValue != kRemoteScrollStyleEdge,
                  child: EdgeThicknessControl(
                    value: edgeScrollEdgeThickness.toDouble(),
                    onChanged: onChangeEdgeScrollEdgeThickness,
                    colorScheme: colorScheme,
                  )),
            ],
            Divider(),
          ]));
    });
  }

  imageQuality() {
    return futureBuilder(
        future: toolbarImageQuality(context, widget.id, widget.ffi),
        hasData: (data) {
          final v = data as List<TRadioMenu<String>>;
          return _SubmenuButton(
            ffi: widget.ffi,
            child: Text(translate('Image Quality')),
            menuChildren: v
                .map((e) => RdoMenuButton<String>(
                    value: e.value,
                    groupValue: e.groupValue,
                    onChanged: e.onChanged,
                    child: e.child,
                    ffi: ffi))
                .toList(),
          );
        });
  }

  codec() {
    return futureBuilder(
        future: toolbarCodec(context, id, ffi),
        hasData: (data) {
          final v = data as List<TRadioMenu<String>>;
          if (v.isEmpty) return Offstage();

          return _SubmenuButton(
              ffi: widget.ffi,
              child: Text(translate('Codec')),
              menuChildren: v
                  .map((e) => RdoMenuButton(
                      value: e.value,
                      groupValue: e.groupValue,
                      onChanged: e.onChanged,
                      child: e.child,
                      ffi: ffi))
                  .toList());
        });
  }

  cursorToggles() {
    return futureBuilder(
        future: toolbarCursor(context, id, ffi),
        hasData: (data) {
          final v = data as List<TToggleMenu>;
          if (v.isEmpty) return Offstage();
          return Column(children: [
            Divider(),
            ...v
                .map((e) => CkbMenuButton(
                    value: e.value,
                    onChanged: e.onChanged,
                    child: e.child,
                    ffi: ffi))
                .toList(),
          ]);
        });
  }

  toggles() {
    return futureBuilder(
        future: toolbarDisplayToggle(context, id, ffi),
        hasData: (data) {
          final v = data as List<TToggleMenu>;
          if (v.isEmpty) return Offstage();
          return Column(
              children: v
                  .map((e) => CkbMenuButton(
                      value: e.value,
                      onChanged: e.onChanged,
                      child: e.child,
                      ffi: ffi))
                  .toList());
        });
  }
}

class _CustomScaleMenuControls extends StatefulWidget {
  final FFI ffi;
  final ValueChanged<int>? onChanged;
  const _CustomScaleMenuControls({Key? key, required this.ffi, this.onChanged})
      : super(key: key);

  @override
  State<_CustomScaleMenuControls> createState() =>
      _CustomScaleMenuControlsState();
}

class _CustomScaleMenuControlsState
    extends CustomScaleControls<_CustomScaleMenuControls> {
  @override
  FFI get ffi => widget.ffi;

  @override
  ValueChanged<int>? get onScaleChanged => widget.onChanged;

  @override
  Widget build(BuildContext context) {
    final colorScheme = Theme.of(context).colorScheme;
    const smallBtnConstraints = BoxConstraints(minWidth: 28, minHeight: 28);

    final sliderControl = Semantics(
      label: translate('Custom scale slider'),
      value: '$scaleValue%',
      child: SliderTheme(
        data: SliderTheme.of(context).copyWith(
          activeTrackColor: colorScheme.primary,
          thumbColor: colorScheme.primary,
          overlayColor: colorScheme.primary.withOpacity(0.1),
          showValueIndicator: ShowValueIndicator.never,
          thumbShape: _RectValueThumbShape(
            min: CustomScaleControls.minPercent.toDouble(),
            max: CustomScaleControls.maxPercent.toDouble(),
            width: 52,
            height: 24,
            radius: 4,
            displayValueForNormalized: (t) => mapPosToPercent(t),
          ),
        ),
        child: Slider(
          value: scalePos,
          min: 0.0,
          max: 1.0,
          // 使用较大的分段数（按 (CustomScaleControls.maxPercent - CustomScaleControls.minPercent) 计算），以提供约 1% 的精度增量。
          // 这样用户可以设置精确的缩放值。更小的分段数需要通过 +/- 按钮做更多微调，对大范围而言并不可取。
          divisions:
              (CustomScaleControls.maxPercent - CustomScaleControls.minPercent)
                  .round(),
          onChanged: onSliderChanged,
        ),
      ),
    );

    return Column(children: [
      Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12.0),
        child: Row(children: [
          Tooltip(
            message: translate('Decrease'),
            child: IconButton(
              iconSize: 16,
              padding: EdgeInsets.all(1),
              constraints: smallBtnConstraints,
              icon: const Icon(Icons.remove),
              onPressed: () => nudgeScale(-1),
            ),
          ),
          Expanded(child: sliderControl),
          Tooltip(
            message: translate('Increase'),
            child: IconButton(
              iconSize: 16,
              padding: EdgeInsets.all(1),
              constraints: smallBtnConstraints,
              icon: const Icon(Icons.add),
              onPressed: () => nudgeScale(1),
            ),
          ),
        ]),
      ),
      Divider(),
    ]);
  }
}

// 轻量的矩形滑块，绘制当前百分比。
// 无状态，仅使用 SliderTheme 的颜色；除每帧一个 TextPainter 外不产生额外分配。
class _RectValueThumbShape extends SliderComponentShape {
  final double min;
  final double max;
  final double width;
  final double height;
  final double radius;
  final String unit;
  // 可选的映射函数，根据归一化位置 [0,1] 计算显示值
  // 如果为 null，则回退到在最小值和最大值之间做线性插值。
  final int Function(double normalized)? displayValueForNormalized;

  const _RectValueThumbShape({
    required this.min,
    required this.max,
    required this.width,
    required this.height,
    required this.radius,
    this.displayValueForNormalized,
    this.unit = '%',
  });

  @override
  Size getPreferredSize(bool isEnabled, bool isDiscrete) {
    return Size(width, height);
  }

  @override
  void paint(
    PaintingContext context,
    Offset center, {
    required Animation<double> activationAnimation,
    required Animation<double> enableAnimation,
    required bool isDiscrete,
    required TextPainter labelPainter,
    required RenderBox parentBox,
    required SliderThemeData sliderTheme,
    required TextDirection textDirection,
    required double value,
    required double textScaleFactor,
    required Size sizeWithOverflow,
  }) {
    final Canvas canvas = context.canvas;

    // 根据启用/禁用动画解析颜色，并提供安全的回退。
    final ColorTween colorTween = ColorTween(
      begin: sliderTheme.disabledThumbColor,
      end: sliderTheme.thumbColor,
    );
    final Color? evaluatedColor = colorTween.evaluate(enableAnimation);
    final Color? thumbColor = sliderTheme.thumbColor;
    final Color fillColor = evaluatedColor ?? thumbColor ?? Colors.blueAccent;

    final RRect rrect = RRect.fromRectAndRadius(
      Rect.fromCenter(center: center, width: width, height: height),
      Radius.circular(radius),
    );
    final Paint paint = Paint()..color = fillColor;
    canvas.drawRRect(rrect, paint);

    // 根据归一化的滑块值计算显示值。
    final int displayValue = displayValueForNormalized != null
        ? displayValueForNormalized!(value)
        : (min + value * (max - min)).round();
    final TextSpan span = TextSpan(
      text: '$displayValue$unit',
      style: const TextStyle(
        color: Colors.white,
        fontSize: 12,
        fontWeight: FontWeight.w600,
      ),
    );
    final TextPainter tp = TextPainter(
      text: span,
      textAlign: TextAlign.center,
      textDirection: textDirection,
    );
    tp.layout(maxWidth: width - 4);
    tp.paint(
        canvas, Offset(center.dx - tp.width / 2, center.dy - tp.height / 2));
  }
}

class _ResolutionsMenu extends StatefulWidget {
  final String id;
  final FFI ffi;
  final ScreenAdjustor screenAdjustor;

  _ResolutionsMenu({
    Key? key,
    required this.id,
    required this.ffi,
    required this.screenAdjustor,
  }) : super(key: key);

  @override
  State<_ResolutionsMenu> createState() => _ResolutionsMenuState();
}

const double _kCustomResolutionEditingWidth = 42;
const _kCustomResolutionValue = 'custom';

class _ResolutionsMenuState extends State<_ResolutionsMenu> {
  String _groupValue = '';
  Resolution? _localResolution;

  late final TextEditingController _customWidth =
      TextEditingController(text: rect?.width.toInt().toString() ?? '');
  late final TextEditingController _customHeight =
      TextEditingController(text: rect?.height.toInt().toString() ?? '');

  FFI get ffi => widget.ffi;
  PeerInfo get pi => widget.ffi.ffiModel.pi;
  FfiModel get ffiModel => widget.ffi.ffiModel;
  Rect? get rect => scaledRect();
  List<Resolution> get resolutions => pi.resolutions;
  bool get isWayland => bind.mainCurrentIsWayland();

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _getLocalResolutionWayland();
    });
  }

  Rect? scaledRect() {
    final scale = pi.scaleOfDisplay(pi.currentDisplay);
    final rect = ffiModel.rect;
    if (rect == null) {
      return null;
    }
    return Rect.fromLTWH(
      rect.left,
      rect.top,
      rect.width / scale,
      rect.height / scale,
    );
  }

  @override
  Widget build(BuildContext context) {
    final isVirtualDisplay = ffiModel.isVirtualDisplayResolution;
    final visible = ffiModel.keyboard &&
        (isVirtualDisplay || resolutions.length > 1) &&
        pi.currentDisplay != kAllDisplayValue;
    if (!visible) return Offstage();
    final showOriginalBtn =
        ffiModel.isOriginalResolutionSet && !ffiModel.isOriginalResolution;
    final showFitLocalBtn = !_isRemoteResolutionFitLocal();
    _setGroupValue();
    return _SubmenuButton(
      ffi: widget.ffi,
      menuChildren: <Widget>[
            _OriginalResolutionMenuButton(context, showOriginalBtn),
            _FitLocalResolutionMenuButton(context, showFitLocalBtn),
            _customResolutionMenuButton(context, isVirtualDisplay),
            _menuDivider(showOriginalBtn, showFitLocalBtn, isVirtualDisplay),
          ] +
          _supportedResolutionMenuButtons(),
      child: Text(translate("Resolution")),
    );
  }

  _setGroupValue() {
    if (pi.currentDisplay == kAllDisplayValue) {
      return;
    }
    final lastGroupValue =
        stateGlobal.getLastResolutionGroupValue(widget.id, pi.currentDisplay);
    if (lastGroupValue == _kCustomResolutionValue) {
      _groupValue = _kCustomResolutionValue;
    } else {
      _groupValue =
          '${(rect?.width ?? 0).toInt()}x${(rect?.height ?? 0).toInt()}';
    }
  }

  _menuDivider(
      bool showOriginalBtn, bool showFitLocalBtn, bool isVirtualDisplay) {
    return Offstage(
      offstage: !(showOriginalBtn || showFitLocalBtn || isVirtualDisplay),
      child: Divider(),
    );
  }

  Future<void> _getLocalResolutionWayland() async {
    if (!isWayland) return _getLocalResolution();
    try {
      final window = await window_size.getWindowInfo();
      final screen = window.screen;
      if (screen != null) {
        setState(() {
          _localResolution = Resolution(
            screen.frame.width.toInt(),
            screen.frame.height.toInt(),
          );
        });
      }
    } catch (e) {
      debugPrint('Failed to get local resolution on Wayland: $e');
    }
  }

  _getLocalResolution() {
    _localResolution = null;
    final String mainDisplay = bind.mainGetMainDisplay();
    if (mainDisplay.isNotEmpty) {
      try {
        final display = json.decode(mainDisplay);
        if (display['w'] != null && display['h'] != null) {
          _localResolution = Resolution(display['w'], display['h']);
          if (isWeb) {
            if (display['scaleFactor'] != null) {
              _localResolution = Resolution(
                (display['w'] / display['scaleFactor']).toInt(),
                (display['h'] / display['scaleFactor']).toInt(),
              );
            }
          }
        }
      } catch (e) {
        debugPrint('Failed to decode $mainDisplay, $e');
      }
    }
  }

  // 该组件已卸载，因此 State 不再有 context
  _onChanged(String? value) async {
    if (pi.currentDisplay == kAllDisplayValue) {
      return;
    }
    stateGlobal.setLastResolutionGroupValue(
        widget.id, pi.currentDisplay, value);
    if (value == null) return;

    int? w;
    int? h;
    if (value == _kCustomResolutionValue) {
      w = int.tryParse(_customWidth.text);
      h = int.tryParse(_customHeight.text);
    } else {
      final list = value.split('x');
      if (list.length == 2) {
        w = int.tryParse(list[0]);
        h = int.tryParse(list[1]);
      }
    }

    if (w != null && h != null) {
      if (w != rect?.width.toInt() || h != rect?.height.toInt()) {
        await _changeResolution(w, h);
      }
    }
  }

  _changeResolution(int w, int h) async {
    if (pi.currentDisplay == kAllDisplayValue) {
      return;
    }
    await bind.sessionChangeResolution(
      sessionId: ffi.sessionId,
      display: pi.currentDisplay,
      width: w,
      height: h,
    );
    Future.delayed(Duration(seconds: 3), () async {
      final rect = ffiModel.rect;
      if (rect == null) {
        return;
      }
      if (w == rect.width.toInt() && h == rect.height.toInt()) {
        if (!await widget.screenAdjustor.isWindowCanBeAdjusted()) {
          return;
        }
        if (widget.screenAdjustor.isFullscreen) {
          return;
        }
        if ((await widget.screenAdjustor.isWindowMaximized()) == false) {
          // 这个延迟回调可能比菜单 State 活得更久，因此它的 context
          // 是不安全的。
          widget.screenAdjustor.doAdjustWindow();
        }
      }
    });
  }

  Widget _OriginalResolutionMenuButton(
      BuildContext context, bool showOriginalBtn) {
    final display = pi.tryGetDisplayIfNotAllDisplay();
    if (display == null) {
      return Offstage();
    }
    if (!resolutions.any((e) =>
        e.width == display.originalWidth &&
        e.height == display.originalHeight)) {
      return Offstage();
    }
    return Offstage(
      offstage: !showOriginalBtn,
      child: MenuButton(
        onPressed: () =>
            _changeResolution(display.originalWidth, display.originalHeight),
        ffi: widget.ffi,
        child: Text(
            '${translate('resolution_original_tip')} ${display.originalWidth}x${display.originalHeight}'),
      ),
    );
  }

  Widget _FitLocalResolutionMenuButton(
      BuildContext context, bool showFitLocalBtn) {
    return Offstage(
      offstage: !showFitLocalBtn,
      child: MenuButton(
        onPressed: () {
          final resolution = _getBestFitResolution();
          if (resolution != null) {
            _changeResolution(resolution.width, resolution.height);
          }
        },
        ffi: widget.ffi,
        child: Text(
            '${translate('resolution_fit_local_tip')} ${_localResolution?.width ?? 0}x${_localResolution?.height ?? 0}'),
      ),
    );
  }

  Widget _customResolutionMenuButton(BuildContext context, isVirtualDisplay) {
    return Offstage(
      offstage: !isVirtualDisplay,
      child: RdoMenuButton(
        value: _kCustomResolutionValue,
        groupValue: _groupValue,
        onChanged: (String? value) => _onChanged(value),
        ffi: widget.ffi,
        child: Row(
          children: [
            Text('${translate('resolution_custom_tip')} '),
            SizedBox(
              width: _kCustomResolutionEditingWidth,
              child: _resolutionInput(_customWidth),
            ),
            Text(' x '),
            SizedBox(
              width: _kCustomResolutionEditingWidth,
              child: _resolutionInput(_customHeight),
            ),
          ],
        ),
      ),
    );
  }

  Widget _resolutionInput(TextEditingController controller) {
    return TextField(
      decoration: InputDecoration(
        border: InputBorder.none,
        isDense: true,
        contentPadding: EdgeInsets.fromLTRB(3, 3, 3, 3),
      ),
      keyboardType: TextInputType.number,
      inputFormatters: <TextInputFormatter>[
        FilteringTextInputFormatter.digitsOnly,
        LengthLimitingTextInputFormatter(4),
        FilteringTextInputFormatter.allow(RegExp(r'[0-9]')),
      ],
      controller: controller,
    ).workaroundFreezeLinuxMint();
  }

  List<Widget> _supportedResolutionMenuButtons() => resolutions
      .map((e) => RdoMenuButton(
          value: '${e.width}x${e.height}',
          groupValue: _groupValue,
          onChanged: (String? value) => _onChanged(value),
          ffi: widget.ffi,
          child: Text('${e.width}x${e.height}')))
      .toList();

  Resolution? _getBestFitResolution() {
    if (_localResolution == null) {
      return null;
    }

    if (ffiModel.isVirtualDisplayResolution) {
      return _localResolution!;
    }

    for (final r in resolutions) {
      if (r.width == _localResolution!.width &&
          r.height == _localResolution!.height) {
        return r;
      }
    }

    return null;
  }

  bool _isRemoteResolutionFitLocal() {
    if (_localResolution == null) {
      return true;
    }
    final bestFitResolution = _getBestFitResolution();
    if (bestFitResolution == null) {
      return true;
    }
    return bestFitResolution.width == rect?.width.toInt() &&
        bestFitResolution.height == rect?.height.toInt();
  }
}

class _KeyboardMenu extends StatelessWidget {
  final String id;
  final FFI ffi;
  _KeyboardMenu({
    Key? key,
    required this.id,
    required this.ffi,
  }) : super(key: key);

  PeerInfo get pi => ffi.ffiModel.pi;

  @override
  Widget build(BuildContext context) {
    var ffiModel = Provider.of<FfiModel>(context);
    if (!ffiModel.keyboard) return Offstage();
    toolbarToggles() {
      final toggles = toolbarKeyboardToggles(ffi)
          .map((e) => CkbMenuButton(
              value: e.value,
              onChanged: e.onChanged,
              child: e.child,
              ffi: ffi) as Widget)
          .toList();
      if (toggles.isNotEmpty) {
        toggles.add(Divider());
      }
      return toggles;
    }

    return _IconSubmenuButton(
        tooltip: 'Keyboard Settings',
        svg: "assets/keyboard_mouse.svg",
        ffi: ffi,
        color: _ToolbarTheme.blueColor,
        hoverColor: _ToolbarTheme.hoverBlueColor,
        menuChildrenGetter: (_) => [
              keyboardMode(),
              localKeyboardType(),
              inputSource(),
              Divider(),
              viewMode(),
              if ([kPeerPlatformWindows, kPeerPlatformMacOS, kPeerPlatformLinux]
                  .contains(pi.platform))
                showMyCursor(),
              Divider(),
              ...toolbarToggles(),
              ...mouseSpeed(),
              ...mobileActions(),
            ]);
  }

  mouseSpeed() {
    final speedWidgets = [];
    final sessionId = ffi.sessionId;
    if (isDesktop) {
      if (ffi.ffiModel.keyboard) {
        final enabled = !ffi.ffiModel.viewOnly;
        final trackpad = MenuButton(
          child: Text(translate('Trackpad speed')).paddingOnly(left: 26.0),
          onPressed: enabled ? () => trackpadSpeedDialog(sessionId, ffi) : null,
          ffi: ffi,
        );
        speedWidgets.add(trackpad);
      }
    }
    return speedWidgets;
  }

  keyboardMode() {
    return futureBuilder(future: () async {
      return await bind.sessionGetKeyboardMode(sessionId: ffi.sessionId) ??
          kKeyLegacyMode;
    }(), hasData: (data) {
      final groupValue = data as String;
      List<InputModeMenu> modes = [
        InputModeMenu(key: kKeyLegacyMode, menu: 'Legacy mode'),
        InputModeMenu(key: kKeyMapMode, menu: 'Map mode'),
        InputModeMenu(key: kKeyTranslateMode, menu: 'Translate mode'),
      ];
      List<RdoMenuButton> list = [];
      final enabled = !ffi.ffiModel.viewOnly;
      onChanged(String? value) async {
        if (value == null) return;
        await bind.sessionSetKeyboardMode(
            sessionId: ffi.sessionId, value: value);
        await ffi.inputModel.updateKeyboardMode();
      }

      // 如果用 flutter 获取按键，我们只能使用一种模式。
      // Map 模式和 Legacy 模式，至少支持其中之一。
      String? modeOnly;
      // 目前 web 上同时保留 map 和 legacy 模式。
      // TODO: Remove legacy mode after web supports translate mode on web.
      if (isInputSourceFlutter && isDesktop) {
        if (bind.sessionIsKeyboardModeSupported(
            sessionId: ffi.sessionId, mode: kKeyMapMode)) {
          modeOnly = kKeyMapMode;
        } else if (bind.sessionIsKeyboardModeSupported(
            sessionId: ffi.sessionId, mode: kKeyLegacyMode)) {
          modeOnly = kKeyLegacyMode;
        }
      }

      for (InputModeMenu mode in modes) {
        if (modeOnly != null && mode.key != modeOnly) {
          continue;
        } else if (!bind.sessionIsKeyboardModeSupported(
            sessionId: ffi.sessionId, mode: mode.key)) {
          continue;
        }

        if (pi.isWayland && mode.key != kKeyMapMode) {
          continue;
        }

        var text = translate(mode.menu);
        if (mode.key == kKeyTranslateMode) {
          text = '$text beta';
        }
        list.add(RdoMenuButton<String>(
          child: Text(text),
          value: mode.key,
          groupValue: groupValue,
          onChanged: enabled ? onChanged : null,
          ffi: ffi,
        ));
      }
      return Column(children: list);
    });
  }

  localKeyboardType() {
    final localPlatform = getLocalPlatformForKBLayoutType(pi.platform);
    final visible = localPlatform != '';
    if (!visible) return Offstage();
    final enabled = !ffi.ffiModel.viewOnly;
    return Column(
      children: [
        Divider(),
        MenuButton(
          child: Text(
              '${translate('Local keyboard type')}: ${KBLayoutType.value}'),
          trailingIcon: const Icon(Icons.settings),
          ffi: ffi,
          onPressed: enabled
              ? () => showKBLayoutTypeChooser(localPlatform, ffi.dialogManager)
              : null,
        )
      ],
    );
  }

  inputSource() {
    final supportedInputSource = bind.mainSupportedInputSource();
    if (supportedInputSource.isEmpty) return Offstage();
    late final List<dynamic> supportedInputSourceList;
    try {
      supportedInputSourceList = jsonDecode(supportedInputSource);
    } catch (e) {
      debugPrint('Failed to decode $supportedInputSource, $e');
      return;
    }
    if (supportedInputSourceList.length < 2) return Offstage();
    final inputSource = stateGlobal.getInputSource();
    final enabled = !ffi.ffiModel.viewOnly;
    final children = <Widget>[Divider()];
    children.addAll(supportedInputSourceList.map((e) {
      final d = e as List<dynamic>;
      return RdoMenuButton<String>(
        child: Text(translate(d[1] as String)),
        value: d[0] as String,
        groupValue: inputSource,
        onChanged: enabled
            ? (v) async {
                if (v != null) {
                  await stateGlobal.setInputSource(ffi.sessionId, v);
                  // 释放原生输入；参见 RemotePage 中关于 macOS 的权衡说明。
                  if (isMacOS) ffi.inputModel.enterOrLeave(false);
                  await ffi.ffiModel.checkDesktopKeyboardMode();
                  await ffi.inputModel.updateKeyboardMode();
                }
              }
            : null,
        ffi: ffi,
      );
    }));
    return Column(children: children);
  }

  viewMode() {
    final ffiModel = ffi.ffiModel;
    final enabled = versionCmp(pi.version, '1.2.0') >= 0 && ffiModel.keyboard;
    return CkbMenuButton(
        value: ffiModel.viewOnly,
        onChanged: enabled
            ? (value) async {
                if (value == null) return;
                await bind.sessionToggleOption(
                    sessionId: ffi.sessionId, value: kOptionToggleViewOnly);
                final viewOnly = await bind.sessionGetToggleOption(
                    sessionId: ffi.sessionId, arg: kOptionToggleViewOnly);
                ffiModel.setViewOnly(id, viewOnly ?? value);
                final showMyCursor = await bind.sessionGetToggleOption(
                    sessionId: ffi.sessionId, arg: kOptionToggleShowMyCursor);
                ffiModel.setShowMyCursor(showMyCursor ?? value);
              }
            : null,
        ffi: ffi,
        child: Text(translate('View Mode')));
  }

  showMyCursor() {
    final ffiModel = ffi.ffiModel;
    return CkbMenuButton(
            value: ffiModel.showMyCursor,
            onChanged: (value) async {
              if (value == null) return;
              await bind.sessionToggleOption(
                  sessionId: ffi.sessionId, value: kOptionToggleShowMyCursor);
              final showMyCursor = await bind.sessionGetToggleOption(
                      sessionId: ffi.sessionId,
                      arg: kOptionToggleShowMyCursor) ??
                  value;
              ffiModel.setShowMyCursor(showMyCursor);

              // 仅当 showMyCursor 启用且 viewOnly 未启用时才设置为仅查看。
              if (showMyCursor && !ffiModel.viewOnly) {
                await bind.sessionToggleOption(
                    sessionId: ffi.sessionId, value: kOptionToggleViewOnly);
                final viewOnly = await bind.sessionGetToggleOption(
                    sessionId: ffi.sessionId, arg: kOptionToggleViewOnly);
                ffiModel.setViewOnly(id, viewOnly ?? value);
              }
            },
            ffi: ffi,
            child: Text(translate('Show my cursor')))
        .paddingOnly(left: 26.0);
  }

  mobileActions() {
    if (pi.platform != kPeerPlatformAndroid) return [];
    final enabled = versionCmp(pi.version, '1.2.7') >= 0;
    if (!enabled) return [];
    return [
      Divider(),
      MenuButton(
          child: Text(translate('Back')),
          onPressed: () => ffi.inputModel.onMobileBack(),
          ffi: ffi),
      MenuButton(
          child: Text(translate('Home')),
          onPressed: () => ffi.inputModel.onMobileHome(),
          ffi: ffi),
      MenuButton(
          child: Text(translate('Apps')),
          onPressed: () => ffi.inputModel.onMobileApps(),
          ffi: ffi),
      MenuButton(
          child: Text(translate('Volume up')),
          onPressed: () => ffi.inputModel.onMobileVolumeUp(),
          ffi: ffi),
      MenuButton(
          child: Text(translate('Volume down')),
          onPressed: () => ffi.inputModel.onMobileVolumeDown(),
          ffi: ffi),
      MenuButton(
          child: Text(translate('Power')),
          onPressed: () => ffi.inputModel.onMobilePower(),
          ffi: ffi),
    ];
  }
}

class _ChatMenu extends StatefulWidget {
  final String id;
  final FFI ffi;
  _ChatMenu({
    Key? key,
    required this.id,
    required this.ffi,
  }) : super(key: key);

  @override
  State<_ChatMenu> createState() => _ChatMenuState();
}

class _ChatMenuState extends State<_ChatMenu> {
  // 在 StatelessWidget 中使用会报 `Looking up a deactivated widget's ancestor is unsafe`。
  final chatButtonKey = GlobalKey();

  @override
  Widget build(BuildContext context) {
    if (isWeb) {
      return buildTextChatButton();
    } else {
      return _IconSubmenuButton(
          tooltip: 'Chat',
          key: chatButtonKey,
          svg: 'assets/chat.svg',
          ffi: widget.ffi,
          color: _ToolbarTheme.blueColor,
          hoverColor: _ToolbarTheme.hoverBlueColor,
          menuChildrenGetter: (_) => [textChat(), voiceCall()]);
    }
  }

  buildTextChatButton() {
    return _IconMenuButton(
      assetName: 'assets/message_24dp_5F6368.svg',
      tooltip: 'Text chat',
      key: chatButtonKey,
      onPressed: _textChatOnPressed,
      color: _ToolbarTheme.blueColor,
      hoverColor: _ToolbarTheme.hoverBlueColor,
    );
  }

  textChat() {
    return MenuButton(
        child: Text(translate('Text chat')),
        ffi: widget.ffi,
        onPressed: _textChatOnPressed);
  }

  _textChatOnPressed() {
    RenderBox? renderBox =
        chatButtonKey.currentContext?.findRenderObject() as RenderBox?;
    Offset? initPos;
    if (renderBox != null) {
      final pos = renderBox.localToGlobal(Offset.zero);
      initPos = Offset(pos.dx, pos.dy + _ToolbarTheme.dividerHeight);
    }
    widget.ffi.chatModel
        .changeCurrentKey(MessageKey(widget.ffi.id, ChatModel.clientModeID));
    widget.ffi.chatModel.toggleChatOverlay(chatInitPos: initPos);
  }

  voiceCall() {
    return MenuButton(
      child: Text(translate('Voice call')),
      ffi: widget.ffi,
      onPressed: () =>
          bind.sessionRequestVoiceCall(sessionId: widget.ffi.sessionId),
    );
  }
}

class _VoiceCallMenu extends StatelessWidget {
  final String id;
  final FFI ffi;
  _VoiceCallMenu({
    Key? key,
    required this.id,
    required this.ffi,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    menuChildrenGetter(_IconSubmenuButtonState state) {
      final audioInput = AudioInput(
        builder: (devices, currentDevice, setDevice) {
          return Column(
            children: devices
                .map((d) => RdoMenuButton<String>(
                      child: Container(
                        child: Text(
                          d,
                          overflow: TextOverflow.ellipsis,
                        ),
                        constraints: BoxConstraints(maxWidth: 250),
                      ),
                      value: d,
                      groupValue: currentDevice,
                      onChanged: (v) {
                        if (v != null) setDevice(v);
                      },
                      ffi: ffi,
                    ))
                .toList(),
          );
        },
        isCm: false,
        isVoiceCall: true,
      );
      return [
        audioInput,
        Divider(),
        MenuButton(
          child: Text(translate('End call')),
          onPressed: () => bind.sessionCloseVoiceCall(sessionId: ffi.sessionId),
          ffi: ffi,
        ),
      ];
    }

    return Obx(
      () {
        switch (ffi.chatModel.voiceCallStatus.value) {
          case VoiceCallStatus.waitingForResponse:
            return buildCallWaiting(context);
          case VoiceCallStatus.connected:
            return _IconSubmenuButton(
              tooltip: 'Voice call',
              svg: 'assets/voice_call.svg',
              color: _ToolbarTheme.blueColor,
              hoverColor: _ToolbarTheme.hoverBlueColor,
              menuChildrenGetter: menuChildrenGetter,
              ffi: ffi,
            );
          default:
            return Offstage();
        }
      },
    );
  }

  Widget buildCallWaiting(BuildContext context) {
    return _IconMenuButton(
      assetName: "assets/call_wait.svg",
      tooltip: "Waiting",
      onPressed: () => bind.sessionCloseVoiceCall(sessionId: ffi.sessionId),
      color: _ToolbarTheme.redColor,
      hoverColor: _ToolbarTheme.hoverRedColor,
    );
  }
}

class _RecordMenu extends StatelessWidget {
  const _RecordMenu({Key? key}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    var ffi = Provider.of<FfiModel>(context);
    var recordingModel = Provider.of<RecordingModel>(context);
    final hideRecordingButton =
        bind.mainGetLocalOption(key: kOptionHideRecordingButton) == 'Y';
    final visible = !hideRecordingButton &&
        (recordingModel.start || ffi.permissions['recording'] != false);
    if (!visible) return Offstage();
    return _IconMenuButton(
      assetName: 'assets/rec.svg',
      tooltip: recordingModel.start
          ? 'Stop session recording'
          : 'Start session recording',
      onPressed: () => recordingModel.toggle(),
      color: recordingModel.start
          ? _ToolbarTheme.redColor
          : _ToolbarTheme.blueColor,
      hoverColor: recordingModel.start
          ? _ToolbarTheme.hoverRedColor
          : _ToolbarTheme.hoverBlueColor,
    );
  }
}

class _CloseMenu extends StatelessWidget {
  final String id;
  final FFI ffi;
  const _CloseMenu({Key? key, required this.id, required this.ffi})
      : super(key: key);

  @override
  Widget build(BuildContext context) {
    return _IconMenuButton(
      assetName: 'assets/close.svg',
      tooltip: 'Close',
      onPressed: () async {
        if (await showConnEndAuditDialogCloseCanceled(ffi: ffi)) {
          return;
        }
        closeConnection(id: id);
      },
      color: _ToolbarTheme.redColor,
      hoverColor: _ToolbarTheme.hoverRedColor,
    );
  }
}

class _IconMenuButton extends StatefulWidget {
  final String? assetName;
  final Widget? icon;
  final String tooltip;
  final Color color;
  final Color hoverColor;
  final VoidCallback? onPressed;
  final double? hMargin;
  final double? vMargin;
  final bool topLevel;
  final double? width;
  const _IconMenuButton({
    Key? key,
    this.assetName,
    this.icon,
    required this.tooltip,
    required this.color,
    required this.hoverColor,
    required this.onPressed,
    this.hMargin,
    this.vMargin,
    this.topLevel = true,
    this.width,
  }) : super(key: key);

  @override
  State<_IconMenuButton> createState() => _IconMenuButtonState();
}

class _IconMenuButtonState extends State<_IconMenuButton> {
  bool hover = false;

  @override
  Widget build(BuildContext context) {
    assert(widget.assetName != null || widget.icon != null);
    final icon = widget.icon ??
        SvgPicture.asset(
          widget.assetName!,
          colorFilter: ColorFilter.mode(Colors.white, BlendMode.srcIn),
          width: _ToolbarTheme.buttonSize,
          height: _ToolbarTheme.buttonSize,
        );
    var button = SizedBox(
      width: widget.width ?? _ToolbarTheme.buttonSize,
      height: _ToolbarTheme.buttonSize,
      child: MenuItemButton(
          style: ButtonStyle(
              backgroundColor: MaterialStatePropertyAll(Colors.transparent),
              padding: MaterialStatePropertyAll(EdgeInsets.zero),
              overlayColor: MaterialStatePropertyAll(Colors.transparent)),
          onHover: (value) => setState(() {
                hover = value;
              }),
          onPressed: widget.onPressed,
          child: Tooltip(
            message: translate(widget.tooltip),
            child: Material(
                type: MaterialType.transparency,
                child: Ink(
                    decoration: BoxDecoration(
                      borderRadius:
                          BorderRadius.circular(_ToolbarTheme.iconRadius),
                      color: hover ? widget.hoverColor : widget.color,
                    ),
                    child: icon)),
          )),
    ).marginSymmetric(
        horizontal: widget.hMargin ?? _ToolbarTheme.buttonHMargin,
        vertical: widget.vMargin ?? _ToolbarTheme.buttonVMargin);
    button = Tooltip(
      message: translate(widget.tooltip),
      child: button,
    );
    if (widget.topLevel) {
      return MenuBar(children: [button]);
    } else {
      return button;
    }
  }
}

class _IconSubmenuButton extends StatefulWidget {
  final String tooltip;
  final String? svg;
  final Widget? icon;
  final Color color;
  final Color hoverColor;
  final List<Widget> Function(_IconSubmenuButtonState state) menuChildrenGetter;
  final MenuStyle? menuStyle;
  final FFI? ffi;
  final double? width;

  _IconSubmenuButton({
    Key? key,
    this.svg,
    this.icon,
    required this.tooltip,
    required this.color,
    required this.hoverColor,
    required this.menuChildrenGetter,
    this.ffi,
    this.menuStyle,
    this.width,
  }) : super(key: key);

  @override
  State<_IconSubmenuButton> createState() => _IconSubmenuButtonState();
}

class _IconSubmenuButtonState extends State<_IconSubmenuButton> {
  bool hover = false;

  @override // discard @protected
  void setState(VoidCallback fn) {
    super.setState(fn);
  }

  @override
  Widget build(BuildContext context) {
    assert(widget.svg != null || widget.icon != null);
    final icon = widget.icon ??
        SvgPicture.asset(
          widget.svg!,
          colorFilter: ColorFilter.mode(Colors.white, BlendMode.srcIn),
          width: _ToolbarTheme.buttonSize,
          height: _ToolbarTheme.buttonSize,
        );
    final button = SizedBox(
        width: widget.width ?? _ToolbarTheme.buttonSize,
        height: _ToolbarTheme.buttonSize,
        child: SubmenuButton(
            menuStyle:
                widget.menuStyle ?? _ToolbarTheme.defaultMenuStyle(context),
            style: _ToolbarTheme.defaultMenuButtonStyle,
            onHover: (value) => setState(() {
                  hover = value;
                }),
            child: Tooltip(
                message: translate(widget.tooltip),
                child: Material(
                    type: MaterialType.transparency,
                    child: Ink(
                        decoration: BoxDecoration(
                          borderRadius:
                              BorderRadius.circular(_ToolbarTheme.iconRadius),
                          color: hover ? widget.hoverColor : widget.color,
                        ),
                        child: icon))),
            menuChildren: widget
                .menuChildrenGetter(this)
                .map((e) => _buildPointerTrackWidget(e, widget.ffi))
                .toList()));
    return MenuBar(children: [
      button.marginSymmetric(
          horizontal: _ToolbarTheme.buttonHMargin,
          vertical: _ToolbarTheme.buttonVMargin)
    ]);
  }
}

class _SubmenuButton extends StatelessWidget {
  final List<Widget> menuChildren;
  final Widget? child;
  final FFI ffi;
  const _SubmenuButton({
    Key? key,
    required this.menuChildren,
    required this.child,
    required this.ffi,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return SubmenuButton(
      key: key,
      child: child,
      menuChildren:
          menuChildren.map((e) => _buildPointerTrackWidget(e, ffi)).toList(),
      menuStyle: _ToolbarTheme.defaultMenuStyle(context),
    );
  }
}

class MenuButton extends StatelessWidget {
  final VoidCallback? onPressed;
  final Widget? trailingIcon;
  final Widget? child;
  final FFI? ffi;
  MenuButton(
      {Key? key,
      this.onPressed,
      this.trailingIcon,
      required this.child,
      this.ffi})
      : super(key: key);

  @override
  Widget build(BuildContext context) {
    return MenuItemButton(
        key: key,
        onPressed: onPressed != null
            ? () {
                if (ffi != null) {
                  _menuDismissCallback(ffi!);
                }
                onPressed?.call();
              }
            : null,
        trailingIcon: trailingIcon,
        child: child);
  }
}

class CkbMenuButton extends StatelessWidget {
  final bool? value;
  final ValueChanged<bool?>? onChanged;
  final Widget? child;
  final FFI? ffi;
  const CkbMenuButton(
      {Key? key,
      required this.value,
      required this.onChanged,
      required this.child,
      this.ffi})
      : super(key: key);

  @override
  Widget build(BuildContext context) {
    return CheckboxMenuButton(
      key: key,
      value: value,
      child: child,
      onChanged: onChanged != null
          ? (bool? value) {
              if (ffi != null) {
                _menuDismissCallback(ffi!);
              }
              onChanged?.call(value);
            }
          : null,
    );
  }
}

class RdoMenuButton<T> extends StatelessWidget {
  final T value;
  final T? groupValue;
  final ValueChanged<T?>? onChanged;
  final Widget? child;
  final FFI? ffi;
  // 为 true 时，子菜单在激活后关闭；为 false 时保持打开。
  final bool closeOnActivate;
  const RdoMenuButton({
    Key? key,
    required this.value,
    required this.groupValue,
    required this.child,
    this.ffi,
    this.onChanged,
    this.closeOnActivate = true,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return RadioMenuButton(
      value: value,
      groupValue: groupValue,
      child: child,
      closeOnActivate: closeOnActivate,
      onChanged: onChanged != null
          ? (T? value) {
              if (ffi != null && closeOnActivate) {
                _menuDismissCallback(ffi!);
              }
              onChanged?.call(value);
            }
          : null,
    );
  }
}

class _DraggableShowHide extends StatefulWidget {
  final String id;
  final FFI ffi;
  final SessionID sessionId;
  final RxDouble fraction;
  final Rx<_ToolbarEdge> edge;
  final Rxn<_ToolbarEdge> previewEdge;
  final Rxn<double> previewFraction;
  final Rxn<Size> toolbarSize;
  final VoidCallback markDragEpoch;
  final VoidCallback syncDockingOptionsAfterDragIfNeeded;
  final bool isHorizontal;
  // 本次会话是否启用多边缘停靠（在
  // 设置 -> 其他 中切换）。为 false 时，拖拽手柄让工具栏
  // 在顶部边缘水平滑动，不会切换边缘。
  final bool multiEdgeEnabled;
  final RxBool dragging;
  final ToolbarState toolbarState;
  final BorderRadius borderRadius;

  final Function(bool) setFullscreen;
  final Function() setMinimize;

  const _DraggableShowHide({
    Key? key,
    required this.id,
    required this.ffi,
    required this.sessionId,
    required this.fraction,
    required this.edge,
    required this.previewEdge,
    required this.previewFraction,
    required this.toolbarSize,
    required this.markDragEpoch,
    required this.syncDockingOptionsAfterDragIfNeeded,
    required this.isHorizontal,
    required this.multiEdgeEnabled,
    required this.dragging,
    required this.toolbarState,
    required this.setFullscreen,
    required this.setMinimize,
    required this.borderRadius,
  }) : super(key: key);

  @override
  State<_DraggableShowHide> createState() => _DraggableShowHideState();
}

class _DraggableShowHideState extends State<_DraggableShowHide> {
  double left = 0.0;
  double right = 1.0;
  Offset? _lastPointerDown;
  Offset? _dragGrabOffset;
  double? _dragLongAxisGrabOffset;
  Size? _dragToolbarSize;

  RxBool get collapse => widget.toolbarState.collapse;

  @override
  initState() {
    super.initState();

    final confLeft = double.tryParse(
        bind.mainGetLocalOption(key: kOptionRemoteMenubarDragLeft));
    if (confLeft == null) {
      bind.mainSetLocalOption(
          key: kOptionRemoteMenubarDragLeft, value: left.toString());
    } else {
      left = confLeft;
    }
    final confRight = double.tryParse(
        bind.mainGetLocalOption(key: kOptionRemoteMenubarDragRight));
    if (confRight == null) {
      bind.mainSetLocalOption(
          key: kOptionRemoteMenubarDragRight, value: right.toString());
    } else {
      right = confRight;
    }
  }

  // 应用于当前预览边缘的偏置，使在两条边缘之间悬停的拖拽
  // 不会闪烁。仅在启用多边缘时相关。
  static const double _switchHysteresisPx = 50.0;

  _ToolbarEdge _nearestToolbarEdge(Offset cursor, Size mediaSize) {
    if (!widget.multiEdgeEnabled) return widget.edge.value;

    double rawDist(_ToolbarEdge e) {
      switch (e) {
        case _ToolbarEdge.top:
          return cursor.dy;
        case _ToolbarEdge.bottom:
          return mediaSize.height - cursor.dy;
        case _ToolbarEdge.left:
          return cursor.dx;
        case _ToolbarEdge.right:
          return mediaSize.width - cursor.dx;
      }
    }

    final previewed = widget.previewEdge.value;
    var winner = widget.edge.value;
    var best = double.infinity;
    for (final e in _ToolbarEdge.values) {
      final biased =
          e == previewed ? rawDist(e) - _switchHysteresisPx : rawDist(e);
      if (biased < best) {
        best = biased;
        winner = e;
      }
    }
    return winner;
  }

  void _ensureDragGrabOffset(Offset cursor) {
    if (_dragGrabOffset != null) return;
    final mediaSize = MediaQueryData.fromView(View.of(context)).size;
    final toolbarSize =
        _toolbarSizeForEdge(widget.edge.value, widget.toolbarSize.value);
    _dragToolbarSize = toolbarSize;
    final toolbarOffset = _toolbarOffsetForEdge(
      edge: widget.edge.value,
      fraction: widget.fraction.value,
      parentSize: mediaSize,
      toolbarSize: toolbarSize,
    );
    _dragGrabOffset = cursor - toolbarOffset;
    _dragLongAxisGrabOffset = _isHorizontalEdge(widget.edge.value)
        ? _dragGrabOffset?.dx
        : _dragGrabOffset?.dy;
  }

  double _dragGrabOffsetForEdge(_ToolbarEdge edge, Size toolbarSize) {
    final offset = _dragLongAxisGrabOffset ?? 0;
    final extent =
        _isHorizontalEdge(edge) ? toolbarSize.width : toolbarSize.height;
    return _clampToolbarFraction(offset, 0, extent);
  }

  void _updatePreview(Offset cursor) {
    _ensureDragGrabOffset(cursor);
    final mediaSize = MediaQueryData.fromView(View.of(context)).size;
    final winner = _nearestToolbarEdge(cursor, mediaSize);
    widget.previewEdge.value = winner;

    final toolbarSize = _toolbarSizeForEdge(winner, _dragToolbarSize);
    final grabOffset = _dragGrabOffsetForEdge(winner, toolbarSize);
    final double frac;
    if (winner == _ToolbarEdge.top || winner == _ToolbarEdge.bottom) {
      frac = _fractionForAlignedDrag(
        cursor: cursor.dx,
        grabOffset: grabOffset,
        parentExtent: mediaSize.width,
        toolbarExtent: toolbarSize.width,
        left: left,
        right: right,
      );
    } else {
      final fractionBounds = _fractionBoundsForEdge(winner, left, right);
      frac = _fractionForAlignedDrag(
        cursor: cursor.dy,
        grabOffset: grabOffset,
        parentExtent: mediaSize.height,
        toolbarExtent: toolbarSize.height,
        left: fractionBounds.left,
        right: fractionBounds.right,
      );
    }
    widget.previewFraction.value = frac;
  }

  void _resetDragTracking() {
    _lastPointerDown = null;
    _dragGrabOffset = null;
    _dragLongAxisGrabOffset = null;
    _dragToolbarSize = null;
  }

  void _commitPreview() {
    final newEdge = widget.previewEdge.value;
    final frac = widget.previewFraction.value;
    widget.previewEdge.value = null;
    widget.previewFraction.value = null;
    widget.dragging.value = false;
    widget.markDragEpoch();
    _resetDragTracking();
    widget.syncDockingOptionsAfterDragIfNeeded();
    if (newEdge == null || frac == null) return;
    widget.edge.value = newEdge;
    widget.fraction.value = frac;
    _cacheToolbarDockingOptions(
      sessionId: widget.sessionId,
      edge: newEdge,
      fraction: frac,
      multiEdgeEnabled: widget.multiEdgeEnabled,
    );
    bind.sessionPeerOption(
      sessionId: widget.sessionId,
      name: kOptionRemoteMenubarEdge,
      value: _toolbarEdgeToString(newEdge),
    );
    bind.sessionPeerOption(
      sessionId: widget.sessionId,
      name: kOptionRemoteMenubarFraction,
      value: frac.toString(),
    );
    if (widget.multiEdgeEnabled) {
      return;
    }
    bind.sessionPeerOption(
      sessionId: widget.sessionId,
      name: _legacyRemoteMenubarDragX,
      value: frac.toString(),
    );
  }

  Widget _buildDraggable(BuildContext context) {
    return Listener(
      onPointerDown: (event) => _lastPointerDown = event.position,
      child: Draggable(
        // 关闭多边缘停靠时，工具栏停留在顶部边缘，
        // 因此把反馈限制为水平移动——否则拖拽时手柄
        // 会从顶部飘走，工具栏看起来
        // 像是没有锚定。启用多边缘时需要二维拖拽来实现吸附边缘。
        axis: widget.multiEdgeEnabled ? null : Axis.horizontal,
        child: Icon(
          widget.isHorizontal ? Icons.drag_indicator : Icons.drag_handle,
          size: 20,
          color: MyTheme.color(context).drag_indicator,
        ),
        feedback: widget,
        onDragStarted: () {
          widget.markDragEpoch();
          final pointerDown = _lastPointerDown;
          if (pointerDown != null) {
            _ensureDragGrabOffset(pointerDown);
          }
          widget.dragging.value = true;
          // 以当前停靠的边缘/比例初始化预览，使得
          // 拖拽一开始就能显示内容，早于第一次 onDragUpdate。
          widget.previewEdge.value = widget.edge.value;
          widget.previewFraction.value = widget.fraction.value;
        },
        onDragUpdate: (details) {
          _updatePreview(details.globalPosition);
        },
        onDragEnd: (_) => _commitPreview(),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final ButtonStyle buttonStyle = ButtonStyle(
      minimumSize: MaterialStateProperty.all(const Size(0, 0)),
      padding: MaterialStateProperty.all(EdgeInsets.zero),
    );
    final isFullscreen = stateGlobal.fullscreen;
    const double iconSize = 20;

    buttonWrapper(VoidCallback? onPressed, Widget child,
        {Color hoverColor = _ToolbarTheme.blueColor}) {
      final bgColor = buttonStyle.backgroundColor?.resolve({});
      return TextButton(
        onPressed: onPressed,
        child: child,
        style: buttonStyle.copyWith(
          backgroundColor: MaterialStateProperty.resolveWith((states) {
            if (states.contains(MaterialState.hovered)) {
              return (bgColor ?? hoverColor).withOpacity(0.15);
            }
            return bgColor;
          }),
        ),
      );
    }

    final axis = widget.isHorizontal ? Axis.horizontal : Axis.vertical;
    final child = Flex(
      direction: axis,
      mainAxisSize: MainAxisSize.min,
      children: [
        _buildDraggable(context),
        Obx(() => collapse.isTrue
            ? _MinimizedMonitorSwitchButton(id: widget.id, ffi: widget.ffi)
            : const Offstage()),
        Obx(() => buttonWrapper(
              () {
                widget.setFullscreen(!isFullscreen.value);
              },
              Tooltip(
                message: translate(
                    isFullscreen.isTrue ? 'Exit Fullscreen' : 'Fullscreen'),
                child: Icon(
                  isFullscreen.isTrue
                      ? Icons.fullscreen_exit
                      : Icons.fullscreen,
                  size: iconSize,
                ),
              ),
            )),
        if (!isMacOS && !isWebDesktop)
          Obx(() => Offstage(
                offstage: isFullscreen.isFalse,
                child: buttonWrapper(
                  widget.setMinimize,
                  Tooltip(
                    message: translate('Minimize'),
                    child: Icon(
                      Icons.remove,
                      size: iconSize,
                    ),
                  ),
                ),
              )),
        buttonWrapper(
          () => setState(() {
            widget.toolbarState.switchCollapse(widget.sessionId);
          }),
          Obx((() => Tooltip(
                message: translate(
                    collapse.isFalse ? 'Hide Toolbar' : 'Show Toolbar'),
                child: Icon(
                  _toolbarCollapseIcon(widget.edge.value, collapse.isTrue),
                  size: iconSize,
                ),
              ))),
        ),
        if (isWebDesktop)
          Obx(() {
            if (collapse.isFalse) {
              return Offstage();
            } else {
              return buttonWrapper(
                () => closeConnection(id: widget.id),
                Tooltip(
                  message: translate('Close'),
                  child: Icon(
                    Icons.close,
                    size: iconSize,
                    color: _ToolbarTheme.redColor,
                  ),
                ),
                hoverColor: _ToolbarTheme.redColor,
              ).paddingOnly(left: iconSize / 2);
            }
          })
      ],
    );
    return TextButtonTheme(
      data: TextButtonThemeData(style: buttonStyle),
      child: Container(
        decoration: BoxDecoration(
          color: Theme.of(context)
              .menuBarTheme
              .style
              ?.backgroundColor
              ?.resolve(MaterialState.values.toSet()),
          border: Border.all(
            color: _ToolbarTheme.borderColor(context),
            width: 1,
          ),
          borderRadius: widget.borderRadius,
        ),
        child: SizedBox(
          height: widget.isHorizontal ? 20 : null,
          width: widget.isHorizontal ? null : 20,
          child: child,
        ),
      ),
    );
  }
}

class InputModeMenu {
  final String key;
  final String menu;

  InputModeMenu({required this.key, required this.menu});
}

_menuDismissCallback(FFI ffi) => ffi.inputModel.refreshMousePos();

Widget _buildPointerTrackWidget(Widget child, FFI? ffi) {
  return Listener(
    onPointerHover: (PointerHoverEvent e) => {
      if (ffi != null) {ffi.inputModel.lastMousePos = e.position}
    },
    child: MouseRegion(
      child: child,
    ),
  );
}

class EdgeThicknessControl extends StatelessWidget {
  final double value;
  final ValueChanged<double>? onChanged;
  final ColorScheme? colorScheme;

  const EdgeThicknessControl({
    Key? key,
    required this.value,
    this.onChanged,
    this.colorScheme,
  }) : super(key: key);

  static const double kMin = 20;
  static const double kMax = 150;

  @override
  Widget build(BuildContext context) {
    final colorScheme = this.colorScheme ?? Theme.of(context).colorScheme;

    final slider = SliderTheme(
      data: SliderTheme.of(context).copyWith(
        activeTrackColor: colorScheme.primary,
        thumbColor: colorScheme.primary,
        overlayColor: colorScheme.primary.withOpacity(0.1),
        showValueIndicator: ShowValueIndicator.never,
        thumbShape: _RectValueThumbShape(
          min: EdgeThicknessControl.kMin,
          max: EdgeThicknessControl.kMax,
          width: 52,
          height: 24,
          radius: 4,
          unit: 'px',
        ),
      ),
      child: Semantics(
        value: value.toInt().toString(),
        child: Slider(
          value: value,
          min: EdgeThicknessControl.kMin,
          max: EdgeThicknessControl.kMax,
          divisions:
              (EdgeThicknessControl.kMax - EdgeThicknessControl.kMin).round(),
          semanticFormatterCallback: (double newValue) =>
              "${newValue.round()}px",
          onChanged: onChanged,
        ),
      ),
    );

    return slider;
  }
}

class _MinimizedMonitorSwitchButton extends StatelessWidget {
  final String id;
  final FFI ffi;

  const _MinimizedMonitorSwitchButton({
    Key? key,
    required this.id,
    required this.ffi,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    const double iconSize = 20;
    final cycle = _MonitorCycle(id, ffi);

    return Obx(() {
      final label = cycle.label;
      if (!mainGetLocalBoolOptionSync(kOptionAllowMonitorSwitchMainToolbar) ||
          !mainGetLocalBoolOptionSync(kOptionAllowMonitorSwitchMinToolbar)) {
        return const Offstage();
      }
      if (cycle.total < 2) return const Offstage();
      final privacyModeState = PrivacyModeState.find(id);
      if (privacyModeState.isNotEmpty &&
          !allowDisplaySwitchInPrivacyMode(
              ffi.ffiModel.pi, privacyModeState.value)) {
        return const Offstage();
      }

      return Tooltip(
        message: cycle.tooltip,
        child: TextButton(
          onPressed: cycle.next,
          style: ButtonStyle(
            minimumSize: MaterialStateProperty.all(const Size(0, 0)),
            padding: MaterialStateProperty.all(EdgeInsets.zero),
            backgroundColor: MaterialStateProperty.resolveWith((states) {
              if (states.contains(MaterialState.hovered)) {
                return _ToolbarTheme.blueColor.withOpacity(0.15);
              }
              return null;
            }),
          ),
          child: Stack(
            alignment: const Alignment(0, -0.125),
            children: [
              SvgPicture.asset(
                'assets/display_switcher.svg',
                colorFilter:
                    ColorFilter.mode(_ToolbarTheme.blueColor, BlendMode.srcIn),
                width: iconSize,
                height: iconSize,
              ),
              Text(
                label,
                style: const TextStyle(
                  color: Colors.white,
                  fontSize: 9,
                  height: 1,
                  fontWeight: FontWeight.bold,
                ),
              ),
            ],
          ),
        ),
      );
    });
  }
}
