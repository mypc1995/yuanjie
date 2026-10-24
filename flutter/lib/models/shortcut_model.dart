import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';

import '../common.dart';
import '../common/shared_state.dart' show PrivacyModeState;
import '../common/widgets/dialog.dart'
    show desktopTryShowTabAuditDialogCloseCancelled;
import '../common/widgets/keyboard_shortcuts/shortcut_utils.dart';
import '../common/widgets/keyboard_shortcuts/shortcut_actions.dart';
import '../common/widgets/toolbar.dart'
    show allowDisplaySwitchInPrivacyMode, showVirtualDisplayMenu;
import '../consts.dart';
import '../desktop/widgets/remote_toolbar.dart' show ToolbarState;
import 'chat_model.dart' show ChatModel, MessageKey, VoiceCallStatus;
import '../desktop/widgets/tabbar_widget.dart' show DesktopTabController;
import '../models/model.dart';
import '../models/platform_model.dart';
import '../models/state_model.dart';

typedef ShortcutCallback = FutureOr<void> Function();

/// 按会话分发的快捷键调度器。创建会话时挂接到 FFI。
///
/// Rust 匹配器（src/keyboard/shortcuts.rs）发出 `shortcut_triggered`
/// 会话事件，其中包含匹配到的 `action` id。会话事件
/// 监听器（位于 [FfiModel.startEventListener] 中）将这些事件转发给本 model
/// ，通过 [onTriggered] 触发，执行工具栏 / 菜单
/// 构建器此前为该 action id 注册的回调。
class ShortcutModel {
  static WeakReference<ShortcutModel>? _activeWebModel;

  final WeakReference<FFI> parent;
  final Map<String, ShortcutCallback> _callbacks = {};
  final Map<String, ShortcutCallback> _refreshers = {};

  ShortcutModel(this.parent);

  /// 由工具栏 / 菜单构建器调用，用于注册匹配到的快捷键触发时
  /// 要执行的操作。
  void register(String actionId, ShortcutCallback callback) {
    if (parent.target?.closed != false) return;
    _callbacks[actionId] = callback;
    _activeWebModel = WeakReference(this);
  }

  void unregister(String actionId) {
    _callbacks.remove(actionId);
  }

  void registerRefresher(List<String> actionIds, ShortcutCallback refresh) {
    if (parent.target?.closed != false) return;
    for (final actionId in actionIds) {
      _refreshers[actionId] = refresh;
    }
  }

  void clear() {
    _callbacks.clear();
    _refreshers.clear();
    if (identical(_activeWebModel?.target, this)) {
      _activeWebModel = null;
    }
  }

  static void onWebTriggered(String actionId) {
    final model = _activeWebModel?.target;
    if (model != null) {
      model.onTriggered(actionId);
    } else {
      debugPrint('shortcut_triggered: no active web shortcut model');
    }
  }

  /// 由会话事件监听器调用，当本会话收到 `shortcut_triggered` 事件
  /// 到达时触发。
  ///
  /// 匹配器已经消费了该组合键。若没有对应的处理函数，该操作
  /// 在本会话中不可用（例如在对端不支持截图时）
  /// ，此时该组合键是有意不做任何事：已绑定的组合键始终
  /// 属于 RustDesk，绝不会被转发到远端。
  void onTriggered(String actionId) {
    if (parent.target?.closed != false) return;
    unawaited(Future.sync(() async {
      final refresh = _refreshers[actionId];
      if (refresh != null) {
        await refresh();
        if (!identical(_refreshers[actionId], refresh)) return;
      }
      if (parent.target?.closed != false) return;
      final cb = _callbacks[actionId];
      if (cb != null) {
        await cb();
      } else {
        debugPrint('shortcut_triggered: no handler for $actionId');
      }
    }).catchError((e, st) {
      debugPrint('shortcut_triggered: handler failed for $actionId: $e\n$st');
    }));
  }

  static String _configRaw = '';
  static ShortcutConfig _config = ShortcutConfig.parse('');
  static ShortcutConfig _activeConfig = _config;

