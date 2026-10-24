import 'dart:async';
import 'dart:math';
import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/common/widgets/dialog.dart';
import 'package:flutter_hbb/models/input_modifier_utils.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:flutter_hbb/models/terminal_copy_shortcut.dart';
import 'package:flutter_hbb/models/terminal_model.dart';
import 'package:flutter_hbb/models/terminal_mouse_handler.dart';
import 'package:flutter_hbb/mobile/terminal_keyboard_utils.dart';
import 'package:flutter_hbb/web/dummy.dart'
    if (dart.library.html) 'package:flutter_hbb/web/terminal_font.dart';
import 'package:google_fonts/google_fonts.dart';
import 'package:xterm/xterm.dart';
import '../../desktop/pages/terminal_connection_manager.dart';
import '../../consts.dart';

const _terminalBackgroundOpacity = 0.7;

Widget _buildTerminalViewForPlatform({
  required bool reportMouseInput,
  required bool reportTouchInput,
  required Terminal terminal,
  required TerminalController controller,
  required TerminalStyle textStyle,
  required EdgeInsets padding,
  required bool deleteDetection,
  required Map<ShortcutActivator, Intent>? shortcuts,
  required FocusOnKeyEventCallback onKeyEvent,
  required void Function(TapDownDetails, CellOffset) onSecondaryTapDown,
}) {
  if (reportMouseInput || reportTouchInput) {
    return TerminalMouseInteraction(
      terminal,
      controller: controller,
      autofocus: true,
      textStyle: textStyle,
      deleteDetection: deleteDetection,
      reportTouchInput: reportTouchInput,
      shortcuts: shortcuts,
      onKeyEvent: onKeyEvent,
      backgroundOpacity: _terminalBackgroundOpacity,
      padding: padding,
      onSecondaryTapDown: onSecondaryTapDown,
    );
  }
  return TerminalView(
    terminal,
    controller: controller,
    autofocus: true,
    textStyle: textStyle,
    deleteDetection: deleteDetection,
    shortcuts: shortcuts,
    onKeyEvent: onKeyEvent,
    backgroundOpacity: _terminalBackgroundOpacity,
    padding: padding,
    onSecondaryTapDown: onSecondaryTapDown,
  );
}

class TerminalPage extends StatefulWidget {
  const TerminalPage({
    Key? key,
    required this.id,
    required this.password,
    required this.isSharedPassword,
    this.forceRelay,
    this.connToken,
  }) : super(key: key);
  final String id;
  final String? password;
  final bool? forceRelay;
  final bool? isSharedPassword;
  final String? connToken;
  final terminalId = 0;

  @override
  State<TerminalPage> createState() => _TerminalPageState();
}

