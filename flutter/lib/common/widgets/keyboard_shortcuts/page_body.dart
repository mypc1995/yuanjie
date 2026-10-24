// flutter/lib/common/widgets/keyboard_shortcuts/page_body.dart
//
// 键盘快捷键配置页面的共享主体组件。桌面端（
// desktop (`desktop/pages/desktop_keyboard_shortcuts_page.dart`) 和移动端
// (`mobile/pages/mobile_keyboard_shortcuts_page.dart`) 页面都将该组件
// 渲染在各自平台风格的 Scaffold + AppBar 外壳中。
//
// 该主体负责：
//   * 顶层的启用/禁用开关（与 General 选项卡中的开关一致——
//     同一个 JSON key，语义相同）；
//   * 分组排列的操作列表，每项包含当前绑定以及
//     编辑 / 清除图标；
//   * 在 [kShortcutLocalConfigKey] 下的 JSON 读写辅助方法，采用
//     规范的 {enabled, bindings:[{action,mods,key}]} 结构；
//   * 录制对话框的来回流程以及冲突替换的处理；
//   * "恢复默认"（由平台 AppBar 调用）。
//
// 平台外壳只提供：
//   * AppBar（其中的"恢复默认"操作会调用
//     [KeyboardShortcutsPageBodyState.resetToDefaultsWithConfirm]）；
//   * 外围内边距 / list-tile 与紧凑行两种视觉效果由
//     [compact] 标志控制。

import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../../../common.dart';
import '../../../consts.dart';
import '../../../models/platform_model.dart';
import '../../../models/shortcut_model.dart';
import 'display.dart';
import 'recording_dialog.dart';
import 'shortcut_actions.dart';
import 'shortcut_utils.dart';

/// 共享的主体组件。请将它渲染在平台风格的 Scaffold 内部。
///
/// [compact] 在桌面端的紧凑行布局（`true`）与移动端的
/// 适合触控的 ListTile 布局（`false`）之间切换。
///
/// [editButtonHint] 作为 Edit 图标的提示文字显示。移动端外壳
/// 可以用它说明录制快捷键需要物理键盘。
///
/// [headerBanner] 是渲染在开关上方的可选组件。移动端
/// 用它显示"录制快捷键需要物理键盘"的提示。
class KeyboardShortcutsPageBody extends StatefulWidget {
  final bool compact;
  final String? editButtonHint;
  final Widget? headerBanner;

  /// 是否在主体内部渲染总开关 Enable + Pass-through。
  /// 桌面端外壳将其设为 false，因为 General 设置选项卡
  /// 已经提供了这两个复选框（而且是桌面端进入本
  /// 页面的唯一入口）。移动端默认为 true：其入口是设置中的一个普通
  /// 导航磁贴，因此本页是用户唯一可以
  /// 切换这些总开关的地方。
  final bool showMasterToggles;

  const KeyboardShortcutsPageBody({
    Key? key,
    this.compact = true,
    this.editButtonHint,
    this.headerBanner,
    this.showMasterToggles = true,
  }) : super(key: key);

  @override
  State<KeyboardShortcutsPageBody> createState() =>
      KeyboardShortcutsPageBodyState();
}

/// 公开状态，便于平台外壳调用 [resetToDefaultsWithConfirm]，
/// 调用入口是它们的 AppBar 操作。
class KeyboardShortcutsPageBodyState extends State<KeyboardShortcutsPageBody> {
  // ----- 持久化辅助方法 -----

  Map<String, dynamic> _readJson() {
    final raw = bind.mainGetLocalOption(key: kShortcutLocalConfigKey);
    if (raw.isEmpty) return {'enabled': false, 'bindings': <dynamic>[]};
    try {
      final parsed = jsonDecode(raw) as Map<String, dynamic>;
      parsed['bindings'] ??= <dynamic>[];
      parsed['enabled'] ??= false;
      return parsed;
    } catch (_) {
      return {'enabled': false, 'bindings': <dynamic>[]};
    }
  }