  /// 已保存的配置，按不同的存储值只解析一次。Flutter
  /// 匹配器每次按键都会读取它，因此在配置未变时，一次按键只需一次配置
  /// 读取，无需 JSON 解析。
  /// 运行时匹配会请求 [active]，以排除被平台隐藏的绑定，
  /// 同时保留供其他平台使用的存储配置。
  static ShortcutConfig config({bool active = false}) {
    final raw = bind.mainGetLocalOption(key: kShortcutLocalConfigKey);
    if (raw != _configRaw) {
      _config = ShortcutConfig.parse(raw);
      _activeConfig = _config.forPlatform(currentPlatformCapabilities());
      _configRaw = raw;
    }
    return active ? _activeConfig : _config;
  }

  /// 从 LocalConfig 读取绑定 JSON。返回一份副本，调用方可以
  /// 对其进行修改。
  static List<Map<String, dynamic>> readBindings() => [
        for (final binding in config().bindings)
          Map<String, dynamic>.of(binding)
      ];

  static bool isEnabled() => config().enabled;

  static bool isPassThrough() => config().passThrough;

  /// [isEnabled] 的持久化搭档：开启时匹配器会提前返回，
  /// 所有按键都会直接透传到远端（即所有绑定都被
  /// 暂停）。它保存在同一个 JSON 数据块中，因此一次重新加载即可刷新
  /// 每个活动匹配器上的这两个标志。
  static Future<void> setPassThrough(bool v) async {
    final raw = bind.mainGetLocalOption(key: kShortcutLocalConfigKey);
    Map<String, dynamic> json = {};
    if (raw.isNotEmpty) {
      try {
        json = jsonDecode(raw) as Map<String, dynamic>;
      } catch (_) {
        json = {};
      }
    }
    json['pass_through'] = v;
    await bind.mainSetLocalOption(
        key: kShortcutLocalConfigKey, value: jsonEncode(json));
    bind.mainReloadKeyboardShortcuts();
  }

  /// 翻转主 `enabled` 标志并持久化。首次启用时会写入
  /// 默认绑定，使常用组合键开箱即用；否则我们
  /// 保留用户已有的配置（包括空列表）。同时刷新匹配器缓存，使
  /// 改动立即生效（原生端用 Rust，Web 端通过 bridge
  /// 使用 JS）。
  static Future<void> setEnabled(bool v) async {
    final raw = bind.mainGetLocalOption(key: kShortcutLocalConfigKey);
    Map<String, dynamic> json = {};
    if (raw.isNotEmpty) {
      try {
        json = jsonDecode(raw) as Map<String, dynamic>;
      } catch (_) {
        json = {};
      }
    }
    json['enabled'] = v;
    if (v && shouldSeedDefaultShortcutBindings(json)) {
      json['bindings'] = filterDefaultBindingsForPlatform(
        jsonDecode(bind.mainGetDefaultKeyboardShortcuts()) as List,
        currentPlatformCapabilities(),
      );
    } else if (json.containsKey('bindings')) {
      json['bindings'] = shortcutBindingMapsFrom(json['bindings']);
    }
    await bind.mainSetLocalOption(
        key: kShortcutLocalConfigKey, value: jsonEncode(json));
    bind.mainReloadKeyboardShortcuts();
  }

