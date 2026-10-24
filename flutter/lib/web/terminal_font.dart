import 'dart:html' as html;
import 'dart:js' as js;
import 'dart:typed_data';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

bool _loadRequested = false;

/// 当 Google CDN 不可达时，`index.html` 会设置
/// `window.rustdeskLocalFonts`，而 `GoogleFonts.robotoMono()` 无法下载
/// 终端字体。此时改为加载随 Web 应用打包的那份副本，
/// 它以 google_fonts 提供的字体族名注册，终端的
/// TextStyle（'RobotoMono_regular'）使用该字体。
Future<void> loadLocalTerminalFontIfNeeded() async {
  if (_loadRequested || js.context['rustdeskLocalFonts'] != true) {
    return;
  }
  _loadRequested = true;
  try {
    final req = await html.HttpRequest.request(
      'fonts/RobotoMono-Regular.ttf',
      responseType: 'arraybuffer',
    );
    final data = ByteData.view(req.response as ByteBuffer);
    final loader = FontLoader('RobotoMono_regular')
      ..addFont(Future.value(data));
    await loader.load();
  } catch (e) {
    _loadRequested = false;
    debugPrint('Failed to load bundled Roboto Mono: $e');
  }
}
