import 'dart:async';
import 'dart:convert';

import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/common/widgets/dialog.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/utils/multi_window_manager.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_hbb/models/terminal_copy_shortcut.dart';
import 'package:flutter_hbb/models/terminal_model.dart';
import 'package:get/get.dart';

import '../../models/platform_model.dart';
import 'terminal_page.dart';
import 'terminal_connection_manager.dart';
import '../widgets/material_mod_popup_menu.dart' as mod_menu;
import '../widgets/popup_menu.dart';
import 'package:bot_toast/bot_toast.dart';

typedef _TerminalClipboardSource = ({
  String peerId,
  int terminalId,
  String tabKey,
});

class TerminalTabPage extends StatefulWidget {
  final Map<String, dynamic> params;

  const TerminalTabPage({Key? key, required this.params}) : super(key: key);

  @override
  State<TerminalTabPage> createState() => _TerminalTabPageState(params);
}

class _TerminalTabPageState extends State<TerminalTabPage> {
  DesktopTabController get tabController => Get.find<DesktopTabController>();
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

  static const IconData selectedIcon = Icons.terminal;
  static const IconData unselectedIcon = Icons.terminal_outlined;
  int _nextTerminalId = 1;
  // 用于异步关闭操作的轻量级幂等保护
  final Set<String> _closingTabs = {};
  // 为 true 时，所有会话清理都应保留（窗口级关闭正在进行）
  bool _windowClosing = false;
  CancelFunc? _terminalClipboardNoticeCancel;
  final _terminalClipboardNotice =
      TerminalClipboardNoticeCoordinator<_TerminalClipboardSource>();

  _TerminalTabPageState(Map<String, dynamic> params) {
    Get.put(DesktopTabController(tabType: DesktopTabType.terminal));
    tabController.onSelected = (id) {
      WindowController.fromWindowId(windowId())
          .setTitle(getWindowNameWithId(id));
    };
    tabController.onRemoved = (_, id) {
      _closeTerminalClipboardNoticeForTab(id);
      onRemoveId(id);
    };
    tabController.onCloseWindow = _closeWindowFromConnection;
    final terminalId = params['terminalId'] ?? _nextTerminalId++;
    tabController.add(_createTerminalTab(
      peerId: params['id'],
      terminalId: terminalId,
      password: params['password'],
      isSharedPassword: params['isSharedPassword'],
      forceRelay: params['forceRelay'],
      connToken: params['connToken'],
    ));
  }

  TabInfo _createTerminalTab({
    required String peerId,
    required int terminalId,
    String? password,
    bool? isSharedPassword,
    bool? forceRelay,
    String? connToken,
  }) {
    final tabKey = '${peerId}_$terminalId';
    final alias = bind.mainGetPeerOptionSync(id: peerId, key: 'alias');
    final tabLabel =
        alias.isNotEmpty ? '$alias #$terminalId' : '$peerId #$terminalId';
    final clipboardSource = (
      peerId: peerId,
      terminalId: terminalId,
      tabKey: tabKey,
    );
    return TabInfo(
      key: tabKey,
      label: tabLabel,
      selectedIcon: selectedIcon,
      unselectedIcon: unselectedIcon,
      onTabCloseButton: () => _closeTab(tabKey),
      page: TerminalPage(
        key: ValueKey(tabKey),
        id: peerId,
        terminalId: terminalId,
        tabKey: tabKey,
        password: password,
        isSharedPassword: isSharedPassword,
        tabController: tabController,
        forceRelay: forceRelay,
        connToken: connToken,
        onClipboardWriteBlocked: _canHandleTerminalClipboardWriteRequest
            ? (text) => _handleTerminalClipboardWriteBlocked(
                  clipboardSource,
                  text,
                )
            : null,
        onClipboardWriteSucceeded: (_) {
          _handleTerminalClipboardWriteSucceeded(clipboardSource);
        },
      ),
    );
  }

