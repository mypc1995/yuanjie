import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:back_button_interceptor/back_button_interceptor.dart';
import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common/formatter/id_formatter.dart';
import 'package:flutter_hbb/desktop/widgets/refresh_wrapper.dart';
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/main.dart';
import 'package:flutter_hbb/models/peer_model.dart';
import 'package:flutter_hbb/models/peer_tab_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:flutter_hbb/utils/multi_window_manager.dart';
import 'package:flutter_hbb/utils/platform_channel.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:get/get.dart';
import 'package:get/get_rx/src/rx_workers/utils/debouncer.dart';
import 'package:provider/provider.dart';
import 'package:uni_links/uni_links.dart';
import 'package:url_launcher/url_launcher.dart';
import 'package:uuid/uuid.dart';
import 'package:wakelock_plus/wakelock_plus.dart';
import 'package:window_manager/window_manager.dart';
import 'package:window_size/window_size.dart' as window_size;

import '../consts.dart';
import 'common/server_preset.dart';
import 'common/widgets/overlay.dart';
import 'mobile/pages/file_manager_page.dart';
import 'mobile/pages/remote_page.dart';
import 'mobile/pages/view_camera_page.dart';
import 'mobile/pages/terminal_page.dart';
import 'desktop/pages/remote_page.dart' as desktop_remote;
import 'desktop/pages/file_manager_page.dart' as desktop_file_manager;
import 'desktop/pages/view_camera_page.dart' as desktop_view_camera;
import 'package:flutter_hbb/desktop/widgets/remote_toolbar.dart';
import 'models/model.dart';
import 'models/platform_model.dart';

import 'package:flutter_hbb/native/win32.dart'
    if (dart.library.html) 'package:flutter_hbb/web/win32.dart';
import 'package:flutter_hbb/native/common.dart'
    if (dart.library.html) 'package:flutter_hbb/web/common.dart';
import 'package:flutter_hbb/utils/http_service.dart' as http;

final globalKey = GlobalKey<NavigatorState>();
final navigationBarKey = GlobalKey();

final isAndroid = isAndroid_;
final isIOS = isIOS_;
final isWindows = isWindows_;
final isMacOS = isMacOS_;
final isLinux = isLinux_;
final isDesktop = isDesktop_;
final isWeb = isWeb_;
final isWebDesktop = isWebDesktop_;
final isWebOnWindows = isWebOnWindows_;
final isWebOnLinux = isWebOnLinux_;
final isWebOnMacOs = isWebOnMacOS_;
var isMobile = isAndroid || isIOS;
var version = '';
int androidVersion = 0;

// 仅在 Linux 上使用。
// 在 Linux 上，`windowManager.setResizable(false)` 会把窗口尺寸重置为默认尺寸。
// https://stackoverflow.com/questions/8193613/gtk-window-resize-disable-without-going-back-to-default
// 所以我们需要用这个标志来启用/禁用窗口尺寸调整。
bool _linuxWindowResizable = true;

// 仅在 Windows（窗口管理器）上使用。
bool _ignoreDevicePixelRatio = true;

/// 仅 Windows 目标平台可用
int windowsBuildNumber = 0;
DesktopType? desktopType;

// 用于浮点位置比较的容差，避免精度误差。
const double _kPositionEpsilon = 1e-6;

bool get isMainDesktopWindow =>
    desktopType == DesktopType.main || desktopType == DesktopType.cm;

/// 检查应用是否以单视图模式运行。
bool isSingleViewApp() {
  return desktopType == DesktopType.cm;
}

/// * 仅用于调试或测试，发布构建中请勿启用
bool isTest = false;

typedef F = String Function(String);
typedef FMethod = String Function(String, dynamic);

typedef StreamEventHandler = Future<void> Function(Map<String, dynamic>);
typedef SessionID = UuidValue;
final iconHardDrive = MemoryImage(Uint8List.fromList(base64Decode(
    'iVBORw0KGgoAAAANSUhEUgAAAMgAAADICAMAAACahl6sAAAAmVBMVEUAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAjHWqVAAAAMnRSTlMAv0BmzLJNXlhiUu2fxXDgu7WuSUUe29LJvpqUjX53VTstD7ilNujCqTEk5IYH+vEoFjKvAagAAAPpSURBVHja7d0JbhpBEIXhB3jYzb5vBgzYgO04df/DJXGUKMwU9ECmZ6pQfSfw028LCXW3YYwxxhhjjDHGGGOM0eZ9VV1MckdKWLM1bRQ/35GW/WxHHu1me6ShuyHvNl34VhlTKsYVeDWj1EzgUZ1S1DrAk/UDparZgxd9Sl0BHnxSBhpI3jfKQG2FpLUpE69I2ILikv1nsvygjBwPSNKYMlNHggqUoSKS80AZCnwHqQ1zCRvW+CRegwRFeFAMKKrtM8gTPJlzSfwFgT9dJom3IDN4VGaSeAryAK8m0SSeghTg1ZYiql6CjBDhO8mzlyAVhKhIwgXxrh5NojGIhyRckEdwpCdhgpSQgiWTRGMQNonGIGySp0SDvMDBX5KWxiB8Eo1BgE00SYJBykhNnkmSWJAcLpGaJNMgfJKyxiDAK4WNEwryhMtkJsk8CJtEYxA+icYgQIfCcgkEqcJNXhIRQdgkGoPwSTQG+e8khdu/7JOVREwQIKCwF41B2CQljUH4JLcH6SI+OUlEBQHa0SQag/BJNAbhkjxqDMIn0RgEeI4muSlID9eSkERgEKAVTaIxCJ9EYxA2ydVB8hCASVLRGAQYR5NoDMIn0RgEyFHYSGMQPonGII4kziCNvBgNJonEk4u3GAk8Sprk6eYaqbMDY0oKvUm5jfC/viGiSypV7+M3i2iDsAGpNEDYjlTa3W8RdR/r544g50ilnA0RxoZIE2NIXqQbhkAkGyKNDZHGhkhjQ6SxIdLYEGlsiDQ2JGTVeD0264U9zipPh7XOooffpA6pfNCXjxl4/c3pUzlChwzor53zwYYVfpI5pOV6LWFF/2jiJ5FDSs5jdY/0rwUAkUMeXWdBqnSqD0DikBqdqCHsjTvELm9In0IOri/0pwAEDtlSyNaRjAIAAoesKWTtuusxByBwCJp0oomwBXcYUuCQgE50ENajE4OvZAKHLB1/68Br5NqiyCGYOY8YRd77kTkEb64n7lZN+mOIX4QOwb5FX0ZVx3uOxwW+SB0CbBubemWP8/rlaaeRX+M3uUOuZENsiA25zIbYkPsZElBIHwL13U/PTjJ/cyOOEoVM3I+hziDQlELm7pPxw3eI8/7gPh1fpLA6xGnEeDDgO0UcIAzzM35HxLPIq5SXe9BLzOsj9eUaQqyXzxS1QFSfWM2cCANiHcAISJ0AnCKpUwTuIkkA3EeSInAXSQKcs1V18e24wlllUmQp9v9zXKeHi+akRAMOPVKhAqdPBZeUmnnEsO6QcJ0+4qmOSbBxFfGVRiTUqITrdKcCbyYO3/K4wX4+aQ+FfNjXhu3JfAVjjDHGGGOMMcYYY4xIPwCgfqT6TbhCLAAAAABJRU5ErkJggg==')));

enum DesktopType {
  main,
  remote,
  fileTransfer,
  viewCamera,
  terminal,
  cm,
  portForward,
}

bool isDoubleEqual(double a, double b) {
  return (a - b).abs() < _kPositionEpsilon;
}

class IconFont {
  static const _family1 = 'Tabbar';
  static const _family2 = 'PeerSearchbar';
  static const _family3 = 'AddressBook';
  static const _family4 = 'DeviceGroup';
  static const _family5 = 'More';

  IconFont._();

  static const IconData max = IconData(0xe606, fontFamily: _family1);
  static const IconData restore = IconData(0xe607, fontFamily: _family1);
  static const IconData close = IconData(0xe668, fontFamily: _family1);
  static const IconData min = IconData(0xe609, fontFamily: _family1);
  static const IconData add = IconData(0xe664, fontFamily: _family1);
  static const IconData menu = IconData(0xe628, fontFamily: _family1);
  static const IconData search = IconData(0xe6a4, fontFamily: _family2);
  static const IconData roundClose = IconData(0xe6ed, fontFamily: _family2);
  static const IconData addressBook = IconData(0xe602, fontFamily: _family3);
  static const IconData deviceGroupOutline =
      IconData(0xe623, fontFamily: _family4);
  static const IconData deviceGroupFill =
      IconData(0xe748, fontFamily: _family4);
  static const IconData more = IconData(0xe609, fontFamily: _family5);
}

class ColorThemeExtension extends ThemeExtension<ColorThemeExtension> {
  const ColorThemeExtension({
    required this.border,
    required this.border2,
    required this.border3,
    required this.highlight,
    required this.drag_indicator,
    required this.shadow,
    required this.errorBannerBg,
    required this.me,
    required this.toastBg,
    required this.toastText,
    required this.divider,
  });

  final Color? border;
  final Color? border2;
  final Color? border3;
  final Color? highlight;
  final Color? drag_indicator;
  final Color? shadow;
  final Color? errorBannerBg;
  final Color? me;
  final Color? toastBg;
  final Color? toastText;
  final Color? divider;

  static final light = ColorThemeExtension(
    border: Color(0xFFCCCCCC),
    border2: Color(0xFFBBBBBB),
    border3: Colors.black26,
    highlight: Color(0xFFE5E5E5),
    drag_indicator: Colors.grey[800],
    shadow: Colors.black,
    errorBannerBg: Color(0xFFFDEEEB),
    me: Colors.green,
    toastBg: Colors.black.withOpacity(0.6),
    toastText: Colors.white,
    divider: Colors.black38,
  );

  static final dark = ColorThemeExtension(
    border: Color(0xFF555555),
    border2: Color(0xFFE5E5E5),
    border3: Colors.white24,
    highlight: Color(0xFF3F3F3F),
    drag_indicator: Colors.grey,
    shadow: Colors.grey,
    errorBannerBg: Color(0xFF470F2D),
    me: Colors.greenAccent,
    toastBg: Colors.white.withOpacity(0.6),
    toastText: Colors.black,
    divider: Colors.white38,
  );

  @override
  ThemeExtension<ColorThemeExtension> copyWith({
    Color? border,
    Color? border2,
    Color? border3,
    Color? highlight,
    Color? drag_indicator,
    Color? shadow,
    Color? errorBannerBg,
    Color? me,
    Color? toastBg,
    Color? toastText,
    Color? divider,
  }) {
    return ColorThemeExtension(
      border: border ?? this.border,
      border2: border2 ?? this.border2,
      border3: border3 ?? this.border3,
      highlight: highlight ?? this.highlight,
      drag_indicator: drag_indicator ?? this.drag_indicator,
      shadow: shadow ?? this.shadow,
      errorBannerBg: errorBannerBg ?? this.errorBannerBg,
      me: me ?? this.me,
      toastBg: toastBg ?? this.toastBg,
      toastText: toastText ?? this.toastText,
      divider: divider ?? this.divider,
    );
  }

  @override
  ThemeExtension<ColorThemeExtension> lerp(
      ThemeExtension<ColorThemeExtension>? other, double t) {
    if (other is! ColorThemeExtension) {
      return this;
    }
    return ColorThemeExtension(
      border: Color.lerp(border, other.border, t),
      border2: Color.lerp(border2, other.border2, t),
      border3: Color.lerp(border3, other.border3, t),
      highlight: Color.lerp(highlight, other.highlight, t),
      drag_indicator: Color.lerp(drag_indicator, other.drag_indicator, t),
      shadow: Color.lerp(shadow, other.shadow, t),
      errorBannerBg: Color.lerp(shadow, other.errorBannerBg, t),
      me: Color.lerp(shadow, other.me, t),
      toastBg: Color.lerp(shadow, other.toastBg, t),
      toastText: Color.lerp(shadow, other.toastText, t),
      divider: Color.lerp(shadow, other.divider, t),
    );
  }
}

class MyTheme {
  MyTheme._();

  static const Color grayBg = Color(0xFFEFEFF2);
  static const Color accent = Color(0xFF0071FF);
  static const Color accent50 = Color(0x770071FF);
  static const Color accent80 = Color(0xAA0071FF);
  static const Color canvasColor = Color(0xFF212121);
  static const Color border = Color(0xFFCCCCCC);
  static const Color idColor = Color(0xFF00B6F0);
  static const Color darkGray = Color.fromARGB(255, 148, 148, 148);
  static const Color cmIdColor = Color(0xFF21790B);
  static const Color dark = Colors.black87;
  static const Color button = Color(0xFF2C8CFF);
  static const Color hoverBorder = Color(0xFF999999);

  // ListTile
  static const ListTileThemeData listTileTheme = ListTileThemeData(
    shape: RoundedRectangleBorder(
      borderRadius: BorderRadius.all(
        Radius.circular(5),
      ),
    ),
  );

  static SwitchThemeData switchTheme() {
    return SwitchThemeData(
        splashRadius: (isDesktop || isWebDesktop) ? 0 : kRadialReactionRadius);
  }

  static RadioThemeData radioTheme() {
    return RadioThemeData(
        splashRadius: (isDesktop || isWebDesktop) ? 0 : kRadialReactionRadius);
  }

  // Checkbox
  static const CheckboxThemeData checkboxTheme = CheckboxThemeData(
    splashRadius: 0,
    shape: RoundedRectangleBorder(
      borderRadius: BorderRadius.all(
        Radius.circular(5),
      ),
    ),
  );

  // TextButton
  // 该值用于计算 "dialog.actionsPadding"
  static const double mobileTextButtonPaddingLR = 20;

