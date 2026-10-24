import 'dart:convert';
import 'dart:io';

import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_hbb/common/widgets/keyboard_shortcuts/shortcut_actions.dart';
import 'package:flutter_hbb/common/widgets/keyboard_shortcuts/shortcut_constants.dart';
import 'package:flutter_hbb/common/widgets/keyboard_shortcuts/shortcut_utils.dart';
import 'package:flutter_hbb/models/shortcut_model.dart';

ShortcutPlatformCapabilities capabilities({
  bool includeFullscreenShortcut = true,
  bool includeScreenshotShortcut = true,
  bool includeRelativeMouseModeShortcut = true,
  bool includeTabShortcuts = true,
  bool includeToolbarShortcut = true,
  bool includeCloseTabShortcut = true,
  bool includeSwitchSidesShortcut = true,
  bool includeRecordingShortcut = true,
  bool includeResetCanvasShortcut = true,
  bool includePinToolbarShortcut = true,
  bool includeViewModeShortcut = true,
  bool includeVoiceCallShortcut = true,
  Set<String> excludedActions = const {},
}) {
  return ShortcutPlatformCapabilities(
    includeFullscreenShortcut: includeFullscreenShortcut,
    includeScreenshotShortcut: includeScreenshotShortcut,
    includeRelativeMouseModeShortcut: includeRelativeMouseModeShortcut,
    includeTabShortcuts: includeTabShortcuts,
    includeToolbarShortcut: includeToolbarShortcut,
    includeCloseTabShortcut: includeCloseTabShortcut,
    includeSwitchSidesShortcut: includeSwitchSidesShortcut,
    includeRecordingShortcut: includeRecordingShortcut,
    includeResetCanvasShortcut: includeResetCanvasShortcut,
    includePinToolbarShortcut: includePinToolbarShortcut,
    includeViewModeShortcut: includeViewModeShortcut,
    includeVoiceCallShortcut: includeVoiceCallShortcut,
    excludedActions: excludedActions,
  );
}

