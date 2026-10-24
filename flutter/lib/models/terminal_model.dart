import 'dart:async';
import 'dart:convert';
import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/main.dart';
import 'package:xterm/xterm.dart';

import 'input_modifier_utils.dart';
import 'model.dart';
import 'platform_model.dart';
import 'rustdesk_terminal.dart';
import 'terminal_copy_shortcut.dart';
import 'terminal_mouse_handler.dart';

bool canConfigureTerminalClipboardPermission({
  required bool settingsDisabled,
  required bool optionFixed,
}) =>
    !settingsDisabled && !optionFixed;

bool canHandleTerminalClipboardWriteRequest({
  required String localOption,
  required bool canConfigurePermission,
}) =>
    canConfigurePermission || localOption == kTerminalClipboardWriteAllowed;

TerminalClipboardWritePermission terminalClipboardWritePermission(
  String localOption, {
  required bool remoteClipboardEnabled,
  bool canRequestConsent = true,
}) {
  if (!remoteClipboardEnabled) {
    return TerminalClipboardWritePermission.denied;
  }
  if (localOption == kTerminalClipboardWriteAllowed) {
    return TerminalClipboardWritePermission.allowed;
  }
  if (localOption == kTerminalClipboardWriteUnconfigured && canRequestConsent) {
    return TerminalClipboardWritePermission.unconfigured;
  }
  return TerminalClipboardWritePermission.denied;
}

class TerminalModel with ChangeNotifier {
  final String id; // peer id
  final FFI parent;
  final int terminalId;
  late final Terminal terminal;
  late final TerminalController terminalController;

  bool _terminalOpened = false;
  bool get terminalOpened => _terminalOpened;

  bool _disposed = false;

  /// 用于检查 Ctrl 修饰键锁定当前是否处于激活状态的回调。
  /// 激活时，键盘输入会被映射为控制码（例如 'b' → \x02）。
  bool Function()? isCtrlLocked;

  /// 在按下某个键后清除 Ctrl 锁定的回调（单次触发模式）。
  void Function()? clearCtrlLock;

  /// 用于检查 Alt 修饰键锁定当前是否处于激活状态的回调。
  bool Function()? isAltLocked;

  /// 在按下某个键后清除 Alt 锁定的回调（单次触发模式）。
  void Function()? clearAltLock;

  final _inputBuffer = <String>[];

  // 在终端视图获得有效尺寸之前接收到的输出数据的缓冲区。
  // 这样可以避免在布局完成之前写入终端时出现 NaN 错误。
  final _pendingOutputChunks = <String>[];
  final _pendingOutputSuppressFlags = <bool>[];
  int _pendingOutputSize = 0;
  static const int _kMaxOutputBufferChars = 8 * 1024;
  // 视图就绪状态：当终端具有有效尺寸时为 true，此时可以安全写入
  bool _terminalViewReady = false;
  bool _markViewReadyScheduled = false;
  bool _suppressTerminalOutput = false;
  bool _suppressNextTerminalDataOutput = false;

  void Function(int w, int h, int pw, int ph)? onResizeExternal;

  /// 在终端会话结束（shell 退出）时调用。
  /// 监听者（通常是 TerminalPage）可据此自动关闭标签页/页面。
  VoidCallback? onClosed;

  ValueChanged<String>? onClipboardWriteBlocked;
  ValueChanged<String>? onClipboardWriteSucceeded;

