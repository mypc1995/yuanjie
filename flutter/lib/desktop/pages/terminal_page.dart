import 'dart:async';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_hbb/models/terminal_model.dart';
import 'package:flutter_hbb/models/terminal_mouse_handler.dart';
import 'terminal_connection_manager.dart';

class TerminalPage extends StatefulWidget {
  TerminalPage({
    Key? key,
    required this.id,
    required this.password,
    required this.tabController,
    required this.isSharedPassword,
    required this.terminalId,
    required this.tabKey,
    this.forceRelay,
    this.connToken,
    this.onClipboardWriteBlocked,
    this.onClipboardWriteSucceeded,
  }) : super(key: key);
  final String id;
  final String? password;
  final DesktopTabController tabController;
  final bool? forceRelay;
  final bool? isSharedPassword;
  final String? connToken;
  final ValueChanged<String>? onClipboardWriteBlocked;
  final ValueChanged<String>? onClipboardWriteSucceeded;
  final int terminalId;

  /// 用于焦点管理的 Tab 键，由父级传入以避免重复创建
  final String tabKey;
  final SimpleWrapper<State<TerminalPage>?> _lastState = SimpleWrapper(null);

  FFI get ffi => (_lastState.value! as _TerminalPageState)._ffi;

  @override
  State<TerminalPage> createState() {
    final state = _TerminalPageState();
    _lastState.value = state;
    return state;
  }
}