  void _handleTerminalClipboardWriteBlocked(
    _TerminalClipboardSource source,
    String clipboardText,
  ) {
    if (!mounted) return;
    final option = bind.mainGetLocalOption(
      key: kOptionAllowTerminalClipboardWrite,
    );
    final request = _terminalClipboardNotice.recordBlocked(
      source: source,
      text: clipboardText,
      option: option,
      canWrite: _canWriteTerminalClipboard,
    );
    if (request != null) _showTerminalClipboardNotice(request);
  }

  void _showTerminalClipboardNotice(
    TerminalClipboardNoticeRequest<_TerminalClipboardSource> request,
  ) {
    _terminalClipboardNoticeCancel = BotToast.showCustomNotification(
      duration: null,
      enableSlideOff: false,
      onlyOne: true,
      onClose: _handleTerminalClipboardNoticeClosed,
      toastBuilder: (_) => AnimatedBuilder(
        animation: _terminalClipboardNotice,
        builder: (_, __) => MaterialBanner(
          leading: const Icon(Icons.content_copy_outlined),
          content: Text(translate(kTerminalClipboardNoticeMessageKey)),
          actions: [
            TextButton(
              onPressed: _terminalClipboardNotice.canClaimAction
                  ? _handleTerminalClipboardNegativeAction
                  : null,
              child: Text(translate(request.negativeActionKey)),
            ),
            TextButton(
              onPressed: _terminalClipboardNotice.canClaimAction
                  ? _handleTerminalClipboardPositiveAction
                  : null,
              child: Text(translate(request.actionKey)),
            ),
          ],
        ),
      ),
    );
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

  void _handleTerminalClipboardNoticeClosed() {
    _terminalClipboardNoticeCancel = null;
    _terminalClipboardNotice.noticeClosed();
  }

  bool _canWriteTerminalClipboard(
    _TerminalClipboardSource source,
  ) {
    if (!_canHandleTerminalClipboardWriteRequest) return false;
    final ffi = TerminalConnectionManager.getExistingConnection(source.peerId);
    return ffi != null &&
        !ffi.closed &&
        ffi.ffiModel.permissions['clipboard'] != false &&
        tabController.state.value.tabs.any((tab) => tab.key == source.tabKey) &&
        ffi.terminalModels.containsKey(source.terminalId);
  }

  void _handleTerminalClipboardWriteSucceeded(
    _TerminalClipboardSource source,
  ) {
    final request = _terminalClipboardNotice.currentForSource(source);
    if (request == null) return;
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
          '[TerminalTabPage] Failed to save terminal clipboard permission: $error');
      return;
    } finally {
      _terminalClipboardNotice.releaseAction();
    }
    _closeTerminalClipboardNotice();
  }

  Future<void> _completeTerminalClipboardWrite(
    TerminalClipboardNoticeRequest<_TerminalClipboardSource> request,
  ) async {
    final source = request.source;
    var completed = false;
    try {
      completed = await completeTerminalClipboardWrite(
        clipboardText: request.text,
        canWrite: () => _canWriteTerminalClipboard(source),
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
          '[TerminalTabPage] Failed to complete terminal clipboard write: $error');
    } finally {
      _terminalClipboardNotice.releaseAction();
    }
    if (!completed) return;
    _closeTerminalClipboardNotice();
  }

  void _closeTerminalClipboardNoticeForTab(String tabKey) {
    final current = _terminalClipboardNotice.current;
    if (current?.source.tabKey != tabKey) return;
    _closeTerminalClipboardNotice();
  }

  void _closeTerminalClipboardNotice() {
    if (!_terminalClipboardNotice.beginClose()) return;
    final cancel = _terminalClipboardNoticeCancel;
    if (cancel == null) {
      debugPrint('[TerminalTabPage] Clipboard notice controller is missing');
      _terminalClipboardNotice.noticeClosed();
      return;
    }
    cancel();
  }

  /// 统一处理所有关闭路径（按钮、快捷键、程序调用）的标签页关闭。
  /// 显示审计对话框，若非持久模式则清理会话，然后移除 UI 标签页。
  Future<void> _closeTab(String tabKey) async {
    // 幂等保护：若该标签页已在关闭中则跳过
    if (_closingTabs.contains(tabKey)) return;
    _closingTabs.add(tabKey);

    try {
      // 在任何 await 之前对 peerTabCount 取快照，以避免与并发的
      // _closeAllTabs 清空 tabController（这会使实际计数
      // 降为 0 并错误触发会话保留）。
      // 注意：如果在此期间有其他单个标签页被关闭，快照可能过期
      // （在审计对话框期间），但这是可以接受的取舍。
      int? snapshotPeerTabCount;
      final parsed = _parseTabKey(tabKey);
      if (parsed != null) {
        final (peerId, _) = parsed;
        snapshotPeerTabCount = tabController.state.value.tabs.where((t) {
          final p = _parseTabKey(t.key);
          return p != null && p.$1 == peerId;
        }).length;
      }

      if (await desktopTryShowTabAuditDialogCloseCancelled(
        id: tabKey,
        tabController: tabController,
      )) {
        return;
      }

      // 如果不在持久模式下，则关闭终端会话。
      // 单独包裹，确保会话清理失败不会阻塞 UI 标签页的移除。
      try {
        await _closeTerminalSessionIfNeeded(tabKey,
            peerTabCount: snapshotPeerTabCount);
      } catch (e) {
        debugPrint('[TerminalTabPage] Session cleanup failed for $tabKey: $e');
      }
      // 无论会话清理结果如何，都从 UI 关闭该标签页
      tabController.closeBy(tabKey);
    } catch (e) {
      debugPrint('[TerminalTabPage] Error closing tab $tabKey: $e');
    } finally {
      _closingTabs.remove(tabKey);
    }
  }

  /// 关闭所有标签页并清理会话。
  /// 用于窗口级关闭操作（onDestroy、handleWindowCloseButton）。
  /// UI 标签页立即移除；会话清理并行执行，并带有
  /// 有界超时，以免窗口关闭被无限期阻塞。
  Future<void> _closeAllTabs() async {
    _windowClosing = true;
    final tabKeys = tabController.state.value.tabs.map((t) => t.key).toList();
    // 立即移除所有 UI 标签页（与旧的 tabController.clear() 行为完全一致）
    // 保持下面的清理目标查找在首次 await 之前是同步的：
    // 它依赖当前帧仍保留每个 TerminalPage 的 FFI/model。
    _terminalClipboardNotice.clear();
    _terminalClipboardNoticeCancel?.call();
    tabController.clear();
    // 并行执行会话清理并设有界超时（closeTerminal() 内部有 3 秒超时）。
    // 跳过已被并发 _closeTab() 关闭中的标签页，避免重复调用 FFI。
    final futures = tabKeys
        .where((tabKey) => !_closingTabs.contains(tabKey))
        .map((tabKey) async {
      try {
        await _closeTerminalSessionIfNeeded(tabKey, persistAll: true);
      } catch (e) {
        debugPrint('[TerminalTabPage] Session cleanup failed for $tabKey: $e');
      }
    }).toList();
    if (futures.isNotEmpty) {
      await Future.wait(futures).timeout(
        const Duration(seconds: 4),
        onTimeout: () {
          debugPrint(
              '[TerminalTabPage] Session cleanup timed out for batch close');
          return [];
        },
      );
    }
  }

  /// 根据持久模式决定是否在服务端关闭终端会话。
  ///
  /// 持久模式启用时，[persistAll] 控制行为：
  /// - `true`（窗口关闭）：保留所有会话，不关闭任何一个。
  /// - `false`（标签页关闭）：只保留该 peer 最后一个会话，
  ///   关闭其余会话，使只有最近断开的会话得以保留。
  ///
  /// 注意：如果 [_windowClosing] 为 true，则 persistAll 被强制设为 true，以便
  /// 正在进行的 _closeTab() 调用不会误关掉那些
  /// 窗口关闭流程打算保留的会话。
  Future<void> _closeTerminalSessionIfNeeded(String tabKey,
      {bool persistAll = false, int? peerTabCount}) async {
    // 如果窗口关闭正在进行，则覆盖为保留所有会话
    // 即使本次调用源自单个标签页关闭。
    if (_windowClosing) {
      persistAll = true;
    }
    final parsed = _parseTabKey(tabKey);
    if (parsed == null) return;
    final (peerId, terminalId) = parsed;

    final ffi = TerminalConnectionManager.getExistingConnection(peerId);
    if (ffi == null) return;

    final isPersistent = bind.sessionGetToggleOptionSync(
      sessionId: ffi.sessionId,
      arg: kOptionTerminalPersistent,
    );

    if (isPersistent) {
      if (persistAll) {
        // 窗口关闭：保留所有会话
        return;
      }
      // 标签页关闭：仅当这是该 peer 的最后一个标签页时才保留。
      // 如果提供了快照值则使用它（避免与并发的标签页移除竞争）。
      final effectivePeerTabCount = peerTabCount ??
          tabController.state.value.tabs.where((t) {
            final p = _parseTabKey(t.key);
            return p != null && p.$1 == peerId;
          }).length;
      if (effectivePeerTabCount <= 1) {
        // 该 peer 的最后一个标签页 —— 保留会话
        return;
      }
      // 不是最后一个标签页 —— 继续执行以关闭会话
    }

    final terminalModel = ffi.terminalModels[terminalId];
    if (terminalModel != null) {
      // closeTerminal() 内部有 3 秒超时，无需外部超时
      await terminalModel.closeTerminal();
    }
  }

  /// 将 tabKey（格式："peerId_terminalId"）解析为各组成部分。
  /// 注意：peerId 可能包含下划线，因此使用 lastIndexOf('_')。
  /// 若 tabKey 格式无效则返回 null。
  (String peerId, int terminalId)? _parseTabKey(String tabKey) {
    final lastUnderscore = tabKey.lastIndexOf('_');
    if (lastUnderscore <= 0) {
      debugPrint('[TerminalTabPage] Invalid tabKey format: $tabKey');
      return null;
    }
    final terminalIdStr = tabKey.substring(lastUnderscore + 1);
    final terminalId = int.tryParse(terminalIdStr);
    if (terminalId == null) {
      debugPrint('[TerminalTabPage] Invalid terminalId in tabKey: $tabKey');
      return null;
    }
    final peerId = tabKey.substring(0, lastUnderscore);
    return (peerId, terminalId);
  }

  Widget _tabMenuBuilder(String peerId, CancelFunc cancelFunc) {
    final List<MenuEntryBase<String>> menu = [];
    const EdgeInsets padding = EdgeInsets.only(left: 8.0, right: 5.0);

    // 新建标签页菜单项
    menu.add(MenuEntryButton<String>(
      childBuilder: (TextStyle? style) => Text(
        translate('New tab'),
        style: style,
      ),
      proc: () {
        _addNewTerminal(peerId);
        cancelFunc();
        // 同时尝试关闭所有 BotToast 浮层
        BotToast.cleanAll();
      },
      padding: padding,
    ));

    menu.add(MenuEntryDivider());

    menu.add(MenuEntrySwitch<String>(
      switchType: SwitchType.scheckbox,
      text: translate('Keep terminal sessions on disconnect'),
      getter: () async {
        final ffi = Get.find<FFI>(tag: 'terminal_$peerId');
        return bind.sessionGetToggleOptionSync(
          sessionId: ffi.sessionId,
          arg: kOptionTerminalPersistent,
        );
      },
      setter: (bool v) async {
        final ffi = Get.find<FFI>(tag: 'terminal_$peerId');
        await bind.sessionToggleOption(
          sessionId: ffi.sessionId,
          value: kOptionTerminalPersistent,
        );
      },
      padding: padding,
    ));

    return mod_menu.PopupMenu<String>(
      items: menu
          .map((e) => e.build(
                context,
                const MenuConfig(
                  commonColor: CustomPopupMenuTheme.commonColor,
                  height: CustomPopupMenuTheme.height,
                  dividerHeight: CustomPopupMenuTheme.dividerHeight,
                ),
              ))
          .expand((i) => i)
          .toList(),
    );
  }

  @override
  void initState() {
    super.initState();

    // 添加键盘快捷键处理器
    HardwareKeyboard.instance.addHandler(_handleKeyEvent);

    rustDeskWinManager.setMethodHandler((call, fromWindowId) async {
      print(
          "[Remote Terminal] call ${call.method} with args ${call.arguments} from window $fromWindowId");
      if (call.method == kWindowEventNewTerminal) {
        final args = jsonDecode(call.arguments);
        final id = args['id'];
        windowOnTop(windowId());
        // 允许同一连接使用多个终端
        final terminalId = args['terminalId'] ?? _nextTerminalId++;
        tabController.add(_createTerminalTab(
          peerId: id,
          terminalId: terminalId,
          password: args['password'],
          isSharedPassword: args['isSharedPassword'],
          forceRelay: args['forceRelay'],
          connToken: args['connToken'],
        ));
      } else if (call.method == kWindowEventRestoreTerminalSessions) {
        _restoreSessions(call.arguments);
      } else if (call.method == "onDestroy") {
        // 窗口销毁前清理会话（有界等待）
        await _closeAllTabs();
      } else if (call.method == kWindowActionRebuild) {
        reloadCurrentWindow();
      } else if (call.method == kWindowEventActiveSession) {
        if (tabController.state.value.tabs.isEmpty) {
          return false;
        }
        final currentTab = tabController.state.value.selectedTabInfo;
        assert(call.arguments is String,
            "Expected String arguments for kWindowEventActiveSession, got ${call.arguments.runtimeType}");
        // 使用 lastIndexOf 处理含下划线的 peerId
        final lastUnderscore = currentTab.key.lastIndexOf('_');
        if (lastUnderscore > 0 &&
            currentTab.key.substring(0, lastUnderscore) == call.arguments) {
          windowOnTop(windowId());
          return true;
        }
        return false;
      }
    });
    Future.delayed(Duration.zero, () {
      restoreWindowPosition(WindowType.Terminal, windowId: windowId());
    });
  }

  @override
  void dispose() {
    HardwareKeyboard.instance.removeHandler(_handleKeyEvent);
    _terminalClipboardNotice.clear();
    _terminalClipboardNoticeCancel?.call();
    super.dispose();
  }

  Future<void> _restoreSessions(String arguments) async {
    Map<String, dynamic>? args;
    try {
      args = jsonDecode(arguments) as Map<String, dynamic>;
    } catch (e) {
      debugPrint("Error parsing JSON arguments in _restoreSessions: $e");
      return;
    }
    final persistentSessions =
        args['persistent_sessions'] as List<dynamic>? ?? [];
    final sortedSessions = persistentSessions.whereType<int>().toList()..sort();
    var peerId = args['peer_id'] as String? ?? '';
    if (peerId.isEmpty) {
      if (tabController.state.value.tabs.isEmpty ||
          tabController.state.value.selected >=
              tabController.state.value.tabs.length) {
        debugPrint('[TerminalTabPage] Skip restore: no selected tab');
        return;
      }
      final currentTab = tabController.state.value.selectedTabInfo;
      final parsed = _parseTabKey(currentTab.key);
      if (parsed == null) return;
      peerId = parsed.$1;
    }
    final existingTerminalIds = tabController.state.value.tabs
        .map((tab) => _parseTabKey(tab.key))
        .where((parsed) => parsed != null && parsed.$1 == peerId)
        .map((parsed) => parsed!.$2)
        .toSet();
    if (existingTerminalIds.isEmpty) {
      debugPrint(
          '[TerminalTabPage] Skip restore: no seed tab for peer $peerId');
      return;
    }
    for (final terminalId in sortedSessions) {
      if (!existingTerminalIds.add(terminalId)) {
        continue;
      }
      _addNewTerminal(peerId, terminalId: terminalId);
      // 需要一段延迟，以确保 UI 有充足时间更新
      // 然后再添加下一个终端。没有这段延迟，`_TerminalPageState::dispose()`
      // 可能会在标签页组件仍留在标签页控制器中时被提前调用。
      // 这种行为很可能源于 UI 渲染生命周期
      // 与添加新标签页之间的竞争。尝试使用 `_TerminalPageState::addPostFrameCallback()`
      // 等待上一页就绪的做法没有奏效，因为观察到的调用顺序是：
      // `initState() 2 -> dispose() 2 -> postFrameCallback() 2`，随后是 `initState() 3`。
      // `Future.delayed` 方案通过引入一段缓冲时间来缓解该问题，
      // 让 UI 先稳定下来再继续。
      await Future.delayed(const Duration(milliseconds: 300));
    }
  }

  bool _handleKeyEvent(KeyEvent event) {
    if (event is KeyDownEvent) {
      // macOS 上使用 Cmd+T，其他平台使用 Ctrl+Shift+T
      if (event.logicalKey == LogicalKeyboardKey.keyT) {
        if (isMacOS &&
            HardwareKeyboard.instance.isMetaPressed &&
            !HardwareKeyboard.instance.isShiftPressed) {
          // macOS：Cmd+T（新建标签页的标准快捷键）
          _addNewTerminalForCurrentPeer();
          return true;
        } else if (!isMacOS &&
            HardwareKeyboard.instance.isControlPressed &&
            HardwareKeyboard.instance.isShiftPressed) {
          // 其他平台：Ctrl+Shift+T（避免与终端中的 Ctrl+T 冲突）
          _addNewTerminalForCurrentPeer();
          return true;
        }
      }

      // macOS 上使用 Cmd+W，其他平台使用 Ctrl+Shift+W
      if (event.logicalKey == LogicalKeyboardKey.keyW) {
        if (isMacOS &&
            HardwareKeyboard.instance.isMetaPressed &&
            !HardwareKeyboard.instance.isShiftPressed) {
          // macOS：Cmd+W（关闭标签页的标准快捷键）
          final currentTab = tabController.state.value.selectedTabInfo;
          if (tabController.state.value.tabs.length > 1) {
            _closeTab(currentTab.key);
            return true;
          }
        } else if (!isMacOS &&
            HardwareKeyboard.instance.isControlPressed &&
            HardwareKeyboard.instance.isShiftPressed) {
          // 其他平台：Ctrl+Shift+W（避免与 Ctrl+W 的按词删除冲突）
          final currentTab = tabController.state.value.selectedTabInfo;
          if (tabController.state.value.tabs.length > 1) {
            _closeTab(currentTab.key);
            return true;
          }
        }
      }

      // 使用 Alt+Left/Right 进行标签页导航（避免冲突）
      if (HardwareKeyboard.instance.isAltPressed) {
        if (event.logicalKey == LogicalKeyboardKey.arrowLeft) {
          // 上一个标签页
          final currentIndex = tabController.state.value.selected;
          if (currentIndex > 0) {
            tabController.jumpTo(currentIndex - 1);
          }
          return true;
        } else if (event.logicalKey == LogicalKeyboardKey.arrowRight) {
          // 下一个标签页
          final currentIndex = tabController.state.value.selected;
          if (currentIndex < tabController.length - 1) {
            tabController.jumpTo(currentIndex + 1);
          }
          return true;
        }
      }

      // 检查 Cmd/Ctrl + 数字（切换到指定标签页）
      final numberKeys = [
        LogicalKeyboardKey.digit1,
        LogicalKeyboardKey.digit2,
        LogicalKeyboardKey.digit3,
        LogicalKeyboardKey.digit4,
        LogicalKeyboardKey.digit5,
        LogicalKeyboardKey.digit6,
        LogicalKeyboardKey.digit7,
        LogicalKeyboardKey.digit8,
        LogicalKeyboardKey.digit9,
      ];

      for (int i = 0; i < numberKeys.length; i++) {
        if (event.logicalKey == numberKeys[i] &&
            ((isMacOS && HardwareKeyboard.instance.isMetaPressed) ||
                (!isMacOS && HardwareKeyboard.instance.isControlPressed))) {
          if (i < tabController.length) {
            tabController.jumpTo(i);
            return true;
          }
        }
      }
    }
    return false;
  }

  void _addNewTerminal(String peerId, {int? terminalId}) {
    // 找到该 peer 的第一个标签页以获取连接参数
    final firstTab = tabController.state.value.tabs.firstWhere(
      (tab) {
        final last = tab.key.lastIndexOf('_');
        return last > 0 && tab.key.substring(0, last) == peerId;
      },
    );
    if (firstTab.page is TerminalPage) {
      final page = firstTab.page as TerminalPage;
      final newTerminalId = terminalId ?? _nextTerminalId++;
      if (terminalId != null && terminalId >= _nextTerminalId) {
        _nextTerminalId = terminalId + 1;
      }
      tabController.add(_createTerminalTab(
        peerId: peerId,
        terminalId: newTerminalId,
        password: page.password,
        isSharedPassword: page.isSharedPassword,
        forceRelay: page.forceRelay,
        connToken: page.connToken,
      ));
    }
  }

  void _addNewTerminalForCurrentPeer({int? terminalId}) {
    final currentTab = tabController.state.value.selectedTabInfo;
    final parsed = _parseTabKey(currentTab.key);
    if (parsed == null) return;
    final (peerId, _) = parsed;
    _addNewTerminal(peerId, terminalId: terminalId);
  }

  @override
  Widget build(BuildContext context) {
    final child = Scaffold(
        backgroundColor: Theme.of(context).cardColor,
        body: DesktopTab(
          controller: tabController,
          onWindowCloseButton: handleWindowCloseButton,
          tail: _buildAddButton(),
          selectedBorderColor: MyTheme.accent,
          labelGetter: DesktopTab.tablabelGetter,
          tabMenuBuilder: (key) {
            final parsed = _parseTabKey(key);
            if (parsed == null) return Container();
            final (peerId, _) = parsed;
            return _tabMenuBuilder(peerId, () {});
          },
        ));
    final tabWidget = isLinux
        ? buildVirtualWindowFrame(context, child)
        : workaroundWindowBorder(
            context,
            Container(
              decoration: BoxDecoration(
                  border: Border.all(color: MyTheme.color(context).border!)),
              child: child,
            ));
    return isMacOS || kUseCompatibleUiMode
        ? tabWidget
        : SubWindowDragToResizeArea(
            child: tabWidget,
            resizeEdgeSize: stateGlobal.resizeEdgeSize.value,
            enableResizeEdges: subWindowManagerEnableResizeEdges,
            windowId: stateGlobal.windowId,
          );
  }

  void onRemoveId(String id) {
    if (tabController.state.value.tabs.isEmpty) {
      WindowController.fromWindowId(windowId()).close();
    }
  }

  Future<void> _closeWindowFromConnection() async {
    await _closeAllTabs();
    await WindowController.fromWindowId(windowId()).close();
  }

  int windowId() {
    return widget.params["windowId"];
  }

  Widget _buildAddButton() {
    return ActionIcon(
      message: 'New tab',
      icon: IconFont.add,
      onTap: () {
        _addNewTerminalForCurrentPeer();
      },
      isClose: false,
    );
  }

  Future<bool> handleWindowCloseButton() async {
    final connLength = tabController.state.value.tabs.length;
    if (connLength == 1) {
      if (await desktopTryShowTabAuditDialogCloseCancelled(
        id: tabController.state.value.tabs[0].key,
        tabController: tabController,
      )) {
        return false;
      }
    }
    if (connLength <= 1) {
      await _closeAllTabs();
      return true;
    } else {
      final bool res;
      if (!option2bool(kOptionEnableConfirmClosingTabs,
          bind.mainGetLocalOption(key: kOptionEnableConfirmClosingTabs))) {
        res = true;
      } else {
        res = await closeConfirmDialog();
      }
      if (res) {
        await _closeAllTabs();
      }
      return res;
    }
  }
}