  /// 判断"这个快捷键是否适用于该平台"的唯一事实来源。
  /// [setEnabled] 的默认写入流程和配置页面的
  /// 重置 / 列表渲染路径都从这里读取，因此写入列表与
  /// 可见的操作列表在某个操作属于哪个平台这件事上
  /// 绝不会出现分歧。
  ///
  /// 能力判定依据：
  ///   * 全屏 / 工具栏 / 固定：在适用
  ///     桌面布局的地方渲染（原生桌面端 + Web）。原生移动端
  ///     始终全屏，没有桌面风格的工具栏。
  ///   * 截图 / 切换屏幕：仅原生桌面端。Web bridge
  ///     对 `sessionTakeScreenshot` 抛出 UnimplementedError；移动端
  ///     工具栏也不提供这两项操作。
  ///   * 标签页导航 / 关闭标签页：只有原生桌面端提供
  ///     `DesktopTabController`；Web 的 `RemotePage` 调用时不带它。
  ///   * 录屏：原生桌面端有 `_RecordMenu` 组件以及
  ///     `registerSessionShortcutActions` 注册；原生 Android 有
  ///     `toolbarControls` 中的入口；iOS 在
  ///     `recordingModel.toggle()` 内部短路处理；Web 没有实现。
  ///   * 重置画布：只有移动端工具栏会构建该菜单项
  ///     （`toolbarControls` 中的 `isDefaultConn && isMobile`）。
  ///   * 语音通话：iOS 没有音频采集实现；Web bridge 对
  ///     `UnimplementedError` 对 `sessionRequestVoiceCall` 和
  ///     `sessionCloseVoiceCall` 都会抛出 UnimplementedError。
  static ShortcutPlatformCapabilities currentPlatformCapabilities() {
    final desktopLayout = isDesktop || isWeb;
    return ShortcutPlatformCapabilities(
      includeFullscreenShortcut: desktopLayout,
      includeScreenshotShortcut: isDesktop,
      includeRelativeMouseModeShortcut:
          isDesktop && !(isLinux && bind.mainCurrentIsWayland()),
      includeTabShortcuts: isDesktop,
      includeToolbarShortcut: desktopLayout,
      includeCloseTabShortcut: isDesktop,
      includeSwitchSidesShortcut: isDesktop,
      includeRecordingShortcut: !isWeb && !isIOS,
      includeResetCanvasShortcut: isMobile,
      includePinToolbarShortcut: desktopLayout,
      includeViewModeShortcut: true,
      includeVoiceCallShortcut: !isWeb && !isIOS,
      excludedActions: Set.unmodifiable({
        if (!isDesktop) ...[
          kShortcutActionSwitchDisplayAll,
          kShortcutActionToggleEnableFileCopyPaste,
        ],
        if (isMobile) ...[
          kShortcutActionKeyboardModeLegacy,
          kShortcutActionKeyboardModeMap,
          kShortcutActionKeyboardModeTranslate,
          kShortcutActionToggleShowMyCursor,
          kShortcutActionToggleZoomCursor,
        ],
      }),
    );
  }
}

/// `keyboard-shortcuts` LocalConfig 值的一份解析视图。格式错误
/// 或缺失的输入都按"已禁用且无绑定"解析。
class ShortcutConfig {
  final bool enabled;
  final bool passThrough;
  final List<Map<String, dynamic>> bindings;

  const ShortcutConfig._(this.enabled, this.passThrough, this.bindings);

  ShortcutConfig forPlatform(ShortcutPlatformCapabilities capabilities) {
    final visible = allActionEntries(
            filterKeyboardShortcutActionGroupsForPlatform(capabilities))
        .map((entry) => entry.id)
        .toSet();
    final hidden = allActionEntries(kKeyboardShortcutActionGroups)
        .map((entry) => entry.id)
        .where((id) => !visible.contains(id))
        .toSet();
    return ShortcutConfig._(
      enabled,
      passThrough,
      List.unmodifiable(bindings.where((b) => !hidden.contains(b['action']))),
    );
  }

  static ShortcutConfig parse(String raw) {
    if (raw.isEmpty) return const ShortcutConfig._(false, false, []);
    try {
      final parsed = jsonDecode(raw) as Map<String, dynamic>;
      return ShortcutConfig._(
        parsed['enabled'] == true,
        parsed['pass_through'] == true,
        List.unmodifiable(shortcutBindingMapsFrom(parsed['bindings'])),
      );
    } catch (_) {
      return const ShortcutConfig._(false, false, []);
    }
  }
}

