import 'dart:async';

import 'package:flutter/foundation.dart';

typedef EventCallback<Data> = Future<dynamic> Function(Data data);

abstract class BaseEvent<EventType, Data> {
  EventType type;
  Data data;

  /// 构造函数。
  BaseEvent(this.type, this.data);

  /// 消费该事件。
  @visibleForTesting
  Future<dynamic> consume() async {
    final cb = findCallback(type);
    if (cb == null) {
      return null;
    } else {
      return cb(data);
    }
  }

  EventCallback<Data>? findCallback(EventType type);
}

abstract class BaseEventLoop<EventType, Data> {
  final List<BaseEvent<EventType, Data>> _evts = [];
  Timer? _timer;

  List<BaseEvent<EventType, Data>> get evts => _evts;

  Future<void> onReady() async {
    // 每 100ms 轮询一次。
    _timer = Timer.periodic(Duration(milliseconds: 100), _handleTimer);
  }

  /// 某个事件即将被消费。
  Future<void> onPreConsume(BaseEvent<EventType, Data> evt) async {}
  /// 某个事件已被消费。
  Future<void> onPostConsume(BaseEvent<EventType, Data> evt) async {}
  /// 所有事件都已处理并清空。
  Future<void> onEventsClear() async {}
  /// 开始消费事件。
  Future<void> onEventsStartConsuming() async {}

  Future<void> _handleTimer(Timer timer) async {
      if (_evts.isEmpty) {
        return;
      }
      timer.cancel();
      _timer = null;
      // 处理逻辑。
      await onEventsStartConsuming();
      while (_evts.isNotEmpty) {
        final evt = _evts.first;
        _evts.remove(evt);
        await onPreConsume(evt);
        await evt.consume();
        await onPostConsume(evt);
      }
      await onEventsClear();
      // 现在所有事件都已处理完毕。
      _timer = Timer.periodic(Duration(milliseconds: 100), _handleTimer);
  }

  Future<void> close() async {
    _timer?.cancel();
  }

  void pushEvent(BaseEvent<EventType, Data> evt) {
    _evts.add(evt);
  }

  void clear() {
    _evts.clear();
  }
}
