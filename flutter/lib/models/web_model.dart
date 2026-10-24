// ignore_for_file: avoid_web_libraries_in_flutter

import 'dart:convert';
import 'dart:js_interop';
import 'dart:js_interop_unsafe';
import 'dart:typed_data';
import 'dart:js';
import 'dart:html';
import 'dart:async';
import 'dart:ui' as ui;
import 'dart:ui_web' as ui_web;

import 'package:flutter/foundation.dart';
import 'package:flutter_hbb/common/widgets/login.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:flutter_hbb/models/web_video_frame_queue.dart';

import 'package:flutter_hbb/web/bridge.dart';
import 'package:flutter_hbb/common.dart';
import 'package:uuid/uuid.dart';

final List<StreamSubscription<MouseEvent>> mouseListeners = [];
final List<StreamSubscription<KeyboardEvent>> keyListeners = [];

// 从 js/src/webcodecs.js 传过来的 WebCodecs VideoFrames 是以普通的
// 互操作对象形式到达的（该包的 language 版本早于 extension type）。
// 这一侧拥有每个帧，必须快速关闭它：硬件解码器
// 一旦其较小的输出帧池耗尽就会卡住。
int _videoFrameWidth(JSObject frame) =>
    frame.getProperty<JSNumber>('displayWidth'.toJS).toDartInt;
int _videoFrameHeight(JSObject frame) =>
    frame.getProperty<JSNumber>('displayHeight'.toJS).toDartInt;
void _closeVideoFrame(JSObject frame) {
  try {
    frame.callMethod<JSAny?>('close'.toJS);
  } catch (error) {
    debugPrint('VideoFrame.close failed: $error');
  }
}

typedef HandleEvent = Future<void> Function(Map<String, dynamic> evt);

class PlatformFFI {
  final _eventHandlers = <String, Map<String, HandleEvent>>{};
  final RustdeskImpl _ffiBind = RustdeskImpl();

  static String getByName(String name, [String arg = '']) {
    return context.callMethod('getByName', [name, arg]);
  }

  static void setByName(String name, [String value = '']) {
    context.callMethod('setByName', [name, value]);
  }

  PlatformFFI._() {
    _videoFrameQueue = WebVideoFrameQueue(
      importFrame: _importVideoFrame,
      closeFrame: _closeVideoFrame,
      disposeImage: (image) => image.dispose(),
      onImportError: _handleVideoFrameImportError,
      onCallbackError: _handleVideoImageCallbackError,
    );
    window.document.addEventListener(
        'visibilitychange',
        (event) => {
              stateGlobal.isWebVisible =
                  window.document.visibilityState == 'visible'
            });
  }

  static final PlatformFFI instance = PlatformFFI._();

  static get localeName => window.navigator.language;
  RustdeskImpl get ffiBind => _ffiBind;

  static Future<String> getVersion() async {
    throw UnimplementedError();
  }

  bool registerEventHandler(
      String eventName, String handlerName, HandleEvent handler,
      {bool replace = false}) {
    debugPrint('registerEventHandler $eventName $handlerName');
    var handlers = _eventHandlers[eventName];
    if (handlers == null) {
      _eventHandlers[eventName] = {handlerName: handler};
      return true;
    } else {
      if (!replace && handlers.containsKey(handlerName)) {
        return false;
      } else {
        handlers[handlerName] = handler;
        return true;
      }
    }
  }

  void unregisterEventHandler(String eventName, String handlerName) {
    debugPrint('unregisterEventHandler $eventName $handlerName');
    var handlers = _eventHandlers[eventName];
    if (handlers != null) {
      handlers.remove(handlerName);
    }
  }

  Future<bool> tryHandle(Map<String, dynamic> evt) async {
    final name = evt['name'];
    if (name != null) {
      final handlers = _eventHandlers[name];
      if (handlers != null) {
        if (handlers.isNotEmpty) {
          for (var handler in handlers.values) {
            await handler(evt);
          }
          return true;
        }
      }
    }
    return false;
  }

  String translate(String name, String locale) =>
      _ffiBind.translate(name: name, locale: locale);

  Uint8List? getRgba(SessionID sessionId, int display, int bufSize) {
    throw UnimplementedError();
  }

  int getRgbaSize(SessionID sessionId, int display) =>
      _ffiBind.sessionGetRgbaSize(sessionId: sessionId, display: display);
  void nextRgba(SessionID sessionId, int display) =>
      _ffiBind.sessionNextRgba(sessionId: sessionId, display: display);
  void registerPixelbufferTexture(SessionID sessionId, int display, int ptr) =>
      _ffiBind.sessionRegisterPixelbufferTexture(
          sessionId: sessionId, display: display, ptr: ptr);
  void registerGpuTexture(SessionID sessionId, int display, int ptr) =>
      _ffiBind.sessionRegisterGpuTexture(
          sessionId: sessionId, display: display, ptr: ptr);