  Future<void> _handleInput(String data) async {
    // 在 Flutter 页面已经消失之后，xterm 仍可能完成异步输入
    // 。在读取或清除由 widget 持有的修饰键状态之前应先停止。
    if (_disposed) return;

    // 软键盘（尤其是 iOS）按下回车时会输出 '\n'，而
    // 真实键盘的回车会发送 '\r'。部分 Android 键盘同样会输出 '\n'。
    // - 对端是 Windows：'\r' 可用，'\n' 只是一个换行。
    // - 对端是 Linux：规范模式（canonical mode）的 shell 两者都接受，但原始模式（raw mode）的应用
    //   （readline、prompt_toolkit、vim、TUI 框架）期望的是 '\r'。
    // - 对端是 macOS：与 Linux 相同，原始模式的应用期望 '\r'
    //   （https://github.com/rustdesk/rustdesk/issues/14907）。
    // 因此在移动端 / Web 移动端，要把原本单独的 '\n' 归一化为 '\r'，
    // 并且这一步要放在修饰键映射之前。这样 Ctrl+J 仍然映射为 LF，而不是
    // 事后再把生成的控制码改写成 CR。
    // 多字符的键盘负载（例如终端转义序列）
    // 保持不变。粘贴输入走单独的预处理路径。
    final ctrlLocked = isCtrlLocked?.call() ?? false;
    final altLocked = isAltLocked?.call() ?? false;
    final modifiersActive = ctrlLocked || altLocked;
    // 变换和消费使用同一个判定条件。控制键
    // 和转义序列不得悄悄消费掉待触发的单次锁定。
    final shouldConsumeModifiers =
        modifiersActive && shouldApplyTerminalInputModifiers(data);
    data = prepareTerminalInputPayload(
      data,
      // IME 软键盘的粘贴提示目前从 xterm 以普通
      // 文本输入的形式到达，没有粘贴来源元数据。让它们继续走键盘路径；
      // 基于剪贴板内容的启发式判断可能把普通输入误判为粘贴。
      source: TerminalInputSource.keyboard,
      isMobileOrWebMobile: isMobile || (isWeb && !isWebDesktop),
      bracketedPasteMode: terminal.bracketedPasteMode,
      ctrlLocked: ctrlLocked,
      altLocked: altLocked,
    );
    if (shouldConsumeModifiers) {
      if (ctrlLocked) clearCtrlLock?.call();
      if (altLocked) clearAltLock?.call();
    }
    return _sendInputPayload(data);
  }

  /// 发送已经准备好的负载，不施加键盘语义。
  /// 普通输入和粘贴在各自的来源
  /// 特定预处理完成后都走这条传输路径。
  Future<void> _sendInputPayload(String data) async {
    // 剪贴板读取和原生发送可能在终端页面
    // 关闭之后才完成。本 model 一旦释放，绝不要发送或重新缓冲输入。
    if (_disposed) return;

    if (_terminalOpened) {
      // 把用户输入发送到远端终端
      try {
        await bind.sessionSendTerminalInput(
          sessionId: parent.sessionId,
          terminalId: terminalId,
          data: data,
        );
      } catch (e) {
        debugPrint('[TerminalModel] Error sending terminal input: $e');
      }
    } else {
      debugPrint('[TerminalModel] Terminal not opened yet, buffering input');
      _inputBuffer.add(data);
    }
  }