  Future<void> _writeJson(Map<String, dynamic> json) async {
    await bind.mainSetLocalOption(
        key: kShortcutLocalConfigKey, value: jsonEncode(json));
    // 刷新 matcher 缓存，使写入立即生效。在原生平台上
    // 这会调用 Rust matcher；在 Web 上 bridge 会转发给 JS 端的
    // matcher（位于 flutter/web/js/）。
    bind.mainReloadKeyboardShortcuts();
    if (mounted) setState(() {});
  }

  /// 用 [binding] 替换 [actionId] 的绑定条目。如果 [binding]
  /// 为 null，则移除已有条目。如果用户正在替换一个
  /// 冲突的绑定，[clearActionId] 指向那个操作，其
  /// （现已失效的）绑定应在同一次写入中被移除。
  Future<void> _setBinding(
    String actionId, {
    Map<String, dynamic>? binding,
    String? clearActionId,
  }) async {
    final json = _readJson();
    final list = shortcutBindingMapsFrom(json['bindings']);
    list.removeWhere((b) {
      final a = b['action'];
      return a == actionId || (clearActionId != null && a == clearActionId);
    });
    if (binding != null) {
      list.add(binding);
    }
    json['bindings'] = list;
    await _writeJson(json);
  }

  Future<void> _setEnabled(bool v) async {
    await ShortcutModel.setEnabled(v);
    if (mounted) setState(() {});
  }

  Future<void> _setPassThrough(bool v) async {
    await ShortcutModel.setPassThrough(v);
    if (mounted) setState(() {});
  }

  Future<void> _resetToDefaults() async {
    final json = _readJson();
    // 唯一的事实来源位于 `ShortcutModel.currentPlatformCapabilities`
    // —— 同一个辅助方法同时供首次启用时的种子填充、本重置操作、
    // 以及下方的操作列表过滤使用，因此这三者永远不会在
    // 哪些操作属于当前平台这一点上产生分歧。
    json['bindings'] = filterDefaultBindingsForPlatform(
      jsonDecode(bind.mainGetDefaultKeyboardShortcuts()) as List,
      ShortcutModel.currentPlatformCapabilities(),
    );
    await _writeJson(json);
  }

  String _labelFor(String actionId) {
    // 有意遍历未过滤的列表（借助递归辅助方法，因此
    // 直接条目和子分组条目都能覆盖）—— 一个过期的
    // 跨平台绑定（例如从
    // 桌面端延续过来的 Toggle Toolbar）仍应在冲突
    // 警告中解析出它可读的名称。
    for (final entry in allActionEntries(kKeyboardShortcutActionGroups)) {
      if (entry.id == actionId) return translate(entry.labelKey);
    }
    return actionId;
  }

  /// 当前平台上可见的操作分组。读取的能力集与种子默认值 /
  /// 恢复默认值路径所用的完全相同，来自
  /// `ShortcutModel.currentPlatformCapabilities`，因此 UI 恰好列出
  /// 匹配器在此平台上能派发的那些操作。
  List<KeyboardShortcutActionGroup> _groupsForCurrentPlatform() {
    return filterKeyboardShortcutActionGroupsForPlatform(
      ShortcutModel.currentPlatformCapabilities(),
    );
  }

  // ----- UI 处理器 -----

  Future<void> _onEdit(KeyboardShortcutActionEntry entry) async {
    final json = _readJson();
    final bindings = shortcutBindingMapsFrom(json['bindings']);
    final result = await showRecordingDialog(
      context: context,
      actionId: entry.id,
      actionLabel: translate(entry.labelKey),
      existingBindings: bindings,
      actionLabelLookup: _labelFor,
    );
    if (result == null) return;
    await _setBinding(
      entry.id,
      binding: result.binding,
      clearActionId: result.clearActionId,
    );
  }

  Future<void> _onClear(KeyboardShortcutActionEntry entry) async {
    await _setBinding(entry.id, binding: null);
  }