  Future<void> init(String appType) async {
    Completer completer = Completer();
    context["onInitFinished"] = () {
      completer.complete();
    };
    context['dialog'] = (type, title, text) {
      final uuid = Uuid();
      msgBox(SessionID(uuid.v4()), type, title, text, '', gFFI.dialogManager);
    };
    context['loginDialog'] = () {
      loginDialog();
    };
    context['closeConnection'] = () {
      gFFI.dialogManager.dismissAll();
      closeConnection();
    };
    await _ffiBind.mainInit(appDir: '');
    context.callMethod('init');
    version = getByName('version');
    window.onContextMenu.listen((event) {
      event.preventDefault();
    });

    context['onRegisteredEvent'] = (String message) {
      try {
        Map<String, dynamic> event = json.decode(message);
        tryHandle(event);
      } catch (e) {
        print('json.decode fail(): $e');
      }
    };
    return completer.future;
  }

  void setEventCallback(void Function(Map<String, dynamic>) fun) {
    context["onGlobalEvent"] = (String message) {
      try {
        Map<String, dynamic> event = json.decode(message);
        fun(event);
      } catch (e) {
        print('json.decode fail(): $e');
      }
    };
  }

  void setRgbaCallback(void Function(int, Uint8List) fun) {
    context["onRgba"] = (int display, Uint8List? rgba) {
      if (rgba != null) {
        fun(display, rgba);
      }
    };
  }

  // Web 核心会像对帧调用 onRgba 一样，用字节形式传入图形各通道的像素调用此方法。
  void setCursorDataCallback(
      void Function(String, int, int, int, int, Uint8List) fun) {
    context["onCursorData"] = (String id, int hotx, int hoty, int width,
        int height, Uint8List? rgba) {
      if (rgba != null) {
        fun(id, hotx, hoty, width, height, rgba);
      }
    };
  }

  late final WebVideoFrameQueue<JSObject, ui.Image> _videoFrameQueue;

  // 零回读视频路径：JS 解码器把解码后的 VideoFrames 传到这里
  // （每帧之前都会检查 typeof window.onVideoFrame），引擎
  // 通过 createImageBitmap 在 GPU 之间直接导入它们。注销该 JS 全局变量
  // 会让 JS 侧退回 RGBA 回读路径。
  void setVideoFrameCallback(
      Future<void> Function(int, ui.Image, bool Function()) fun) {
    _videoFrameQueue.beginSession(fun);
    if (!_videoFrameQueue.isEnabled) return;
    globalContext.setProperty(
      'onVideoFrame'.toJS,
      ((JSNumber display, JSObject frame) {
        _videoFrameQueue.submit(display.toDartInt, frame);
      }).toJS,
    );
  }

  void clearVideoFrameCallback() {
    _videoFrameQueue.endSession();
    globalContext.setProperty('onVideoFrame'.toJS, null);
  }

  Future<ui.Image> _importVideoFrame(JSObject frame) async {
    return await ui_web.createImageFromTextureSource(frame,
        width: _videoFrameWidth(frame), height: _videoFrameHeight(frame));
  }

  void _handleVideoFrameImportError(Object error, StackTrace stackTrace) {
    debugPrintStack(
        label: 'createImageFromTextureSource failed, using RGBA path: $error',
        stackTrace: stackTrace);
    globalContext.setProperty('onVideoFrame'.toJS, null);
  }

  void _handleVideoImageCallbackError(Object error, StackTrace stackTrace) {
    debugPrintStack(
        label: 'video image callback error: $error', stackTrace: stackTrace);
  }

  void startDesktopWebListener() {
    mouseListeners.add(
        window.document.onContextMenu.listen((evt) => evt.preventDefault()));
  }

  void stopDesktopWebListener() {
    for (var ml in mouseListeners) {
      ml.cancel();
    }
    mouseListeners.clear();
    for (var kl in keyListeners) {
      kl.cancel();
    }
    keyListeners.clear();
  }

  void setMethodCallHandler(FMethod callback) {}

  invokeMethod(String method, [dynamic arguments]) async {
    return true;
  }

  Future<T?> invokeMethodWithResult<T>(String method,
      [dynamic arguments]) async {
    return null;
  }

  // 仅为编译通过而存在
  void syncAndroidServiceAppDirConfigPath() {}

  void setFullscreenCallback(void Function(bool) fun) {
    context["onFullscreenChanged"] = (bool v) {
      fun(v);
    };
  }
}