  // 移动端上的 TextButton 需要固定内边距，否则像
  // "OK" 这样的小按钮左右内边距会更大。
  static TextButtonThemeData mobileTextButtonTheme = TextButtonThemeData(
    style: TextButton.styleFrom(
      padding: EdgeInsets.symmetric(horizontal: mobileTextButtonPaddingLR),
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(8.0),
      ),
    ),
  );

  // 工具提示
  static TooltipThemeData tooltipTheme() {
    return TooltipThemeData(
      waitDuration: Duration(seconds: 1, milliseconds: 500),
    );
  }

  // 对话框
  static const double dialogPadding = 24;

  // 底部内边距取决于内容（有些对话框没有内容）
  static EdgeInsets dialogTitlePadding({bool content = true}) {
    final double p = dialogPadding;

    return EdgeInsets.fromLTRB(p, p, p, content ? 0 : p);
  }

  // 底部内边距取决于操作按钮（移动端有些对话框没有操作按钮）
  static EdgeInsets dialogContentPadding({bool actions = true}) {
    final double p = dialogPadding;

    return (isDesktop || isWebDesktop)
        ? EdgeInsets.fromLTRB(p, p, p, actions ? (p - 4) : p)
        : EdgeInsets.fromLTRB(p, p, p, actions ? (p / 2) : p);
  }

  static EdgeInsets dialogActionsPadding() {
    final double p = dialogPadding;

    return (isDesktop || isWebDesktop)
        ? EdgeInsets.fromLTRB(p, 0, p, (p - 4))
        : EdgeInsets.fromLTRB(p, 0, (p - mobileTextButtonPaddingLR), (p / 2));
  }

  static EdgeInsets dialogButtonPadding = (isDesktop || isWebDesktop)
      ? EdgeInsets.only(left: dialogPadding)
      : EdgeInsets.only(left: dialogPadding / 3);

  static ScrollbarThemeData scrollbarTheme = ScrollbarThemeData(
    thickness: MaterialStateProperty.all(6),
    thumbColor: MaterialStateProperty.resolveWith<Color?>((states) {
      if (states.contains(MaterialState.dragged)) {
        return Colors.grey[900];
      } else if (states.contains(MaterialState.hovered)) {
        return Colors.grey[700];
      } else {
        return Colors.grey[500];
      }
    }),
    crossAxisMargin: 4,
  );

  static ScrollbarThemeData scrollbarThemeDark = scrollbarTheme.copyWith(
    thumbColor: MaterialStateProperty.resolveWith<Color?>((states) {
      if (states.contains(MaterialState.dragged)) {
        return Colors.grey[100];
      } else if (states.contains(MaterialState.hovered)) {
        return Colors.grey[300];
      } else {
        return Colors.grey[500];
      }
    }),
  );

  static ThemeData lightTheme = ThemeData(
    // https://stackoverflow.com/questions/77537315/after-upgrading-to-flutter-3-16-the-app-bar-background-color-button-size-and
    useMaterial3: false,
    brightness: Brightness.light,
    hoverColor: Color.fromARGB(255, 224, 224, 224),
    scaffoldBackgroundColor: Colors.white,
    dialogBackgroundColor: Colors.white,
    appBarTheme: AppBarTheme(
      shadowColor: Colors.transparent,
    ),
    dialogTheme: DialogTheme(
      elevation: 15,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(18.0),
        side: BorderSide(
          width: 1,
          color: grayBg,
        ),
      ),
    ),
    scrollbarTheme: scrollbarTheme,
    inputDecorationTheme: isDesktop
        ? InputDecorationTheme(
            fillColor: grayBg,
            filled: true,
            isDense: true,
            border: OutlineInputBorder(
              borderRadius: BorderRadius.circular(8),
            ),
          )
        : null,
    textTheme: const TextTheme(
        titleLarge: TextStyle(fontSize: 19, color: Colors.black87),
        titleSmall: TextStyle(fontSize: 14, color: Colors.black87),
        bodySmall: TextStyle(fontSize: 12, color: Colors.black87, height: 1.25),
        bodyMedium:
            TextStyle(fontSize: 14, color: Colors.black87, height: 1.25),
        labelLarge: TextStyle(fontSize: 16.0, color: MyTheme.accent80)),
    cardColor: grayBg,
    hintColor: Color(0xFFAAAAAA),
    visualDensity: VisualDensity.adaptivePlatformDensity,
    tabBarTheme: const TabBarTheme(
      labelColor: Colors.black87,
    ),
    tooltipTheme: tooltipTheme(),
    splashColor: (isDesktop || isWebDesktop) ? Colors.transparent : null,
    highlightColor: (isDesktop || isWebDesktop) ? Colors.transparent : null,
    splashFactory: (isDesktop || isWebDesktop) ? NoSplash.splashFactory : null,
    textButtonTheme: (isDesktop || isWebDesktop)
        ? TextButtonThemeData(
            style: TextButton.styleFrom(
              splashFactory: NoSplash.splashFactory,
              shape: RoundedRectangleBorder(
                borderRadius: BorderRadius.circular(18.0),
              ),
            ),
          )
        : mobileTextButtonTheme,
    elevatedButtonTheme: ElevatedButtonThemeData(
      style: ElevatedButton.styleFrom(
        backgroundColor: MyTheme.accent,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8.0),
        ),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        backgroundColor: grayBg,
        foregroundColor: Colors.black87,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8.0),
        ),
      ),
    ),
    switchTheme: switchTheme(),
    radioTheme: radioTheme(),
    checkboxTheme: checkboxTheme,
    listTileTheme: listTileTheme,
    menuBarTheme: MenuBarThemeData(
        style:
            MenuStyle(backgroundColor: MaterialStatePropertyAll(Colors.white))),
    colorScheme: ColorScheme.light(
        primary: Colors.blue, secondary: accent, background: grayBg),
    popupMenuTheme: PopupMenuThemeData(
        color: Colors.white,
        shape: RoundedRectangleBorder(
          side: BorderSide(
              color: (isDesktop || isWebDesktop)
                  ? Color(0xFFECECEC)
                  : Colors.transparent),
          borderRadius: BorderRadius.all(Radius.circular(8.0)),
        )),
  ).copyWith(
    extensions: <ThemeExtension<dynamic>>[
      ColorThemeExtension.light,
      TabbarTheme.light,
    ],
  );
  static ThemeData darkTheme = ThemeData(
    useMaterial3: false,
    brightness: Brightness.dark,
    hoverColor: Color.fromARGB(255, 45, 46, 53),
    scaffoldBackgroundColor: Color(0xFF18191E),
    dialogBackgroundColor: Color(0xFF18191E),
    appBarTheme: AppBarTheme(
      shadowColor: Colors.transparent,
    ),
    dialogTheme: DialogTheme(
      elevation: 15,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(18.0),
        side: BorderSide(
          width: 1,
          color: Color(0xFF24252B),
        ),
      ),
    ),
    scrollbarTheme: scrollbarThemeDark,
    inputDecorationTheme: (isDesktop || isWebDesktop)
        ? InputDecorationTheme(
            fillColor: Color(0xFF24252B),
            filled: true,
            isDense: true,
            border: OutlineInputBorder(
              borderRadius: BorderRadius.circular(8),
            ),
          )
        : null,
    textTheme: const TextTheme(
      titleLarge: TextStyle(fontSize: 19),
      titleSmall: TextStyle(fontSize: 14),
      bodySmall: TextStyle(fontSize: 12, height: 1.25),
      bodyMedium: TextStyle(fontSize: 14, height: 1.25),
      labelLarge: TextStyle(
        fontSize: 16.0,
        fontWeight: FontWeight.bold,
        color: accent80,
      ),
    ),
    cardColor: Color(0xFF24252B),
    visualDensity: VisualDensity.adaptivePlatformDensity,
    tabBarTheme: const TabBarTheme(
      labelColor: Colors.white70,
    ),
    tooltipTheme: tooltipTheme(),
    splashColor: (isDesktop || isWebDesktop) ? Colors.transparent : null,
    highlightColor: (isDesktop || isWebDesktop) ? Colors.transparent : null,
    splashFactory: (isDesktop || isWebDesktop) ? NoSplash.splashFactory : null,
    textButtonTheme: (isDesktop || isWebDesktop)
        ? TextButtonThemeData(
            style: TextButton.styleFrom(
              splashFactory: NoSplash.splashFactory,
              disabledForegroundColor: Colors.white70,
              foregroundColor: Colors.white70,
              shape: RoundedRectangleBorder(
                borderRadius: BorderRadius.circular(18.0),
              ),
            ),
          )
        : mobileTextButtonTheme,
    elevatedButtonTheme: ElevatedButtonThemeData(
      style: ElevatedButton.styleFrom(
        backgroundColor: MyTheme.accent,
        foregroundColor: Colors.white,
        disabledForegroundColor: Colors.white70,
        disabledBackgroundColor: Colors.white10,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8.0),
        ),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        backgroundColor: Color(0xFF24252B),
        side: BorderSide(color: Colors.white12, width: 0.5),
        disabledForegroundColor: Colors.white70,
        foregroundColor: Colors.white70,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8.0),
        ),
      ),
    ),
    switchTheme: switchTheme(),
    radioTheme: radioTheme(),
    checkboxTheme: checkboxTheme,
    listTileTheme: listTileTheme,
    menuBarTheme: MenuBarThemeData(
        style: MenuStyle(
            backgroundColor: MaterialStatePropertyAll(Color(0xFF121212)))),
    colorScheme: ColorScheme.dark(
      primary: Colors.blue,
      secondary: accent,
      background: Color(0xFF24252B),
    ),
    popupMenuTheme: PopupMenuThemeData(
        shape: RoundedRectangleBorder(
      side: BorderSide(color: Colors.white24),
      borderRadius: BorderRadius.all(Radius.circular(8.0)),
    )),
  ).copyWith(
    extensions: <ThemeExtension<dynamic>>[
      ColorThemeExtension.dark,
      TabbarTheme.dark,
    ],
  );

  static ThemeMode getThemeModePreference() {
    return themeModeFromString(bind.mainGetLocalOption(key: kCommConfKeyTheme));
  }

  static Future<void> changeDarkMode(ThemeMode mode) async {
    Get.changeThemeMode(mode);
    if (desktopType == DesktopType.main || isAndroid || isIOS || isWeb) {
      if (mode == ThemeMode.system) {
        await bind.mainSetLocalOption(
            key: kCommConfKeyTheme, value: defaultOptionTheme);
      } else {
        await bind.mainSetLocalOption(
            key: kCommConfKeyTheme, value: mode.toShortString());
      }
      if (!isWeb) await bind.mainChangeTheme(dark: mode.toShortString());
      // 同步系统的窗口主题。
      updateSystemWindowTheme();
    }
  }

  static ThemeMode currentThemeMode() {
    final preference = getThemeModePreference();
    if (preference == ThemeMode.system) {
      if (WidgetsBinding.instance.platformDispatcher.platformBrightness ==
          Brightness.light) {
        return ThemeMode.light;
      } else {
        return ThemeMode.dark;
      }
    } else {
      return preference;
    }
  }

  static ColorThemeExtension color(BuildContext context) {
    return Theme.of(context).extension<ColorThemeExtension>()!;
  }

  static TabbarTheme tabbar(BuildContext context) {
    return Theme.of(context).extension<TabbarTheme>()!;
  }

  static ThemeMode themeModeFromString(String v) {
    switch (v) {
      case "light":
        return ThemeMode.light;
      case "dark":
        return ThemeMode.dark;
      default:
        return ThemeMode.system;
    }
  }
}

extension ParseToString on ThemeMode {
  String toShortString() {
    return toString().split('.').last;
  }
}

final ButtonStyle flatButtonStyle = TextButton.styleFrom(
  minimumSize: Size(0, 36),
  padding: EdgeInsets.symmetric(horizontal: 16.0, vertical: 10.0),
  shape: const RoundedRectangleBorder(
    borderRadius: BorderRadius.all(Radius.circular(2.0)),
  ),
);

List<Locale> supportedLocales = const [
  Locale('en', 'US'),
  Locale('zh', 'CN'),
  Locale('zh', 'TW'),
  Locale('zh', 'SG'),
  Locale('fr'),
  Locale('de'),
  Locale('it'),
  Locale('ja'),
  Locale('cs'),
  Locale('pl'),
  Locale('ko'),
  Locale('hu'),
  Locale('pt'),
  Locale('ru'),
  Locale('sk'),
  Locale('id'),
  Locale('da'),
  Locale('eo'),
  Locale('tr'),
  Locale('kz'),
  Locale('es'),
  Locale('nl'),
  Locale('nb'),
  Locale('et'),
  Locale('eu'),
  Locale('bg'),
  Locale('be'),
  Locale('vn'),
  Locale('uk'),
  Locale('fa'),
  Locale('ca'),
  Locale('el'),
  Locale('sv'),
  Locale('sq'),
  Locale('sr'),
  Locale('th'),
  Locale('sl'),
  Locale('ro'),
  Locale('lt'),
  Locale('lv'),
  Locale('ar'),
  Locale('he'),
  Locale('hr'),
];

String formatDurationToTime(Duration duration) {
  var totalTime = duration.inSeconds;
  final secs = totalTime % 60;
  totalTime = (totalTime - secs) ~/ 60;
  final mins = totalTime % 60;
  totalTime = (totalTime - mins) ~/ 60;
  return "${totalTime.toString().padLeft(2, "0")}:${mins.toString().padLeft(2, "0")}:${secs.toString().padLeft(2, "0")}";
}

closeConnection({String? id}) {
  if (isAndroid || isIOS) {
    () async {
      await SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual,
          overlays: SystemUiOverlay.values);
      gFFI.chatModel.hideChatOverlay();
      Navigator.popUntil(globalKey.currentContext!, ModalRoute.withName("/"));
      stateGlobal.isInMainPage = true;
    }();
  } else {
    if (isWeb) {
      Navigator.popUntil(globalKey.currentContext!, ModalRoute.withName("/"));
      stateGlobal.isInMainPage = true;
    } else {
      final controller = Get.find<DesktopTabController>();
      if (controller.tabType == DesktopTabType.terminal &&
          controller.onCloseWindow != null) {
        // 终端窗口的作用域限定为单个被控端。传递给
        // closeConnection() 的是该被控端 ID，而不是终端标签页的 key
        // （${peerId}_${terminalId}）。从终端对话框关闭时应当关闭
        // 该被控端的整个终端窗口，包含所有终端标签页。
        unawaited(controller.onCloseWindow!().catchError((e, _) {
          debugPrint('[closeConnection] Failed to close terminal window: $e');
        }));
        return;
      }
      controller.closeBy(id);
    }
  }
}

Future<void> windowOnTop(int? id) async {
  if (!isDesktop) {
    return;
  }
  print("Bring window '$id' on top");
  if (id == null) {
    // 主窗口
    if (stateGlobal.isMinimized) {
      await windowManager.restore();
    }
    await windowManager.show();
    await windowManager.focus();
    await rustDeskWinManager.registerActiveWindow(kWindowMainId);
  } else {
    WindowController.fromWindowId(id)
      ..focus()
      ..show();
    rustDeskWinManager.call(WindowType.Main, kWindowEventShow, {"id": id});
  }
}

typedef DialogBuilder = CustomAlertDialog Function(
    StateSetter setState, void Function([dynamic]) close, BuildContext context);

class Dialog<T> {
  OverlayEntry? entry;
  Completer<T?> completer = Completer<T?>();

  Dialog();

  void complete(T? res) {
    try {
      if (!completer.isCompleted) {
        completer.complete(res);
      }
    } catch (e) {
      debugPrint("Dialog complete catch error: $e");
    } finally {
      entry?.remove();
    }
  }
}

class OverlayKeyState {
  final _overlayKey = GlobalKey<OverlayState>();

  /// 默认使用全局 overlay
  OverlayState? get state =>
      _overlayKey.currentState ?? globalKey.currentState?.overlay;

  GlobalKey<OverlayState>? get key => _overlayKey;
}

class OverlayDialogManager {
  final Map<String, Dialog> _dialogs = {};
  var _overlayKeyState = OverlayKeyState();
  int _tagCount = 0;

  OverlayEntry? _mobileActionsOverlayEntry;
  RxBool mobileActionsOverlayVisible = true.obs;

  setMobileActionsOverlayVisible(bool v, {store = true}) {
    if (store) {
      bind.setLocalFlutterOption(k: kOptionShowMobileAction, v: v ? 'Y' : 'N');
    }
    // 设置后无需再从本地存储读取该值。
    // 更好的做法是直接切换该值。
    mobileActionsOverlayVisible.value = v;
  }

  loadMobileActionsOverlayVisible() {
    mobileActionsOverlayVisible.value =
        bind.getLocalFlutterOption(k: kOptionShowMobileAction) != 'N';
  }

  void setOverlayState(OverlayKeyState overlayKeyState) {
    _overlayKeyState = overlayKeyState;
  }

  void dismissAll() {
    _dialogs.forEach((key, value) {
      value.complete(null);
      BackButtonInterceptor.removeByName(key);
    });
    _dialogs.clear();
  }

  void dismissByTag(String tag) {
    _dialogs[tag]?.complete(null);
    _dialogs.remove(tag);
    BackButtonInterceptor.removeByName(tag);
  }

  Future<T?> show<T>(DialogBuilder builder,
      {bool clickMaskDismiss = false,
      bool backDismiss = false,
      String? tag,
      bool useAnimation = true,
      bool forceGlobal = false}) {
    final overlayState =
        forceGlobal ? globalKey.currentState?.overlay : _overlayKeyState.state;

    if (overlayState == null) {
      return Future.error(
          "[OverlayDialogManager] Failed to show dialog, _overlayState is null, call [setOverlayState] first");
    }

    final String dialogTag;
    if (tag != null) {
      dialogTag = tag;
    } else {
      dialogTag = _tagCount.toString();
      _tagCount++;
    }

    final dialog = Dialog<T>();
    _dialogs[dialogTag] = dialog;

    close([res]) {
      _dialogs.remove(dialogTag);
      try {
        dialog.complete(res);
      } catch (e) {
        debugPrint("Dialog complete catch error: $e");
      }
      BackButtonInterceptor.removeByName(dialogTag);
    }

    dialog.entry = OverlayEntry(builder: (context) {
      bool innerClicked = false;
      return Listener(
          onPointerUp: (_) {
            if (!innerClicked && clickMaskDismiss) {
              close();
            }
            innerClicked = false;
          },
          child: Container(
              color: Theme.of(context).brightness == Brightness.light
                  ? Colors.black12
                  : Colors.black45,
              child: StatefulBuilder(builder: (context, setState) {
                return Listener(
                  onPointerUp: (_) => innerClicked = true,
                  child: builder(setState, close, overlayState.context),
                );
              })));
    });
    overlayState.insert(dialog.entry!);
    BackButtonInterceptor.add((stopDefaultButtonEvent, routeInfo) {
      if (backDismiss) {
        close();
      }
      return true;
    }, name: dialogTag);
    return dialog.completer.future;
  }

  String showLoading(String text,
      {bool clickMaskDismiss = false,
      bool showCancel = true,
      VoidCallback? onCancel,
      String? tag}) {
    if (tag == null) {
      tag = _tagCount.toString();
      _tagCount++;
    }
    show((setState, close, context) {
      cancel() {
        dismissAll();
        if (onCancel != null) {
          onCancel();
        }
      }

      return CustomAlertDialog(
        content: Container(
            constraints: const BoxConstraints(maxWidth: 240),
            child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  const SizedBox(height: 30),
                  const Center(child: CircularProgressIndicator()),
                  const SizedBox(height: 20),
                  Center(
                      child: Text(translate(text),
                          style: const TextStyle(fontSize: 15))),
                  const SizedBox(height: 20),
                  Offstage(
                      offstage: !showCancel,
                      child: Center(
                          child: (isDesktop || isWebDesktop)
                              ? dialogButton('Cancel', onPressed: cancel)
                              : TextButton(
                                  style: flatButtonStyle,
                                  onPressed: cancel,
                                  child: Text(translate('Cancel'),
                                      style: const TextStyle(
                                          color: MyTheme.accent)))))
                ])),
        onCancel: showCancel ? cancel : null,
      );
    }, tag: tag);
    return tag;
  }

  void resetMobileActionsOverlay({FFI? ffi}) {
    if (_mobileActionsOverlayEntry == null) return;
    hideMobileActionsOverlay();
    showMobileActionsOverlay(ffi: ffi);
  }

  void showMobileActionsOverlay({FFI? ffi}) {
    if (_mobileActionsOverlayEntry != null) return;
    final overlayState = _overlayKeyState.state;
    if (overlayState == null) return;

    final overlay = makeMobileActionsOverlayEntry(
      () => hideMobileActionsOverlay(),
      ffi: ffi,
    );
    overlayState.insert(overlay);
    _mobileActionsOverlayEntry = overlay;
    setMobileActionsOverlayVisible(true);
  }

  void hideMobileActionsOverlay({store = true}) {
    if (_mobileActionsOverlayEntry != null) {
      _mobileActionsOverlayEntry!.remove();
      _mobileActionsOverlayEntry = null;
      setMobileActionsOverlayVisible(false, store: store);
      return;
    }
  }

  void toggleMobileActionsOverlay({FFI? ffi}) {
    if (_mobileActionsOverlayEntry == null) {
      showMobileActionsOverlay(ffi: ffi);
    } else {
      hideMobileActionsOverlay();
    }
  }

  bool existing(String tag) {
    return _dialogs.keys.contains(tag);
  }
}

