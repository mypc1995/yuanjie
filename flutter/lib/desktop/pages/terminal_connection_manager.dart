import 'package:flutter/foundation.dart';
import 'package:get/get.dart';
import '../../models/model.dart';

/// 管理终端连接，确保每个 peer 只有一个 FFI 实例
class TerminalConnectionManager {
  static final Map<String, FFI> _connections = {};
  static final Map<String, int> _connectionRefCount = {};
  
  // 按 peer 跟踪 service ID
  static final Map<String, String> _serviceIds = {};

  /// 获取或创建某个 peer 的 FFI 实例
  static FFI getConnection({
    required String peerId,
    required String? password,
    required bool? isSharedPassword,
    required bool? forceRelay,
    required String? connToken,
  }) {
    final existingFfi = _connections[peerId];
    if (existingFfi != null && !existingFfi.closed) {
      // 增加引用计数
      _connectionRefCount[peerId] = (_connectionRefCount[peerId] ?? 0) + 1;
      debugPrint('[TerminalConnectionManager] Reusing existing connection for peer $peerId. Reference count: ${_connectionRefCount[peerId]}');
      return existingFfi;
    }

    // 为第一个终端创建新的 FFI 实例
    debugPrint('[TerminalConnectionManager] Creating new terminal connection for peer $peerId');
    final ffi = FFI(null);
    ffi.start(
      peerId,
      password: password,
      isSharedPassword: isSharedPassword,
      forceRelay: forceRelay,
      connToken: connToken,
      isTerminal: true,
    );
    
    _connections[peerId] = ffi;
    _connectionRefCount[peerId] = 1;
    
    // 用 Get 注册该 FFI 实例以进行依赖注入
    Get.put<FFI>(ffi, tag: 'terminal_$peerId');
    
    debugPrint('[TerminalConnectionManager] New connection created. Total connections: ${_connections.length}');
    return ffi;
  }

  /// 释放一个连接引用
  static void releaseConnection(String peerId) {
    final refCount = _connectionRefCount[peerId] ?? 0;
    debugPrint('[TerminalConnectionManager] Releasing connection for peer $peerId. Current ref count: $refCount');
    
    if (refCount <= 1) {
      // 最后一个引用，关闭连接
      final ffi = _connections[peerId];
      if (ffi != null) {
        debugPrint('[TerminalConnectionManager] Closing connection for peer $peerId (last reference)');
        ffi.close();
        _connections.remove(peerId);
        _connectionRefCount.remove(peerId);
        Get.delete<FFI>(tag: 'terminal_$peerId');
      }
    } else {
      // 减少引用计数
      _connectionRefCount[peerId] = refCount - 1;
      debugPrint('[TerminalConnectionManager] Connection still in use. New ref count: ${_connectionRefCount[peerId]}');
    }
  }

  /// 检查某个 peer 是否存在连接
  static bool hasConnection(String peerId) {
    final ffi = _connections[peerId];
    return ffi != null && !ffi.closed;
  }
  
  /// 获取已有连接，不创建新的
  static FFI? getExistingConnection(String peerId) {
    return _connections[peerId];
  }

  /// 获取连接数，用于调试
  static int getConnectionCount() => _connections.length;
  
  /// 获取某个 peer 的终端数
  static int getTerminalCount(String peerId) => _connectionRefCount[peerId] ?? 0;
  
  /// 获取某个 peer 的 service ID
  static String? getServiceId(String peerId) => _serviceIds[peerId];
  
  /// 设置某个 peer 的 service ID
  static void setServiceId(String peerId, String serviceId) {
    _serviceIds[peerId] = serviceId;
    debugPrint('[TerminalConnectionManager] Service ID for $peerId: $serviceId');
  }
}