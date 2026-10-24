// flutter/lib/desktop/pages/desktop_keyboard_shortcuts_page.dart
//
// 键盘快捷键配置页面的桌面端外壳。用户从
// General 设置选项卡进入本页。本页提供：
//   * 一个顶层的启用/禁用开关（与 General 选项卡中的开关一致——
//     同一个 JSON key，语义相同）。
//   * 一个分组的、可滚动的操作列表，每项包含当前绑定以及
//     编辑 / 清除图标。
//   * 一个 AppBar "恢复默认" 操作，带确认对话框。
//
// 所有修改都会在 [kShortcutLocalConfigKey] 下写回 LocalConfig，采用
// 规范的 {enabled, bindings:[{action,mods,key}]} 结构，供 Rust 与
// Web matcher 使用。
//
// 主体——分组定义、JSON 读写、冲突替换流程、
// 录制对话框的来回流程——位于
// `common/widgets/keyboard_shortcuts/page_body.dart`，并与
// `mobile/pages/mobile_keyboard_shortcuts_page.dart` 的移动端外壳共享。

import 'package:flutter/material.dart';
import 'package:get/get.dart';

import '../../common.dart';
import '../../common/widgets/keyboard_shortcuts/page_body.dart';

class DesktopKeyboardShortcutsPage extends StatefulWidget {
  const DesktopKeyboardShortcutsPage({Key? key}) : super(key: key);

  @override
  State<DesktopKeyboardShortcutsPage> createState() =>
      _DesktopKeyboardShortcutsPageState();
}

class _DesktopKeyboardShortcutsPageState
    extends State<DesktopKeyboardShortcutsPage> {
  final GlobalKey<KeyboardShortcutsPageBodyState> _bodyKey = GlobalKey();

  @override
  Widget build(BuildContext context) {
    final foregroundColor =
        AppBarTheme.of(context).titleTextStyle?.color ?? Colors.white;
    return Scaffold(
      appBar: AppBar(
        title: Text(translate('Keyboard shortcuts')),
        actions: [
          TextButton.icon(
            style: TextButton.styleFrom(foregroundColor: foregroundColor),
            onPressed: () =>
                _bodyKey.currentState?.resetToDefaultsWithConfirm(),
            icon: const Icon(Icons.restore),
            label: Text(translate('Reset to defaults')),
          ).marginOnly(right: 12),
        ],
      ),
      body: KeyboardShortcutsPageBody(
        key: _bodyKey,
        compact: true,
        // 桌面端的 General 设置选项卡已经提供了 Enable +
        // Pass-through 复选框（它是进入本页的唯一入口），
        // 因此这里隐藏重复项。移动端外壳保持默认值
        // （true），因为它们的入口磁贴不包含这些开关。
        showMasterToggles: false,
      ),
    );
  }
}
