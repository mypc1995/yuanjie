import 'dart:js' as js;
// 循环导入，也许我们可以改进这一点
import 'package:flutter_hbb/consts.dart';

final isAndroid_ = false;
final isIOS_ = false;
final isWindows_ = false;
final isMacOS_ = false;
final isLinux_ = false;
final isWeb_ = true;
final isWebDesktop_ = !js.context.callMethod('isMobile');

final isDesktop_ = false;

final _localOs = js.context.callMethod('getByName', ['local_os', '']);
final isWebOnWindows_ = _localOs == kPeerPlatformWindows;
final isWebOnLinux_ = _localOs == kPeerPlatformLinux;
final isWebOnMacOS_ = _localOs == kPeerPlatformMacOS;