void main() {
  test('kDefaultShortcutBindings matches fixture', () {
    // 该 fixture 是跨语言默认绑定的唯一事实来源。
    // Rust 侧有针对同一文件的一致性测试
    // （src/keyboard/shortcuts.rs 中的 `default_bindings_match_fixture_json`），
    // 因此任何一侧发生偏移都会让 CI 失败。
    final fixturePath = 'test/fixtures/default_keyboard_shortcuts.json';
    final fixture =
        jsonDecode(File(fixturePath).readAsStringSync()) as List<dynamic>;
    expect(kDefaultShortcutBindings, equals(fixture),
        reason: 'kDefaultShortcutBindings drifted from $fixturePath — update '
            'shortcut_constants.dart, the fixture, and Rust default_bindings() '
            'together');
  });

  test('save order preserves macOS control modifier', () {
    expect(canonicalShortcutModsForSave({'ctrl'}), ['ctrl']);
    expect(canonicalShortcutModsForSave({'shift', 'ctrl', 'primary', 'alt'}),
        ['primary', 'ctrl', 'alt', 'shift']);
  });

  test('shortcutBindingMapsFrom ignores malformed bindings', () {
    expect(shortcutBindingMapsFrom('not a list'), isEmpty);

    final bindings = shortcutBindingMapsFrom([
      {
        'action': kShortcutActionScreenshot,
        'mods': ['primary'],
        'key': 'p',
      },
      'bad',
      1,
      {
        'action': kShortcutActionToggleMute,
        'mods': ['alt'],
        'key': 's',
      },
    ]);

    expect(bindings, hasLength(2));
    expect(bindings.map((binding) => binding['action']), [
      kShortcutActionScreenshot,
      kShortcutActionToggleMute,
    ]);
  });

  test('shortcutBindingMapsFrom ignores malformed binding fields', () {
    final valid = {
      'action': kShortcutActionScreenshot,
      'mods': ['primary', 'ctrl', 'alt', 'shift'],
      'key': 'p',
    };
    final malformed = [
      for (final field in ['action', 'mods', 'key'])
        {...valid}..remove(field),
      for (final fields in [
        {'action': 1},
        {'action': null},
        {'key': 1},
        {'key': null},
        {'mods': null},
        {'mods': 'primary'},
        {'mods': ['primary', 1]},
        {'mods': ['primary', null]},
        {'mods': ['primary', 'unknown']},
      ])
        {...valid, ...fields},
    ];

    for (final binding in malformed) {
      expect(shortcutBindingMapsFrom([binding, valid]), [valid],
          reason: 'malformed binding should be ignored: $binding');
    }
  });

  test('shortcutBindingMapsFrom preserves unbound and unknown bindings', () {
    expect(
      shortcutBindingMapsFrom([
        {'action': kShortcutActionScreenshot, 'mods': [], 'key': ''},
        {'action': 'future_action', 'mods': ['ctrl'], 'key': 'future_key'},
      ]),
      [
        {'action': kShortcutActionScreenshot, 'mods': [], 'key': ''},
        {'action': 'future_action', 'mods': ['ctrl'], 'key': 'future_key'},
      ],
    );
  });

  test('platform filtering ignores malformed binding fields', () {
    final filtered = filterDefaultBindingsForPlatform([
      {'action': 1, 'mods': ['primary'], 'key': 'p'},
      {'action': kShortcutActionScreenshot, 'mods': ['primary'], 'key': 1},
      {'action': kShortcutActionToggleMute, 'mods': ['alt'], 'key': 's'},
      {
        'action': kShortcutActionToggleFullscreen,
        'mods': ['primary'],
        'key': 'enter',
      },
    ], capabilities(includeFullscreenShortcut: false));

    expect(filtered, [
      {'action': kShortcutActionToggleMute, 'mods': ['alt'], 'key': 's'},
    ]);
  });

  test('shortcutModSetFrom ignores malformed modifiers', () {
    expect(shortcutModSetFrom('not a list'), isEmpty);
    expect(shortcutModSetFrom(['primary', 1, 'alt', null, 'primary']), {
      'primary',
      'alt',
    });
  });

  test('non-desktop defaults exclude desktop-only and tab shortcuts', () {
    final defaults = [
      {
        'action': kShortcutActionSendCtrlAltDel,
        'mods': ['primary', 'alt', 'shift'],
        'key': 'delete',
      },
      {
        'action': kShortcutActionToggleFullscreen,
        'mods': ['primary', 'alt', 'shift'],
        'key': 'enter',
      },
      {
        'action': kShortcutActionSwitchDisplayNext,
        'mods': ['primary', 'alt', 'shift'],
        'key': 'arrow_right',
      },
      {
        'action': kShortcutActionScreenshot,
        'mods': ['primary', 'alt', 'shift'],
        'key': 'p',
      },
      {
        'action': kShortcutActionSwitchTabNext,
        'mods': ['primary', 'alt', 'shift'],
        'key': 'right_bracket',
      },
      {
        'action': kShortcutActionToggleRelativeMouseMode,
        'mods': ['primary', 'alt', 'shift'],
        'key': 'g',
      },
    ];

    final filtered = filterDefaultBindingsForPlatform(
      defaults,
      capabilities(
        includeFullscreenShortcut: false,
        includeScreenshotShortcut: false,
        includeRelativeMouseModeShortcut: false,
        includeTabShortcuts: false,
        includeToolbarShortcut: false,
        includeCloseTabShortcut: false,
        includeSwitchSidesShortcut: false,
        includeRecordingShortcut: false,
        includeResetCanvasShortcut: false,
        includePinToolbarShortcut: false,
        includeViewModeShortcut: false,
        includeVoiceCallShortcut: false,
      ),
    );

    expect(filtered.map((binding) => binding['action']), [
      kShortcutActionSendCtrlAltDel,
      kShortcutActionSwitchDisplayNext,
    ]);
  });

  Set<String> idSet(Iterable<KeyboardShortcutActionGroup> groups) =>
      {for (final e in allActionEntries(groups)) e.id};

  test('filterKeyboardShortcutActionGroupsForPlatform strips desktop-only', () {
    final groups = filterKeyboardShortcutActionGroupsForPlatform(
      capabilities(
        includeFullscreenShortcut: false,
        includeScreenshotShortcut: false,
        includeRelativeMouseModeShortcut: false,
        includeTabShortcuts: false,
        includeToolbarShortcut: false,
        includeCloseTabShortcut: false,
        includeSwitchSidesShortcut: false,
        // 录屏 / 重置画布在此处仍被有意保留 ——
        // 它们也有非桌面端平台（移动端 Android / 两者均为移动端）。
        includeRecordingShortcut: true,
        includeResetCanvasShortcut: true,
        includePinToolbarShortcut: false,
        includeViewModeShortcut: false,
        includeVoiceCallShortcut: false,
      ),
    );
    final ids = idSet(groups);
    // 仅桌面端的操作会被剔除。
    expect(ids, isNot(contains(kShortcutActionToggleFullscreen)));
    expect(ids, isNot(contains(kShortcutActionToggleRelativeMouseMode)));
    expect(ids, isNot(contains(kShortcutActionScreenshot)));
    expect(ids, isNot(contains(kShortcutActionToggleToolbar)));
    expect(ids, isNot(contains(kShortcutActionCloseTab)));
    expect(ids, isNot(contains(kShortcutActionSwitchSides)));
    expect(ids, isNot(contains(kShortcutActionPinToolbar)));
    expect(ids, isNot(contains(kShortcutActionViewModeOriginal)));
    expect(ids, isNot(contains(kShortcutActionViewModeAdaptive)));
    expect(ids, isNot(contains(kShortcutActionSwitchTabNext)));
    expect(ids, isNot(contains(kShortcutActionSwitchTabPrev)));
    // 跨平台的操作被保留。
    expect(ids, contains(kShortcutActionSendCtrlAltDel));
    expect(ids, contains(kShortcutActionInsertLock));
    expect(ids, contains(kShortcutActionRestartRemote));
    expect(ids, contains(kShortcutActionSwitchDisplayNext));
    expect(ids, contains(kShortcutActionToggleRecording));
    expect(ids, contains(kShortcutActionResetCanvas));
    expect(ids, contains(kShortcutActionToggleMute));
  });

  test(
      'filterKeyboardShortcutActionGroupsForPlatform hides Toggle Recording on Web/iOS',
      () {
    final groups = filterKeyboardShortcutActionGroupsForPlatform(
      capabilities(includeRecordingShortcut: false),
    );
    final ids = idSet(groups);
    expect(ids, isNot(contains(kShortcutActionToggleRecording)));
    // “会话控制”中的其他条目不受影响。
    expect(ids, contains(kShortcutActionSendCtrlAltDel));
    expect(ids, contains(kShortcutActionInsertLock));
  });

  test(
      'filterKeyboardShortcutActionGroupsForPlatform keeps full set on desktop',
      () {
    final groups =
        filterKeyboardShortcutActionGroupsForPlatform(capabilities());
    expect(idSet(groups), equals(idSet(kKeyboardShortcutActionGroups)));
  });

  test('Wayland hides relative mouse mode but keeps screenshot', () {
    final ids = idSet(filterKeyboardShortcutActionGroupsForPlatform(
      capabilities(includeRelativeMouseModeShortcut: false),
    ));
    expect(ids, isNot(contains(kShortcutActionToggleRelativeMouseMode)));
    expect(ids, contains(kShortcutActionScreenshot));
  });

  test('filterKeyboardShortcutActionGroupsForPlatform drops empty groups', () {
    // 当所有能力标志都关闭时，条目全部被这些标志
    // 挡住的分组会被丢弃。显示 / 键盘父分组仍带有
    // 跨平台的直属条目，因此即使受控的
    // 子分组被清空，它们依然保留。
    final groups = filterKeyboardShortcutActionGroupsForPlatform(
      capabilities(
        includeFullscreenShortcut: false,
        includeScreenshotShortcut: false,
        includeRelativeMouseModeShortcut: false,
        includeTabShortcuts: false,
        includeToolbarShortcut: false,
        includeCloseTabShortcut: false,
        includeSwitchSidesShortcut: false,
        includeRecordingShortcut: false,
        includeResetCanvasShortcut: false,
        includePinToolbarShortcut: false,
        includeViewModeShortcut: false,
        includeVoiceCallShortcut: false,
      ),
    );
    final titles = groups.map((g) => g.titleKey).toList();
    // "其他"只剩下受平台控制的条目 → 被整体丢弃。
    expect(titles, isNot(contains('Other')));
    // 带有跨平台直属条目的父分组被保留。
    expect(titles, contains('Display'));
    expect(titles, contains('Keyboard'));
    // 显示下的 "View Mode" 子分组受 includeViewModeShortcut 控制，
    // 因此它必须不出现在显示保留下来的子项中。
    final displayChildren =
        groups.firstWhere((g) => g.titleKey == 'Display').children;
    final subgroupTitles = displayChildren
        .whereType<KeyboardShortcutActionSubgroup>()
        .map((s) => s.titleKey)
        .toList();
    expect(subgroupTitles, isNot(contains('View Mode')));
    // 无论哪种情况，保留下来的分组都不为空。
    expect(groups.every((g) => g.children.isNotEmpty), isTrue);
    // 保留下来的子分组都不为空。
    for (final group in groups) {
      for (final child in group.children) {
        if (child is KeyboardShortcutActionSubgroup) {
          expect(child.entries, isNotEmpty,
              reason: 'subgroup "${child.titleKey}" should not be empty');
        }
      }
    }
  });

  test('physicalKeyName covers the supported-keys fixture', () {
    // `shortcut_key_usb_hid.json` 把每个键名固定到它记录和匹配所依据的 USB HID usage
    // 上。Rust 侧有针对同一文件的镜像测试
    // （src/keyboard/shortcuts.rs 中的 `usb_hid_keys_match_fixture`），
    // 并且这些名字的集合必须与 `supported_shortcut_keys.json` 相等。
    final supported = (jsonDecode(
                File('test/fixtures/supported_shortcut_keys.json')
                    .readAsStringSync()) as List<dynamic>)
        .cast<String>()
        .toSet();
    final pairs = (jsonDecode(File('test/fixtures/shortcut_key_usb_hid.json')
            .readAsStringSync()) as List<dynamic>)
        .cast<List<dynamic>>();

    for (final pair in pairs) {
      final name = pair[0] as String;
      final usage = pair[1] as int;
      expect(physicalKeyName(PhysicalKeyboardKey(0x00070000 | usage)),
          equals(name),
          reason: 'USB HID 0x${usage.toRadixString(16)} should be "$name"');
    }
    expect(pairs.map((p) => p[0] as String).toSet(), equals(supported));

    // 仅含修饰键或不支持的键必须返回 null。
    expect(physicalKeyName(PhysicalKeyboardKey.shiftLeft), isNull);
    expect(physicalKeyName(PhysicalKeyboardKey.escape), isNull);
    expect(physicalKeyName(PhysicalKeyboardKey.f13), isNull);
    expect(physicalKeyName(PhysicalKeyboardKey.numpad1), isNull);
  });

  test('enable seeds defaults only for a config without bindings', () {
    expect(shouldSeedDefaultShortcutBindings(<String, dynamic>{}), isTrue);
    expect(
        shouldSeedDefaultShortcutBindings(
            <String, dynamic>{'enabled': false, 'pass_through': true}),
        isTrue);
    // 用户清空了所有绑定：重新启用不得把默认绑定带回来。
    expect(
        shouldSeedDefaultShortcutBindings(
            <String, dynamic>{'enabled': false, 'bindings': <dynamic>[]}),
        isFalse);
  });

  test('ShortcutConfig.parse reads the flags and bindings once', () {
    final config = ShortcutConfig.parse(jsonEncode({
      'enabled': true,
      'pass_through': false,
      'bindings': [
        {'action': 'screenshot', 'mods': ['primary'], 'key': 'p'},
        'not a binding',
      ],
    }));
    expect(config.enabled, isTrue);
    expect(config.passThrough, isFalse);
    expect(config.bindings, [
      {'action': 'screenshot', 'mods': ['primary'], 'key': 'p'},
    ]);

    for (final raw in ['', 'not json', '[]', '{"bindings": "x"}']) {
      final broken = ShortcutConfig.parse(raw);
      expect(broken.enabled, isFalse, reason: raw);
      expect(broken.passThrough, isFalse, reason: raw);
      expect(broken.bindings, isEmpty, reason: raw);
    }
  });

  test('Wayland active config preserves stored bindings and flags', () {
    const relative = {
      'action': kShortcutActionToggleRelativeMouseMode,
      'mods': ['primary'],
      'key': 'r',
    };
    const mute = {
      'action': kShortcutActionToggleMute,
      'mods': ['primary'],
      'key': 'm',
    };
    for (final flags in [(true, false), (false, false), (true, true)]) {
      final stored = ShortcutConfig.parse(jsonEncode({
        'enabled': flags.$1,
        'pass_through': flags.$2,
        'bindings': [relative, mute],
      }));
      final active = stored.forPlatform(
          capabilities(includeRelativeMouseModeShortcut: false));
      expect(active.bindings, [mute]);
      expect(active.enabled, flags.$1);
      expect(active.passThrough, flags.$2);
      expect(stored.bindings, [relative, mute]);
      expect(stored.forPlatform(capabilities()).bindings, [relative, mute]);
    }
  });

  test('active saved bindings follow Settings capabilities without reseeding', () {
    final actions = [
      kShortcutActionSwitchDisplayAll,
      kShortcutActionKeyboardModeMap,
      kShortcutActionToggleShowMyCursor,
      kShortcutActionToggleZoomCursor,
      kShortcutActionToggleRecording,
      kShortcutActionToggleVoiceCall,
      kShortcutActionResetCanvas,
      kShortcutActionToggleMute,
      kShortcutActionSendCtrlAltDel,
    ];
    final saved = ShortcutConfig.parse(jsonEncode({
      'enabled': true,
      'bindings': [
        for (final action in actions)
          {'action': action, 'mods': ['primary'], 'key': 'm'},
      ],
    }));
    final cap = capabilities(
      includeRecordingShortcut: false,
      includeVoiceCallShortcut: false,
      includeResetCanvasShortcut: false,
      excludedActions: {
        kShortcutActionSwitchDisplayAll,
        kShortcutActionKeyboardModeMap,
        kShortcutActionToggleShowMyCursor,
        kShortcutActionToggleZoomCursor,
      },
    );
    expect(saved.forPlatform(cap).bindings.map((b) => b['action']),
        [kShortcutActionToggleMute, kShortcutActionSendCtrlAltDel]);
    expect(saved.bindings.map((b) => b['action']), actions);
    expect(saved.forPlatform(capabilities()).bindings, saved.bindings);
  });

  test('non-US layouts record and match the physical key', () {
    // AZERTY：标为 "A" 的键位于美式 QWERTY 中 Q 的位置。原生
    // 匹配器只能看到物理位置（USB HID usage），所以
    // 绑定也必须使用该位置名称。
    const azertyA = KeyDownEvent(
      physicalKey: PhysicalKeyboardKey.keyQ,
      logicalKey: LogicalKeyboardKey.keyA,
      character: 'a',
      timeStamp: Duration.zero,
    );
    expect(shortcutKeyNameForEvent(azertyA), 'q');

    // QWERTZ：标为 "Z" 的键位于美式 QWERTY 中 Y 的位置。
    const qwertzZ = KeyDownEvent(
      physicalKey: PhysicalKeyboardKey.keyY,
      logicalKey: LogicalKeyboardKey.keyZ,
      character: 'z',
      timeStamp: Duration.zero,
    );
    expect(shortcutKeyNameForEvent(qwertzZ), 'y');
  });

  test('kShortcutActionsRunOnKeyUp matches the fixture and names real actions',
      () {
    // Rust 侧有针对同一文件的镜像测试
    // （src/keyboard/shortcuts.rs 中的 `key_up_actions_match_fixture`）。
    final fixture = (jsonDecode(
                File('test/fixtures/key_up_shortcut_actions.json')
                    .readAsStringSync()) as List<dynamic>)
        .cast<String>()
        .toSet();
    expect(kShortcutActionsRunOnKeyUp, equals(fixture),
        reason: 'kShortcutActionsRunOnKeyUp drifted from the fixture — update '
            'shortcut_constants.dart, the fixture, and Rust runs_on_release '
            'together');
    final actions = idSet(kKeyboardShortcutActionGroups);
    for (final id in kShortcutActionsRunOnKeyUp) {
      expect(actions, contains(id), reason: '"$id" is not a configurable action');
    }
  });

}