class _TerminalPageState extends State<TerminalPage>
    with AutomaticKeepAliveClientMixin {
  static const EdgeInsets _defaultTerminalPadding =
      EdgeInsets.symmetric(horizontal: 5.0, vertical: 2.0);

  late FFI _ffi;
  late TerminalModel _terminalModel;
  double? _cellHeight;
  final FocusNode _terminalFocusNode = FocusNode(canRequestFocus: false);
  StreamSubscription<DesktopTabState>? _tabStateSubscription;

  @override
  void initState() {
    super.initState();

    // 监听标签页选中变化以请求焦点
    _tabStateSubscription = widget.tabController.state.listen(_onTabStateChanged);

    // 使用连接管理器提供的共享 FFI 实例
    _ffi = TerminalConnectionManager.getConnection(
      peerId: widget.id,
      password: widget.password,
      isSharedPassword: widget.isSharedPassword,
      forceRelay: widget.forceRelay,
      connToken: widget.connToken,
    );

    // 用特定的终端 ID 创建终端模型
    _terminalModel = TerminalModel(_ffi, widget.terminalId);
    _terminalModel.onClipboardWriteBlocked = widget.onClipboardWriteBlocked;
    _terminalModel.onClipboardWriteSucceeded = widget.onClipboardWriteSucceeded;
    debugPrint(
        '[TerminalPage] Terminal model created for terminal ${widget.terminalId}');

    _terminalModel.onResizeExternal = (w, h, pw, ph) {
      _cellHeight = ph * 1.0;

      // 终端具备有效尺寸后（第一次有效调整大小）启用焦点
      if (!_terminalFocusNode.canRequestFocus && w > 0 && h > 0) {
        _terminalFocusNode.canRequestFocus = true;
        // 如果该标签页当前被选中则自动聚焦
        _requestFocusIfSelected();
      }

      // 把 setState 安排到下一帧
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) {
          setState(() {});
        }
      });
    };

    // 把该终端模型注册到 FFI 以进行事件路由
    _ffi.registerTerminalModel(widget.terminalId, _terminalModel);

    // shell 退出时自动关闭标签页
    _terminalModel.onClosed = () {
      if (mounted) {
        widget.tabController.closeBy(widget.tabKey);
      }
    };

    // 初始化终端连接
    WidgetsBinding.instance.addPostFrameCallback((_) {
      widget.tabController.onSelected?.call(widget.id);

      // 检查这是新连接还是新增终端
      // 注意：连接已存在时，加入该终端后引用计数将 > 1
      final isExistingConnection =
          TerminalConnectionManager.hasConnection(widget.id) &&
              TerminalConnectionManager.getTerminalCount(widget.id) > 1;

      if (!isExistingConnection) {
        // 第一个终端 —— 显示加载对话框，等待 onReady
        _ffi.dialogManager
            .showLoading(translate('Connecting...'), onCancel: closeConnection);
      } else {
        // 新增终端 —— 连接已经建立
        // 直接打开终端
        _terminalModel.openTerminal();
      }
    });
  }

  @override
  void dispose() {
    // 取消标签页状态订阅，防止内存泄漏
    _tabStateSubscription?.cancel();
    // 从 FFI 注销终端模型
    _ffi.unregisterTerminalModel(widget.terminalId);
    _terminalModel.dispose();
    _terminalFocusNode.dispose();
    // 释放连接引用，而不是直接关闭
    TerminalConnectionManager.releaseConnection(widget.id);
    super.dispose();
  }

  void _onTabStateChanged(DesktopTabState state) {
    // 检查该标签页现在是否被选中并请求焦点
    if (state.selected >= 0 && state.selected < state.tabs.length) {
      final selectedTab = state.tabs[state.selected];
      if (selectedTab.key == widget.tabKey && mounted) {
        _requestFocusIfSelected();
      }
    }
  }

  void _requestFocusIfSelected() {
    if (!mounted || !_terminalFocusNode.canRequestFocus) return;
    // 使用 post-frame 回调确保组件已完全布局在焦点树中
    WidgetsBinding.instance.addPostFrameCallback((_) {
      // 帧结束后重新检查条件：已挂载、可聚焦、仍被选中、尚未获得焦点
      if (!mounted || !_terminalFocusNode.canRequestFocus || _terminalFocusNode.hasFocus) return;
      final state = widget.tabController.state.value;
      if (state.selected >= 0 && state.selected < state.tabs.length) {
        if (state.tabs[state.selected].key == widget.tabKey) {
          _terminalFocusNode.requestFocus();
        }
      }
    });
  }

  // 该方法通过计算
  // 可用高度除以单个
  // 终端行高度（`_cellHeight`）后剩余的额外空间，并将其均分为上下内边距，使可见行数为整数。
  EdgeInsets _calculatePadding(double heightPx) {
    final cellHeight = _cellHeight;
    if (!heightPx.isFinite ||
        heightPx <= 0 ||
        cellHeight == null ||
        !cellHeight.isFinite ||
        cellHeight <= 0) {
      return _defaultTerminalPadding;
    }
    final rows = (heightPx / cellHeight).floor();
    if (rows <= 0) {
      return _defaultTerminalPadding;
    }
    final extraSpace = heightPx - rows * cellHeight;
    if (!extraSpace.isFinite || extraSpace < 0) {
      return _defaultTerminalPadding;
    }
    final topBottom = extraSpace / 2.0;
    return EdgeInsets.symmetric(
      horizontal: _defaultTerminalPadding.horizontal / 2,
      vertical: topBottom,
    );
  }

  @override
  Widget build(BuildContext context) {
    super.build(context);
    return Scaffold(
      backgroundColor: Theme.of(context).scaffoldBackgroundColor,
      body: LayoutBuilder(
        builder: (context, constraints) {
          final heightPx = constraints.maxHeight;
          return TerminalMouseInteraction(
            _terminalModel.terminal,
            controller: _terminalModel.terminalController,
            focusNode: _terminalFocusNode,
            // 注意：这里不使用 autofocus，因为焦点是手动管理的
            // （通过 _onTabStateChanged() 管理），以便正确处理标签页切换。
            backgroundOpacity: 0.7,
            padding: _calculatePadding(heightPx),
            onSecondaryTapDown: (details, offset) async {
              final selection = _terminalModel.terminalController.selection;
              if (selection != null) {
                final text = _terminalModel.terminal.buffer.getText(selection);
                _terminalModel.terminalController.clearSelection();
                await Clipboard.setData(ClipboardData(text: text));
              } else {
                final data = await Clipboard.getData('text/plain');
                final text = data?.text;
                if (text != null) {
                  _terminalModel.terminal.paste(text);
                }
              }
            },
          );
        },
      ),
    );
  }

  @override
  bool get wantKeepAlive => true;
}
