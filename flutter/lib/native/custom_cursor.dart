import 'package:flutter_custom_cursor/cursor_manager.dart'
    as custom_cursor_manager;
import 'package:flutter_custom_cursor/flutter_custom_cursor.dart';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import 'package:flutter_hbb/common.dart' show isLinux;
import 'package:flutter_hbb/models/model.dart';
import 'package:image/image.dart' as img;

deleteCustomCursor(String key) =>
    custom_cursor_manager.CursorManager.instance.deleteCursor(key);
resetSystemCursor() {}

MouseCursor buildCursorOfCache(
    CursorModel cursor, double scale, CursorData? cache) {
  if (cache == null) {
    return _shownCursor(cursor);
  } else {
    final key = cursor.nativeKey(cache, scale);
    if (!cursor.cachedKeys.contains(key)) {
      // 数据应在此处检查，因为它可能在 `updateGetKey()` 之后被改变
      final data = cache.data;
      if (data == null) {
        cursor.restorePixels(cache.id);
        return _shownCursor(cursor);
      }
      // 正方形画布可以避免在 Linux 上出现裁剪或边缘杂散像素。
      final width = isLinux && cache.rasterWidth < cache.rasterHeight
          ? cache.rasterHeight
          : cache.rasterWidth;
      final height = isLinux ? width : cache.rasterHeight;
      debugPrint(
          "Register custom cursor with key $key (${cache.hotx},${cache.hoty})");
      // [安全性]
      // 在当前同步上下文中调用异步的 registerCursor 是可以的，
      // 因为激活光标同样是异步调用，并且总会
      // 在此之后才执行。
      custom_cursor_manager.CursorManager.instance
          .registerCursor(custom_cursor_manager.CursorData()
            ..name = key
            ..buffer =
                width == cache.rasterWidth && height == cache.rasterHeight
                    ? data
                    : _padCursor(data, width)
            ..width = width
            ..height = height
            ..hotX = cache.hotx
            ..hotY = cache.hoty);
      cursor.addKey(key);
      cursor.registered(cache, key);
    }
    cursor.shown(cache, key);
    return FlutterCustomMemoryImageCursor(key: key);
  }
}

// 在生成当前使用的光标期间，保持的是上一个图形的光标，而不是系统光标。
MouseCursor _shownCursor(CursorModel cursor) {
  final key = cursor.shownKey;
  return key == null
      ? MouseCursor.defer
      : FlutterCustomMemoryImageCursor(key: key);
}

Uint8List _padCursor(Uint8List data, int size) {
  final bitmap = img.decodePng(data);
  if (bitmap == null) {
    throw const FormatException('Invalid native cursor PNG');
  }
  final padded = img.copyExpandCanvas(bitmap,
      newWidth: size,
      newHeight: size,
      position: img.ExpandCanvasPosition.topLeft,
      toImage: img.Image(width: size, height: size, numChannels: 4));
  return Uint8List.fromList(img.encodePng(padded));
}