/// 注册尚未由
/// `toolbarControls(...)` 接好的默认绑定快捷键操作（后者处理 Ctrl+Alt+Shift+Del 以及
/// 截图等操作）。每个会话从桌面端 / 移动端
/// 远程页面调用一次，且在工具栏注册完成之后。
///
/// 我们无条件注册 —— 即使快捷键被主开关禁用 ——
/// 因为匹配器（Rust + JS）通过 `enabled` 标志控制分发，
/// 所以在用户打开快捷键开关之前，已注册的闭包实际上不会生效。
/// 这让接线保持简单（跨会话无需重新绑定回调），
/// 并允许用户在会话中途开关快捷键而无需
/// 重连。
///
/// [tabController] 是桌面窗口的标签页控制器；移动端 / Web 上为 `null`，
/// 这些地方不适用切换标签页的快捷键。
///
/// 当执行该操作所需的底层状态不可用时（例如只有一个显示器、只有一个标签页），
/// 下面的每个回调都什么也不做。
void registerSessionShortcutActions(
  FFI ffi, {
  DesktopTabController? tabController,
  ToolbarState? toolbarState,
}) {
  final sessionId = ffi.sessionId;

  // 关于释放的说明：下面注册的每个闭包都通过闭包环境捕获 `ffi`，
  // 因此 FFI 对象在闭包执行期间始终存活 ——
  // 即使跨越 await，即使会话在执行中途
  // 被关闭也是如此。因此我们不添加逐闭包的存活
  // 检查：闭包在调用栈上时，`WeakReference<FFI>` 检查永远不会为 null，
  // 而底层的 `bind.session*` /
  // model setter 也能容忍会话已失效的调用（在已销毁的
  // 会话上不做任何事）。ShortcutModel.onTriggered 中已有的入口检查
  // （释放后查找 `_callbacks` 返回 null）才是真正的
  // 存活门禁。

  // 切换全屏 —— 在桌面布局渲染的地方都可用
  // （原生桌面端 + 所有 Web 浏览器，因为 Web 使用桌面版
  // RemotePage）。`stateGlobal.setFullscreen` 同时处理原生窗口与
  // 浏览器全屏。原生移动端始终全屏，因此该
  // 操作有意不在那里注册。
  if (isDesktop || isWeb) {
    ffi.shortcutModel.register(kShortcutActionToggleFullscreen, () {
      stateGlobal.setFullscreen(!stateGlobal.fullscreen.value);
    });
  }

  // 切换录屏 —— 这里仅限桌面端。移动端已通过
  // `toolbarControls` 接线（在 `!(isDesktop||isWeb)` 时添加录屏项），
  // 但桌面端工具栏使用单独的 `_RecordMenu` 组件，它没有
  // `actionId`。若没有这次显式注册，桌面端用户可以在设置中绑定
  // "切换录屏"，但按下时不会有任何处理函数。
  // `recordingModel.toggle()` 本身在 iOS 以及没有录屏权限的会话上
  // 会短路处理。
  if (isDesktop) {
    ffi.shortcutModel.register(kShortcutActionToggleRecording, () {
      ffi.recordingModel.toggle();
    });
  }

  void openDisplay(int display, PeerInfo pi, {bool updateCursorPos = true}) {
    if (isDesktop &&
        pi.isSupportMultiDisplay &&
        bind.sessionGetDisplaysAsIndividualWindows(sessionId: sessionId) ==
            'Y') {
      openMonitorInNewTabOrWindow(display, ffi.id, pi);
    } else {
      openMonitorInTheSameTab(display, ffi, pi,
          updateCursorPos: updateCursorPos);
    }
  }

  // 切换到下一个 / 上一个显示器 —— 要求对端至少有 2 个
  // 显示器。从 "All displays" 合并视图中，“下一个”跳到显示器 0，
  // “上一个”跳到最后一个显示器，因此用户总能借此退出合并
  // 视图。
  void switchDisplayBy(int delta) {
    final pi = ffi.ffiModel.pi;
    final privacyMode = PrivacyModeState.find(ffi.id).value;
    if (privacyMode.isNotEmpty &&
        !allowDisplaySwitchInPrivacyMode(pi, privacyMode)) {
      return;
    }
    final count = pi.displays.length;
    if (count <= 1) return;
    final current = pi.currentDisplay;
    final int next;
    if (current == kAllDisplayValue) {
      next = delta > 0 ? 0 : count - 1;
    } else {
      next = ((current + delta) % count + count) % count;
    }
    openDisplay(next, pi, updateCursorPos: isMobile);
  }

  ffi.shortcutModel.register(kShortcutActionSwitchDisplayNext, () {
    switchDisplayBy(1);
  });
  ffi.shortcutModel.register(kShortcutActionSwitchDisplayPrev, () {
    switchDisplayBy(-1);
  });

  // 切换到全部显示器视图 —— 对应工具栏显示器菜单的
  // "all monitors"按钮（仅在对端有多个显示器时构建）。它不是
  // 开关：工具栏按钮只是设置合并视图；需要另一个操作
  // （切换到下一个/上一个显示器，或点击另一个显示器按钮）才能
  // 回到单个显示器。
  //
  // 若要在同一标签页中显示，请使用 `openMonitorInTheSameTab`，而不要直接
  // 用 `[kAllDisplayValue]` 调用 `sessionSwitchDisplay` —— 工具栏
  // 路径会把 `kAllDisplayValue` 视为 UI 哨兵值，并将其展开为
  // 真实的显示器索引列表（`[0, 1, ...]`）后再发送，随后更新
  // 本地 FfiModel 状态。直接发送 `[-1]` 会产生一个远端
  // 无法处理的线上值，并且会跳过本地状态更新，导致合并
  // 视图始终无法生效。
  ffi.shortcutModel.register(kShortcutActionSwitchDisplayAll, () {
    final pi = ffi.ffiModel.pi;
    final privacyMode = PrivacyModeState.find(ffi.id).value;
    if (privacyMode.isNotEmpty &&
        !allowDisplaySwitchInPrivacyMode(pi, privacyMode)) {
      return;
    }
    if (!pi.isSupportMultiDisplay || pi.displays.length <= 1) return;
    if (pi.currentDisplay == kAllDisplayValue) return;
    openDisplay(kAllDisplayValue, pi);
  });

  // 切换到下一个 / 上一个标签页 —— 仅限桌面端。远程屏幕标签页位于
  // 窗口级作用域的 DesktopTabController 上，而不在 FFI 上，因此我们
  // 需要从拥有该会话的页面获取控制器。我们
  // 有意不提供位置式（"Switch to tab N"）快捷键：
  // 在长列表中数标签页并不现实，而 AnyDesk / Chrome
  // 的惯例是优先使用上一个/下一个导航。
  if (tabController != null) {
    void switchTabBy(int delta) {
      final tabs = tabController.state.value.tabs;
      if (tabs.length <= 1) return;
      final cur = tabs.indexWhere((t) => t.key == ffi.id);
      if (cur < 0) return;
      final next = (cur + delta + tabs.length) % tabs.length;
      tabController.jumpTo(next);
    }

    ffi.shortcutModel
        .register(kShortcutActionSwitchTabNext, () => switchTabBy(1));
    ffi.shortcutModel
        .register(kShortcutActionSwitchTabPrev, () => switchTabBy(-1));

    // 关闭标签页 —— 仅限桌面端。对应标签页右键菜单中的 "Close" 项，
    // 包括审计日志确认对话框，因此快捷键关闭与菜单关闭
    // 走的是同一条路径。
    ffi.shortcutModel.register(kShortcutActionCloseTab, () async {
      if (await desktopTryShowTabAuditDialogCloseCancelled(
        id: ffi.id,
        tabController: tabController,
      )) {
        return;
      }
      tabController.closeBy(ffi.id);
    });
  }

  // 切换工具栏 —— 仅限桌面端。ToolbarState 是窗口/会话作用域的，
  // 由承载该会话的 RemotePage 持有。
  if (toolbarState != null) {
    ffi.shortcutModel.register(kShortcutActionToggleToolbar, () {
      toolbarState.switchHide(sessionId);
    });
    ffi.shortcutModel.register(kShortcutActionPinToolbar, () {
      toolbarState.switchPin();
    });
  }

  // 切换聊天浮层（打开/关闭本会话的聊天面板）。
  // _ChatMenu 是一个独立的工具栏图标 —— 不属于任何返回 TToggleMenu 列表的
  // 工具栏辅助函数 —— 因此它的处理函数在这里
  // 接线，而不是由辅助函数自动注册拾取。
  ffi.shortcutModel.register(kShortcutActionToggleChat, () {
    ffi.chatModel.changeCurrentKey(MessageKey(ffi.id, ChatModel.clientModeID));
    ffi.chatModel.toggleChatOverlay();
  });

  // 切换语音通话 —— 空闲时接通，通话中挂断。对应工具栏的
  // `_VoiceCallMenu` 状态驱动按钮。Web bridge 对
  // sessionRequestVoiceCall 和
  // sessionCloseVoiceCall 都会抛出 UnimplementedError；iOS 没有音频采集实现。
  if (!isWeb && !isIOS) {
    ffi.shortcutModel.register(kShortcutActionToggleVoiceCall, () {
      final status = ffi.chatModel.voiceCallStatus.value;
      if (status == VoiceCallStatus.connected ||
          status == VoiceCallStatus.waitingForResponse) {
        bind.sessionCloseVoiceCall(sessionId: sessionId);
      } else {
        bind.sessionRequestVoiceCall(sessionId: sessionId);
      }
    });
  }

  // ── 内联的 _KeyboardMenu 项以及没有对应工具栏 TToggleMenu/TRadioMenu 的操作 ─
  // 工具栏的 TToggleMenu / TRadioMenu 辅助函数（toolbarDisplayToggle、
  // toolbarCursor、toolbarKeyboardToggles、toolbarCodec、toolbarPrivacyMode、
  // toolbarViewStyle、toolbarImageQuality）会在各自辅助函数末尾
  // 自动注册带标签的条目。下面的处理函数覆盖那些
  // 辅助函数"不负责"的部分：
  //   * 显示我的光标 / 键盘模式（Map/Translate/Legacy）/ 仅查看
  //     （桌面端）—— 直接在 `_KeyboardMenu` 中以组件形式构建，而非
  //     TToggleMenu 列表。（移动端的“仅查看”确实在 toolbarDisplayToggle 中，会
  //     自动注册；下方的桌面端会话启动处理函数先注册，
  //     而移动端在辅助函数的自动注册在其未等待的 future 解析后接管。
  //     ）
  //   * 拔出所有虚拟显示器 —— 在 `getVirtualDisplayMenuChildren`
  //     中以 MenuButton 形式构建，而非 TToggleMenu。
  //   * 切换输入源 —— 循环类操作；工具栏提供的是按输入源划分的
  //     单选项，没有"循环到下一个输入源"这一项。

  // 显示我的光标 —— 工具栏（`_KeyboardMenu.showMyCursor`）会把新
  // 值写入 FfiModel.setShowMyCursor，并在开关打开时
  // 自动开启仅查看模式，这样用户就不会在自己光标可见的情况下
  // 控制远端。
  ffi.shortcutModel.register(kShortcutActionToggleShowMyCursor, () async {
    await bind.sessionToggleOption(
        sessionId: sessionId, value: kOptionToggleShowMyCursor);
    final showMyCursor = await bind.sessionGetToggleOption(
            sessionId: sessionId, arg: kOptionToggleShowMyCursor) ??
        false;
    ffi.ffiModel.setShowMyCursor(showMyCursor);
    if (showMyCursor && !ffi.ffiModel.viewOnly) {
      await bind.sessionToggleOption(
          sessionId: sessionId, value: kOptionToggleViewOnly);
      final viewOnly = await bind.sessionGetToggleOption(
              sessionId: sessionId, arg: kOptionToggleViewOnly) ??
          false;
      ffi.ffiModel.setViewOnly(ffi.id, viewOnly);
    }
  });

  // 键盘模式（Map / Translate / Legacy）。对应
  // `_KeyboardMenu.keyboardMode()` 中的单选按钮（以 RdoMenuButton 构建，而非 TRadioMenu）。
  void registerKeyboardMode(String actionId, String mode) {
    ffi.shortcutModel.register(actionId, () async {
      if (!ffi.ffiModel.keyboard ||
          ffi.ffiModel.viewOnly ||
          !bind.sessionIsKeyboardModeSupported(
              sessionId: sessionId, mode: mode) ||
          (ffi.ffiModel.pi.isWayland && mode != kKeyMapMode)) {
        return;
      }
      if (isInputSourceFlutter && isDesktop) {
        final modeOnly = bind.sessionIsKeyboardModeSupported(
                sessionId: sessionId, mode: kKeyMapMode)
            ? kKeyMapMode
            : kKeyLegacyMode;
        if (mode != modeOnly) return;
      }
      await bind.sessionSetKeyboardMode(sessionId: sessionId, value: mode);
      await ffi.inputModel.updateKeyboardMode();
    });
  }

  registerKeyboardMode(kShortcutActionKeyboardModeMap, kKeyMapMode);
  registerKeyboardMode(kShortcutActionKeyboardModeTranslate, kKeyTranslateMode);
  registerKeyboardMode(kShortcutActionKeyboardModeLegacy, kKeyLegacyMode);

  // 拔出所有虚拟显示器（仅 Windows + IDD）。对应工具栏的
  // "Plug out all"按钮 —— 在两种 IDD 模式下都有（RustDesk + Amyuni），
  // 在 `getVirtualDisplayMenuChildren` 中以 MenuButton 构建。
  ffi.shortcutModel.register(kShortcutActionPlugOutAllVirtualDisplays, () {
    if (ffi.connType != ConnType.defaultConn ||
        !showVirtualDisplayMenu(ffi) ||
        PrivacyModeState.find(ffi.id).value.isNotEmpty ||
        (ffi.ffiModel.pi.isAmyuniIdd &&
            ffi.ffiModel.pi.amyuniVirtualDisplayCount == 0)) {
      return;
    }
    bind.sessionToggleVirtualDisplay(
      sessionId: sessionId,
      index: kAllVirtualDisplay,
      on: false,
    );
  });

  // 隐私模式 1 / 2 —— 单实现分支和空实现分支的
  // 回退处理函数，对应 `toolbarPrivacyMode`。多实现分支会给每个条目打上匹配的 actionId，
  // 而 `_registerToggleMenuShortcuts` 会用工具栏自身的 onChanged 覆盖
  // 这些闭包。但当对端只
  // 声明了一个实现时（较老的 Linux 对端、某些平台配置），
  // toolbarPrivacyMode 返回的是没有 actionId 的 `getDefaultMenu` 条目，
  // 自动注册流程会跳过它 —— 正是这些回退处理函数在这种情况下
  // 真正接上了快捷键。
  String? findPrivacyImpl(String nameKey) {
    final impls = ffi.ffiModel.pi
            .platformAdditions[kPlatformAdditionsSupportedPrivacyModeImpl]
        as List<dynamic>?;
    if (impls == null) return null;
    for (final e in impls) {
      if (e is List && e.length >= 2 && e[1] == nameKey) return e[0] as String;
    }
    return null;
  }

  bool checkPrivacyModeAllowed(String implKey) {
    final ffiModel = ffi.ffiModel;
    final pi = ffiModel.pi;
    final privacyMode = PrivacyModeState.find(ffi.id).value;
    if (ffi.connType != ConnType.defaultConn ||
        ffiModel.viewOnly ||
        (privacyMode.isEmpty &&
            (!pi.features.privacyMode || !ffiModel.keyboard))) {
      return false;
    }
    if (privacyMode == implKey) return true;
    if (ffiModel.permissions['privacy_mode'] == false) return false;
    if (allowDisplaySwitchInPrivacyMode(pi, implKey) ||
        (pi.currentDisplay == 0 &&
            !bind.sessionIsMultiUiSession(sessionId: sessionId))) {
      return true;
    }
    msgBox(sessionId, 'custom-nook-nocancel-hasclose', 'info',
        'Please switch to Display 1 first', '', ffi.dialogManager);
    return false;
  }

  // 与 `toolbarPrivacyMode` 的多实现分支保持一致：仅当
  // 当前启用的实现不是本实现时才开启本实现。比较 `.value == implKey`
  // （而不是 `.value.isEmpty`）意味着在模式 2 开启时按下模式 1 快捷键
  // 会正确地把模式 1 打开，而不是把
  // "任意模式已启用"误读为"本模式已启用"从而反向关闭。
  ffi.shortcutModel.register(kShortcutActionPrivacyMode1, () {
    if (ffi.ffiModel.pi
            .platformAdditions[kPlatformAdditionsSupportedPrivacyModeImpl] ==
        null) {
      // 较老的对端使用传统选项而非具名实现。
      if (!checkPrivacyModeAllowed(kPrivacyModeImplMag)) return;
      bind.sessionToggleOption(sessionId: sessionId, value: 'privacy-mode');
      return;
    }
    final implKey = findPrivacyImpl('privacy_mode_impl_mag_tip');
    if (implKey == null) return;
    if (!checkPrivacyModeAllowed(implKey)) return;
    bind.sessionTogglePrivacyMode(
      sessionId: sessionId,
      implKey: implKey,
      on: PrivacyModeState.find(ffi.id).value != implKey,
    );
  });
  ffi.shortcutModel.register(kShortcutActionPrivacyMode2, () {
    final implKey = findPrivacyImpl('privacy_mode_impl_virtual_display_tip');
    if (implKey == null) return;
    if (!checkPrivacyModeAllowed(implKey)) return;
    bind.sessionTogglePrivacyMode(
      sessionId: sessionId,
      implKey: implKey,
      on: PrivacyModeState.find(ffi.id).value != implKey,
    );
  });

  // 仅查看 —— 桌面端工具栏在 `_KeyboardMenu.viewMode` 中内联提供
  // （移动端在 toolbarDisplayToggle 中，通过辅助函数自动注册）。
  // 对应桌面端回调：切换并同步 FfiModel.viewOnly +
  // FfiModel.showMyCursor（工具栏会让两者保持一致）。
  ffi.shortcutModel.register(kShortcutActionToggleViewOnly, () async {
    await bind.sessionToggleOption(
        sessionId: sessionId, value: kOptionToggleViewOnly);
    final viewOnly = await bind.sessionGetToggleOption(
            sessionId: sessionId, arg: kOptionToggleViewOnly) ??
        false;
    ffi.ffiModel.setViewOnly(ffi.id, viewOnly);
    final showMyCursor = await bind.sessionGetToggleOption(
            sessionId: sessionId, arg: kOptionToggleShowMyCursor) ??
        false;
    ffi.ffiModel.setShowMyCursor(showMyCursor);
  });

  // 切换反向鼠标滚轮 —— 读取当前的 'Y'/'N'（回退到用户
  // 默认值），取反后写回。
  ffi.shortcutModel.register(kShortcutActionToggleReverseMouseWheel, () async {
    if (!ffi.ffiModel.keyboard || ffi.ffiModel.viewOnly) return;
    var cur = bind.sessionGetReverseMouseWheelSync(sessionId: sessionId) ?? '';
    if (cur == '') {
      cur = bind.mainGetUserDefaultOption(key: kKeyReverseMouseWheel);
    }
    final next = cur == 'Y' ? 'N' : 'Y';
    await bind.sessionSetReverseMouseWheel(sessionId: sessionId, value: next);
  });

  // 切换相对鼠标模式（游戏模式）。仅限桌面端。
  if (isDesktop && !isWeb) {
    ffi.shortcutModel.register(kShortcutActionToggleRelativeMouseMode, () {
      ffi.inputModel.toggleRelativeMouseMode();
    });
  }
}
