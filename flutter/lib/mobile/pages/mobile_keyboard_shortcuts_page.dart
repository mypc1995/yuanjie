// flutter/lib/mobile/pages/mobile_keyboard_shortcuts_page.dart
//
// 键盘快捷键配置页的移动端外壳。对应
// `desktop/pages/desktop_keyboard_shortcuts_page.dart`，但采用适合触摸
// 的布局（使用 ListTile 行而不是紧凑的行），并带有提示横幅
// 说明录制流程只在使用物理键盘时才能工作。
//
// 所有实际逻辑——分组定义、JSON 读写、冲突替换流程、
// 录制对话框的往返流程、"Reset to defaults"——都放在共享的
// `common/widgets/keyboard_shortcuts/page_body.dart` 中。本文件只
// 提供 AppBar、AppBar 操作按钮和平台提示横幅。
//
// 移动端键盘检测的限制：Flutter 在 iOS 或 Android 上没有可靠的
// "is a physical keyboard attached?" API。软键盘
// 不会产生录制对话框所监听的 `KeyDownEvent`，因此
// 实际上只有当用户真的
// 插入了硬件键盘（USB / 蓝牙 / Smart Connector）时，该对话框才有用。
// 在 V1 中我们不尝试检测是否连接了键盘——只是把这一
// 要求以页面内提示的形式呈现，而不是禁用编辑按钮。

import 'package:flutter/material.dart';

import '../../common.dart';
import '../../common/widgets/keyboard_shortcuts/page_body.dart';

class MobileKeyboardShortcutsPage extends StatefulWidget {
  const MobileKeyboardShortcutsPage({Key? key}) : super(key: key);

  @override
  State<MobileKeyboardShortcutsPage> createState() =>
      _MobileKeyboardShortcutsPageState();
}

class _MobileKeyboardShortcutsPageState
    extends State<MobileKeyboardShortcutsPage> {
  final GlobalKey<KeyboardShortcutsPageBodyState> _bodyKey = GlobalKey();

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      appBar: AppBar(
        title: Text(translate('Keyboard shortcuts')),
        actions: [
          IconButton(
            tooltip: translate('Reset to defaults'),
            onPressed: () =>
                _bodyKey.currentState?.resetToDefaultsWithConfirm(),
            icon: const Icon(Icons.restore),
          ),
        ],
      ),
      body: KeyboardShortcutsPageBody(
        key: _bodyKey,
        compact: false,
        editButtonHint: translate('shortcut-mobile-physical-keyboard-tip'),
        headerBanner: _PhysicalKeyboardHintBanner(theme: theme),
      ),
    );
  }
}

/// 移动端显示在总开关上方的弱化提示横幅。我们无法
/// 可靠地检测是否连接了物理键盘，因此我们没有
/// 禁用编辑按钮，而是提前把这一要求提示出来。
class _PhysicalKeyboardHintBanner extends StatelessWidget {
  final ThemeData theme;
  const _PhysicalKeyboardHintBanner({required this.theme});

  @override
  Widget build(BuildContext context) {
    final color = theme.colorScheme.primary.withOpacity(0.08);
    return Container(
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: color,
        borderRadius: BorderRadius.circular(8),
      ),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(Icons.info_outline,
              size: 18, color: theme.colorScheme.primary),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              translate('shortcut-mobile-physical-keyboard-tip'),
              style: TextStyle(color: theme.colorScheme.onSurface),
            ),
          ),
        ],
      ),
    );
  }
}
