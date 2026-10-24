import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:uuid/uuid.dart';

/// 把自定义缩放百分比钳制到支持的范围内。
/// 请与桌面端工具栏 UI 中滑块的最小值保持一致。
///
/// 此函数用于确保整个应用中的钳制行为一致，
/// 并为有效缩放范围提供唯一的参照。
int clampCustomScalePercent(int percent) {
  return percent.clamp(kScaleCustomMinPercent, kScaleCustomMaxPercent);
}

/// 解析百分比字符串并钳制。无效时默认为 100。
int parseCustomScalePercent(String? s, {int defaultPercent = 100}) {
  final parsed = int.tryParse(s ?? '') ?? defaultPercent;
  return clampCustomScalePercent(parsed);
}

/// 钳制后把百分比值转换为缩放系数。
double percentToScale(int percent) => clampCustomScalePercent(percent) / 100.0;

/// 获取、解析并钳制某个会话的自定义缩放百分比。
Future<int> getSessionCustomScalePercent(UuidValue sessionId) async {
  final opt = await bind.sessionGetFlutterOption(
      sessionId: sessionId, k: kCustomScalePercentKey);
  return parseCustomScalePercent(opt);
}

/// 获取并计算某个会话的自定义缩放系数。
Future<double> getSessionCustomScale(UuidValue sessionId) async {
  final p = await getSessionCustomScalePercent(sessionId);
  return percentToScale(p);
}