  TerminalModel(this.parent, [this.terminalId = 0]) : id = parent.id {
    terminal = RustDeskTerminal(
      maxLines: 10000,
      onClipboardWrite: writeTerminalClipboard,
      clipboardWritePermission: () => terminalClipboardWritePermission(
        bind.mainGetLocalOption(key: kOptionAllowTerminalClipboardWrite),
        remoteClipboardEnabled:
            parent.ffiModel.permissions['clipboard'] != false,
        canRequestConsent: onClipboardWriteBlocked != null,
      ),
      onClipboardWriteBlocked: (text) => onClipboardWriteBlocked?.call(text),
      onClipboardWriteSucceeded: (text) =>
          onClipboardWriteSucceeded?.call(text),
    );
    terminal.mouseHandler = const WheelButtonFixMouseHandler();
    terminalController = TerminalController();

    // 设置终端回调
    terminal.onOutput = (data) {
      if (_suppressTerminalOutput) return;
      _handleInput(data);
    };

    terminal.onResize = (w, h, pw, ph) async {
      // 使用前校验所有尺寸
      if (w > 0 && h > 0 && pw > 0 && ph > 0) {
        debugPrint(
            '[TerminalModel] Terminal resized to ${w}x$h (pixel: ${pw}x$ph)');

        // 这段代码必须放在条件判断之前，才能正确初始化。
        onResizeExternal?.call(w, h, pw, ph);

        // 首次有效 resize 时把终端视图标记为就绪，并冲刷已缓冲的输出。
        // 必须放在 onResizeExternal 之后，这样冲刷前视图层已有有效尺寸。
        if (!_terminalViewReady) {
          _scheduleMarkViewReady();
        }

        if (_terminalOpened) {
          // 把 resize 通知远端终端
          try {
            await bind.sessionResizeTerminal(
              sessionId: parent.sessionId,
              terminalId: terminalId,
              rows: h,
              cols: w,
            );
          } catch (e) {
            debugPrint('[TerminalModel] Error resizing terminal: $e');
          }
        }
      } else {
        debugPrint(
            '[TerminalModel] Invalid terminal dimensions: ${w}x$h (pixel: ${pw}x$ph)');
      }
    };
  }

  void onReady() {
    parent.dialogManager.dismissAll();

    // 发出即不管 —— 不要阻塞 onReady。如果在
    // 本 model 仍然打开期间传输层发生重连，就重新发送 OpenTerminal，让远端服务
    // 重新把该持久会话标记为活动状态并恢复输出流。
    openTerminal(force: _terminalOpened).catchError((e) {
      debugPrint('[TerminalModel] Error opening terminal: $e');
    });
  }

  Future<void> openTerminal({bool force = false}) async {
    if (_terminalOpened && !force) return;
    // 请求远端以默认 shell 打开一个终端
    // 远端会根据自身操作系统决定使用哪种 shell

    // 获取终端尺寸，并确保其有效
    int rows = 24;
    int cols = 80;

    if (terminal.viewHeight > 0) {
      rows = terminal.viewHeight;
    }
    if (terminal.viewWidth > 0) {
      cols = terminal.viewWidth;
    }

    debugPrint(
        '[TerminalModel] Opening terminal $terminalId, sessionId: ${parent.sessionId}, size: ${cols}x$rows');
    try {
      await bind
          .sessionOpenTerminal(
        sessionId: parent.sessionId,
        terminalId: terminalId,
        rows: rows,
        cols: cols,
      )
          .timeout(
        const Duration(seconds: 5),
        onTimeout: () {
          throw TimeoutException(
              'sessionOpenTerminal timed out after 5 seconds');
        },
      );
      debugPrint('[TerminalModel] sessionOpenTerminal called successfully');
    } catch (e) {
      debugPrint('[TerminalModel] Error calling sessionOpenTerminal: $e');
      // 可选地向用户显示错误
      if (e is TimeoutException) {
        _writeToTerminal('Failed to open terminal: Connection timeout\r\n');
      }
    }
  }

  Future<void> sendVirtualKey(String data) async {
    return _handleInput(data);
  }

  Future<void> pasteText(String data) async {
    final payload = prepareTerminalInputPayload(
      data,
      source: TerminalInputSource.paste,
      isMobileOrWebMobile: false,
      bracketedPasteMode: terminal.bracketedPasteMode,
      ctrlLocked: false,
      altLocked: false,
    );
    return _sendInputPayload(payload);
  }

  Future<void> closeTerminal() async {
    if (_terminalOpened) {
      try {
        await bind
            .sessionCloseTerminal(
          sessionId: parent.sessionId,
          terminalId: terminalId,
        )
            .timeout(
          const Duration(seconds: 3),
          onTimeout: () {
            throw TimeoutException(
                'sessionCloseTerminal timed out after 3 seconds');
          },
        );
        debugPrint('[TerminalModel] sessionCloseTerminal called successfully');
      } catch (e) {
        debugPrint('[TerminalModel] Error calling sessionCloseTerminal: $e');
        // 即使关闭失败也继续清理
      }
      _terminalOpened = false;
      notifyListeners();
    }
  }