makeMobileActionsOverlayEntry(VoidCallback? onHide, {FFI? ffi}) {
  makeMobileActions(BuildContext context, double s) {
    final scale = s < 0.85 ? 0.85 : s;
    final session = ffi ?? gFFI;
    const double overlayW = 200;
    const double overlayH = 45;
    computeOverlayPosition() {
      final screenW = MediaQuery.of(context).size.width;
      final screenH = MediaQuery.of(context).size.height;
      final left = (screenW - overlayW * scale) / 2;
      final top = screenH - (overlayH + 80) * scale;
      return Offset(left, top);
    }

    if (draggablePositions.mobileActions.isInvalid()) {
      draggablePositions.mobileActions.update(computeOverlayPosition());
    } else {
      draggablePositions.mobileActions.tryAdjust(overlayW, overlayH, scale);
    }
    return DraggableMobileActions(
      scale: scale,
      position: draggablePositions.mobileActions,
      width: overlayW,
      height: overlayH,
      onBackPressed: session.inputModel.onMobileBack,
      onHomePressed: session.inputModel.onMobileHome,
      onRecentPressed: session.inputModel.onMobileApps,
      onHidePressed: onHide,
    );
  }

  return OverlayEntry(builder: (context) {
    if (isDesktop) {
      final c = Provider.of<CanvasModel>(context);
      return makeMobileActions(context, c.scale * 2.0);
    } else {
      return makeMobileActions(globalKey.currentContext!, 1.0);
    }
  });
}

void showToast(String text,
    {Duration timeout = const Duration(seconds: 3),
    Alignment alignment = const Alignment(0.0, 0.8)}) {
  final overlayState = globalKey.currentState?.overlay;
  if (overlayState == null) return;
  final entry = OverlayEntry(builder: (context) {
    return IgnorePointer(
        child: Align(
            alignment: alignment,
            child: Container(
              decoration: BoxDecoration(
                color: MyTheme.color(context).toastBg,
                borderRadius: const BorderRadius.all(
                  Radius.circular(20),
                ),
              ),
              padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 5),
              child: Text(
                text,
                textAlign: TextAlign.center,
                style: TextStyle(
                    decoration: TextDecoration.none,
                    fontWeight: FontWeight.w300,
                    fontSize: 18,
                    color: MyTheme.color(context).toastText),
              ),
            )));
  });
  overlayState.insert(entry);
  Future.delayed(timeout, () {
    entry.remove();
  });
}

// TODO
// - 移除参数 "contentPadding"，不需要它，所有地方显示效果应保持一致。
// - 移除参数 "content" 的 "required"。参见简单确认对话框 "delete peer"，只使用了 title 和 actions。不需要 "content: SizedBox.shrink()"。
// - 让死代码复活，把参数 "onSubmit" 和 "onCancel" 转换为对应的按钮 "ConfirmOkButton"、"CancelButton"。
class CustomAlertDialog extends StatelessWidget {
  const CustomAlertDialog(
      {Key? key,
      this.title,
      this.titlePadding,
      required this.content,
      this.actions,
      this.contentPadding,
      this.contentBoxConstraints = const BoxConstraints(maxWidth: 500),
      this.onSubmit,
      this.onCancel})
      : super(key: key);

  final Widget? title;
  final EdgeInsetsGeometry? titlePadding;
  final Widget content;
  final List<Widget>? actions;
  final double? contentPadding;
  final BoxConstraints contentBoxConstraints;
  final Function()? onSubmit;
  final Function()? onCancel;

  @override
  Widget build(BuildContext context) {
    // 请求焦点
    FocusScopeNode scopeNode = FocusScopeNode();
    Future.delayed(Duration.zero, () {
      if (!scopeNode.hasFocus) scopeNode.requestFocus();
    });
    bool tabTapped = false;
    if (isAndroid) gFFI.invokeMethod("enable_soft_keyboard", true);

    return FocusScope(
      node: scopeNode,
      autofocus: true,
      onKey: (node, key) {
        if (key.logicalKey == LogicalKeyboardKey.escape) {
          if (key is RawKeyDownEvent) {
            onCancel?.call();
          }
          return KeyEventResult.handled; // avoid TextField exception on escape
        } else if (!tabTapped &&
            onSubmit != null &&
            (key.logicalKey == LogicalKeyboardKey.enter ||
                key.logicalKey == LogicalKeyboardKey.numpadEnter)) {
          if (key is RawKeyDownEvent) onSubmit?.call();
          return KeyEventResult.handled;
        } else if (key.logicalKey == LogicalKeyboardKey.tab) {
          if (key is RawKeyDownEvent) {
            scopeNode.nextFocus();
            tabTapped = true;
          }
          return KeyEventResult.handled;
        }
        return KeyEventResult.ignored;
      },
      child: AlertDialog(
          scrollable: true,
          title: title,
          content: ConstrainedBox(
            constraints: contentBoxConstraints,
            child: content,
          ),
          actions: actions,
          titlePadding: titlePadding ?? MyTheme.dialogTitlePadding(),
          contentPadding:
              MyTheme.dialogContentPadding(actions: actions is List),
          actionsPadding: MyTheme.dialogActionsPadding(),
          buttonPadding: MyTheme.dialogButtonPadding),
    );
  }
}

Widget createDialogContent(String text) {
  final RegExp linkRegExp = RegExp(r'(https?://[^\s]+)');
  bool hasLink = linkRegExp.hasMatch(text);

  // 提前返回：没有链接，使用默认主题色
  if (!hasLink) {
    return SelectableText(text, style: const TextStyle(fontSize: 15));
  }

  final List<TextSpan> spans = [];
  int start = 0;

  linkRegExp.allMatches(text).forEach((match) {
    if (match.start > start) {
      spans.add(TextSpan(text: text.substring(start, match.start)));
    }
    spans.add(TextSpan(
      text: match.group(0) ?? '',
      style: const TextStyle(
        color: Colors.blue,
        decoration: TextDecoration.underline,
      ),
      recognizer: TapGestureRecognizer()
        ..onTap = () {
          String linkText = match.group(0) ?? '';
          linkText = linkText.replaceAll(RegExp(r'[.,;!?]+$'), '');
          launchUrl(Uri.parse(linkText));
        },
    ));
    start = match.end;
  });

  if (start < text.length) {
    spans.add(TextSpan(text: text.substring(start)));
  }

  return SelectableText.rich(
    TextSpan(
      style: const TextStyle(fontSize: 15),
      children: spans,
    ),
  );
}

void msgBox(SessionID sessionId, String type, String title, String text,
    String link, OverlayDialogManager dialogManager,
    {bool? hasCancel,
    ReconnectHandle? reconnect,
    int? reconnectTimeout,
    VoidCallback? onSubmit,
    int? submitTimeout}) {
  dialogManager.dismissAll();
  if (type.contains('insecure-connection')) {
    Future<void> closeSession() async {
      await bind.sessionSetCommon(
        sessionId: sessionId,
        key: 'continue-insecure-connection',
        value: 'N',
      );
      dialogManager.dismissAll();
      closeConnection();
    }

    void continueSession() {
      unawaited(
        bind.sessionSetCommon(
          sessionId: sessionId,
          key: 'continue-insecure-connection',
          value: 'Y',
        ),
      );
      dialogManager.dismissAll();
    }

    dialogManager.show(
      (setState, close, context) => CustomAlertDialog(
        title: null,
        content: SelectionArea(child: msgboxContent(type, title, text)),
        actions: [
          dialogButton(
            'Continue',
            onPressed: continueSession,
            isOutline: true,
          ),
          dialogButton('Disconnect', onPressed: closeSession),
        ],
        onSubmit: closeSession,
        onCancel: closeSession,
      ),
      tag: '$sessionId-$type-$title-$text-$link',
    );
    return;
  }

  List<Widget> buttons = [];
  bool hasOk = false;
  submit() {
    dialogManager.dismissAll();
    if (onSubmit != null) {
      onSubmit.call();
    } else {
      // https://github.com/rustdesk/rustdesk/blob/5e9a31340b899822090a3731769ae79c6bf5f3e5/src/ui/common.tis#L263
      if (!type.contains("custom") && desktopType != DesktopType.portForward) {
        closeConnection();
      }
    }
  }

  cancel() {
    dialogManager.dismissAll();
  }

  jumplink() {
    if (link.startsWith('http')) {
      launchUrl(Uri.parse(link));
    }
  }

  if (type != "connecting" && type != "success" && !type.contains("nook")) {
    hasOk = true;
    late final Widget btn;
    if (submitTimeout != null) {
      btn = _CountDownButton(
        text: 'OK',
        second: submitTimeout,
        onPressed: submit,
        submitOnTimeout: true,
      );
    } else {
      btn = dialogButton('OK', onPressed: submit);
    }
    buttons.insert(0, btn);
  }
  hasCancel ??= !type.contains("error") &&
      !type.contains("nocancel") &&
      type != "restarting";
  if (hasCancel) {
    buttons.insert(
        0, dialogButton('Cancel', onPressed: cancel, isOutline: true));
  }
  if (type.contains("hasclose")) {
    buttons.insert(
        0,
        dialogButton('Close', onPressed: () {
          dialogManager.dismissAll();
        }));
  }
  if (reconnect != null &&
      title == "Connection Error" &&
      reconnectTimeout != null) {
    // `enabled` 用于在按钮被点击后禁用该对话框按钮。
    final enabled = true.obs;
    final button = Obx(() => _CountDownButton(
          text: 'Reconnect',
          second: reconnectTimeout,
          onPressed: enabled.isTrue
              ? () {
                  // 禁用按钮
                  enabled.value = false;
                  reconnect(dialogManager, sessionId, false);
                }
              : null,
        ));
    buttons.insert(0, button);
  }
  if (link.isNotEmpty) {
    buttons.insert(0, dialogButton('JumpLink', onPressed: jumplink));
  }
  dialogManager.show(
    (setState, close, context) => CustomAlertDialog(
      title: null,
      content: SelectionArea(child: msgboxContent(type, title, text)),
      actions: buttons,
      onSubmit: hasOk ? submit : null,
      onCancel: hasCancel == true ? cancel : null,
    ),
    tag: '$sessionId-$type-$title-$text-$link',
  );
}

Color? _msgboxColor(String type) {
  if (type == "input-password" || type == "custom-os-password") {
    return Color(0xFFAD448E);
  }
  if (type.contains("success")) {
    return Color(0xFF32bea6);
  }
  if (type.contains("error") || type == "re-input-password") {
    return Color(0xFFE04F5F);
  }
  return Color(0xFF2C8CFF);
}

Widget msgboxIcon(String type) {
  IconData? iconData;
  if (type.contains("error") || type == "re-input-password") {
    iconData = Icons.cancel;
  }
  if (type.contains("success")) {
    iconData = Icons.check_circle;
  }
  if (type == "wait-uac" || type == "wait-remote-accept-nook") {
    iconData = Icons.hourglass_top;
  }
  if (type == 'on-uac' || type == 'on-foreground-elevated') {
    iconData = Icons.admin_panel_settings;
  }
  if (type.contains('info')) {
    iconData = Icons.info;
  }
  if (iconData != null) {
    return Icon(iconData, size: 50, color: _msgboxColor(type))
        .marginOnly(right: 16);
  }

  return Offstage();
}

// title 应为 null
Widget msgboxContent(String type, String title, String text) {
  String translateText(String text) {
    if (text.indexOf('Failed') == 0 && text.indexOf(': ') > 0) {
      List<String> words = text.split(': ');
      for (var i = 0; i < words.length; ++i) {
        words[i] = translate(words[i]);
      }
      text = words.join(': ');
    } else {
      List<String> words = text.split(' ');
      if (words.length > 1 && words[0].endsWith('_tip')) {
        words[0] = translate(words[0]);
        final rest = text.substring(words[0].length + 1);
        text = '${words[0]} ${translate(rest)}';
      } else {
        text = translate(text);
      }
    }
    return text;
  }

  return Row(
    children: [
      msgboxIcon(type),
      Expanded(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              translate(title),
              style: TextStyle(fontSize: 21),
            ).marginOnly(bottom: 10),
            createDialogContent(translateText(text)),
          ],
        ),
      ),
    ],
  ).marginOnly(bottom: 12);
}

void msgBoxCommon(OverlayDialogManager dialogManager, String title,
    Widget content, List<Widget> buttons,
    {bool hasCancel = true}) {
  dialogManager.show((setState, close, context) => CustomAlertDialog(
        title: Text(
          translate(title),
          style: TextStyle(fontSize: 21),
        ),
        content: content,
        actions: buttons,
        onCancel: hasCancel ? close : null,
      ));
}

Color str2color(String str, [alpha = 0xFF]) {
  var hash = 160 << 16 + 114 << 8 + 91;
  for (var i = 0; i < str.length; i += 1) {
    hash = str.codeUnitAt(i) + ((hash << 5) - hash);
  }
  hash = hash % 16777216;
  return Color((hash & 0xFF7FFF) | (alpha << 24));
}

Color str2color2(String str, {List<int> existing = const []}) {
  Map<String, Color> colorMap = {
    "red": Colors.red,
    "green": Colors.green,
    "blue": Colors.blue,
    "orange": Colors.orange,
    "purple": Colors.purple,
    "grey": Colors.grey,
    "cyan": Colors.cyan,
    "lime": Colors.lime,
    "teal": Colors.teal,
    "pink": Colors.pink[200]!,
    "indigo": Colors.indigo,
    "brown": Colors.brown,
  };
  final color = colorMap[str.toLowerCase()];
  if (color != null) {
    return color.withAlpha(0xFF);
  }
  if (str.toLowerCase() == 'yellow') {
    return Colors.yellow.withAlpha(0xFF);
  }
  var hash = 0;
  for (var i = 0; i < str.length; i++) {
    hash += str.codeUnitAt(i);
  }
  List<Color> colorList = colorMap.values.toList();
  hash = hash % colorList.length;
  var result = colorList[hash].withAlpha(0xFF);
  if (existing.contains(result.value)) {
    Color? notUsed =
        colorList.firstWhereOrNull((e) => !existing.contains(e.value));
    if (notUsed != null) {
      result = notUsed;
    }
  }
  return result;
}

const K = 1024;
const M = K * K;
const G = M * K;

String readableFileSize(double size) {
  if (size < K) {
    return "${size.toStringAsFixed(2)} B";
  } else if (size < M) {
    return "${(size / K).toStringAsFixed(2)} KB";
  } else if (size < G) {
    return "${(size / M).toStringAsFixed(2)} MB";
  } else {
    return "${(size / G).toStringAsFixed(2)} GB";
  }
}

/// 当尺寸为 1 时，Flutter 无法捕获 PointerMoveEvent
/// 这会在 Android AccessibilityService 输入时发生
/// android 尚无法初始化手势分发尺寸，参见：https://stackoverflow.com/questions/59960451/android-accessibility-dispatchgesture-is-it-possible-to-specify-pressure-for-a
/// 在 flutter 或 android 修复该 bug 之前，先用这个临时方案
class AccessibilityListener extends StatelessWidget {
  final Widget? child;
  static final offset = 100;

  AccessibilityListener({this.child});

  @override
  Widget build(BuildContext context) {
    return Listener(
        onPointerDown: (evt) {
          if (evt.size == 1) {
            GestureBinding.instance.handlePointerEvent(PointerAddedEvent(
                pointer: evt.pointer + offset, position: evt.position));
            GestureBinding.instance.handlePointerEvent(PointerDownEvent(
                pointer: evt.pointer + offset,
                size: 0.1,
                position: evt.position));
          }
        },
        onPointerUp: (evt) {
          if (evt.size == 1) {
            GestureBinding.instance.handlePointerEvent(PointerUpEvent(
                pointer: evt.pointer + offset,
                size: 0.1,
                position: evt.position));
            GestureBinding.instance.handlePointerEvent(PointerRemovedEvent(
                pointer: evt.pointer + offset, position: evt.position));
          }
        },
        onPointerMove: (evt) {
          if (evt.size == 1) {
            GestureBinding.instance.handlePointerEvent(PointerMoveEvent(
                pointer: evt.pointer + offset,
                size: 0.1,
                delta: evt.delta,
                position: evt.position));
          }
        },
        child: child);
  }
}