  /// 公开 —— 由平台 AppBar 操作调用。
  Future<void> resetToDefaultsWithConfirm() async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(translate('Reset to defaults')),
        content: Text(translate('shortcut-reset-confirm-tip')),
        actions: [
          dialogButton('Cancel',
              onPressed: () => Navigator.of(ctx).pop(false), isOutline: true),
          dialogButton('OK', onPressed: () => Navigator.of(ctx).pop(true)),
        ],
      ),
    );
    if (confirmed == true) {
      await _resetToDefaults();
    }
  }

  // ----- 构建 -----

  @override
  Widget build(BuildContext context) {
    final enabled = ShortcutModel.isEnabled();
    final theme = Theme.of(context);

    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        if (widget.headerBanner != null) ...[
          widget.headerBanner!,
          const SizedBox(height: 12),
        ],
        if (widget.showMasterToggles) ...[
          _toggleRow(
            enabled,
            'Enable keyboard shortcuts in remote session',
            (v) => _setEnabled(v),
          ),
          if (enabled)
            _toggleRow(
              ShortcutModel.isPassThrough(),
              'Pass-through to remote',
              (v) => _setPassThrough(v),
            ),
        ],
        const SizedBox(height: 8),
        Padding(
          padding: const EdgeInsets.symmetric(horizontal: 8),
          child: Text(
            translate('shortcut-page-description'),
            style: TextStyle(color: theme.hintColor),
          ),
        ),
        const SizedBox(height: 16),
        // 只有当快捷键处于
        // 启用状态时，绑定列表和配置入口才显示——匹配器关闭时没有可配置的内容。
        if (enabled)
          Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              for (final group in _groupsForCurrentPlatform())
                _buildGroup(context, group),
            ],
          ),
      ],
    );
  }

  Widget _toggleRow(
      bool value, String labelKey, Future<void> Function(bool) onChanged,
      {String? tooltipKey}) {
    return Row(
      children: [
        Checkbox(
          value: value,
          onChanged: (v) async {
            if (v == null) return;
            await onChanged(v);
          },
        ),
        const SizedBox(width: 4),
        Expanded(
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onTap: () => onChanged(!value),
            child: Text(translate(labelKey)),
          ),
        ),
        if (tooltipKey != null) InfoTooltipIcon(tipKey: tooltipKey),
      ],
    );
  }

  // 每一层嵌套对应一个缩进单位。"顶级标题下的顶级条目"和
  // "顶级分组下的子分组标题"都比顶级标题
  // 深*一*级，因此它们共用这个缩进——也就是说，顶层的直接
  // 条目与同级的子分组标题正好对齐在同一个 x 位置。
  // 子分组的条目要深*两*级。
  static const double _kIndentStep = 16.0;

  /// 顶级分组：标题零缩进，然后按声明顺序遍历 `children`，
  /// 直接条目获得 [_kIndentStep] 的缩进，这样
  /// 它们读起来就像"这个标题下的条目"；子分组标题则位于
  /// 相同的缩进处（子分组与直接条目是同级关系，只是
  /// 在下方带有自己的嵌套条目）。
  Widget _buildGroup(BuildContext context, KeyboardShortcutActionGroup group) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const SizedBox(height: 12),
        _buildHeading(context, group.titleKey, isSub: false),
        const SizedBox(height: 4),
        for (final child in group.children)
          switch (child) {
            KeyboardShortcutActionEntry() => Padding(
                padding: const EdgeInsets.only(left: _kIndentStep),
                child: _buildEntryRow(context, child),
              ),
            KeyboardShortcutActionSubgroup() =>
              _buildSubgroup(context, child),
          },
      ],
    );
  }

  Widget _buildSubgroup(
      BuildContext context, KeyboardShortcutActionSubgroup subgroup) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const SizedBox(height: 8),
        _buildHeading(context, subgroup.titleKey, isSub: true),
        const SizedBox(height: 4),
        for (final entry in subgroup.entries)
          Padding(
            // 两个缩进步进：一个用于"子分组标题嵌套在
            // 顶级标题之下"（与该标题自身的缩进相同），另一个用于
            // "该条目位于子分组标题之下"。
            padding: const EdgeInsets.only(left: _kIndentStep * 2),
            child: _buildEntryRow(context, entry),
          ),
      ],
    );
  }

  Widget _buildHeading(BuildContext context, String titleKey,
      {required bool isSub}) {
    // 子分组标题比顶级标题多嵌套一级——缩进与
    // 顶层的直接条目相同，因此两者对齐在同一 x 位置。
    final indent = isSub ? _kIndentStep : 0.0;
    return Padding(
      padding: EdgeInsets.only(left: 8 + indent, right: 8),
      child: Row(
        children: [
          Text(
            translate(titleKey),
            style: TextStyle(
              fontWeight: isSub ? FontWeight.w500 : FontWeight.w600,
              color: isSub
                  ? Theme.of(context).hintColor
                  : Theme.of(context).colorScheme.primary,
            ),
          ),
          const SizedBox(width: 8),
          Expanded(child: Divider(thickness: isSub ? 0.5 : 1)),
        ],
      ),
    );
  }

  Widget _buildEntryRow(
      BuildContext context, KeyboardShortcutActionEntry entry) {
    return widget.compact
        ? _buildCompactRow(context, entry)
        : _buildTouchRow(context, entry);
  }

  /// 桌面端紧凑行：名称 | 快捷键 | 编辑 | 清除，全部放在同一个 Row 中。
  Widget _buildCompactRow(
      BuildContext context, KeyboardShortcutActionEntry entry) {
    final shortcut = ShortcutDisplay.formatFor(entry.id, requireEnabled: false);
    final hasBinding = shortcut != null;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 6),
      child: Row(
        children: [
          Expanded(
            flex: 5,
            child: Text(translate(entry.labelKey)),
          ),
          Expanded(
            flex: 4,
            child: Text(
              shortcut ?? '—',
              style: TextStyle(
                fontFamily: defaultTargetPlatform == TargetPlatform.windows
                    ? 'Consolas'
                    : 'monospace',
                color: hasBinding ? null : Theme.of(context).hintColor,
              ),
            ),
          ),
          IconButton(
            tooltip: widget.editButtonHint ?? translate('Edit'),
            onPressed: () => _onEdit(entry),
            icon: const Icon(Icons.edit_outlined, size: 18),
          ),
          SizedBox(
            width: 40,
            child: hasBinding
                ? IconButton(
                    tooltip: translate('Clear'),
                    onPressed: () => _onClear(entry),
                    icon: const Icon(Icons.close, size: 18),
                  )
                : const SizedBox.shrink(),
          ),
        ],
      ),
    );
  }

  /// 移动端触控行：带 title + subtitle + 尾部图标的 ListTile。
  Widget _buildTouchRow(
      BuildContext context, KeyboardShortcutActionEntry entry) {
    final shortcut = ShortcutDisplay.formatFor(entry.id, requireEnabled: false);
    final hasBinding = shortcut != null;
    return ListTile(
      dense: false,
      contentPadding: const EdgeInsets.symmetric(horizontal: 8),
      title: Text(translate(entry.labelKey)),
      subtitle: Text(
        shortcut ?? '—',
        style: TextStyle(
          fontFamily: defaultTargetPlatform == TargetPlatform.windows
              ? 'Consolas'
              : 'monospace',
          color: hasBinding ? null : Theme.of(context).hintColor,
        ),
      ),
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          IconButton(
            tooltip: widget.editButtonHint ?? translate('Edit'),
            onPressed: () => _onEdit(entry),
            icon: const Icon(Icons.edit_outlined),
          ),
          if (hasBinding)
            IconButton(
              tooltip: translate('Clear'),
              onPressed: () => _onClear(entry),
              icon: const Icon(Icons.close),
            )
          else
            const SizedBox(width: 48),
        ],
      ),
    );
  }
}

/// 用于在复选框 /
/// 行旁提供内联说明的小型帮助图标提示。桌面端悬停、移动端点击时触发。公开以便桌面端
/// General 设置选项卡可以复用它。
class InfoTooltipIcon extends StatelessWidget {
  final String tipKey;
  const InfoTooltipIcon({Key? key, required this.tipKey}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    return Tooltip(
      message: translate(tipKey),
      triggerMode: TooltipTriggerMode.tap,
      preferBelow: false,
      waitDuration: const Duration(milliseconds: 250),
      showDuration: const Duration(seconds: 6),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 6),
        child: Icon(
          Icons.help_outline,
          size: 16,
          color: Theme.of(context).hintColor,
        ),
      ),
    );
  }
}