  static int getTerminalIdFromEvt(Map<String, dynamic> evt) {
    if (evt.containsKey('terminal_id')) {
      final v = evt['terminal_id'];
      if (v is int) {
        // 桌面端和移动端以 int 形式发送 terminal_id
        return v;
      } else if (v is String) {
        // Web 端以 string 形式发送 terminal_id
        final parsed = int.tryParse(v);
        if (parsed != null) {
          return parsed;
        } else {
          debugPrint(
              '[TerminalModel] Failed to parse terminal_id as integer: $v. Expected a numeric string.');
          return 0;
        }
      } else {
        // 类型意外，记录日志并优雅处理
        debugPrint(
            '[TerminalModel] Unexpected terminal_id type: ${v.runtimeType}, value: $v. Expected int or String.');
        return 0;
      }
    } else {
      debugPrint('[TerminalModel] Event does not contain terminal_id');
      return 0;
    }
  }

  static bool getSuccessFromEvt(Map<String, dynamic> evt) {
    if (evt.containsKey('success')) {
      final v = evt['success'];
      if (v is bool) {
        // 桌面端和移动端
        return v;
      } else if (v is String) {
        // Web
        return v.toLowerCase() == 'true';
      } else {
        // 类型意外，记录日志并优雅处理
        debugPrint(
            '[TerminalModel] Unexpected success type: ${v.runtimeType}, value: $v. Expected bool or String.');
        return false;
      }
    } else {
      debugPrint('[TerminalModel] Event does not contain success');
      return false;
    }
  }

  static int getExitCodeFromEvt(Map<String, dynamic> evt) {
    if (evt.containsKey('exit_code')) {
      final v = evt['exit_code'];
      if (v is int) {
        // 桌面端和移动端以 int 形式发送 exit_code
        return v;
      } else if (v is String) {
        // Web 端以 string 形式发送 exit_code
        final parsed = int.tryParse(v);
        if (parsed != null) {
          return parsed;
        } else {
          debugPrint(
              '[TerminalModel] Failed to parse exit_code as integer: $v. Expected a numeric string.');
          return 0;
        }
      } else {
        debugPrint(
            '[TerminalModel] Unexpected exit_code type: ${v.runtimeType}, value: $v. Expected int or String.');
        return 0;
      }
    } else {
      debugPrint('[TerminalModel] Event does not contain exit_code');
      return 0;
    }
  }

  void handleTerminalResponse(Map<String, dynamic> evt) {
    final String? type = evt['type'];
    final int evtTerminalId = getTerminalIdFromEvt(evt);

    // 只处理该终端的事件
    if (evtTerminalId != terminalId) {
      debugPrint(
          '[TerminalModel] Ignoring event for terminal $evtTerminalId (not mine)');
      return;
    }

    switch (type) {
      case 'opened':
        _handleTerminalOpened(evt);
        break;
      case 'data':
        _handleTerminalData(evt);
        break;
      case 'closed':
        _handleTerminalClosed(evt);
        break;
      case 'error':
        _handleTerminalError(evt);
        break;
    }
  }