class AndroidPermissionManager {
  static Completer<bool>? _completer;
  static Timer? _timer;
  static var _current = "";

  static Future<bool> check(String type) {
    if (isDesktop || isWeb) {
      return Future.value(true);
    }
    return gFFI.invokeMethod("check_permission", type);
  }

  // 通过 startActivity 跳转到 Android 设置页面，由用户手动申请权限
  static void startAction(String action) {
    gFFI.invokeMethod(AndroidChannel.kStartAction, action);
  }

  /// 我们使用 XXPermissions 来申请权限，
  /// 支持的类型参见 https://github.com/getActivity/XXPermissions/blob/e46caea32a64ad7819df62d448fb1c825481cd28/library/src/main/java/com/hjq/permissions/Permission.java
  static Future<bool> request(String type) {
    if (isDesktop || isWeb) {
      return Future.value(true);
    }

    gFFI.invokeMethod("request_permission", type);

    // 清除上一个任务
    if (_completer?.isCompleted == false) {
      _completer?.complete(false);
    }
    _timer?.cancel();

    _current = type;
    _completer = Completer<bool>();

    _timer = Timer(Duration(seconds: 120), () {
      if (_completer == null) return;
      if (!_completer!.isCompleted) {
        _completer!.complete(false);
      }
      _completer = null;
      _current = "";
    });
    return _completer!.future;
  }

  static complete(String type, bool res) {
    if (type != _current) {
      res = false;
    }
    _timer?.cancel();
    _completer?.complete(res);
    _current = "";
  }
}

RadioListTile<T> getRadio<T>(
    Widget title, T toValue, T curValue, ValueChanged<T?>? onChange,
    {bool? dense}) {
  return RadioListTile<T>(
    visualDensity: VisualDensity.compact,
    controlAffinity: ListTileControlAffinity.trailing,
    title: title,
    value: toValue,
    groupValue: curValue,
    onChanged: onChange,
    dense: dense,
  );
}

/// 查找 ffi，tag 为 Remote ID
/// 供会话专用
FFI ffi(String? tag) {
  return Get.find<FFI>(tag: tag);
}

/// 全局 FFI 对象
late FFI _globalFFI;

FFI get gFFI => _globalFFI;

Future<void> initGlobalFFI() async {
  debugPrint("_globalFFI init");
  _globalFFI = FFI(null);
  debugPrint("_globalFFI init end");
  // 执行 `put` 之后，也可以通过 Get.find<FFI>() 全局找到；
  Get.put<FFI>(_globalFFI, permanent: true);
}

String translate(String name) {
  if (name.startsWith('Failed to') && name.contains(': ')) {
    return name.split(': ').map((x) => translate(x)).join(': ');
  }
  return platformFFI.translate(name, localeName);
}

// 此函数必须与 rust 和 sciter 代码中的实现保持一致。
// rust：libs/hbb_common/src/config.rs -> option2bool()
// sciter：没有该函数，但仍应保持一致。
bool option2bool(String option, String value) {
  bool res;
  if (option.startsWith("enable-")) {
    res = value != "N";
  } else if (option.startsWith("allow-") ||
      option == kOptionStopService ||
      option == kOptionDirectServer ||
      option == kOptionForceAlwaysRelay) {
    res = value == "Y";
  } else {
    // "" 为 true
    res = value != "N";
  }
  return res;
}

String bool2option(String option, bool b) {
  String res;
  if (option.startsWith('enable-') &&
      option != kOptionEnableUdpPunch &&
      option != kOptionEnableIpv6Punch &&
      option != kOptionEnableWebrtc) {
    res = b ? defaultOptionYes : 'N';
  } else if (option.startsWith('allow-') ||
      option == kOptionStopService ||
      option == kOptionDirectServer ||
      option == kOptionForceAlwaysRelay) {
    res = b ? 'Y' : defaultOptionNo;
  } else {
    res = b ? 'Y' : 'N';
  }
  return res;
}

mainSetBoolOption(String key, bool value) async {
  String v = bool2option(key, value);
  await bind.mainSetOption(key: key, value: v);
}

Future<bool> mainGetBoolOption(String key) async {
  return option2bool(key, await bind.mainGetOption(key: key));
}

bool mainGetBoolOptionSync(String key) {
  return option2bool(key, bind.mainGetOptionSync(key: key));
}

mainSetLocalBoolOption(String key, bool value) async {
  String v = bool2option(key, value);
  await bind.mainSetLocalOption(key: key, value: v);
}

bool mainGetLocalBoolOptionSync(String key) {
  return option2bool(key, bind.mainGetLocalOption(key: key));
}

bool mainGetPeerBoolOptionSync(String id, String key) {
  return option2bool(key, bind.mainGetPeerOptionSync(id: id, key: key));
}

// 不要使用 `option2bool()` 和 `bool2option()` 来转换会话选项。
// 请改用 `sessionGetToggleOption()` 和 `sessionToggleOption()`。
// 因为所有会话选项都使用 `Y` 和 `<Empty>` 作为值。

Future<bool> matchPeer(
    String searchText, Peer peer, PeerTabIndex peerTabIndex) async {
  if (searchText.isEmpty) {
    return true;
  }
  if (peer.id.toLowerCase().contains(searchText)) {
    return true;
  }
  if (peer.hostname.toLowerCase().contains(searchText) ||
      peer.username.toLowerCase().contains(searchText)) {
    return true;
  }
  if (peer.alias.toLowerCase().contains(searchText)) {
    return true;
  }
  if (peerTabShowNote(peerTabIndex) &&
      peer.note.toLowerCase().contains(searchText)) {
    return true;
  }
  return false;
}

/// 获取当前 [platform] 的镜像。
Widget getPlatformImage(String platform, {double size = 50}) {
  if (platform.isEmpty) {
    return Container(width: size, height: size);
  }
  if (platform == kPeerPlatformMacOS) {
    platform = 'mac';
  } else if (platform != kPeerPlatformLinux &&
      platform != kPeerPlatformAndroid) {
    platform = 'win';
  } else {
    platform = platform.toLowerCase();
  }
  return SvgPicture.asset('assets/$platform.svg', height: size, width: size);
}

class LastWindowPosition {
  double? width;
  double? height;
  double? offsetWidth;
  double? offsetHeight;
  bool? isMaximized;
  bool? isFullscreen;

  LastWindowPosition(this.width, this.height, this.offsetWidth,
      this.offsetHeight, this.isMaximized, this.isFullscreen);

  bool equals(LastWindowPosition other) {
    return ((width == other.width) &&
        (height == other.height) &&
        (offsetWidth == other.offsetWidth) &&
        (offsetHeight == other.offsetHeight) &&
        (isMaximized == other.isMaximized) &&
        (isFullscreen == other.isFullscreen));
  }

  Map<String, dynamic> toJson() {
    return <String, dynamic>{
      "width": width,
      "height": height,
      "offsetWidth": offsetWidth,
      "offsetHeight": offsetHeight,
      "isMaximized": isMaximized,
      "isFullscreen": isFullscreen,
    };
  }

  @override
  String toString() {
    return jsonEncode(toJson());
  }

  static LastWindowPosition? loadFromString(String content) {
    if (content.isEmpty) {
      return null;
    }
    try {
      final m = jsonDecode(content);
      return LastWindowPosition(m["width"], m["height"], m["offsetWidth"],
          m["offsetHeight"], m["isMaximized"], m["isFullscreen"]);
    } catch (e) {
      debugPrintStack(
          label:
              'Failed to load LastWindowPosition "$content" ${e.toString()}');
      return null;
    }
  }
}

String get windowFramePrefix =>
    kWindowPrefix +
    (bind.isIncomingOnly()
        ? "incoming_"
        : (bind.isOutgoingOnly() ? "outgoing_" : ""));

typedef WindowKey = ({WindowType type, int? windowId});

LastWindowPosition? _lastWindowPosition = null;
final Debouncer _saveWindowDebounce = Debouncer(delay: Duration(seconds: 1));