class _TerminalPageState extends State<TerminalPage>
    with AutomaticKeepAliveClientMixin, WidgetsBindingObserver {
  bool get _canConfigureTerminalClipboardPermission =>
      canConfigureTerminalClipboardPermission(
        settingsDisabled: bind.isDisableSettings(),
        optionFixed: isOptionFixed(kOptionAllowTerminalClipboardWrite),
      );
  bool get _canHandleTerminalClipboardWriteRequest =>
      canHandleTerminalClipboardWriteRequest(
        localOption: bind.mainGetLocalOption(
          key: kOptionAllowTerminalClipboardWrite,
        ),
        canConfigurePermission: _canConfigureTerminalClipboardPermission,
      );

  late FFI _ffi;
  late TerminalModel _terminalModel;
  double? _cellHeight;
  double _sysKeyboardHeight = 0;
  Timer? _keyboardDebounce;
  final GlobalKey _keyboardKey = GlobalKey();
  double _keyboardHeight = 0;
  late bool _showTerminalExtraKeys;
  // 虚拟键盘的 Ctrl 锁定状态：当前按下的按键会被映射为控制码
  bool _ctrlLocked = false;
  bool _altLocked = false;
  // 紧凑键盘布局中 Row3 的展开/折叠状态
  bool _row3Expanded = false;
  // 用于 iOS 的边缘滑动手势
  double _swipeStartX = 0;
  double _swipeCurrentX = 0;
  ScaffoldFeatureController<MaterialBanner, MaterialBannerClosedReason>?
      _terminalClipboardNoticeController;
  final _terminalClipboardNotice = TerminalClipboardNoticeCoordinator<int>();

  // 仅用于 web。
  // 'monospace' 在 web 上无效，请使用 Google Fonts，`??` 只是用于空安全。
  final String _robotoMonoFontFamily = isWeb
      ? (GoogleFonts.robotoMono().fontFamily ?? 'monospace')
      : 'monospace';

  SessionID get sessionId => _ffi.sessionId;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);

    if (isWeb) {
      loadLocalTerminalFontIfNeeded();
    }

    debugPrint(
        '[TerminalPage] Initializing terminal ${widget.terminalId} for peer ${widget.id}');

    // 使用连接管理器提供的共享 FFI 实例
    _ffi = TerminalConnectionManager.getConnection(
      peerId: widget.id,
      password: widget.password,
      isSharedPassword: widget.isSharedPassword,
      forceRelay: widget.forceRelay,
      connToken: widget.connToken,
    );

    // 用指定的终端 ID 创建终端模型
    _terminalModel = TerminalModel(_ffi, widget.terminalId);
    if (_canHandleTerminalClipboardWriteRequest) {
      _terminalModel.onClipboardWriteBlocked =
          _handleTerminalClipboardWriteBlocked;
      _terminalModel.onClipboardWriteSucceeded =
          _handleTerminalClipboardWriteSucceeded;
    }
    debugPrint(
        '[TerminalPage] Terminal model created for terminal ${widget.terminalId}');

    _terminalModel.onResizeExternal = (w, h, pw, ph) {
      _cellHeight = ph * 1.0;
    };

    // 把该终端模型注册到 FFI 以进行事件路由
    _ffi.registerTerminalModel(widget.terminalId, _terminalModel);

    // shell 退出时自动关闭连接
    _terminalModel.onClosed = () {
      if (mounted) {
        closeConnection(id: widget.id);
      }
    };

    // Web 桌面端用户可以完全使用硬件键盘，因此屏幕上的
    // 终端扩展按键栏没有必要，已禁用。
    _showTerminalExtraKeys = !isWebDesktop &&
        mainGetLocalBoolOptionSync(kOptionEnableShowTerminalExtraKeys);
    _terminalModel.isCtrlLocked = () => _ctrlLocked;
    _terminalModel.clearCtrlLock = () {
      if (_ctrlLocked) setState(() => _ctrlLocked = false);
    };
    _terminalModel.isAltLocked = () => _altLocked;
    _terminalModel.clearAltLock = () {
      if (_altLocked) setState(() => _altLocked = false);
    };
    // 从持久化存储中加载 Row3 的展开/折叠状态。直接读取原始选项
    // 可以在尚未保存任何值时保持 Row3 折叠。
    _row3Expanded =
        bind.mainGetLocalOption(key: kOptionShowTerminalCtrlKeys) == 'Y';
    // 初始化终端连接
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _ffi.dialogManager
          .showLoading(translate('Connecting...'), onCancel: closeConnection);

      if (_showTerminalExtraKeys) {
        _updateKeyboardHeight();
      }
    });
    _ffi.ffiModel.updateEventListener(_ffi.sessionId, widget.id);
  }

  void _handleTerminalClipboardWriteBlocked(String clipboardText) {
    if (!mounted) return;
    final option = bind.mainGetLocalOption(
      key: kOptionAllowTerminalClipboardWrite,
    );
    final request = _terminalClipboardNotice.recordBlocked(
      source: widget.terminalId,
      text: clipboardText,
      option: option,
      canWrite: (_) => _canWriteTerminalClipboard,
    );
    if (request != null) _showTerminalClipboardNotice(request);
  }

  void _showTerminalClipboardNotice(
    TerminalClipboardNoticeRequest<int> request,
  ) {
    final controller = ScaffoldMessenger.of(context).showMaterialBanner(
      MaterialBanner(
        leading: const Icon(Icons.content_copy_outlined),
        content: Text(translate(kTerminalClipboardNoticeMessageKey)),
        actions: [
          AnimatedBuilder(
            animation: _terminalClipboardNotice,
            builder: (_, __) => TextButton(
              onPressed: _terminalClipboardNotice.canClaimAction
                  ? _handleTerminalClipboardNegativeAction
                  : null,
              child: Text(translate(request.negativeActionKey)),
            ),
          ),
          AnimatedBuilder(
            animation: _terminalClipboardNotice,
            builder: (_, __) => TextButton(
              onPressed: _terminalClipboardNotice.canClaimAction
                  ? _handleTerminalClipboardPositiveAction
                  : null,
              child: Text(translate(request.actionKey)),
            ),
          ),
        ],
      ),
    );
    _terminalClipboardNoticeController = controller;
    unawaited(controller.closed.then<void>((_) {
      if (identical(_terminalClipboardNoticeController, controller)) {
        _terminalClipboardNoticeController = null;
        _terminalClipboardNotice.noticeClosed();
      }
    }));
  }

  void _handleTerminalClipboardNegativeAction() {
    final request = _terminalClipboardNotice.claimCurrentAction();
    if (request == null) return;
    if (request.persistAllowed) {
      unawaited(_declineTerminalClipboardWrite());
    } else {
      _closeTerminalClipboardNotice();
    }
  }

  void _handleTerminalClipboardPositiveAction() {
    final request = _terminalClipboardNotice.claimCurrentAction();
    if (request == null) return;
    unawaited(_completeTerminalClipboardWrite(request));
  }

  bool get _canWriteTerminalClipboard =>
      _canHandleTerminalClipboardWriteRequest &&
      !_ffi.closed &&
      _ffi.ffiModel.permissions['clipboard'] != false;

  void _handleTerminalClipboardWriteSucceeded(String _) {
    _closeTerminalClipboardNotice();
  }

  Future<void> _declineTerminalClipboardWrite() async {
    try {
      await bind.mainSetLocalOption(
        key: kOptionAllowTerminalClipboardWrite,
        value: kTerminalClipboardWriteDenied,
      );
    } catch (error) {
      debugPrint(
          '[TerminalPage] Failed to save terminal clipboard permission: $error');
      return;
    } finally {
      _terminalClipboardNotice.releaseAction();
    }
    _closeTerminalClipboardNotice();
  }

  Future<void> _completeTerminalClipboardWrite(
    TerminalClipboardNoticeRequest<int> request,
  ) async {
    var completed = false;
    try {
      completed = await completeTerminalClipboardWrite(
        clipboardText: request.text,
        canWrite: () => _canWriteTerminalClipboard,
        writeClipboard: writeTerminalClipboard,
        persistAllowed: request.persistAllowed
            ? () => bind.mainSetLocalOption(
                  key: kOptionAllowTerminalClipboardWrite,
                  value: kTerminalClipboardWriteAllowed,
                )
            : null,
      );
    } catch (error) {
      debugPrint(
          '[TerminalPage] Failed to complete terminal clipboard write: $error');
    } finally {
      _terminalClipboardNotice.releaseAction();
    }
    if (!completed) return;
    _closeTerminalClipboardNotice();
  }

  void _closeTerminalClipboardNotice() {
    if (!_terminalClipboardNotice.beginClose()) return;
    final controller = _terminalClipboardNoticeController;
    if (controller == null) {
      debugPrint('[TerminalPage] Clipboard notice controller is missing');
      _terminalClipboardNotice.noticeClosed();
      return;
    }
    controller.close();
  }

  @override
  void dispose() {
    // 从 FFI 注销终端模型
    _ffi.unregisterTerminalModel(widget.terminalId);
    _terminalModel.dispose();
    _keyboardDebounce?.cancel();
    _terminalClipboardNotice.clear();
    _terminalClipboardNoticeController?.close();
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
    TerminalConnectionManager.releaseConnection(widget.id);
  }

  @override
  void didChangeMetrics() {
    super.didChangeMetrics();

    _keyboardDebounce?.cancel();
    _keyboardDebounce = Timer(const Duration(milliseconds: 20), () {
      final bottomInset = MediaQuery.of(context).viewInsets.bottom;
      setState(() {
        _sysKeyboardHeight = bottomInset;
      });
    });
  }

  void _updateKeyboardHeight() {
    if (_keyboardKey.currentContext != null) {
      final renderBox = _keyboardKey.currentContext!.findRenderObject() as RenderBox;
      _keyboardHeight = renderBox.size.height;
    }
  }

  EdgeInsets _calculatePadding(double heightPx) {
    if (_cellHeight == null) {
      return const EdgeInsets.symmetric(horizontal: 5.0, vertical: 2.0);
    }
    final realHeight = heightPx - _sysKeyboardHeight - _keyboardHeight;
    final rows = (realHeight / _cellHeight!).floor();
    final extraSpace = realHeight - rows * _cellHeight!;
    final topBottom = max(0.0, extraSpace / 2.0);
    return EdgeInsets.only(left: 5.0, right: 5.0, top: topBottom, bottom: topBottom + _sysKeyboardHeight + _keyboardHeight);
  }

  /// 通过 TerminalModel 粘贴剪贴板文本，这样纯键盘的修饰键和
  /// 移动端回车归一化都不会改动剪贴板数据。
  Future<void> _pasteClipboardText() async {
    final data = await Clipboard.getData(Clipboard.kTextPlain);
    final text = data?.text;
    if (text == null || !mounted) return;

    await _terminalModel.pasteText(text);
    if (mounted) {
      _terminalModel.terminalController.clearSelection();
    }
  }

  KeyEventResult _handleTerminalKeyEvent(FocusNode _, KeyEvent event) {
    final hardwareKeyboard = HardwareKeyboard.instance;
    final shouldPaste = shouldHandleTerminalPasteShortcut(
      platform: defaultTargetPlatform,
      logicalKey: event.logicalKey,
      isKeyDown: event is KeyDownEvent,
      isKeyRepeat: event is KeyRepeatEvent,
      controlPressed: hardwareKeyboard.isControlPressed,
      metaPressed: hardwareKeyboard.isMetaPressed,
      altPressed: hardwareKeyboard.isAltPressed,
      shiftPressed: hardwareKeyboard.isShiftPressed,
      modifierLockActive: _ctrlLocked || _altLocked,
    );
    if (!shouldPaste) return KeyEventResult.ignored;

    // 只有锁定的虚拟修饰键才需要拦截。没有锁定时，保留
    // xterm 默认的硬件粘贴行为，包括 bracketed paste 模式。
    unawaited(_pasteClipboardText());
    return KeyEventResult.handled;
  }

  @override
  Widget build(BuildContext context) {
    super.build(context);
    return WillPopScope(
      onWillPop: () async {
        clientClose(sessionId, _ffi);
        return false; // Prevent default back behavior
      },
      child: buildBody(),
    );
  }

  Widget buildBody() {
    final scaffold = Scaffold(
      resizeToAvoidBottomInset: false, // Disable automatic layout adjustment; manually control UI updates to prevent flickering when the keyboard shows/hides
      backgroundColor: Theme.of(context).scaffoldBackgroundColor,
      body: Stack(
        children: [
          Positioned.fill(
            child: SafeArea(
              top: true,
              child: LayoutBuilder(
                builder: (context, constraints) {
                  final heightPx = constraints.maxHeight;
                  return _buildTerminalViewForPlatform(
                    reportMouseInput: isWebDesktop || isAndroid,
                    reportTouchInput: isIOS,
                    terminal: _terminalModel.terminal,
                    controller: _terminalModel.terminalController,
                    textStyle: _getTerminalStyle(),
                    // 下面的注释来自 xterm.dart 源码：
                    // 用于在不
                    // 发出硬件 delete 事件的平台和输入法上检测删除键的变通方案。移动平台上优先使用。[false] 为
                    // 默认值。
                    //
                    // Android 上不用这个变通方案也没问题。
                    deleteDetection: isIOS,
                    shortcuts: platformTerminalShortcuts(),
                    onKeyEvent: terminalCopyHandler(
                      _terminalModel.terminal,
                      _terminalModel.terminalController,
                      fallback: _handleTerminalKeyEvent,
                    ),
                    padding: _calculatePadding(heightPx),
                    onSecondaryTapDown: (details, offset) async {
                      final selection = _terminalModel.terminalController.selection;
                      if (selection != null) {
                        final text = _terminalModel.terminal.buffer.getText(selection);
                        _terminalModel.terminalController.clearSelection();
                        await Clipboard.setData(ClipboardData(text: text));
                      } else {
                        await _pasteClipboardText();
                      }
                    },
                  );
                },
              ),
            ),
          ),
          if (_showTerminalExtraKeys) _buildFloatingKeyboard(),
          // 右上角 iOS 风格的圆形关闭按钮
          if (isIOS) _buildCloseButton(),
        ],
      ),
    );

    // 添加 iOS 边缘滑动退出手势（类似 Android 的返回键）
    if (isIOS) {
      return LayoutBuilder(
        builder: (context, constraints) {
          final screenWidth = constraints.maxWidth;
          // 阈值以屏幕宽度为基准，但要限制在合理的逻辑像素范围内
          // 边缘检测区域：宽度的约 10%，限制在 20 到 80 逻辑像素之间
          final edgeThreshold = (screenWidth * 0.1).clamp(20.0, 80.0);
          // 所需的水平移动距离：宽度的约 25%，限制在 80 到 300 逻辑像素之间
          final swipeThreshold = (screenWidth * 0.25).clamp(80.0, 300.0);

          return RawGestureDetector(
            behavior: HitTestBehavior.translucent,
            gestures: <Type, GestureRecognizerFactory>{
              HorizontalDragGestureRecognizer: GestureRecognizerFactoryWithHandlers<HorizontalDragGestureRecognizer>(
                () => HorizontalDragGestureRecognizer(
                  debugOwner: this,
                  // 只响应触摸输入，排除鼠标/触控板
                  supportedDevices: kTouchBasedDeviceKinds,
                ),
                (HorizontalDragGestureRecognizer instance) {
                  instance
                    // 记录最初的按下位置（在触摸容差生效之前）
                    ..onDown = (details) {
                      _swipeStartX = details.localPosition.dx;
                      _swipeCurrentX = details.localPosition.dx;
                    }
                    ..onUpdate = (details) {
                      _swipeCurrentX = details.localPosition.dx;
                    }
                    ..onEnd = (details) {
                      // 检查滑动是否从左边缘开始并向右移动
                      if (_swipeStartX < edgeThreshold && (_swipeCurrentX - _swipeStartX) > swipeThreshold) {
                        clientClose(sessionId, _ffi);
                      }
                      _swipeStartX = 0;
                      _swipeCurrentX = 0;
                    }
                    ..onCancel = () {
                      _swipeStartX = 0;
                      _swipeCurrentX = 0;
                    };
                },
              ),
            },
            child: scaffold,
          );
        },
      );
    }

    return scaffold;
  }

  Widget _buildCloseButton() {
    return Positioned(
      top: 0,
      right: 0,
      child: SafeArea(
        minimum: const EdgeInsets.only(
          top: 16, // iOS standard margin
          right: 16, // iOS standard margin
        ),
        child: Semantics(
          button: true,
          label: translate('Close'),
          child: Container(
            width: 44, // iOS standard tap target size
            height: 44,
            decoration: BoxDecoration(
              color: Colors.black.withOpacity(0.5), // Half transparency
              shape: BoxShape.circle,
            ),
            child: Material(
              color: Colors.transparent,
              shape: const CircleBorder(),
              clipBehavior: Clip.antiAlias,
              child: InkWell(
                customBorder: const CircleBorder(),
                onTap: () {
                  clientClose(sessionId, _ffi);
                },
                child: Tooltip(
                  message: translate('Close'),
                  child: const Icon(
                    Icons.chevron_left, // iOS-style back arrow
                    color: Colors.white,
                    size: 28,
                  ),
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  Widget _buildFloatingKeyboard() {
    return AnimatedPositioned(
      duration: const Duration(milliseconds: 200),
      left: 0,
      right: 0,
      bottom: _sysKeyboardHeight,
      child: Container(
        key: _keyboardKey,
        color: Theme.of(context).scaffoldBackgroundColor,
        padding: EdgeInsets.zero,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            // 第 1 行遵循最近评审过的 PR 布局。
            Row(
              mainAxisAlignment: MainAxisAlignment.center,
              children: _buildKeyboardKeyButtons(terminalKeyboardRow1Keys),
            ),
            // 第 2 行以全宽的 Row3 折叠/展开开关结尾。
            Row(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                ..._buildKeyboardKeyButtons(terminalKeyboardRow2Keys),
                const SizedBox(width: terminalKeyboardKeySpacing),
                _buildCollapseButton(),
              ],
            ),
            // 第 3 行恢复翻页按键和末尾的对齐占位符。
            if (_row3Expanded)
              Row(
                mainAxisAlignment: MainAxisAlignment.center,
                children: [
                  ..._buildKeyboardKeyButtons(terminalKeyboardRow3Keys),
                  for (var i = 0;
                      i < terminalKeyboardRow3TrailingPlaceholderCount;
                      i++) ...[
                    const SizedBox(width: terminalKeyboardKeySpacing),
                    const SizedBox(width: terminalKeyboardKeyWidth),
                  ],
                ],
              ),
          ],
        ),
      ),
    );
  }

  // 带锁定态高亮的 Ctrl 切换按钮
  Widget _buildCtrlKeyButton() {
    return _buildModifierToggleButton(
      text: 'Ctrl',
      semanticsLabel: 'Ctrl',
      isLocked: _ctrlLocked,
      onPressed: () => setState(() => _ctrlLocked = !_ctrlLocked),
    );
  }

  // 带锁定态高亮的 Alt 切换按钮
  Widget _buildAltKeyButton() {
    return _buildModifierToggleButton(
      text: 'Alt',
      semanticsLabel: 'Alt',
      isLocked: _altLocked,
      onPressed: () => setState(() => _altLocked = !_altLocked),
    );
  }

  // Row3 的折叠/展开切换按钮
  void _toggleRow3Expanded() {
    final willExpand = !_row3Expanded;
    final shouldClearModifiers = shouldClearTerminalModifiersWhenRow3Collapses(
      wasExpanded: _row3Expanded,
      willExpand: willExpand,
      ctrlLocked: _ctrlLocked,
      altLocked: _altLocked,
    );
    setState(() {
      _row3Expanded = willExpand;
      if (shouldClearModifiers) {
        _ctrlLocked = false;
        _altLocked = false;
      }
    });
    mainSetLocalBoolOption(kOptionShowTerminalCtrlKeys, willExpand);

    // 插入/移除 Row3 后浮动键盘的高度会变化。
    // 在下一帧重新测量，使终端内边距使用新的高度。
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted || !_showTerminalExtraKeys) return;
      setState(() {
        _updateKeyboardHeight();
      });
    });
  }

  Widget _buildCollapseButton() {
    return Semantics(
      label: translate('Show terminal extra keys'),
      toggled: _row3Expanded,
      child: ElevatedButton(
        onPressed: _toggleRow3Expanded,
        child: Text(_row3Expanded ? '∧' : '∨'),
        style: ElevatedButton.styleFrom(
          minimumSize: const Size(terminalKeyboardKeyWidth, 32),
          padding: EdgeInsets.zero,
          textStyle: const TextStyle(fontSize: 12),
          backgroundColor:
              Theme.of(context).colorScheme.surfaceContainerHighest,
          foregroundColor: Theme.of(context).colorScheme.onSurfaceVariant,
        ),
      ),
    );
  }

  /// 构建固定宽度的按键序列，采用评审过的 2dp 间距。
  List<Widget> _buildKeyboardKeyButtons(List<String> labels) {
    return [
      for (var i = 0; i < labels.length; i++) ...[
        _buildKeyButton(labels[i]),
        if (i < labels.length - 1)
          const SizedBox(width: terminalKeyboardKeySpacing),
      ],
    ];
  }

  /// 构建带一次性行为的修饰键切换按钮（Ctrl/Alt）。
  /// 当 [isLocked] 为 true 时，按钮以蓝色高亮，下一次
  /// 单字符输入会被映射为对应的带修饰键形式。
  Widget _buildModifierToggleButton({
    required String text,
    required String semanticsLabel,
    required bool isLocked,
    required VoidCallback onPressed,
  }) {
    return Semantics(
      // Ctrl 和 Alt 是技术上的按键名，故意保持不变。
      label: semanticsLabel,
      toggled: isLocked,
      child: ElevatedButton(
        onPressed: onPressed,
        child: Text(text),
        style: ElevatedButton.styleFrom(
          minimumSize: const Size(terminalKeyboardKeyWidth, 32),
          padding: EdgeInsets.zero,
          textStyle: const TextStyle(fontSize: 12),
          backgroundColor: isLocked
              ? Colors.blue
              : Theme.of(context).colorScheme.surfaceContainerHighest,
          foregroundColor: isLocked
              ? Colors.white
              : Theme.of(context).colorScheme.onSurfaceVariant,
        ),
      ),
    );
  }

  Widget _buildKeyButton(String label) {
    if (label == 'Ctrl') return _buildCtrlKeyButton();
    if (label == 'Alt') return _buildAltKeyButton();

    return ElevatedButton(
      onPressed: () {
        _sendKeyToTerminal(label);
      },
      child: Text(label),
      style: ElevatedButton.styleFrom(
        minimumSize: const Size(terminalKeyboardKeyWidth, 32),
        padding: EdgeInsets.zero,
        textStyle: const TextStyle(fontSize: 12),
        backgroundColor:
            Theme.of(context).colorScheme.surfaceContainerHighest,
        foregroundColor: Theme.of(context).colorScheme.onSurfaceVariant,
      ),
    );
  }

  void _sendKeyToTerminal(String key) {
    String send;

    switch (key) {
      case 'Esc':
        send = '\x1B';
        break;
      case 'Tab':
        send = '\t';
        break;
      case 'Ctrl+C':
        send = '\x03';
        break;

      case '↑':
        send = '\x1B[A';
        break;
      case '↓':
        send = '\x1B[B';
        break;
      case '→':
        send = '\x1B[C';
        break;
      case '←':
        send = '\x1B[D';
        break;

      case 'Home':
        send = '\x1B[H';
        break;
      case 'End':
        send = '\x1B[F';
        break;
      case 'PgUp':
        send = '\x1B[5~';
        break;
      case 'PgDn':
        send = '\x1B[6~';
        break;

      default:
        send = key;
        break;
    }

    _terminalModel.sendVirtualKey(send);
  }

  // https://github.com/TerminalStudio/xterm.dart/issues/42#issuecomment-877495472
  // https://github.com/TerminalStudio/xterm.dart/issues/198#issuecomment-2526548458
  TerminalStyle _getTerminalStyle() {
    return isWeb
        ? TerminalStyle(
            fontFamily: _robotoMonoFontFamily,
            fontSize: 14,
          )
        : const TerminalStyle();
  }

  @override
  bool get wantKeepAlive => true;
}
