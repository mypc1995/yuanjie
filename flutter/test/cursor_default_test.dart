import 'dart:io';

import 'package:flutter/services.dart';
import 'package:flutter_custom_cursor/flutter_custom_cursor.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/generated_bridge.dart' show CursorShape;
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_hbb/native/custom_cursor.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:uuid/uuid.dart';

// 单独一个文件：在 model 之前，这个 isolate 中没有其他东西创建过默认光标。

/// 一个不保留任何图形的 core。
class _Cursor extends CursorModel {
  _Cursor(FFI ffi) : super(WeakReference(ffi));
  @override
  Future<CursorShape?> fetchCursorShape(String id) => Future.value(null);
  @override
  bool get showsRemoteCursor => false;
}

class _FFI extends Fake implements FFI {
  _FFI() {
    ffiModel = FfiModel(WeakReference(this));
    cursorModel = _Cursor(this);
  }
  @override
  final SessionID sessionId = const Uuid().v4obj();
  @override
  late final FfiModel ffiModel;
  @override
  late final CursorModel cursorModel;
}

String? _key(MouseCursor cursor) =>
    cursor is FlutterCustomMemoryImageCursor ? cursor.key : null;

void main() {
  final binding = TestWidgetsFlutterBinding.ensureInitialized();
  final channel = Platform.isWindows
      ? SystemChannels.mouseCursor
      : const MethodChannel('flutter_custom_cursor');
  binding.defaultBinaryMessenger.setMockMethodCallHandler(channel,
      (call) async => (call.arguments as Map<dynamic, dynamic>)['name']);

  test('a shape the core cannot give shows the default cursor at once',
      () async {
    final ffi = _FFI();
    final cursor = ffi.cursorModel;
    final pixels = Uint8List.fromList(List.generate(8 * 8 * 4, (i) => i % 256));
    await ffi.ffiModel.handleCursorData('1', 0, 0, 8, 8, pixels);
    final shown = _key(buildCursorOfCache(cursor, 1.0, cursor.cache));
    final evt = {'id': '2'};
    ffi.ffiModel.updateLastCursorId(evt);
    ffi.ffiModel.handleCursorId(evt);
    // 不是等待默认光标本身：请求它反而会把它创建出来。
    await Future<void>.delayed(const Duration(milliseconds: 500));
    expect(cursor.cache, isNull);
    final built = _key(buildCursorOfCache(
        cursor, 1.0, cursor.cache ?? preDefaultCursor.cache));
    expect(built, isNot(shown));
    expect(built, contains('_${kPreDefaultCursorId}_'));
    cursor.disposeImages();
  });
}