/// 退出时保存窗口位置和尺寸
/// 注意，如果是子窗口必须提供 windowId
Future<void> saveWindowPosition(WindowType type,
    {int? windowId, bool? flush}) async {
  if (type != WindowType.Main && windowId == null) {
    debugPrint(
        "Error: windowId cannot be null when saving positions for sub window");
  }

  Offset? position;
  Size? sz;
  late bool isMaximized;
  bool isFullscreen = stateGlobal.fullscreen.isTrue;

  setPreFrame() {
    final pos = bind.getLocalFlutterOption(k: windowFramePrefix + type.name);
    var lpos = LastWindowPosition.loadFromString(pos);
    if (lpos != null) {
      if (lpos.offsetWidth != null && lpos.offsetHeight != null) {
        position = Offset(lpos.offsetWidth!, lpos.offsetHeight!);
      }
      if (lpos.width != null && lpos.height != null) {
        sz = Size(lpos.width!, lpos.height!);
      }
    }
  }

  switch (type) {
    case WindowType.Main:
      // 检查 `bind.isIncomingOnly()` 是针对 MacOS 的一个简单变通方案。
      // `await windowManager.isMaximized()` 总是返回 true
      // 当窗口不可调整大小时。原因未知。
      //
      // main.dart 中的 `setResizable(!bind.isIncomingOnly());`
      isMaximized =
          bind.isIncomingOnly() ? false : await windowManager.isMaximized();
      if (isFullscreen || isMaximized) {
        setPreFrame();
      } else {
        position = await windowManager.getPosition(
            ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
        sz = await windowManager.getSize(
            ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
      }
      break;
    default:
      final wc = WindowController.fromWindowId(windowId!);
      isMaximized = await wc.isMaximized();
      if (isFullscreen || isMaximized) {
        setPreFrame();
      } else {
        final Rect frame;
        try {
          frame = await wc.getFrame();
        } catch (e) {
          debugPrint(
              "Failed to get frame of window $windowId, it may be hidden");
          return;
        }
        position = frame.topLeft;
        sz = frame.size;
      }
      break;
  }
  if (isWindows && position != null) {
    const kMinOffset = -10000;
    const kMaxOffset = 10000;
    if (position!.dx < kMinOffset ||
        position!.dy < kMinOffset ||
        position!.dx > kMaxOffset ||
        position!.dy > kMaxOffset) {
      debugPrint("Invalid position: $position, ignore saving position");
      return;
    }
  }

  final pos = LastWindowPosition(sz?.width, sz?.height, position?.dx,
      position?.dy, isMaximized, isFullscreen);

  final WindowKey key = (type: type, windowId: windowId);

  final bool haveNewWindowPosition =
      (_lastWindowPosition == null) || !pos.equals(_lastWindowPosition!);
  final bool isPreviousNewWindowPositionPending = _saveWindowDebounce.isRunning;

  if (haveNewWindowPosition || isPreviousNewWindowPositionPending) {
    _lastWindowPosition = pos;

    if (flush ?? false) {
      // 如果之前的更新还在等待中，就替换它。
      _saveWindowDebounce.cancel();
      await _saveWindowPositionActual(key);
    } else if (haveNewWindowPosition) {
      _saveWindowDebounce.call(() => _saveWindowPositionActual(key));
    }
  }
}

Future<void> _saveWindowPositionActual(WindowKey key) async {
  LastWindowPosition? pos = _lastWindowPosition;

  if (pos != null) {
    debugPrint(
        "Saving frame: ${key.windowId}: ${pos.width}/${pos.height}, offset:${pos.offsetWidth}/${pos.offsetHeight}, isMaximized:${pos.isMaximized}, isFullscreen:${pos.isFullscreen}");

    await bind.setLocalFlutterOption(
        k: windowFramePrefix + key.type.name, v: pos.toString());

    if ((key.type == WindowType.RemoteDesktop ||
            key.type == WindowType.ViewCamera) &&
        key.windowId != null) {
      await _saveSessionWindowPosition(key.type, key.windowId!,
          pos.isMaximized ?? false, pos.isFullscreen ?? false, pos);
    }
  }
}

Future _saveSessionWindowPosition(WindowType windowType, int windowId,
    bool isMaximized, bool isFullscreen, LastWindowPosition pos) async {
  final remoteList = await DesktopMultiWindow.invokeMethod(
      windowId, kWindowEventGetRemoteList, null);
  getPeerPos(String peerId) {
    if (isMaximized || isFullscreen) {
      final peerPos = bind.mainGetPeerFlutterOptionSync(
          id: peerId, k: windowFramePrefix + windowType.name);
      var lpos = LastWindowPosition.loadFromString(peerPos);
      return LastWindowPosition(
              lpos?.width ?? pos.offsetWidth,
              lpos?.height ?? pos.offsetHeight,
              lpos?.offsetWidth ?? pos.offsetWidth,
              lpos?.offsetHeight ?? pos.offsetHeight,
              isMaximized,
              isFullscreen)
          .toString();
    } else {
      return pos.toString();
    }
  }

  if (remoteList != null) {
    for (final peerId in remoteList.split(',')) {
      bind.mainSetPeerFlutterOptionSync(
          id: peerId,
          k: windowFramePrefix + windowType.name,
          v: getPeerPos(peerId));
    }
  }
}

Future<Size> _adjustRestoreMainWindowSize(double? width, double? height) async {
  const double minWidth = 1;
  const double minHeight = 1;
  const double maxWidth = 6480;
  const double maxHeight = 6480;

  final defaultWidth =
      ((isDesktop || isWebDesktop) ? 1280 : kMobileDefaultDisplayWidth)
          .toDouble();
  final defaultHeight =
      ((isDesktop || isWebDesktop) ? 720 : kMobileDefaultDisplayHeight)
          .toDouble();
  double restoreWidth = width ?? defaultWidth;
  double restoreHeight = height ?? defaultHeight;

  if (restoreWidth < minWidth) {
    restoreWidth = defaultWidth;
  }
  if (restoreHeight < minHeight) {
    restoreHeight = defaultHeight;
  }
  if (restoreWidth > maxWidth) {
    restoreWidth = defaultWidth;
  }
  if (restoreHeight > maxHeight) {
    restoreHeight = defaultHeight;
  }
  return Size(restoreWidth, restoreHeight);
}

// 可以考虑改用 Rect.contains()，
// 尽管实现并不完全相同。
bool isPointInRect(Offset point, Rect rect) {
  return point.dx >= rect.left &&
      point.dx <= rect.right &&
      point.dy >= rect.top &&
      point.dy <= rect.bottom;
}

/// 返回 null 表示居中
Future<Offset?> _adjustRestoreMainWindowOffset(
  double? left,
  double? top,
  double? width,
  double? height,
) async {
  if (left == null || top == null || width == null || height == null) {
    return null;
  }

  if (isDesktop || isWebDesktop) {
    final screens = await window_size.getScreenList();
    if (screens.isNotEmpty) {
      final windowRect = Rect.fromLTWH(left, top, width, height);
      bool isVisible = false;
      for (final screen in screens) {
        final intersection = windowRect.intersect(screen.visibleFrame);
        if (intersection.width >= 10.0 && intersection.height >= 10.0) {
          isVisible = true;
          break;
        }
      }
      if (!isVisible) {
        return null;
      }
      return Offset(left, top);
    }
  }

  double frameLeft = 0.0;
  double frameTop = 0.0;
  double frameRight = ((isDesktop || isWebDesktop)
          ? kDesktopMaxDisplaySize
          : kMobileMaxDisplaySize)
      .toDouble();
  double frameBottom = ((isDesktop || isWebDesktop)
          ? kDesktopMaxDisplaySize
          : kMobileMaxDisplaySize)
      .toDouble();

  final minWidth = 10.0;
  if ((left + minWidth) > frameRight ||
      (top + minWidth) > frameBottom ||
      (left + width - minWidth) < frameLeft ||
      top < frameTop) {
    return null;
  } else {
    return Offset(left, top);
  }
}

/// 启动时恢复窗口位置和尺寸
/// 注意，如果是子窗口必须提供 windowId
//
// display 用于在单显示器模式下设置窗口的偏移量。
Future<bool> restoreWindowPosition(WindowType type,
    {int? windowId, String? peerId, int? display}) async {
  if (bind
      .mainGetEnv(key: "DISABLE_RUSTDESK_RESTORE_WINDOW_POSITION")
      .isNotEmpty) {
    return false;
  }
  if (type != WindowType.Main && windowId == null) {
    debugPrint(
        "Error: windowId cannot be null when saving positions for sub window");
    return false;
  }

  bool isRemotePeerPos = false;
  String? pos;
  // 无需检查 mainGetLocalBoolOptionSync(kOptionOpenNewConnInTabs)
  // 虽然 "open in tabs" 为 true 且新窗口会恢复被控端位置，但这样也没问题。
  if ((type == WindowType.RemoteDesktop || type == WindowType.ViewCamera) &&
      windowId != null &&
      peerId != null) {
    final peerPos = bind.mainGetPeerFlutterOptionSync(
        id: peerId, k: windowFramePrefix + type.name);
    if (peerPos.isNotEmpty) {
      pos = peerPos;
    }
    isRemotePeerPos = pos != null;
  }
  pos ??= bind.getLocalFlutterOption(k: windowFramePrefix + type.name);

  var lpos = LastWindowPosition.loadFromString(pos);
  if (lpos == null) {
    debugPrint("No window position saved, trying to center the window.");
    switch (type) {
      case WindowType.Main:
        // 仅在没有保存过位置时（首次运行时）把主窗口居中。
        if (isWindows || isLinux) {
          await windowManager.center();
        }
        // 对于 MacOS，窗口默认已经居中。
        // 参见 https://github.com/rustdesk/rustdesk/blob/9b9276e7524523d7f667fefcd0694d981443df0e/flutter/macos/Runner/Base.lproj/MainMenu.xib#L333
        // 如果 `<window>` 中的 `<windowPositionMask>` 没有设置，窗口将居中显示。
        break;
      default:
        // 如果没有保存过位置，就无需改变子窗口的位置，
        // 因为默认位置已经是居中的。
        // https://github.com/rustdesk/rustdesk/blob/317639169359936f7f9f85ef445ec9774218772d/flutter/lib/utils/multi_window_manager.dart#L163
        break;
    }
    return true;
  }
  if (type == WindowType.RemoteDesktop || type == WindowType.ViewCamera) {
    if (!isRemotePeerPos && windowId != null) {
      if (lpos.offsetWidth != null) {
        lpos.offsetWidth = lpos.offsetWidth! + windowId * kNewWindowOffset;
      }
      if (lpos.offsetHeight != null) {
        lpos.offsetHeight = lpos.offsetHeight! + windowId * kNewWindowOffset;
      }
    }
    if (display != null) {
      if (lpos.offsetWidth != null) {
        lpos.offsetWidth = lpos.offsetWidth! + display * kNewWindowOffset;
      }
      if (lpos.offsetHeight != null) {
        lpos.offsetHeight = lpos.offsetHeight! + display * kNewWindowOffset;
      }
    }
  }

  final size = await _adjustRestoreMainWindowSize(lpos.width, lpos.height);
  final offsetLeftTop = await _adjustRestoreMainWindowOffset(
    lpos.offsetWidth,
    lpos.offsetHeight,
    size.width,
    size.height,
  );
  debugPrint(
      "restore lpos: ${size.width}/${size.height}, offset:${offsetLeftTop?.dx}/${offsetLeftTop?.dy}, isMaximized: ${lpos.isMaximized}, isFullscreen: ${lpos.isFullscreen}");

  switch (type) {
    case WindowType.Main:
      restorePos() async {
        if (offsetLeftTop == null) {
          await windowManager.center();
        } else {
          await windowManager.setPosition(offsetLeftTop,
              ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
        }
      }
      if (lpos.isMaximized == true) {
        await restorePos();
        if (!(bind.isIncomingOnly() || bind.isOutgoingOnly())) {
          await windowManager.maximize();
        }
      } else {
        final storeSize = !bind.isIncomingOnly() || bind.isOutgoingOnly();
        if (isWindows) {
          if (storeSize) {
            // 我们需要先设置窗口尺寸，以避免某些特殊情况下的尺寸错误。
            // 例如有两个显示器，左边是 100% DPI，右边是 175% DPI。
            // 窗口属于左边的显示器，但如果稍微向右移动，就会被认为属于右边的显示器。
            // 恢复之后尺寸就会不正确。
            // 参见 https://github.com/rustdesk/rustdesk/pull/9840 中的已知问题
            await windowManager.setSize(size,
                ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
          }
          await restorePos();
          if (storeSize) {
            await windowManager.setSize(size,
                ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
          }
        } else {
          if (storeSize) {
            await windowManager.setSize(size,
                ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
          }
          await restorePos();
        }
      }
      return true;
    default:
      final wc = WindowController.fromWindowId(windowId!);
      restoreFrame() async {
        if (offsetLeftTop == null) {
          await wc.center();
        } else {
          final frame = Rect.fromLTWH(
              offsetLeftTop.dx, offsetLeftTop.dy, size.width, size.height);
          await wc.setFrame(frame);
        }
      }
      if (lpos.isFullscreen == true) {
        if (!isMacOS) {
          await restoreFrame();
        }
        // 需要一段延时，以避免全屏后窗口又被恢复。
        Future.delayed(Duration(milliseconds: 300), () async {
          if (kWindowId == windowId) {
            stateGlobal.setFullscreen(true);
          } else {
            // 如果不是当前窗口，需要向 `windowId` 发送全屏消息
            DesktopMultiWindow.invokeMethod(
                windowId, kWindowEventSetFullscreen, 'true');
          }
        });
      } else if (lpos.isMaximized == true) {
        await restoreFrame();
        // 需要一段延时，以避免最大化后窗口又被恢复。
        Future.delayed(Duration(milliseconds: 300), () async {
          await wc.maximize();
        });
      } else {
        await restoreFrame();
      }
      break;
  }
  return false;
}

var webInitialLink = "";

/// 为 macos/windows 初始化 uni links
///
/// [可用性]
/// initUniLinks 只应在 macos/windows 上使用。
/// 目前 linux 上我们使用 dbus。
Future<bool> initUniLinks() async {
  if (isLinux) {
    return false;
  }
  // 检查冷启动
  try {
    final initialLink = await getInitialLink();
    print("initialLink: $initialLink");
    if (initialLink == null || initialLink.isEmpty) {
      return false;
    }
    if (isWeb) {
      webInitialLink = initialLink;
      return false;
    } else {
      return handleUriLink(uriString: initialLink);
    }
  } catch (err) {
    debugPrintStack(label: "$err");
    return false;
  }
}

/// 监听 uni links。
///
/// * handleByFlutter：uni links 是否交由 Flutter 处理。
///
/// 返回一个可用于监听 uni links 的 [StreamSubscription]。
StreamSubscription? listenUniLinks({handleByFlutter = true}) {
  if (isLinux || isWeb) {
    return null;
  }

  final sub = uriLinkStream.listen((Uri? uri) {
    debugPrint("A uri was received: $uri. handleByFlutter $handleByFlutter");
    if (uri != null) {
      if (handleByFlutter) {
        handleUriLink(uri: uri);
      } else {
        bind.sendUrlScheme(url: uri.toString());
      }
    } else {
      print("uni listen error: uri is empty.");
    }
  }, onError: (err) {
    print("uni links error: $err");
  });
  return sub;
}

enum UriLinkType {
  remoteDesktop,
  fileTransfer,
  viewCamera,
  portForward,
  rdp,
  terminal,
}

setEnvTerminalAdmin() {
  bind.mainSetEnv(key: 'IS_TERMINAL_ADMIN', value: 'Y');
}

// uri 链接处理器
bool handleUriLink({List<String>? cmdArgs, Uri? uri, String? uriString}) {
  List<String>? args;
  if (cmdArgs != null && cmdArgs.isNotEmpty) {
    args = cmdArgs;
    // rustdesk <uri link>
    if (args[0].startsWith(bind.mainUriPrefixSync())) {
      final uri = Uri.tryParse(args[0]);
      if (uri != null) {
        args = urlLinkToCmdArgs(uri);
      }
    }
  } else if (uri != null) {
    args = urlLinkToCmdArgs(uri);
  } else if (uriString != null) {
    final uri = Uri.tryParse(uriString);
    if (uri != null) {
      args = urlLinkToCmdArgs(uri);
    }
  }
  if (args == null) {
    return false;
  }

  if (args.isEmpty) {
    windowOnTop(null);
    return true;
  }

  UriLinkType? type;
  String? id;
  String? password;
  String? switchUuid;
  bool? forceRelay;
  for (int i = 0; i < args.length; i++) {
    switch (args[i]) {
      case '--connect':
      case '--play':
        type = UriLinkType.remoteDesktop;
        id = args[i + 1];
        i++;
        break;
      case '--file-transfer':
        type = UriLinkType.fileTransfer;
        id = args[i + 1];
        i++;
        break;
      case '--view-camera':
        type = UriLinkType.viewCamera;
        id = args[i + 1];
        i++;
        break;
      case '--port-forward':
        type = UriLinkType.portForward;
        id = args[i + 1];
        i++;
        break;
      case '--rdp':
        type = UriLinkType.rdp;
        id = args[i + 1];
        i++;
        break;
      case '--terminal':
        type = UriLinkType.terminal;
        id = args[i + 1];
        i++;
        break;
      case '--terminal-admin':
        setEnvTerminalAdmin();
        type = UriLinkType.terminal;
        id = args[i + 1];
        i++;
        break;
      case '--password':
        password = args[i + 1];
        i++;
        break;
      case '--switch_uuid':
        switchUuid = args[i + 1];
        i++;
        break;
      case '--relay':
        forceRelay = true;
        break;
      default:
        break;
    }
  }
  if (type != null && id != null) {
    switch (type) {
      case UriLinkType.remoteDesktop:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newRemoteDesktop(id!,
              password: password,
              switchUuid: switchUuid,
              forceRelay: forceRelay);
        });
        break;
      case UriLinkType.fileTransfer:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newFileTransfer(id!,
              password: password, forceRelay: forceRelay);
        });
        break;
      case UriLinkType.viewCamera:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newViewCamera(id!,
              password: password, forceRelay: forceRelay);
        });
        break;
      case UriLinkType.portForward:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newPortForward(id!, false,
              password: password, forceRelay: forceRelay);
        });
        break;
      case UriLinkType.rdp:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newPortForward(id!, true,
              password: password, forceRelay: forceRelay);
        });
        break;
      case UriLinkType.terminal:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newTerminal(id!,
              password: password, forceRelay: forceRelay);
        });
        break;
    }

    return true;
  }

  return false;
}

List<String>? urlLinkToCmdArgs(Uri uri) {
  String? command;
  String? id;
  final options = [
    "connect",
    "play",
    "file-transfer",
    "view-camera",
    "port-forward",
    "rdp",
    "terminal",
    "terminal-admin",
  ];
  if (uri.authority.isEmpty &&
      uri.path.split('').every((char) => char == '/')) {
    return [];
  } else if (uri.authority == "connection" && uri.path.startsWith("/new/")) {
    // 为了兼容
    command = '--connect';
    id = uri.path.substring("/new/".length);
  } else if (uri.authority == "config") {
    if (isAndroid || isIOS) {
      final allowDeepLinkServerSettings =
          bind.mainGetBuildinOption(key: kOptionAllowDeepLinkServerSettings) ==
              'Y';
      if (!allowDeepLinkServerSettings) {
        debugPrint(
            "Ignore rustdesk://config because $kOptionAllowDeepLinkServerSettings is not enabled.");
        // 面向用户的错误提示保持笼统；详细的拒绝原因记录在调试日志中。
        // 延迟弹出 toast，避免冷启动处理 deeplink 时 overlay 丢失。
        Timer(Duration(seconds: 1), () {
          showToast(translate('Failed'));
        });
        return null;
      }
      final config = uri.path.substring("/".length);
      // 加一个定时器让 showToast 生效
      Timer(Duration(seconds: 1), () {
        importConfig(null, null, config);
      });
    }
    return null;
  } else if (uri.authority == "password") {
    if (isAndroid || isIOS) {
      final allowDeepLinkPassword =
          bind.mainGetBuildinOption(key: kOptionAllowDeepLinkPassword) == 'Y';
      if (!allowDeepLinkPassword) {
        debugPrint(
            "Ignore rustdesk://password because $kOptionAllowDeepLinkPassword is not enabled.");
        // 面向用户的错误提示保持笼统；详细的拒绝原因记录在调试日志中。
        // 延迟弹出 toast，避免冷启动处理 deeplink 时 overlay 丢失。
        Timer(Duration(seconds: 1), () {
          showToast(translate('Failed'));
        });
        return null;
      }
      final password = uri.path.substring("/".length);
      if (password.isNotEmpty) {
        Timer(Duration(seconds: 1), () async {
          final ok =
              await bind.mainSetPermanentPasswordWithResult(password: password);
          showToast(translate(ok ? 'Successful' : 'Failed'));
        });
      }
    }
  } else if (options.contains(uri.authority)) {
    command = '--${uri.authority}';
    if (uri.path.length > 1) {
      id = uri.path.substring(1);
    }
  } else if (uri.authority.length > 2 &&
      (uri.path.length <= 1 ||
          (uri.path == '/r' || uri.path.startsWith('/r@')))) {
    // rustdesk://<connect-id>
    // rustdesk://<connect-id>/r
    // rustdesk://<connect-id>/r@<server>
    command = '--connect';
    id = uri.authority;
    if (uri.path.length > 1) {
      id = id + uri.path;
    }
  }

  var queryParameters =
      uri.queryParameters.map((k, v) => MapEntry(k.toLowerCase(), v));

  var key = queryParameters["key"];
  if (id != null) {
    if (key != null) {
      id = "$id?key=$key";
    }
  }

  if (isMobile && id != null) {
    final forceRelay = queryParameters["relay"] != null;
    final password = queryParameters["password"];

    // 根据命令确定连接类型
    if (command == '--file-transfer') {
      connect(Get.context!, id,
          isFileTransfer: true, forceRelay: forceRelay, password: password);
    } else if (command == '--view-camera') {
      connect(Get.context!, id,
          isViewCamera: true, forceRelay: forceRelay, password: password);
    } else if (command == '--terminal') {
      connect(Get.context!, id,
          isTerminal: true, forceRelay: forceRelay, password: password);
    } else if (command == 'terminal-admin') {
      setEnvTerminalAdmin();
      connect(Get.context!, id,
          isTerminal: true, forceRelay: forceRelay, password: password);
    } else {
      // 对 '--connect'、'--play' 或直连，默认使用远程桌面
      connect(Get.context!, id, forceRelay: forceRelay, password: password);
    }
    return null;
  }

  List<String> args = List.empty(growable: true);
  if (command != null && id != null) {
    args.add(command);
    args.add(id);
    var param = queryParameters;
    String? password = param["password"];
    if (password != null) args.addAll(['--password', password]);
    String? switch_uuid = param["switch_uuid"];
    if (switch_uuid != null) args.addAll(['--switch_uuid', switch_uuid]);
    if (param["relay"] != null) args.add("--relay");
    return args;
  }

  return null;
}

connectMainDesktop(String id,
    {required bool isFileTransfer,
    required bool isViewCamera,
    required bool isTerminal,
    required bool isTcpTunneling,
    required bool isRDP,
    bool? forceRelay,
    String? password,
    String? connToken,
    bool? isSharedPassword}) async {
  if (isFileTransfer) {
    await rustDeskWinManager.newFileTransfer(id,
        password: password,
        isSharedPassword: isSharedPassword,
        connToken: connToken,
        forceRelay: forceRelay);
  } else if (isViewCamera) {
    await rustDeskWinManager.newViewCamera(id,
        password: password,
        isSharedPassword: isSharedPassword,
        connToken: connToken,
        forceRelay: forceRelay);
  } else if (isTcpTunneling || isRDP) {
    await rustDeskWinManager.newPortForward(id, isRDP,
        password: password,
        isSharedPassword: isSharedPassword,
        connToken: connToken,
        forceRelay: forceRelay);
  } else if (isTerminal) {
    await rustDeskWinManager.newTerminal(id,
        password: password,
        isSharedPassword: isSharedPassword,
        connToken: connToken,
        forceRelay: forceRelay);
  } else {
    await rustDeskWinManager.newRemoteDesktop(id,
        password: password,
        isSharedPassword: isSharedPassword,
        forceRelay: forceRelay);
  }
}