  void _handleTerminalOpened(Map<String, dynamic> evt) {
    final bool success = getSuccessFromEvt(evt);
    final String message = evt['message']?.toString() ?? '';
    final String? serviceId = evt['service_id']?.toString();

    debugPrint(
        '[TerminalModel] Terminal opened response: success=$success, message=$message, service_id=$serviceId');

    if (success) {
      _terminalOpened = true;

      // 重连时服务端可能会重放最近的输出。这些重放内容可能包含
      // 诸如 DSR/DA 的终端查询；xterm 会通过 onOutput 以
      // "^[[1;1R^[[2;2R^[[>0;0;0c" 的形式作出应答，绝不能被回发给对端。
      final replayTerminalOutput = evt['replay_terminal_output'];
      _suppressNextTerminalDataOutput = replayTerminalOutput == true ||
          message == 'Reconnected to existing terminal with pending output';

      // 回退方案：如果终端视图尚未就绪但已具有有效
      // 尺寸（例如布局在 open 响应到达前就已完成），
      // 现在就标记视图就绪，避免输出无限期滞留在缓冲区中。
      if (!_terminalViewReady &&
          terminal.viewWidth > 0 &&
          terminal.viewHeight > 0) {
        _scheduleMarkViewReady();
      }

      // 处理任何已缓冲的输入
      _processBufferedInputAsync().then((_) {
        notifyListeners();
      }).catchError((e) {
        debugPrint('[TerminalModel] Error processing buffered input: $e');
        notifyListeners();
      });

      final persistentSessions =
          (evt['persistent_sessions'] as List<dynamic>? ?? [])
              .whereType<int>()
              .where((id) => !parent.terminalModels.containsKey(id))
              .toList();
      if (kWindowId != null && persistentSessions.isNotEmpty) {
        DesktopMultiWindow.invokeMethod(
            kWindowId!,
            kWindowEventRestoreTerminalSessions,
            jsonEncode({
              'peer_id': id,
              'persistent_sessions': persistentSessions,
            }));
      }
    } else {
      _writeToTerminal('Failed to open terminal: $message\r\n');
    }
  }

  Future<void> _processBufferedInputAsync() async {
    final buffer = List<String>.from(_inputBuffer);
    _inputBuffer.clear();

    for (final data in buffer) {
      try {
        await bind.sessionSendTerminalInput(
          sessionId: parent.sessionId,
          terminalId: terminalId,
          data: data,
        );
      } catch (e) {
        debugPrint('[TerminalModel] Error sending buffered input: $e');
      }
    }
  }

  void _handleTerminalData(Map<String, dynamic> evt) {
    final data = evt['data'];

    if (data != null) {
      final suppressTerminalOutput = _suppressNextTerminalDataOutput;
      _suppressNextTerminalDataOutput = false;
      try {
        String text = '';
        if (data is String) {
          // 先尝试按 base64 解码
          try {
            final bytes = base64Decode(data);
            text = utf8.decode(bytes, allowMalformed: true);
          } catch (e) {
            // 如果 base64 解码失败，则按纯文本处理
            text = data;
          }
        } else if (data is List) {
          // 处理数据以字节数组形式到达的情况
          text = utf8.decode(List<int>.from(data), allowMalformed: true);
        } else {
          debugPrint('[TerminalModel] Unknown data type: ${data.runtimeType}');
          return;
        }

        _writeToTerminal(text, suppressTerminalOutput: suppressTerminalOutput);
      } catch (e) {
        debugPrint('[TerminalModel] Failed to process terminal data: $e');
      }
    }
  }