/// 连接 ID 为 [id] 的被控端。
/// 如果 [isFileTransfer]，则只启动文件传输会话。
/// 如果 [isViewCamera]，则只启动查看摄像头会话。
/// 如果 [isTcpTunneling]，则只启动 tcp 隧道会话。
/// 如果 [isRDP]，则只启动 rdp 会话。
connect(BuildContext context, String id,
    {bool isFileTransfer = false,
    bool isViewCamera = false,
    bool isTerminal = false,
    bool isTcpTunneling = false,
    bool isRDP = false,
    bool forceRelay = false,
    String? password,
    String? connToken,
    bool? isSharedPassword}) async {
  if (id == '') return;
  if (!isDesktop || desktopType == DesktopType.main) {
    try {
      if (Get.isRegistered<IDTextEditingController>()) {
        final idController = Get.find<IDTextEditingController>();
        idController.text = formatID(id);
      }
      if (Get.isRegistered<TextEditingController>()) {
        final fieldTextEditingController = Get.find<TextEditingController>();
        fieldTextEditingController.text = formatID(id);
      }
    } catch (_) {}
  }
  id = id.replaceAll(' ', '');
  final oldId = id;
  id = await bind.mainHandleRelayId(id: id);
  forceRelay = id != oldId || forceRelay;
  assert(!(isFileTransfer && isTcpTunneling && isRDP),
      "more than one connect type");

  if (isDesktop) {
    if (desktopType == DesktopType.main) {
      await connectMainDesktop(
        id,
        isFileTransfer: isFileTransfer,
        isViewCamera: isViewCamera,
        isTerminal: isTerminal,
        isTcpTunneling: isTcpTunneling,
        isRDP: isRDP,
        password: password,
        isSharedPassword: isSharedPassword,
        forceRelay: forceRelay,
      );
    } else {
      await rustDeskWinManager.call(WindowType.Main, kWindowConnect, {
        'id': id,
        'isFileTransfer': isFileTransfer,
        'isViewCamera': isViewCamera,
        'isTerminal': isTerminal,
        'isTcpTunneling': isTcpTunneling,
        'isRDP': isRDP,
        'password': password,
        'isSharedPassword': isSharedPassword,
        'forceRelay': forceRelay,
        'connToken': connToken,
      });
    }
  } else {
    if (isFileTransfer) {
      if (isWeb) {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) =>
                desktop_file_manager.FileManagerPage(
                    id: id,
                    password: password,
                    isSharedPassword: isSharedPassword),
          ),
        );
      } else {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) => FileManagerPage(
                id: id,
                password: password,
                isSharedPassword: isSharedPassword,
                forceRelay: forceRelay),
          ),
        );
      }
    } else if (isViewCamera) {
      if (isWeb) {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) =>
                desktop_view_camera.ViewCameraPage(
              key: ValueKey(id),
              id: id,
              toolbarState: ToolbarState(),
              password: password,
              isSharedPassword: isSharedPassword,
            ),
          ),
        );
      } else {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) => ViewCameraPage(
                id: id,
                password: password,
                isSharedPassword: isSharedPassword,
                forceRelay: forceRelay),
          ),
        );
      }
    } else if (isTerminal) {
      Navigator.push(
        context,
        MaterialPageRoute(
          builder: (BuildContext context) => TerminalPage(
            id: id,
            password: password,
            isSharedPassword: isSharedPassword,
            forceRelay: forceRelay,
          ),
        ),
      );
    } else {
      if (isWeb) {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) => desktop_remote.RemotePage(
              key: ValueKey(id),
              id: id,
              toolbarState: ToolbarState(),
              password: password,
              isSharedPassword: isSharedPassword,
            ),
          ),
        );
      } else {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) => RemotePage(
                id: id,
                password: password,
                isSharedPassword: isSharedPassword,
                forceRelay: forceRelay),
          ),
        );
      }
    }
    stateGlobal.isInMainPage = false;
  }

  FocusScopeNode currentFocus = FocusScope.of(context);
  if (!currentFocus.hasPrimaryFocus) {
    currentFocus.unfocus();
  }
}

Map<String, String> getHttpHeaders() {
  return {
    'Authorization': 'Bearer ${bind.mainGetLocalOption(key: 'access_token')}'
  };
}

// 内置类型的简单封装，供参考使用。
class SimpleWrapper<T> {
  T value;
  SimpleWrapper(this.value);
}

/// 桌面端带引用计数的屏幕常亮管理器。
/// 确保只有在所有会话都关闭/最小化后才禁用 wakelock。
///
/// 注意：每个 isolate 都有自己的 WakelockPlus 实例，断言相互独立。
/// 只要有一个 isolate 启用了 wakelock，屏幕就会保持常亮。
/// 该管理器负责同一个 isolate 内的多个标签页。
class WakelockManager {
  static final Set<UniqueKey> _enabledKeys = {};
  // 不要使用 WakelockPlus.enabled，它在 Android 上会报错：
  // Unhandled Exception: FormatException: Message corrupted
  //
  // 在 Linux 上，多次调用 enable() 只会创建一个 inhibit，但每次调用 disable()
  // 只有在 _cookie != null 时才会释放。所以我们需要自己的 _enabled 状态，以避免
  // 在未启用时调用 disable()。
  // 参见：https://github.com/fluttercommunity/wakelock_plus/blob/0c74e5bbc6aefac57b6c96bb7ef987705ed559ec/wakelock_plus/lib/src/wakelock_plus_linux_plugin.dart#L48
  static bool _enabled = false;

  static void enable(UniqueKey key, {bool isServer = false}) {
    // 检查在主动连接的会话中是否需要保持常亮
    if (!isServer) {
      final keepAwake =
          mainGetLocalBoolOptionSync(kOptionKeepAwakeDuringOutgoingSessions);
      if (!keepAwake) {
        return; // Don't enable wakelock if user disabled keep awake
      }
    }
    if (isDesktop) {
      _enabledKeys.add(key);
    }
    if (!_enabled) {
      _enabled = true;
      WakelockPlus.enable();
    }
  }

  static void disable(UniqueKey key) {
    if (isDesktop) {
      _enabledKeys.remove(key);
      if (_enabledKeys.isNotEmpty) {
        return;
      }
    }
    if (_enabled) {
      WakelockPlus.disable();
      _enabled = false;
    }
  }
}

/// 调用此函数以重新加载当前窗口。
///
/// [注意]
/// widget 树的顶层必须有 [RefreshWrapper]。
void reloadCurrentWindow() {
  if (Get.context != null) {
    // 重新加载自身窗口
    RefreshWrapper.of(Get.context!)?.rebuild();
  } else {
    debugPrint(
        "reload current window failed, global BuildContext does not exist");
  }
}

/// 调用此函数以重新加载所有窗口，包括主窗口和所有子窗口。
Future<void> reloadAllWindows() async {
  reloadCurrentWindow();
  try {
    final ids = await DesktopMultiWindow.getAllSubWindowIds();
    for (final id in ids) {
      DesktopMultiWindow.invokeMethod(id, kWindowActionRebuild);
    }
  } on AssertionError {
    // 忽略
  }
}

/// 表示 flutter 应用正以便携模式运行。
///
/// [注意]
/// 便携版构建仅在 Windows 上可用。
bool isRunningInPortableMode() {
  if (!isWindows) {
    return false;
  }
  return bool.hasEnvironment(kEnvPortableExecutable);
}

/// 窗口状态回调
Future<void> onActiveWindowChanged() async {
  print(
      "[MultiWindowHandler] active window changed: ${rustDeskWinManager.getActiveWindows()}");
  if (rustDeskWinManager.getActiveWindows().isEmpty) {
    // 关闭所有子窗口
    try {
      if (isLinux) {
        await Future.wait([
          saveWindowPosition(WindowType.Main),
          rustDeskWinManager.closeAllSubWindows()
        ]);
      } else {
        await rustDeskWinManager.closeAllSubWindows();
      }
    } catch (err) {
      debugPrintStack(label: "$err");
    } finally {
      debugPrint("Start closing RustDesk...");
      await windowManager.setPreventClose(false);
      await windowManager.close();
      if (isMacOS) {
        // 如果不加延时直接调用，`flutter/macos/Runner/MainFlutterWindow.swift` 能处理 "terminate" 事件。
        // 但应用不会退出。
        //
        // 不清楚为什么这里需要延时，`terminate()` 本身也是一个异步函数。
        //
        // 一个临时的变通办法：使用 `Timer.periodic` 以避免应用退不出去。
        // 因为 `await windowManager.close()` 和 `RdPlatformChannel.instance.terminate()`
        // 自 `Flutter 3.24.4` 起可能不生效，参见下面的日志。
        // 加上延时就能让应用正常退出。
        //
        //```
        // embedder.cc (2725): 'FlutterPlatformMessageCreateResponseHandle' returned 'kInvalidArguments'. Engine handle was invalid.
        // 2024-11-11 11:41:11.546 RustDesk[90272:2567686] Failed to create a FlutterPlatformMessageResponseHandle (2)
        // embedder.cc (2672): 'FlutterEngineSendPlatformMessage' returned 'kInvalidArguments'. Invalid engine handle.
        // 2024-11-11 11:41:11.565 RustDesk[90272:2567686] Failed to send message to Flutter engine on channel 'flutter/lifecycle' (2).
        // ```
        periodic_immediate(
            Duration(milliseconds: 30), RdPlatformChannel.instance.terminate);
      }
    }
  }
}

Timer periodic_immediate(Duration duration, Future<void> Function() callback) {
  Future.delayed(Duration.zero, callback);
  return Timer.periodic(duration, (timer) async {
    await callback();
  });
}

/// 返回易读的 windows 版本号
WindowsTarget getWindowsTarget(int buildNumber) {
  if (!isWindows) {
    return WindowsTarget.naw;
  }
  if (buildNumber >= 22000) {
    return WindowsTarget.w11;
  } else if (buildNumber >= 10240) {
    return WindowsTarget.w10;
  } else if (buildNumber >= 9600) {
    return WindowsTarget.w8_1;
  } else if (buildNumber >= 9200) {
    return WindowsTarget.w8;
  } else if (buildNumber >= 7601) {
    return WindowsTarget.w7;
  } else if (buildNumber >= 6002) {
    return WindowsTarget.vista;
  } else {
    // 最低支持版本
    return WindowsTarget.xp;
  }
}

/// 获取 windows 目标的 build number。
///
/// [注意]
/// 请把此函数与 `Platform.isWindows` 一起使用以做平台判断。
int getWindowsTargetBuildNumber() {
  return getWindowsTargetBuildNumber_();
}

/// 表示需要使用兼容 ui 模式。
///
/// [适用条件]
/// - Windows 7，使用无边框 ui 时窗口会溢出。
bool get kUseCompatibleUiMode =>
    isWindows &&
    const [WindowsTarget.w7].contains(windowsBuildNumber.windowsVersion);

bool get isWin10 => windowsBuildNumber.windowsVersion == WindowsTarget.w10;

class ServerConfig {
  late String idServer;
  late String relayServer;
  late String apiServer;
  late String key;

  ServerConfig(
      {String? idServer, String? relayServer, String? apiServer, String? key}) {
    this.idServer = idServer?.trim() ?? '';
    this.relayServer = relayServer?.trim() ?? '';
    this.apiServer = apiServer?.trim() ?? '';
    this.key = key?.trim() ?? '';
  }

  /// 从共享字符串解码（由用户分享或 rustdesk-server 生成）
  /// 另见 [encode]
  /// 解码失败时抛出异常
  ServerConfig.decode(String msg) {
    var json = {};
    try {
      // 向后兼容
      json = jsonDecode(msg);
    } catch (err) {
      final input = msg.split('').reversed.join('');
      final bytes = base64Decode(base64.normalize(input));
      json = jsonDecode(utf8.decode(bytes, allowMalformed: true));
    }
    idServer = json['host'] ?? '';
    relayServer = json['relay'] ?? '';
    apiServer = json['api'] ?? '';
    key = json['key'] ?? '';
  }

  /// 编码为共享字符串
  /// 另见 [ServerConfig.decode]
  String encode() {
    Map<String, String> config = {};
    config['host'] = idServer.trim();
    config['relay'] = relayServer.trim();
    config['api'] = apiServer.trim();
    config['key'] = key.trim();
    return base64UrlEncode(Uint8List.fromList(jsonEncode(config).codeUnits))
        .split('')
        .reversed
        .join();
  }

  /// 来自本地选项
  ServerConfig.fromOptions(Map<String, dynamic> options)
      : idServer = options['custom-rendezvous-server'] ?? "",
        relayServer = options['relay-server'] ?? "",
        apiServer = options['api-server'] ?? "",
        key = options['key'] ?? "";
}

Widget dialogButton(String text,
    {required VoidCallback? onPressed,
    bool isOutline = false,
    Widget? icon,
    TextStyle? style,
    ButtonStyle? buttonStyle}) {
  if (isDesktop || isWebDesktop) {
    if (isOutline) {
      return icon == null
          ? OutlinedButton(
              onPressed: onPressed,
              child: Text(translate(text), style: style),
            )
          : OutlinedButton.icon(
              icon: icon,
              onPressed: onPressed,
              label: Text(translate(text), style: style),
            );
    } else {
      return icon == null
          ? ElevatedButton(
              style: ElevatedButton.styleFrom(elevation: 0).merge(buttonStyle),
              onPressed: onPressed,
              child: Text(translate(text), style: style),
            )
          : ElevatedButton.icon(
              icon: icon,
              style: ElevatedButton.styleFrom(elevation: 0).merge(buttonStyle),
              onPressed: onPressed,
              label: Text(translate(text), style: style),
            );
    }
  } else {
    return TextButton(
      onPressed: onPressed,
      child: Text(
        translate(text),
        style: style,
      ),
    );
  }
}

int versionCmp(String v1, String v2) {
  return bind.versionToNumber(v: v1) - bind.versionToNumber(v: v2);
}

String getWindowName({WindowType? overrideType}) {
  final name = appName;
  switch (overrideType ?? kWindowType) {
    case WindowType.Main:
      return name;
    case WindowType.FileTransfer:
      return "File Transfer - $name";
    case WindowType.ViewCamera:
      return "View Camera - $name";
    case WindowType.PortForward:
      return "Port Forward - $name";
    case WindowType.RemoteDesktop:
      return "Remote Desktop - $name";
    default:
      break;
  }
  return name;
}

String getWindowNameWithId(String id, {WindowType? overrideType}) {
  return "${DesktopTab.tablabelGetter(id).value} - ${getWindowName(overrideType: overrideType)}";
}

Future<void> updateSystemWindowTheme() async {
  // 为 macOS 设置系统窗口主题。
  final userPreference = MyTheme.getThemeModePreference();
  if (userPreference != ThemeMode.system) {
    if (isMacOS) {
      await RdPlatformChannel.instance.changeSystemWindowTheme(
          userPreference == ThemeMode.light
              ? SystemWindowTheme.light
              : SystemWindowTheme.dark);
    }
  }
}

/// 仅 macOS
///
/// 注意：尚未找到基于 rust 的 AVFoundation 绑定的通用解决方案。
/// [AVFoundation] crate 存在编译错误。
const kMacOSPermChannel = MethodChannel("org.rustdesk.rustdesk/host");

enum PermissionAuthorizeType {
  undetermined,
  authorized,
  denied, // and restricted
}

Future<PermissionAuthorizeType> osxCanRecordAudio() async {
  int res = await kMacOSPermChannel.invokeMethod("canRecordAudio");
  print(res);
  if (res > 0) {
    return PermissionAuthorizeType.authorized;
  } else if (res == 0) {
    return PermissionAuthorizeType.undetermined;
  } else {
    return PermissionAuthorizeType.denied;
  }
}

Future<bool> osxRequestAudio() async {
  return await kMacOSPermChannel.invokeMethod("requestRecordAudio");
}

Widget futureBuilder(
    {required Future? future, required Widget Function(dynamic data) hasData}) {
  return FutureBuilder(
      future: future,
      builder: (BuildContext context, AsyncSnapshot snapshot) {
        if (snapshot.hasData) {
          return hasData(snapshot.data!);
        } else {
          if (snapshot.hasError) {
            debugPrint(snapshot.error.toString());
          }
          return Container();
        }
      });
}

void onCopyFingerprint(String value) {
  if (value.isNotEmpty) {
    Clipboard.setData(ClipboardData(text: value));
    showToast('$value\n${translate("Copied")}');
  } else {
    showToast(translate("no fingerprints"));
  }
}

void onCopyId(String value) {
  if (value.isNotEmpty) {
    Clipboard.setData(ClipboardData(text: value));
    showToast('$value\n${translate("Copied")}');
  } else {
    showToast(translate("Invalid ID"));
  }
}

Future<bool> callMainCheckSuperUserPermission() async {
  bool checked = await bind.mainCheckSuperUserPermission();
  if (isMacOS) {
    await windowManager.show();
  }
  return checked;
}

Future<void> start_service(bool is_start) async {
  bool checked = !bind.mainIsInstalled() ||
      !isMacOS ||
      await callMainCheckSuperUserPermission();
  if (checked) {
    mainSetBoolOption(kOptionStopService, !is_start);
  }
}

Future<bool> canBeBlocked() async {
  if (isWeb) {
    // Web 只能作为控制端，绝不能作为被控端，
    // 因此它不应被远程会话阻塞。
    return false;
  }
  // 先检查控制权限
  final controlPermission = await bind.mainGetCommon(
      key: "is-remote-modify-enabled-by-control-permissions");
  if (controlPermission == "true") {
    return false;
  } else if (controlPermission == "false") {
    return true;
  }

  // 检查本地设置
  var accessMode = await bind.mainGetOption(key: kOptionAccessMode);
  var isCustomAccessMode = accessMode != 'full' && accessMode != 'view';
  var option = option2bool(kOptionAllowRemoteConfigModification,
      await bind.mainGetOption(key: kOptionAllowRemoteConfigModification));
  return accessMode == 'view' || (isCustomAccessMode && !option);
}

// 待办：web 端尚未实现
Future<void> shouldBeBlocked(RxBool block, WhetherUseRemoteBlock? use) async {
  if (use != null && !await use()) {
    block.value = false;
    return;
  }
  var time0 = DateTime.now().millisecondsSinceEpoch;
  await bind.mainCheckMouseTime();
  Timer(const Duration(milliseconds: 120), () async {
    var d = time0 - await bind.mainGetMouseTime();
    if (d < 120) {
      block.value = true;
    } else {
      block.value = false;
    }
  });
}

typedef WhetherUseRemoteBlock = Future<bool> Function();
Widget buildRemoteBlock(
    {required Widget child,
    required RxBool block,
    required bool mask,
    WhetherUseRemoteBlock? use}) {
  return Obx(() => MouseRegion(
        onEnter: (_) async {
          await shouldBeBlocked(block, use);
        },
        onExit: (event) => block.value = false,
        child: Stack(children: [
          // 阻止作用范围仅限于当前 tab
          preventMouseKeyBuilder(child: child, block: block.value),
          // 遮罩层阻止点击，cm 不阻止点击，仍然使用 check_click_time 以避免阻止本地点击
          if (mask)
            Offstage(
                offstage: !block.value,
                child: Container(
                  color: Colors.black.withOpacity(0.5),
                )),
        ]),
      ));
}

Widget preventMouseKeyBuilder({required Widget child, required bool block}) {
  return ExcludeFocus(
      excluding: block, child: AbsorbPointer(child: child, absorbing: block));
}

Widget unreadMessageCountBuilder(RxInt? count,
    {double? size, double? fontSize}) {
  return Obx(() => Offstage(
      offstage: !((count?.value ?? 0) > 0),
      child: Container(
        width: size ?? 16,
        height: size ?? 16,
        decoration: BoxDecoration(
          color: Colors.red,
          shape: BoxShape.circle,
        ),
        child: Center(
          child: Text("${count?.value ?? 0}",
              maxLines: 1,
              style: TextStyle(color: Colors.white, fontSize: fontSize ?? 10)),
        ),
      )));
}

Widget unreadTopRightBuilder(RxInt? count, {Widget? icon}) {
  return Stack(
    children: [
      icon ?? Icon(Icons.chat),
      Positioned(
          top: 0,
          right: 0,
          child: unreadMessageCountBuilder(count, size: 12, fontSize: 8))
    ],
  );
}

String toCapitalized(String s) {
  if (s.isEmpty) {
    return s;
  }
  return s.substring(0, 1).toUpperCase() + s.substring(1);
}

Widget buildErrorBanner(BuildContext context,
    {required RxBool loading,
    required RxString err,
    required Function? retry,
    required Function close}) {
  return Obx(() => Offstage(
        offstage: !(!loading.value && err.value.isNotEmpty),
        child: Center(
            child: Container(
          color: MyTheme.color(context).errorBannerBg,
          child: Row(
            mainAxisAlignment: MainAxisAlignment.center,
            crossAxisAlignment: CrossAxisAlignment.center,
            children: [
              FittedBox(
                child: Icon(
                  Icons.info,
                  color: Color.fromARGB(255, 249, 81, 81),
                ),
              ).marginAll(4),
              Flexible(
                child: Align(
                    alignment: Alignment.centerLeft,
                    child: Tooltip(
                      message: translate(err.value),
                      child: SelectableText(
                        translate(err.value),
                      ),
                    )).marginSymmetric(vertical: 2),
              ),
              if (retry != null)
                InkWell(
                    onTap: () {
                      retry.call();
                    },
                    child: Text(
                      translate("Retry"),
                      style: TextStyle(color: MyTheme.accent),
                    )).marginSymmetric(horizontal: 5),
              FittedBox(
                child: InkWell(
                  onTap: () {
                    close.call();
                  },
                  child: Icon(Icons.close).marginSymmetric(horizontal: 5),
                ),
              ).marginAll(4)
            ],
          ),
        )).marginOnly(bottom: 14),
      ));
}

String getDesktopTabLabel(String peerId, String alias) {
  String label = alias.isEmpty ? peerId : alias;
  try {
    String peer = bind.mainGetPeerSync(id: peerId);
    Map<String, dynamic> config = jsonDecode(peer);
    if (config['info']['hostname'] is String) {
      String hostname = config['info']['hostname'];
      if (hostname.isNotEmpty &&
          !label.toLowerCase().contains(hostname.toLowerCase())) {
        label += "@$hostname";
      }
    }
  } catch (e) {
    debugPrint("Failed to get hostname:$e");
  }
  return label;
}

sessionRefreshVideo(SessionID sessionId, PeerInfo pi) async {
  if (pi.currentDisplay == kAllDisplayValue) {
    for (int i = 0; i < pi.displays.length; i++) {
      await bind.sessionRefresh(sessionId: sessionId, display: i);
    }
  } else {
    await bind.sessionRefresh(sessionId: sessionId, display: pi.currentDisplay);
  }
}

Future<List<Rect>> getScreenListWayland() async {
  final screenRectList = <Rect>[];
  if (isMainDesktopWindow) {
    for (var screen in await window_size.getScreenList()) {
      final scale = kIgnoreDpi ? 1.0 : screen.scaleFactor;
      double l = screen.frame.left;
      double t = screen.frame.top;
      double r = screen.frame.right;
      double b = screen.frame.bottom;
      final rect = Rect.fromLTRB(l / scale, t / scale, r / scale, b / scale);
      screenRectList.add(rect);
    }
  } else {
    final screenList = await rustDeskWinManager.call(
        WindowType.Main, kWindowGetScreenList, '');
    try {
      for (var screen in jsonDecode(screenList.result) as List<dynamic>) {
        final scale = kIgnoreDpi ? 1.0 : screen['scaleFactor'];
        double l = screen['frame']['l'];
        double t = screen['frame']['t'];
        double r = screen['frame']['r'];
        double b = screen['frame']['b'];
        final rect = Rect.fromLTRB(l / scale, t / scale, r / scale, b / scale);
        screenRectList.add(rect);
      }
    } catch (e) {
      debugPrint('Failed to parse screenList: $e');
    }
  }
  return screenRectList;
}

Future<List<Rect>> getScreenListNotWayland() async {
  final screenRectList = <Rect>[];
  final displays = bind.mainGetDisplays();
  if (displays.isEmpty) {
    return screenRectList;
  }
  try {
    for (var display in jsonDecode(displays) as List<dynamic>) {
      // 待办：缩放系数？
      // final scale = kIgnoreDpi ? 1.0 : screen.scaleFactor;
      double l = display['x'].toDouble();
      double t = display['y'].toDouble();
      double r = (display['x'] + display['w']).toDouble();
      double b = (display['y'] + display['h']).toDouble();
      screenRectList.add(Rect.fromLTRB(l, t, r, b));
    }
  } catch (e) {
    debugPrint('Failed to parse displays: $e');
  }
  return screenRectList;
}

Future<List<Rect>> getScreenRectList() async {
  return bind.mainCurrentIsWayland()
      ? await getScreenListWayland()
      : await getScreenListNotWayland();
}

openMonitorInTheSameTab(int i, FFI ffi, PeerInfo pi,
    {bool updateCursorPos = true, bool recordSelection = true}) {
  if (recordSelection) {
    ffi.ffiModel.lastUserDisplay = i;
    ffi.ffiModel.cancelPendingRestoreTimer();
    ffi.ffiModel.pendingMonitorRestore = null;
  }
  final displays = i == kAllDisplayValue
      ? List.generate(pi.displays.length, (index) => index)
      : [i];
  // 在从所有显示器切换之前，尝试清理图像模型
  // 1. 被控端有多个显示器。
  // 2. 不使用纹理渲染。
  // 3. 连接到显示器 1。
  // 4. 切换到多显示器 `kAllDisplayValue`
  // 5. 切换到显示器 2。
  // 这样远端页面一开始会显示显示器 1 的最后一张画面。
  if (pi.forceTextureRender && i != kAllDisplayValue) {
    ffi.imageModel.clearImage();
  }
  bind.sessionSwitchDisplay(
    isDesktop: isDesktop,
    sessionId: ffi.sessionId,
    value: Int32List.fromList(displays),
  );
  ffi.ffiModel.switchToNewDisplay(i, ffi.sessionId, ffi.id,
      updateCursorPos: updateCursorPos);
}

// 打开新标签页或窗口来显示该显示器。
// 目前只打开新窗口。
//
// screenRect 用于把新窗口移动到指定屏幕并设置为全屏。
openMonitorInNewTabOrWindow(int i, String peerId, PeerInfo pi,
    {Rect? screenRect}) {
  final args = {
    'window_id': stateGlobal.windowId,
    'peer_id': peerId,
    'display': i,
    'display_count': pi.displays.length,
    'window_type': (kWindowType ?? WindowType.RemoteDesktop).index,
  };
  if (screenRect != null) {
    args['screen_rect'] = {
      'l': screenRect.left,
      't': screenRect.top,
      'r': screenRect.right,
      'b': screenRect.bottom,
    };
  }
  DesktopMultiWindow.invokeMethod(
      kMainWindowId, kWindowEventOpenMonitorSession, jsonEncode(args));
}

setNewConnectWindowFrame(int windowId, String peerId, int preSessionCount,
    WindowType windowType, int? display, Rect? screenRect) async {
  if (screenRect == null) {
    // 如果之前已有会话，则不要为新连接恢复窗口位置。
    // https://github.com/rustdesk/rustdesk/discussions/8825
    if (preSessionCount == 0) {
      await restoreWindowPosition(windowType,
          windowId: windowId, display: display, peerId: peerId);
    }
  } else {
    await tryMoveToScreenAndSetFullscreen(screenRect);
  }
}

tryMoveToScreenAndSetFullscreen(Rect? screenRect) async {
  if (screenRect == null) {
    return;
  }
  final wc = WindowController.fromWindowId(stateGlobal.windowId);
  final curFrame = await wc.getFrame();
  final frame =
      Rect.fromLTWH(screenRect.left + 30, screenRect.top + 30, 600, 400);
  if (stateGlobal.fullscreen.isTrue &&
      curFrame.left <= frame.left &&
      curFrame.top <= frame.top &&
      curFrame.width >= frame.width &&
      curFrame.height >= frame.height) {
    return;
  }
  await wc.setFrame(frame);
  // 需要一段延时，以避免全屏后窗口又被恢复。
  Future.delayed(Duration(milliseconds: 300), () async {
    stateGlobal.setFullscreen(true);
  });
}

parseParamScreenRect(Map<String, dynamic> params) {
  Rect? screenRect;
  if (params['screen_rect'] != null) {
    double l = params['screen_rect']['l'];
    double t = params['screen_rect']['t'];
    double r = params['screen_rect']['r'];
    double b = params['screen_rect']['b'];
    screenRect = Rect.fromLTRB(l, t, r, b);
  }
  return screenRect;
}

get isInputSourceFlutter => stateGlobal.getInputSource() == "Input source 2";

class _CountDownButton extends StatefulWidget {
  _CountDownButton({
    Key? key,
    required this.text,
    required this.second,
    required this.onPressed,
    this.submitOnTimeout = false,
  }) : super(key: key);
  final String text;
  final VoidCallback? onPressed;
  final int second;
  final bool submitOnTimeout;

  @override
  State<_CountDownButton> createState() => _CountDownButtonState();
}

class _CountDownButtonState extends State<_CountDownButton> {
  late int _countdownSeconds = widget.second;

  Timer? _timer;