  /// 把文本写入终端，视图尚未就绪时先缓冲。
  /// 所有终端输出都应走这个方法，以避免 NaN 错误
  /// ——即在终端视图尚无有效布局尺寸时写入。
  void _writeToTerminal(
    String text, {
    bool suppressTerminalOutput = false,
  }) {
    if (!_terminalViewReady) {
      // 如果单个数据块超出上限，只保留其尾部。
      // 注意：截断可能会把多字节的 ANSI 转义序列切开，
      // 这会在冲刷时造成短暂的显示抖动。这是可以接受的，
      // 因为它只影响布局完成前的缓冲窗口，而且
      // 终端会在后续输出中自行纠正。
      if (text.length >= _kMaxOutputBufferChars) {
        final truncated = text.substring(text.length - _kMaxOutputBufferChars);
        _pendingOutputChunks
          ..clear()
          ..add(truncated);
        _pendingOutputSuppressFlags
          ..clear()
          ..add(suppressTerminalOutput);
        _pendingOutputSize = truncated.length;
      } else {
        _pendingOutputChunks.add(text);
        _pendingOutputSuppressFlags.add(suppressTerminalOutput);
        _pendingOutputSize += text.length;
        // 超出上限时丢弃最旧的数据块（按整块丢弃，以保留 ANSI 序列）
        while (_pendingOutputSize > _kMaxOutputBufferChars &&
            _pendingOutputChunks.length > 1) {
          final removed = _pendingOutputChunks.removeAt(0);
          _pendingOutputSuppressFlags.removeAt(0);
          _pendingOutputSize -= removed.length;
        }
      }
      return;
    }
    _writeTerminalChunk(text, suppressTerminalOutput: suppressTerminalOutput);
  }

  void _flushOutputBuffer() {
    if (_pendingOutputChunks.isEmpty) return;
    debugPrint(
        '[TerminalModel] Flushing $_pendingOutputSize buffered chars (${_pendingOutputChunks.length} chunks)');
    for (var i = 0; i < _pendingOutputChunks.length; i++) {
      _writeTerminalChunk(
        _pendingOutputChunks[i],
        suppressTerminalOutput: _pendingOutputSuppressFlags[i],
      );
    }
    _pendingOutputChunks.clear();
    _pendingOutputSuppressFlags.clear();
    _pendingOutputSize = 0;
  }

  void _writeTerminalChunk(
    String text, {
    required bool suppressTerminalOutput,
  }) {
    if (!suppressTerminalOutput) {
      terminal.write(text);
      return;
    }
    final previous = _suppressTerminalOutput;
    _suppressTerminalOutput = true;
    try {
      terminal.write(text);
    } finally {
      _suppressTerminalOutput = previous;
    }
  }

  /// 把终端视图标记为就绪并冲刷已缓冲的输出。
  void _scheduleMarkViewReady() {
    if (_disposed || _terminalViewReady || _markViewReadyScheduled) return;
    _markViewReadyScheduled = true;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _markViewReadyScheduled = false;
      if (_disposed || _terminalViewReady) return;
      if (terminal.viewWidth > 0 && terminal.viewHeight > 0) {
        _markViewReady();
      }
    });
    WidgetsBinding.instance.ensureVisualUpdate();
  }

  void _markViewReady() {
    if (_terminalViewReady) return;
    _terminalViewReady = true;
    _flushOutputBuffer();
  }

  void _handleTerminalClosed(Map<String, dynamic> evt) {
    final int exitCode = getExitCodeFromEvt(evt);
    _writeToTerminal('\r\nTerminal closed with exit code: $exitCode\r\n');
    _terminalOpened = false;
    notifyListeners();
    // 自动关闭标签页/页面
    onClosed?.call();
  }

  void _handleTerminalError(Map<String, dynamic> evt) {
    final String message = evt['message'] ?? 'Unknown error';
    _writeToTerminal('\r\nTerminal error: $message\r\n');
  }

  @override
  void dispose() {
    if (_disposed) return;
    _disposed = true;
    terminal.onOutput = null;
    terminal.onResize = null;
    isCtrlLocked = null;
    clearCtrlLock = null;
    isAltLocked = null;
    clearAltLock = null;
    onResizeExternal = null;
    onClosed = null;
    onClipboardWriteBlocked = null;
    onClipboardWriteSucceeded = null;
    // 清空缓冲区以释放内存
    _inputBuffer.clear();
    _pendingOutputChunks.clear();
    _pendingOutputSuppressFlags.clear();
    _pendingOutputSize = 0;
    _markViewReadyScheduled = false;
    _suppressNextTerminalDataOutput = false;
    // 服务关闭时，终端清理由服务端处理
    super.dispose();
  }
}