  @override
  void initState() {
    super.initState();
    _startCountdownTimer();
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  void _startCountdownTimer() {
    _timer = Timer.periodic(Duration(seconds: 1), (timer) {
      if (_countdownSeconds <= 0) {
        timer.cancel();
        if (widget.submitOnTimeout) {
          widget.onPressed?.call();
        }
      } else {
        setState(() {
          _countdownSeconds--;
        });
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    return dialogButton(
      '${translate(widget.text)} (${_countdownSeconds}s)',
      onPressed: widget.onPressed,
      isOutline: true,
    );
  }
}

importConfig(List<TextEditingController>? controllers, List<RxString>? errMsgs,
    String? text) {
  text = text?.trim();
  if (text != null && text.isNotEmpty) {
    try {
      final sc = ServerConfig.decode(text);
      if (isWeb || isIOS) {
        sc.relayServer = '';
      }
      if (sc.idServer.isNotEmpty) {
        Future<bool> success = setServerConfig(controllers, errMsgs, sc);
        success.then((value) {
          if (value) {
            showToast(translate('Import server configuration successfully'));
          } else {
            showToast(translate('Invalid server configuration'));
          }
        });
      } else {
        showToast(translate('Invalid server configuration'));
      }
      return sc;
    } catch (e) {
      showToast(translate('Invalid server configuration'));
    }
  } else {
    showToast(translate('Clipboard is empty'));
  }
}

Future<bool> setServerConfig(
  List<TextEditingController>? controllers,
  List<RxString>? errMsgs,
  ServerConfig config,
) async {
  String removeEndSlash(String input) {
    if (input.endsWith('/')) {
      return input.substring(0, input.length - 1);
    }
    return input;
  }

  config.idServer = removeEndSlash(config.idServer.trim());
  config.relayServer = removeEndSlash(config.relayServer.trim());
  config.apiServer = removeEndSlash(config.apiServer.trim());
  config.key = config.key.trim();
  if (controllers != null) {
    controllers[0].text = config.idServer;
    controllers[1].text = config.relayServer;
    controllers[2].text = config.apiServer;
    controllers[3].text = config.key;
  }
  // id
  if (config.idServer.isNotEmpty && errMsgs != null) {
    errMsgs[0].value = translate(await bind.mainTestIfValidServer(
        server: config.idServer, testWithProxy: true));
    if (errMsgs[0].isNotEmpty) {
      return false;
    }
  }
  // relay
  if (config.relayServer.isNotEmpty && errMsgs != null) {
    errMsgs[1].value = translate(await bind.mainTestIfValidServer(
        server: config.relayServer, testWithProxy: true));
    if (errMsgs[1].isNotEmpty) {
      return false;
    }
  }
  // api
  if (config.apiServer.isNotEmpty && errMsgs != null) {
    if (!config.apiServer.startsWith('http://') &&
        !config.apiServer.startsWith('https://')) {
      errMsgs[2].value =
          '${translate("API Server")}: ${translate("invalid_http")}';
      return false;
    }
  }
  final oldApiServer = await bind.mainGetApiServer();

  // 应该逐个设置
  await bind.mainSetOption(
      key: 'custom-rendezvous-server', value: config.idServer);
  await bind.mainSetOption(key: 'relay-server', value: config.relayServer);
  await bind.mainSetOption(key: 'api-server', value: config.apiServer);
  await bind.mainSetOption(key: 'key', value: config.key);
  final newApiServer = await bind.mainGetApiServer();
  if (oldApiServer.isNotEmpty &&
      oldApiServer != newApiServer &&
      gFFI.userModel.isLogin) {
    gFFI.userModel.logOut(apiServer: oldApiServer);
  }
  return true;
}

ColorFilter? svgColor(Color? color) {
  if (color == null) {
    return null;
  } else {
    return ColorFilter.mode(color, BlendMode.srcIn);
  }
}

// ignore: must_be_immutable
class ComboBox extends StatelessWidget {
  late final List<String> keys;
  late final List<String> values;
  late final String initialKey;
  late final Function(String key) onChanged;
  late final bool enabled;
  late String current;

  ComboBox({
    Key? key,
    required this.keys,
    required this.values,
    required this.initialKey,
    required this.onChanged,
    this.enabled = true,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    var index = keys.indexOf(initialKey);
    if (index < 0) {
      index = 0;
    }
    var ref = values[index].obs;
    current = keys[index];
    return Container(
      decoration: BoxDecoration(
        border: Border.all(
          color: enabled
              ? MyTheme.color(context).border2 ?? MyTheme.border
              : MyTheme.border,
        ),
        borderRadius:
            BorderRadius.circular(8), //border raiuds of dropdown button
      ),
      height: 42, // should be the height of a TextField
      child: Obx(() => DropdownButton<String>(
            isExpanded: true,
            value: ref.value,
            elevation: 16,
            underline: Container(),
            style: TextStyle(
                color: enabled
                    ? Theme.of(context).textTheme.titleMedium?.color
                    : disabledTextColor(context, enabled)),
            icon: const Icon(
              Icons.expand_more_sharp,
              size: 20,
            ).marginOnly(right: 15),
            onChanged: enabled
                ? (String? newValue) {
                    if (newValue != null && newValue != ref.value) {
                      ref.value = newValue;
                      current = newValue;
                      onChanged(keys[values.indexOf(newValue)]);
                    }
                  }
                : null,
            items: values.map<DropdownMenuItem<String>>((String value) {
              return DropdownMenuItem<String>(
                value: value,
                child: Text(
                  value,
                  style: const TextStyle(fontSize: 15),
                  overflow: TextOverflow.ellipsis,
                ).marginOnly(left: 15),
              );
            }).toList(),
          )),
    ).marginOnly(bottom: 5);
  }
}

Color? disabledTextColor(BuildContext context, bool enabled) {
  return enabled
      ? null
      : Theme.of(context).textTheme.titleLarge?.color?.withOpacity(0.6);
}

Widget loadPowered(BuildContext context) {
  if (bind.mainGetBuildinOption(key: "hide-powered-by-me") == 'Y') {
    return SizedBox.shrink();
  }
  return MouseRegion(
    cursor: SystemMouseCursors.click,
    child: GestureDetector(
      onTap: () {
        launchUrl(Uri.parse('https://rustdesk.com'));
      },
      child: Opacity(
          opacity: 0.5,
          child: Text(
            translate("powered_by_me"),
            overflow: TextOverflow.clip,
            style: Theme.of(context)
                .textTheme
                .bodySmall
                ?.copyWith(fontSize: 9, decoration: TextDecoration.underline),
          )),
    ),
  ).marginOnly(top: 6);
}

const _kDefaultLogoAsset = 'assets/logo.png';
const _kLightLogoAsset = 'assets/logo_light.png';
const _kDarkLogoAsset = 'assets/logo_dark.png';

List<String> _logoAssetCandidatesForBrightness(Brightness brightness) {
  return brightness == Brightness.dark
      ? [_kDarkLogoAsset, _kDefaultLogoAsset]
      : [_kLightLogoAsset, _kDefaultLogoAsset];
}

Future<String?> _resolveLogoAsset(Brightness brightness) async {
  for (final asset in _logoAssetCandidatesForBrightness(brightness)) {
    try {
      await rootBundle.load(asset);
      return asset;
    } on FlutterError {
      continue;
    }
  }
  return null;
}

class _Logo extends StatefulWidget {
  const _Logo();

  @override
  State<_Logo> createState() => _LogoState();
}

class _LogoState extends State<_Logo> {
  final Map<Brightness, Future<String?>> _logoFutures = {};

  Future<String?> _logoFutureFor(Brightness brightness) {
    return _logoFutures.putIfAbsent(
      brightness,
      () => _resolveLogoAsset(brightness),
    );
  }

  @override
  Widget build(BuildContext context) {
    return FutureBuilder<String?>(
      future: _logoFutureFor(Theme.of(context).brightness),
      builder: (BuildContext context, AsyncSnapshot<String?> snapshot) {
        final asset = snapshot.data;
        if (asset != null) {
          final image = Image.asset(
            asset,
            fit: BoxFit.contain,
            errorBuilder: (ctx, error, stackTrace) {
              return Container();
            },
          );
          return Container(
            constraints: BoxConstraints(maxWidth: 300, maxHeight: 60),
            child: image,
          ).marginOnly(left: 12, right: 12, top: 12);
        }
        return const Offstage();
      },
    );
  }
}

// 最大 300 x 60
Widget loadLogo() => const _Logo();

Widget loadIcon(double size) {
  return Image.asset('assets/icon.png',
      width: size,
      height: size,
      errorBuilder: (ctx, error, stackTrace) => SvgPicture.asset(
            'assets/icon.svg',
            width: size,
            height: size,
          ));
}

var imcomingOnlyHomeSize = Size(280, 300);
Size getIncomingOnlyHomeSize() {
  final magicWidth = isWindows ? 11.0 : 2.0;
  final magicHeight = 10.0;
  return imcomingOnlyHomeSize +
      Offset(magicWidth, kDesktopRemoteTabBarHeight + magicHeight);
}

Size getIncomingOnlySettingsSize() {
  return Size(768, 600);
}

bool isInHomePage() {
  final controller = Get.find<DesktopTabController>();
  return controller.state.value.selected == 0;
}

Widget _buildPresetPasswordWarning() {
  if (bind.mainGetBuildinOption(key: kOptionRemovePresetPasswordWarning) !=
      'N') {
    return SizedBox.shrink();
  }
  return Container(
    color: Colors.yellow,
    child: Column(
      children: [
        Align(
            child: Text(
          translate("Security Alert"),
          style: TextStyle(
            color: Colors.red,
            fontSize:
                18, // https://github.com/rustdesk/rustdesk-server-pro/issues/261
            fontWeight: FontWeight.bold,
          ),
        )).paddingOnly(bottom: 8),
        Text(
          translate("preset_password_warning"),
          style: TextStyle(color: Colors.red),
        )
      ],
    ).paddingAll(8),
  ); // Show a warning message if the Future completed with true
}

Widget buildPresetPasswordWarningMobile() {
  if (bind.isPresetPasswordMobileOnly()) {
    return _buildPresetPasswordWarning();
  } else {
    return SizedBox.shrink();
  }
}

Widget buildPresetPasswordWarning() {
  return FutureBuilder<bool>(
    future: bind.isPresetPassword(),
    builder: (BuildContext context, AsyncSnapshot<bool> snapshot) {
      if (snapshot.connectionState == ConnectionState.waiting) {
        return CircularProgressIndicator(); // Show a loading spinner while waiting for the Future to complete
      } else if (snapshot.hasError) {
        return Text(
            'Error: ${snapshot.error}'); // Show an error message if the Future completed with an error
      } else if (snapshot.hasData && snapshot.data == true) {
        return _buildPresetPasswordWarning();
      } else {
        return SizedBox
            .shrink(); // Show nothing if the Future completed with false or null
      }
    },
  );
}

// https://github.com/leanflutter/window_manager/blob/87dd7a50b4cb47a375b9fc697f05e56eea0a2ab3/lib/src/widgets/virtual_window_frame.dart#L44
Widget buildVirtualWindowFrame(BuildContext context, Widget child) {
  boxShadow() => isMainDesktopWindow
      ? <BoxShadow>[
          if (stateGlobal.fullscreen.isFalse || stateGlobal.isMaximized.isFalse)
            BoxShadow(
              color: Colors.black.withOpacity(0.1),
              offset: Offset(
                  0.0,
                  stateGlobal.isFocused.isTrue
                      ? kFrameBoxShadowOffsetFocused
                      : kFrameBoxShadowOffsetUnfocused),
              blurRadius: kFrameBoxShadowBlurRadius,
            ),
        ]
      : null;
  return Obx(
    () => Container(
      decoration: BoxDecoration(
        color: isMainDesktopWindow
            ? Colors.transparent
            : Theme.of(context).colorScheme.background,
        border: Border.all(
          color: Theme.of(context).dividerColor,
          width: stateGlobal.windowBorderWidth.value,
        ),
        borderRadius: BorderRadius.circular(
          (stateGlobal.fullscreen.isTrue || stateGlobal.isMaximized.isTrue)
              ? 0
              : kFrameBorderRadius,
        ),
        boxShadow: boxShadow(),
      ),
      child: ClipRRect(
        borderRadius: BorderRadius.circular(
          (stateGlobal.fullscreen.isTrue || stateGlobal.isMaximized.isTrue)
              ? 0
              : kFrameClipRRectBorderRadius,
        ),
        child: child,
      ),
    ),
  );
}

get windowResizeEdgeSize =>
    isLinux && !_linuxWindowResizable ? 0.0 : kWindowResizeEdgeSize;

// 在 Linux 上，`windowManager.setResizable(false)` 会先把窗口尺寸重置为默认尺寸，然后再设置为不可调整大小。
// 更多细节参见 _linuxWindowResizable。
// 因此我们使用 `setResizable()` 而不是 `windowManager.setResizable()`。
//
// 只有在 Linux 上需要默认尺寸时，才调用 `windowManager.setResizable(false)`。
setResizable(bool resizable) {
  if (isLinux) {
    _linuxWindowResizable = resizable;
    stateGlobal.refreshResizeEdgeSize();
  } else {
    windowManager.setResizable(resizable);
  }
}

isOptionFixed(String key) => bind.mainIsOptionFixed(key: key);

bool isChangePermanentPasswordDisabled() =>
    bind.mainGetBuildinOption(key: kOptionDisableChangePermanentPassword) ==
    'Y';

bool isChangeIdDisabled() =>
    bind.mainGetBuildinOption(key: kOptionDisableChangeId) == 'Y';

bool isUnlockPinDisabled() =>
    bind.mainGetBuildinOption(key: kOptionDisableUnlockPin) == 'Y';

bool? _isCustomClient;
bool get isCustomClient {
  _isCustomClient ??= bind.isCustomClient();
  return _isCustomClient!;
}

get defaultOptionLang => isCustomClient ? 'default' : '';
get defaultOptionTheme => isCustomClient ? 'system' : '';
get defaultOptionYes => isCustomClient ? 'Y' : '';
get defaultOptionNo => isCustomClient ? 'N' : '';
get defaultOptionWhitelist => isCustomClient ? ',' : '';
get defaultOptionAccessMode => isCustomClient ? 'custom' : '';
get defaultOptionApproveMode => isCustomClient ? 'password-click' : '';

bool whitelistNotEmpty() {
  // https://rustdesk.com/docs/en/self-host/client-configuration/advanced-settings/#whitelist
  final v = bind.mainGetOptionSync(key: kOptionWhitelist);
  return v != '' && v != ',';
}

bool idWhitelistNotEmpty() {
  final v = bind.mainGetOptionSync(key: kOptionIdWhitelist);
  return v != '' && v != ',';
}

// `setMovable()` 仅在 macOS 上受支持。
//
// 在 macOS 上，默认可以通过标签栏拖动窗口。
// 我们需要禁用可移动特性，以防窗口被标签栏中的标签页拖动。
//
// 拖动空白标签栏（而非标签页）时，通过添加 `onPanStart` 处理器来正常拖动窗口。
//
// 更多细节参见下面的代码：
// https://github.com/rustdesk/rustdesk/blob/ce1dac3b8613596b4d8ae981275f9335489eb935/flutter/lib/desktop/widgets/tabbar_widget.dart#L385
// https://github.com/rustdesk/rustdesk/blob/ce1dac3b8613596b4d8ae981275f9335489eb935/flutter/lib/desktop/widgets/tabbar_widget.dart#L399
//
// @platforms macos
disableWindowMovable(int? windowId) {
  if (!isMacOS) {
    return;
  }

  if (windowId == null) {
    windowManager.setMovable(false);
  } else {
    WindowController.fromWindowId(windowId).setMovable(false);
  }
}

Widget netWorkErrorWidget() {
  return Center(
      child: Column(
    mainAxisAlignment: MainAxisAlignment.center,
    crossAxisAlignment: CrossAxisAlignment.center,
    children: [
      if (!gFFI.userModel.networkErrorFromServer.value)
        Text(translate("network_error_tip")),
      ElevatedButton(
              onPressed: gFFI.userModel.refreshCurrentUser,
              child: Text(translate("Retry")))
          .marginSymmetric(vertical: 16),
      SelectableText(gFFI.userModel.networkError.value,
          style: TextStyle(fontSize: 11, color: Colors.red)),
    ],
  ));
}

List<ResizeEdge>? get windowManagerEnableResizeEdges => isWindows
    ? [
        ResizeEdge.topLeft,
        ResizeEdge.top,
        ResizeEdge.topRight,
      ]
    : null;

List<SubWindowResizeEdge>? get subWindowManagerEnableResizeEdges => isWindows
    ? [
        SubWindowResizeEdge.topLeft,
        SubWindowResizeEdge.top,
        SubWindowResizeEdge.topRight,
      ]
    : null;

void earlyAssert() {
  assert('\1' == '1');
}

void checkUpdate() {
  if (!isWeb) {
    if (!bind.isCustomClient()) {
      platformFFI.registerEventHandler(
          kCheckSoftwareUpdateFinish, kCheckSoftwareUpdateFinish,
          (Map<String, dynamic> evt) async {
        if (evt['url'] is String) {
          stateGlobal.updateUrl.value = evt['url'];
        }
      });
      Timer(const Duration(seconds: 1), () async {
        bind.mainGetSoftwareUpdateUrl();
      });
    }
  }
}

// https://github.com/flutter/flutter/issues/153560#issuecomment-2497160535
// 适用于 TextField、TextFormField
extension WorkaroundFreezeLinuxMint on Widget {
  Widget workaroundFreezeLinuxMint() {
    // 无需判断是否为 Linux Mint，因为这个变通方案在其他平台上也无害。
    if (isLinux) {
      return ExcludeSemantics(child: this);
    } else {
      return this;
    }
  }
}

// 这里不要使用 `extension`，我测试时用 `extension` 边框看起来很奇怪。
Widget workaroundWindowBorder(BuildContext context, Widget child) {
  if (!isWin10) {
    return child;
  }

  final isLight = Theme.of(context).brightness == Brightness.light;
  final borderColor = isLight ? Colors.black87 : Colors.grey;
  final width = isLight ? 0.5 : 0.1;

  getBorderWidget(Widget child) {
    return Obx(() =>
        (stateGlobal.isMaximized.isTrue || stateGlobal.fullscreen.isTrue)
            ? Offstage()
            : child);
  }

  final List<Widget> borders = [
    getBorderWidget(Container(
      color: borderColor,
      height: width + 0.1,
    ))
  ];
  if (kWindowType == WindowType.Main && !isLight) {
    borders.addAll([
      getBorderWidget(Align(
        alignment: Alignment.topLeft,
        child: Container(
          color: borderColor,
          width: width,
        ),
      )),
      getBorderWidget(Align(
        alignment: Alignment.topRight,
        child: Container(
          color: borderColor,
          width: width,
        ),
      )),
      getBorderWidget(Align(
        alignment: Alignment.bottomCenter,
        child: Container(
          color: borderColor,
          height: width,
        ),
      )),
    ]);
  }
  return Stack(
    children: [
      child,
      ...borders,
    ],
  );
}

void updateTextAndPreserveSelection(
    TextEditingController controller, String text) {
  // 目前只关心全选。
  final isSelected = controller.selection.isValid &&
      controller.selection.end > controller.selection.start;

  // 设置文本会使选区失效。
  controller.text = text;

  if (isSelected) {
    controller.selection = TextSelection(
        baseOffset: 0, extentOffset: controller.value.text.length);
  }
}

List<String> getPrinterNames() {
  final printerNamesJson = bind.mainGetPrinterNames();
  if (printerNamesJson.isEmpty) {
    return [];
  }
  try {
    final List<dynamic> printerNamesList = jsonDecode(printerNamesJson);
    final appPrinterName = '$appName Printer';
    return printerNamesList
        .map((e) => e.toString())
        .where((name) => name != appPrinterName)
        .toList();
  } catch (e) {
    debugPrint('failed to parse printer names, err: $e');
    return [];
  }
}

String _appName = '';
String get appName {
  if (_appName.isEmpty) {
    _appName = kPresetAppName.isNotEmpty ? kPresetAppName : bind.mainGetAppNameSync();
  }
  return _appName;
}

String getConnectionText(bool secure, bool direct, String streamType) {
  String connectionText;
  if (secure && direct) {
    connectionText = translate("Direct and encrypted connection");
  } else if (secure && !direct) {
    connectionText = translate("Relayed and encrypted connection");
  } else if (!secure && direct) {
    connectionText = translate("Direct and unencrypted connection");
  } else {
    connectionText = translate("Relayed and unencrypted connection");
  }
  if (streamType == 'Relay') {
    streamType = 'TCP';
  }
  if (streamType.isEmpty) {
    return connectionText;
  } else {
    return '$connectionText ($streamType)';
  }
}

String decode_http_response(http.Response resp) {
  try {
    // https://github.com/rustdesk/rustdesk-server-pro/discussions/758
    return utf8.decode(resp.bodyBytes, allowMalformed: true);
  } catch (e) {
    debugPrint('Failed to decode response as UTF-8: $e');
    // 回退到 bodyString，它会自动处理编码
    return resp.body;
  }
}

bool peerTabShowNote(PeerTabIndex peerTabIndex) {
  return peerTabIndex == PeerTabIndex.ab || peerTabIndex == PeerTabIndex.group;
}

// TODO: We should support individual bits combinations in the future.
// 不过目前先保持简单，因为旧代码只支持单个按钮。
// 目前还没有用户要求支持多按钮。
String mouseButtonsToPeer(int buttons) {
  switch (buttons) {
    case kPrimaryMouseButton:
      return 'left';
    case kSecondaryMouseButton:
      return 'right';
    case kMiddleMouseButton:
      return 'wheel';
    case kBackMouseButton:
      return 'back';
    case kForwardMouseButton:
      return 'forward';
    default:
      return '';
  }
}

/// 根据头像 URL 或 data URI 字符串构建头像组件。
/// 如果头像为空或无法解码，返回 [fallback]。
/// [borderRadius] 默认为 [size]/2（圆形）。
Widget? buildAvatarWidget({
  required String avatar,
  required double size,
  double? borderRadius,
  Widget? fallback,
}) {
  final trimmed = avatar.trim();
  if (trimmed.isEmpty) return fallback;

  ImageProvider? imageProvider;
  if (trimmed.startsWith('data:image/')) {
    final comma = trimmed.indexOf(',');
    if (comma > 0) {
      try {
        imageProvider = MemoryImage(base64Decode(trimmed.substring(comma + 1)));
      } catch (_) {}
    }
  } else if (trimmed.startsWith('http://') || trimmed.startsWith('https://')) {
    imageProvider = NetworkImage(trimmed);
  }

  if (imageProvider == null) return fallback;

  final radius = borderRadius ?? size / 2;
  return ClipRRect(
    borderRadius: BorderRadius.circular(radius),
    child: Image(
      image: imageProvider,
      width: size,
      height: size,
      fit: BoxFit.cover,
      errorBuilder: (_, __, ___) => fallback ?? SizedBox.shrink(),
    ),
  );
}
