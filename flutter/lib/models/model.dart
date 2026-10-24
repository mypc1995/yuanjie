import 'dart:async';
import 'dart:convert';
import 'dart:math';
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:bot_toast/bot_toast.dart';
import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter/scheduler.dart';
import 'package:flutter_hbb/common/widgets/peers_view.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/models/ab_model.dart';
import 'package:flutter_hbb/models/chat_model.dart';
import 'package:flutter_hbb/models/cm_file_model.dart';
import 'package:flutter_hbb/models/file_model.dart';
import 'package:flutter_hbb/models/group_model.dart';
import 'package:flutter_hbb/models/peer_model.dart';
import 'package:flutter_hbb/models/peer_tab_model.dart';
import 'package:flutter_hbb/models/printer_model.dart';
import 'package:flutter_hbb/models/server_model.dart';
import 'package:flutter_hbb/models/shortcut_model.dart';
import 'package:flutter_hbb/models/user_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:flutter_hbb/models/desktop_render_texture.dart';
import 'package:flutter_hbb/models/terminal_model.dart';
import 'package:flutter_hbb/common/shared_state.dart';
import 'package:flutter_hbb/utils/multi_window_manager.dart';
import 'package:flutter_hbb/utils/http_service.dart' as http;
import 'package:tuple/tuple.dart';
import 'package:image/image.dart' as img2;
import 'package:flutter_svg/flutter_svg.dart';
import 'package:get/get.dart';
import 'package:uuid/uuid.dart';
import 'package:window_manager/window_manager.dart';
import 'package:file_picker/file_picker.dart';
import 'package:vector_math/vector_math.dart' show Vector2;

import '../common.dart';
import '../utils/image.dart' as img;
import '../common/widgets/dialog.dart';
import 'input_model.dart';
import 'platform_model.dart';
import 'package:flutter_hbb/utils/scale.dart';

import 'package:flutter_hbb/generated_bridge.dart'
    if (dart.library.html) 'package:flutter_hbb/web/bridge.dart';
import 'package:flutter_hbb/native/custom_cursor.dart'
    if (dart.library.html) 'package:flutter_hbb/web/custom_cursor.dart';

typedef HandleMsgBox = Function(Map<String, dynamic> evt, String id);
typedef ReconnectHandle = Function(OverlayDialogManager, SessionID, bool);
final _constSessionId = Uuid().v4obj();
// 经验性的重启重连节奏：短暂保留最后一帧并快速重试。
const _restartReconnectSilentDelaySecs = 5;

class CachedPeerData {
  Map<String, dynamic> updatePrivacyMode = {};
  Map<String, dynamic> peerInfo = {};
  // 没有形状：核心会为该会话保存它们，而标签页移动到的窗口会向它请求
  // 正在使用的那一个。
  Map<String, dynamic> lastCursorId = {};
  Map<String, bool> permissions = {};

  bool secure = false;
  bool direct = false;
  String streamType = '';

  CachedPeerData();

  @override
  String toString() {
    return jsonEncode({
      'updatePrivacyMode': updatePrivacyMode,
      'peerInfo': peerInfo,
      'lastCursorId': lastCursorId,
      'permissions': permissions,
      'secure': secure,
      'direct': direct,
      'streamType': streamType,
    });
  }

  static CachedPeerData? fromString(String s) {
    try {
      final map = jsonDecode(s);
      final data = CachedPeerData();
      data.updatePrivacyMode = map['updatePrivacyMode'];
      data.peerInfo = map['peerInfo'];
      data.lastCursorId = map['lastCursorId'];
      map['permissions'].forEach((key, value) {
        data.permissions[key] = value;
      });
      data.secure = map['secure'];
      data.direct = map['direct'];
      data.streamType = map['streamType'];
      return data;
    } catch (e) {
      debugPrint('Failed to parse CachedPeerData: $e');
      return null;
    }
  }
}

class FfiModel with ChangeNotifier {
  CachedPeerData cachedPeerData = CachedPeerData();
  PeerInfo _pi = PeerInfo();
  int? lastUserDisplay;
  int? pendingMonitorRestore;
  Timer? _pendingRestoreTimer;
  Rect? _rect;

  var _inputBlocked = false;
  final _permissions = <String, bool>{};
  bool? _secure;
  bool? _direct;
  bool _touchMode = false;
  late VirtualMouseMode virtualMouseMode;
  Timer? _timer;
  Timer? _restartReconnectDelayTimer;
  var _reconnects = 1;
  DateTime? _offlineReconnectStartTime;
  bool _androidDocumentPickerActive = false;
  bool _androidDocumentPickerInterruptedConnection = false;
  bool _viewOnly = false;
  bool _showMyCursor = false;
  WeakReference<FFI> parent;
  late final SessionID sessionId;

  RxBool waitForImageDialogShow = true.obs;
  Timer? waitForImageTimer;
  RxBool waitForFirstImage = true.obs;
  bool isRefreshing = false;

  Timer? timerScreenshot;

  Rect? get rect => _rect;
  bool get isOriginalResolutionSet =>
      _pi.tryGetDisplayIfNotAllDisplay()?.isOriginalResolutionSet ?? false;
  bool get isVirtualDisplayResolution =>
      _pi.tryGetDisplayIfNotAllDisplay()?.isVirtualDisplayResolution ?? false;
  bool get isOriginalResolution =>
      _pi.tryGetDisplayIfNotAllDisplay()?.isOriginalResolution ?? false;

  Map<String, bool> get permissions => _permissions;
  setPermissions(Map<String, bool> permissions) {
    _permissions.clear();
    _permissions.addAll(permissions);
  }

  bool? get secure => _secure;

  bool? get direct => _direct;

  PeerInfo get pi => _pi;

  bool get inputBlocked => _inputBlocked;

  bool get touchMode => _touchMode;

  bool get isPeerAndroid => _pi.platform == kPeerPlatformAndroid;
  bool get isPeerMobile => isPeerAndroid;

  bool get isPeerLinux => _pi.platform == kPeerPlatformLinux;
  bool get isPeerWindows => _pi.platform == kPeerPlatformWindows;

  bool get viewOnly => _viewOnly;
  bool get showMyCursor => _showMyCursor;

  set inputBlocked(v) {
    _inputBlocked = v;
  }

  FfiModel(this.parent) {
    clear();
    sessionId = parent.target!.sessionId;
    cachedPeerData.permissions = _permissions;
    virtualMouseMode = VirtualMouseMode(this);
  }

  Rect? globalDisplaysRect() => _getDisplaysRect(_pi.displays, true);
  Rect? displaysRect() => _getDisplaysRect(_pi.getCurDisplays(), false);
  Rect? _getDisplaysRect(List<Display> displays, bool useDisplayScale) {
    if (displays.isEmpty) {
      return null;
    }
    if (isPeerLinux) {
      useDisplayScale = true;
    }
    int scale(int len, double s) {
      if (useDisplayScale) {
        return len.toDouble() ~/ s;
      } else {
        return len;
      }
    }

    double l = displays[0].x;
    double t = displays[0].y;
    double r = displays[0].x + scale(displays[0].width, displays[0].scale);
    double b = displays[0].y + scale(displays[0].height, displays[0].scale);
    for (var display in displays.sublist(1)) {
      l = min(l, display.x);
      t = min(t, display.y);
      r = max(r, display.x + scale(display.width, display.scale));
      b = max(b, display.y + scale(display.height, display.scale));
    }
    return Rect.fromLTRB(l, t, r, b);
  }

  toggleTouchMode() {
    if (!isPeerAndroid) {
      _touchMode = !_touchMode;
      notifyListeners();
    }
  }

  updatePermission(Map<String, dynamic> evt, String id) {
    // 跟踪上一次的键盘权限，以检测权限被撤销。
    final hadKeyboardPerm = _permissions['keyboard'] != false;

    evt.forEach((k, v) {
      if (k == 'name' || k.isEmpty) return;
      _permissions[k] = v == 'true';
    });
    // 仅在远端页面初始化
    if (parent.target?.connType == ConnType.defaultConn) {
      KeyboardEnabledState.find(id).value = _permissions['keyboard'] != false;
    }

    // 如果相对鼠标模式处于活动状态时键盘权限被撤销，
    // 就强制禁用相对鼠标模式，避免用户被困住。
    final hasKeyboardPerm = _permissions['keyboard'] != false;
    if (hadKeyboardPerm && !hasKeyboardPerm) {
      final inputModel = parent.target?.inputModel;
      if (inputModel != null && inputModel.relativeMouseMode.value) {
        inputModel.setRelativeMouseMode(false);
        showToast(translate('rel-mouse-permission-lost-tip'));
      }
    }

    debugPrint('updatePermission: $_permissions');
    notifyListeners();
  }

  bool get keyboard => _permissions['keyboard'] != false;

  clear() {
    _pi = PeerInfo();
    lastUserDisplay = null;
    _cancelPendingMonitorRestore();
    _secure = null;
    _direct = null;
    _inputBlocked = false;
    _timer?.cancel();
    _timer = null;
    _androidDocumentPickerActive = false;
    _androidDocumentPickerInterruptedConnection = false;
    resetRestartReconnectState();
    clearPermissions();
    waitForImageTimer?.cancel();
    timerScreenshot?.cancel();
  }

  setConnectionType(
      String peerId, bool secure, bool direct, String streamType) {
    cachedPeerData.secure = secure;
    cachedPeerData.direct = direct;
    cachedPeerData.streamType = streamType;
    _secure = secure;
    _direct = direct;
    try {
      var connectionType = ConnectionTypeState.find(peerId);
      connectionType.setSecure(secure);
      connectionType.setDirect(direct);
      connectionType.setStreamType(streamType);
    } catch (e) {
      //
    }
  }

  Widget? getConnectionImageText() {
    if (secure == null || direct == null) {
      return null;
    } else {
      final icon =
          '${secure == true ? 'secure' : 'insecure'}${direct == true ? '' : '_relay'}';
      final iconWidget =
          SvgPicture.asset('assets/$icon.svg', width: 48, height: 48);
      String connectionText =
          getConnectionText(secure!, direct!, cachedPeerData.streamType);
      return Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          iconWidget,
          SizedBox(height: 4),
          Text(
            connectionText,
            style: TextStyle(fontSize: 12),
            textAlign: TextAlign.center,
          ),
        ],
      );
    }
  }

  clearPermissions() {
    _inputBlocked = false;
    _permissions.clear();
  }

  handleCachedPeerData(CachedPeerData data, String peerId) async {
    handleMsgBox({
      'type': 'success',
      'title': 'Successful',
      'text': kMsgboxTextWaitingForImage,
      'link': '',
    }, sessionId, peerId);
    updatePrivacyMode(data.updatePrivacyMode, sessionId, peerId);
    setConnectionType(peerId, data.secure, data.direct, data.streamType);
    await handlePeerInfo(data.peerInfo, peerId, true);
    if (data.lastCursorId.isNotEmpty) {
      updateLastCursorId(data.lastCursorId);
      handleCursorId(data.lastCursorId);
    }
  }

  // 待办：为什么会被两个位置调用
  StreamEventHandler startEventListener(SessionID sessionId, String peerId) {
    return (evt) async {
      var name = evt['name'];
      if (name == 'msgbox') {
        handleMsgBox(evt, sessionId, peerId);
      } else if (name == 'toast') {
        handleToast(evt, sessionId, peerId);
      } else if (name == 'set_multiple_windows_session') {
        handleMultipleWindowsSession(evt, sessionId, peerId);
      } else if (name == 'peer_info') {
        handlePeerInfo(evt, peerId, false);
      } else if (name == 'sync_peer_info') {
        handleSyncPeerInfo(evt, sessionId, peerId);
      } else if (name == 'sync_platform_additions') {
        handlePlatformAdditions(evt, sessionId, peerId);
      } else if (name == 'connection_ready') {
        setConnectionType(peerId, evt['secure'] == 'true',
            evt['direct'] == 'true', evt['stream_type'] ?? '');
        resetRestartReconnectState();
      } else if (name == 'switch_display') {
        // switch display 保留是为了向后兼容
        handleSwitchDisplay(evt, sessionId, peerId);
      } else if (name == 'cursor_id') {
        updateLastCursorId(evt);
        handleCursorId(evt);
      } else if (name == 'cursor_position') {
        await parent.target?.cursorModel.updateCursorPosition(evt, peerId);
      } else if (name == 'clipboard') {
        Clipboard.setData(ClipboardData(text: evt['content']));
      } else if (name == 'permission') {
        updatePermission(evt, peerId);
      } else if (name == 'chat_client_mode') {
        parent.target?.chatModel
            .receive(ChatModel.clientModeID, evt['text'] ?? '');
      } else if (name == 'chat_server_mode') {
        parent.target?.chatModel
            .receive(int.parse(evt['id'] as String), evt['text'] ?? '');
      } else if (name == 'terminal_response') {
        parent.target?.routeTerminalResponse(evt);
      } else if (name == 'file_dir') {
        parent.target?.fileModel.receiveFileDir(evt);
      } else if (name == 'empty_dirs') {
        parent.target?.fileModel.receiveEmptyDirs(evt);
      } else if (name == 'job_progress') {
        parent.target?.fileModel.jobController.tryUpdateJobProgress(evt);
      } else if (name == 'job_done') {
        bool? refresh =
            await parent.target?.fileModel.jobController.jobDone(evt);
        if (refresh == true) {
          // 删除目录时已完成许多任务
          // 待办：确认删除本地目录时刷新可能不生效
          parent.target?.fileModel.refreshAll();
        }
      } else if (name == 'job_error') {
        parent.target?.fileModel.handleJobError(evt);
      } else if (name == 'override_file_confirm') {
        parent.target?.fileModel.postOverrideFileConfirm(evt);
      } else if (name == 'load_last_job') {
        parent.target?.fileModel.jobController.loadLastJob(evt);
      } else if (name == 'update_folder_files') {
        parent.target?.fileModel.jobController.updateFolderFiles(evt);
      } else if (name == 'add_connection') {
        parent.target?.serverModel.addConnection(evt);
      } else if (name == 'on_client_remove') {
        parent.target?.serverModel.onClientRemove(evt);
      } else if (name == 'update_quality_status') {
        parent.target?.qualityMonitorModel.updateQualityStatus(evt);
      } else if (name == 'update_block_input_state') {
        updateBlockInputState(evt, peerId);
      } else if (name == 'update_privacy_mode') {
        updatePrivacyMode(evt, sessionId, peerId);
      } else if (name == 'show_elevation') {
        final show = evt['show'].toString() == 'true';
        parent.target?.serverModel.setShowElevation(show);
      } else if (name == 'cancel_msgbox') {
        cancelMsgBox(evt, sessionId);
      } else if (name == 'switch_back') {
        final peer_id = evt['peer_id'].toString();
        await bind.sessionSwitchSides(sessionId: sessionId);
        closeConnection(id: peer_id);
      } else if (name == 'portable_service_running') {
        _handlePortableServiceRunning(peerId, evt);
      } else if (name == 'on_url_scheme_received') {
        // 目前来自 mac 的 "_url" ipc 和 linux 的 dbus
        onUrlSchemeReceived(evt);
      } else if (name == 'on_voice_call_waiting') {
        // 正在等待对端的响应。
        parent.target?.chatModel.onVoiceCallWaiting();
      } else if (name == 'on_voice_call_started') {
        // 语音通话已连接。
        parent.target?.chatModel.onVoiceCallStarted();
      } else if (name == 'on_voice_call_closed') {
        // 语音通话已关闭并附带原因。
        final reason = evt['reason'].toString();
        parent.target?.chatModel.onVoiceCallClosed(reason);
      } else if (name == 'on_voice_call_incoming') {
        // 对端请求了语音通话。
        parent.target?.chatModel.onVoiceCallIncoming();
      } else if (name == 'update_voice_call_state') {
        parent.target?.serverModel.updateVoiceCallState(evt);
      } else if (name == 'fingerprint') {
        FingerprintState.find(peerId).value = evt['fingerprint'] ?? '';
      } else if (name == "sync_peer_hash_password_to_personal_ab") {
        if (desktopType == DesktopType.main || isWeb || isMobile) {
          final id = evt['id'];
          final hash = evt['hash'];
          if (id != null && hash != null) {
            gFFI.abModel
                .changePersonalHashPassword(id.toString(), hash.toString());
          }
        }
      } else if (name == "cm_file_transfer_log") {
        if (isDesktop) {
          gFFI.cmFileModel.onFileTransferLog(evt);
        }
      } else if (name == 'sync_peer_option') {
        _handleSyncPeerOption(evt, peerId);
      } else if (name == 'follow_current_display') {
        handleFollowCurrentDisplay(evt, sessionId, peerId);
      } else if (name == 'use_texture_render') {
        _handleUseTextureRender(evt, sessionId, peerId);
      } else if (name == "selected_files") {
        if (isWeb) {
          parent.target?.fileModel.onSelectedFiles(evt);
        }
      } else if (name == "send_emptry_dirs") {
        if (isWeb) {
          parent.target?.fileModel.sendEmptyDirs(evt);
        }
      } else if (name == "record_status") {
        if (desktopType == DesktopType.remote ||
            desktopType == DesktopType.viewCamera ||
            isMobile) {
          parent.target?.recordingModel.updateStatus(evt['start'] == 'true');
        }
      } else if (name == "printer_request") {
        _handlePrinterRequest(evt, sessionId, peerId);
      } else if (name == 'screenshot') {
        _handleScreenshot(evt, sessionId, peerId);
      } else if (name == 'exit_relative_mouse_mode') {
        // 处理来自 rdev 抓取循环的退出快捷键（Win/Linux 上是 Ctrl+Alt，macOS 上是 Cmd+G）
        parent.target?.inputModel.exitRelativeMouseModeWithKeyRelease();
      } else if (name == kShortcutEventName) {
        final action = evt['action'];
        if (action is String) {
          parent.target?.shortcutModel.onTriggered(action);
        }
      } else {
        debugPrint('Event is not handled in the fixed branch: $name');
      }
    };
  }

  _handleScreenshot(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) {
    timerScreenshot?.cancel();
    timerScreenshot = null;
    final msg = evt['msg'] ?? '';
    final msgBoxType = 'custom-nook-nocancel-hasclose';
    final msgBoxTitle = 'Take screenshot';
    final dialogManager = parent.target!.dialogManager;
    if (msg.isNotEmpty) {
      msgBox(sessionId, msgBoxType, msgBoxTitle, msg, '', dialogManager);
    } else {
      final msgBoxText = 'screenshot-action-tip';

      close() {
        dialogManager.dismissAll();
      }

      saveAs() {
        close();
        Future.delayed(Duration.zero, () async {
          final ts = DateTime.now().millisecondsSinceEpoch ~/ 1000;
          String? outputFile = await FilePicker.platform.saveFile(
            dialogTitle: '${translate('Save as')}...',
            fileName: 'screenshot_$ts.png',
            allowedExtensions: ['png'],
            type: FileType.custom,
          );
          if (outputFile == null) {
            bind.sessionHandleScreenshot(sessionId: sessionId, action: '2');
          } else {
            final res = await bind.sessionHandleScreenshot(
                sessionId: sessionId, action: '0:$outputFile');
            if (res.isNotEmpty) {
              msgBox(sessionId, 'custom-nook-nocancel-hasclose-error',
                  'Take screenshot', res, '', dialogManager);
            }
          }
        });
      }

      copyToClipboard() {
        bind.sessionHandleScreenshot(sessionId: sessionId, action: '1');
        close();
      }

      cancel() {
        bind.sessionHandleScreenshot(sessionId: sessionId, action: '2');
        close();
      }

      final List<Widget> buttons = [
        dialogButton('${translate('Save as')}...', onPressed: saveAs),
        dialogButton('Copy to clipboard', onPressed: copyToClipboard),
        dialogButton('Cancel', onPressed: cancel),
      ];
      dialogManager.dismissAll();
      dialogManager.show(
        (setState, close, context) => CustomAlertDialog(
          title: null,
          content: SelectionArea(
              child: msgboxContent(msgBoxType, msgBoxTitle, msgBoxText)),
          actions: buttons,
        ),
        tag: '$msgBoxType-$msgBoxTitle-$msgBoxTitle',
      );
    }
  }

  _handlePrinterRequest(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) {
    final id = evt['id'];
    final path = evt['path'];
    final dialogManager = parent.target!.dialogManager;
    dialogManager.show((setState, close, context) {
      PrinterOptions printerOptions = PrinterOptions.load();
      final saveSettings = mainGetLocalBoolOptionSync(kKeyPrinterSave).obs;
      final dontShowAgain = false.obs;
      final Rx<String> selectedPrinterName = printerOptions.printerName.obs;
      final printerNames = printerOptions.printerNames;
      final defaultOrSelectedGroupValue =
          (printerOptions.action == kValuePrinterIncomingJobDismiss
                  ? kValuePrinterIncomingJobDefault
                  : printerOptions.action)
              .obs;

      onRatioChanged(String? value) {
        defaultOrSelectedGroupValue.value =
            value ?? kValuePrinterIncomingJobDefault;
      }

      onSubmit() {
        final printerName = defaultOrSelectedGroupValue.isEmpty
            ? ''
            : selectedPrinterName.value;
        bind.sessionPrinterResponse(
            sessionId: sessionId, id: id, path: path, printerName: printerName);
        if (saveSettings.value || dontShowAgain.value) {
          bind.mainSetLocalOption(key: kKeyPrinterSelected, value: printerName);
          bind.mainSetLocalOption(
              key: kKeyPrinterIncomingJobAction,
              value: defaultOrSelectedGroupValue.value);
        }
        if (dontShowAgain.value) {
          mainSetLocalBoolOption(kKeyPrinterAllowAutoPrint, true);
        }
        close();
      }

      onCancel() {
        if (dontShowAgain.value) {
          bind.mainSetLocalOption(
              key: kKeyPrinterIncomingJobAction,
              value: kValuePrinterIncomingJobDismiss);
        }
        close();
      }

      final printerItemHeight = 30.0;
      final selectionAreaHeight =
          printerItemHeight * min(8.0, max(printerNames.length, 3.0));
      final content = Column(
        children: [
          Text(translate('print-incoming-job-confirm-tip')),
          Row(
            children: [
              Obx(() => Radio<String>(
                  value: kValuePrinterIncomingJobDefault,
                  groupValue: defaultOrSelectedGroupValue.value,
                  onChanged: onRatioChanged)),
              GestureDetector(
                  child: Text(translate('use-the-default-printer-tip')),
                  onTap: () => onRatioChanged(kValuePrinterIncomingJobDefault)),
            ],
          ),
          Column(
            children: [
              Row(children: [
                Obx(() => Radio<String>(
                    value: kValuePrinterIncomingJobSelected,
                    groupValue: defaultOrSelectedGroupValue.value,
                    onChanged: onRatioChanged)),
                GestureDetector(
                    child: Text(translate('use-the-selected-printer-tip')),
                    onTap: () =>
                        onRatioChanged(kValuePrinterIncomingJobSelected)),
              ]),
              SizedBox(
                height: selectionAreaHeight,
                width: 500,
                child: ListView.builder(
                    itemBuilder: (context, index) {
                      return Obx(() => GestureDetector(
                            child: Container(
                              decoration: BoxDecoration(
                                color: selectedPrinterName.value ==
                                        printerNames[index]
                                    ? (defaultOrSelectedGroupValue.value ==
                                            kValuePrinterIncomingJobSelected
                                        ? MyTheme.button
                                        : MyTheme.button.withOpacity(0.5))
                                    : Theme.of(context).cardColor,
                                borderRadius: BorderRadius.all(
                                  Radius.circular(5.0),
                                ),
                              ),
                              key: ValueKey(printerNames[index]),
                              height: printerItemHeight,
                              child: Align(
                                alignment: Alignment.centerLeft,
                                child: Padding(
                                  padding: const EdgeInsets.only(left: 10.0),
                                  child: Text(
                                    printerNames[index],
                                    style: TextStyle(fontSize: 14),
                                  ),
                                ),
                              ),
                            ),
                            onTap: defaultOrSelectedGroupValue.value ==
                                    kValuePrinterIncomingJobSelected
                                ? () {
                                    selectedPrinterName.value =
                                        printerNames[index];
                                  }
                                : null,
                          ));
                    },
                    itemCount: printerNames.length),
              ),
            ],
          ),
          Row(
            children: [
              Obx(() => Checkbox(
                  value: saveSettings.value,
                  onChanged: (value) {
                    if (value != null) {
                      saveSettings.value = value;
                      mainSetLocalBoolOption(kKeyPrinterSave, value);
                    }
                  })),
              GestureDetector(
                  child: Text(translate('save-settings-tip')),
                  onTap: () {
                    saveSettings.value = !saveSettings.value;
                    mainSetLocalBoolOption(kKeyPrinterSave, saveSettings.value);
                  }),
            ],
          ),
          Row(
            children: [
              Obx(() => Checkbox(
                  value: dontShowAgain.value,
                  onChanged: (value) {
                    if (value != null) {
                      dontShowAgain.value = value;
                    }
                  })),
              GestureDetector(
                  child: Text(translate('dont-show-again-tip')),
                  onTap: () {
                    dontShowAgain.value = !dontShowAgain.value;
                  }),
            ],
          ),
        ],
      );
      return CustomAlertDialog(
        title: Text(translate('Incoming Print Job')),
        content: content,
        actions: [
          dialogButton('OK', onPressed: onSubmit),
          dialogButton('Cancel', onPressed: onCancel),
        ],
        onSubmit: onSubmit,
        onCancel: onCancel,
      );
    });
  }

  _handleUseTextureRender(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) {
    parent.target?.imageModel.setUseTextureRender(evt['v'] == 'Y');
    waitForFirstImage.value = true;
    isRefreshing = true;
    showConnectedWaitingForImage(parent.target!.dialogManager, sessionId,
        'success', 'Successful', kMsgboxTextWaitingForImage);
  }

  _handleSyncPeerOption(Map<String, dynamic> evt, String peer) {
    final k = evt['k'];
    final v = evt['v'];
    if (k == kOptionToggleViewOnly) {
      setViewOnly(peer, v as bool);
    } else if (k == 'keyboard_mode') {
      parent.target?.inputModel.updateKeyboardMode();
    } else if (k == 'input_source') {
      stateGlobal.getInputSource(force: true);
    }
  }

  onUrlSchemeReceived(Map<String, dynamic> evt) {
    final url = evt['url'].toString().trim();
    if (url.startsWith(bind.mainUriPrefixSync()) &&
        handleUriLink(uriString: url)) {
      return;
    }
    switch (url) {
      case kUrlActionClose:
        debugPrint("closing all instances");
        Future.microtask(() async {
          await rustDeskWinManager.closeAllSubWindows();
          windowManager.close();
        });
        break;
      default:
        windowOnTop(null);
        break;
    }
  }

  /// 绑定事件监听器以接收来自 Rust 核心的事件。
  updateEventListener(SessionID sessionId, String peerId) {
    platformFFI.setEventCallback(startEventListener(sessionId, peerId));
  }

  _handlePortableServiceRunning(String peerId, Map<String, dynamic> evt) {
    final running = evt['running'] == 'true';
    parent.target?.elevationModel.onPortableServiceRunning(running);
  }

  handleAliasChanged(Map<String, dynamic> evt) {
    if (!(isDesktop || isWebDesktop)) return;
    final String peerId = evt['id'];
    final String alias = evt['alias'];
    String label = getDesktopTabLabel(peerId, alias);
    final rxTabLabel = PeerStringOption.find(evt['id'], 'tabLabel');
    if (rxTabLabel.value != label) {
      rxTabLabel.value = label;
    }
  }

  Future<void> updateCurDisplay(SessionID sessionId,
      {updateCursorPos = false}) async {
    final newRect = displaysRect();
    if (newRect == null) {
      return;
    }
    if (newRect != _rect) {
      if (newRect.left != _rect?.left || newRect.top != _rect?.top) {
        parent.target?.cursorModel.updateDisplayOrigin(
            newRect.left, newRect.top,
            updateCursorPos: updateCursorPos);
      }
      _rect = newRect;
      // await updateViewStyle，确保在
      // 更新指针锁定中心之前视图几何已完全更新。这可以避免使用过期的中心计算结果。
      await parent.target?.canvasModel
          .updateViewStyle(refreshMousePos: updateCursorPos);
      _updateSessionWidthHeight(sessionId);

      // 使用相对鼠标模式时保持指针锁定中心同步。
      // 注意：updatePointerLockCenter 是异步安全的（内部会处理错误），
      // 所以这里发出即忘即可。
      final inputModel = parent.target?.inputModel;
      if (inputModel != null && inputModel.relativeMouseMode.value) {
        inputModel.updatePointerLockCenter();
      }
    }
  }

  handleSwitchDisplay(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) {
    final display = int.parse(evt['display']);

    if (_pi.currentDisplay != kAllDisplayValue) {
      if (bind.peerGetSessionsCount(
              id: peerId, connType: parent.target!.connType.index) >
          1) {
        if (display != _pi.currentDisplay) {
          return;
        }
      }
      if (!_pi.isSupportMultiUiSession) {
        _pi.currentDisplay = display;
      }
      // 如果 `isSupportMultiUiSession` 为 true，就不应该用 switch display 消息来更新当前显示器。
      // 它只用于更新显示器信息。
    }

    var newDisplay = Display();
    newDisplay.x = double.tryParse(evt['x']) ?? newDisplay.x;
    newDisplay.y = double.tryParse(evt['y']) ?? newDisplay.y;
    newDisplay.width = int.tryParse(evt['width']) ?? newDisplay.width;
    newDisplay.height = int.tryParse(evt['height']) ?? newDisplay.height;
    newDisplay.cursorEmbedded = int.tryParse(evt['cursor_embedded']) == 1;
    newDisplay.originalWidth = int.tryParse(
            evt['original_width'] ?? kInvalidResolutionValue.toString()) ??
        kInvalidResolutionValue;
    newDisplay.originalHeight = int.tryParse(
            evt['original_height'] ?? kInvalidResolutionValue.toString()) ??
        kInvalidResolutionValue;
    newDisplay._scale = _pi.scaleOfDisplay(display);
    _pi.displays[display] = newDisplay;

    if (!_pi.isSupportMultiUiSession || _pi.currentDisplay == display) {
      updateCurDisplay(sessionId);
    }

    if (!_pi.isSupportMultiUiSession) {
      try {
        CurrentDisplayState.find(peerId).value = display;
      } catch (e) {
        //
      }
    }

    if (!_pi.isSupportMultiUiSession || _pi.currentDisplay == display) {
      handleResolutions(peerId, evt['resolutions']);
    }
    notifyListeners();
  }

  cancelMsgBox(Map<String, dynamic> evt, SessionID sessionId) {
    if (parent.target == null) return;
    final dialogManager = parent.target!.dialogManager;
    final tag = '$sessionId-${evt['tag']}';
    dialogManager.dismissByTag(tag);
  }

  handleMultipleWindowsSession(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) {
    if (parent.target == null) return;
    final dialogManager = parent.target!.dialogManager;
    final sessions = evt['windows_sessions'];
    final title = translate('Multiple Windows sessions found');
    final text = translate('Please select the session you want to connect to');
    final type = "";

    showWindowsSessionsDialog(
        type, title, text, dialogManager, sessionId, peerId, sessions);
  }

  /// 根据 [evt] 和 [id] 处理消息框事件。
  handleMsgBox(Map<String, dynamic> evt, SessionID sessionId, String peerId) {
    if (parent.target == null) return;
    final dialogManager = parent.target!.dialogManager;
    final type = evt['type'];
    final title = evt['title'];
    final text = evt['text'];
    final link = evt['link'];

    // 对端消失的检测器是在 `restarting-show` 下重连，而不是以错误标题，所以
    // 这里也需要给它命名。按它自己的标题，而不是按类型：明确重启过的远端
    // 设备会通过本次改动未涉及的路径到达同样的类型。
    if (isAndroid &&
        _androidDocumentPickerActive &&
        (title == 'Connection Error' ||
            (type == 'restarting-show' && title == 'Connecting...'))) {
      _androidDocumentPickerInterruptedConnection = true;
      return;
    }

    // 对任何错误类型的消息都禁用相对鼠标模式，以确保光标被释放。
    // 这包括连接错误、会话结束消息、提权错误等。
    // 安全性：在出错时释放指针锁定，可以避免用户被卡住。
    if (title == 'Connection Error' ||
        type == 'error' ||
        type == 'restarting' ||
        (type is String && type.contains('error'))) {
      parent.target?.inputModel.setRelativeMouseMode(false);
    }

    if (type == 're-input-password') {
      wrongPasswordDialog(sessionId, dialogManager, type, title, text);
    } else if (type == 'input-2fa') {
      enter2FaDialog(sessionId, dialogManager);
    } else if (type == 'input-password') {
      enterPasswordDialog(sessionId, dialogManager);
    } else if (type == 'terminal-admin-login') {
      enterUserLoginDialog(
          sessionId, dialogManager, 'terminal-admin-login-tip');
    } else if (type == 'terminal-admin-login-password') {
      enterUserLoginAndPasswordDialog(
          sessionId, dialogManager, 'terminal-admin-login-tip');
    } else if (type == 'restarting') {
      // 把重启消息当作重连控制事件处理。Rust 仍然会发送
      // title/text 以便旧版界面和翻译复用；Flutter 会短暂保留最后一帧，
      // 然后显示 Connecting 浮层。
      if (_restartReconnectDelayTimer == null) {
        parent.target?.inputModel.setRelativeMouseMode(false);
        _cancelPendingMonitorRestore();
        bind.sessionReconnect(sessionId: sessionId, forceRelay: false);
        clearPermissions();
        // 在静默窗口之后再重试一次，让重启重连的尝试
        // 按经验上的短节奏间隔开，而不是只更新界面。
        _restartReconnectDelayTimer =
            Timer(Duration(seconds: _restartReconnectSilentDelaySecs), () {
          _restartReconnectDelayTimer = null;
          if (parent.target?.closed == true) {
            return;
          }
          reconnect(dialogManager, sessionId, false);
        });
      }
    } else if (type == 'restarting-show') {
      _restartReconnectDelayTimer?.cancel();
      _restartReconnectDelayTimer = null;
      reconnect(dialogManager, sessionId, false);
    } else if (type == 'wait-remote-accept-nook') {
      showWaitAcceptDialog(sessionId, type, title, text, dialogManager);
    } else if (type == 'on-uac' || type == 'on-foreground-elevated') {
      showOnBlockDialog(sessionId, type, title, text, dialogManager);
    } else if (type == 'wait-uac') {
      showWaitUacDialog(sessionId, dialogManager, type);
    } else if (type == 'elevation-error') {
      showElevationError(sessionId, type, title, text, dialogManager);
    } else if (type == 'relay-hint' || type == 'relay-hint2') {
      showRelayHintDialog(sessionId, type, title, text, dialogManager, peerId);
    } else if (text == kMsgboxTextWaitingForImage) {
      showConnectedWaitingForImage(dialogManager, sessionId, type, title, text);
    } else if (title == 'Privacy mode') {
      final hasRetry = evt['hasRetry'] == 'true';
      showPrivacyFailedDialog(
          sessionId, type, title, text, link, hasRetry, dialogManager);
    } else {
      var hasRetry = evt['hasRetry'] == 'true';
      if (!hasRetry) {
        hasRetry = shouldAutoRetryOnOffline(type, title, text);
      }
      showMsgBox(sessionId, type, title, text, link, hasRetry, dialogManager);
    }
  }

  void resetRestartReconnectState() {
    _restartReconnectDelayTimer?.cancel();
    _restartReconnectDelayTimer = null;
  }

  void beginAndroidDocumentPicker() {
    if (!isAndroid) return;
    _androidDocumentPickerActive = true;
    _androidDocumentPickerInterruptedConnection = false;
  }

  void endAndroidDocumentPicker() {
    if (!isAndroid) return;
    _androidDocumentPickerActive = false;
    if (!_androidDocumentPickerInterruptedConnection ||
        parent.target?.closed == true) {
      return;
    }
    _androidDocumentPickerInterruptedConnection = false;
    reconnect(parent.target!.dialogManager, sessionId, false);
  }

  /// 针对 "Remote desktop is offline" 错误的自动重试判断。
  /// 返回 true 表示自动重试，否则返回 false。
  bool shouldAutoRetryOnOffline(
    String type,
    String title,
    String text,
  ) {
    if (type == 'error' &&
        title == 'Connection Error' &&
        text == 'Remote desktop is offline' &&
        _pi.isSet.isTrue) {
      // 当被控端账号发生变化时，在约 30 秒内自动重试（服务器判定对端离线的阈值）
      // （例如注销、切换用户、登录操作系统）会通过 websocket/tcp 连接造成临时离线。
      // 实际等待时间可能超过 30 秒（例如已过 20 秒 + 下次重试 16 秒 = 36 秒），这是可以接受的
      // 因为账号变化后被控端会很快重新连接。
      // 使用基于时间的判断而不是 _reconnects 计数，因为用户可以手动重试。
      // https://github.com/rustdesk/rustdesk/discussions/14048
      if (_offlineReconnectStartTime == null) {
        // 首次离线，记录时间并开始重试
        _offlineReconnectStartTime = DateTime.now();
        return true;
      } else {
        final elapsed =
            DateTime.now().difference(_offlineReconnectStartTime!).inSeconds;
        if (elapsed < 30) {
          return true;
        }
      }
    }
    return false;
  }

  handleToast(Map<String, dynamic> evt, SessionID sessionId, String peerId) {
    final type = evt['type'] ?? 'info';
    final text = evt['text'] ?? '';
    final durMsc = evt['dur_msec'] ?? 2000;
    final duration = Duration(milliseconds: durMsc);
    if ((text).isEmpty) {
      BotToast.showLoading(
        duration: duration,
        clickClose: true,
        allowClick: true,
      );
    } else {
      if (type.contains('error')) {
        BotToast.showText(
          contentColor: Colors.red,
          text: translate(text),
          duration: duration,
          clickClose: true,
          onlyOne: true,
        );
      } else {
        BotToast.showText(
          text: translate(text),
          duration: duration,
          clickClose: true,
          onlyOne: true,
        );
      }
    }
  }

  /// 显示带有 [type]、[title] 和 [text] 的消息框。
  showMsgBox(SessionID sessionId, String type, String title, String text,
      String link, bool hasRetry, OverlayDialogManager dialogManager,
      {bool? hasCancel}) async {
    final noteAllowed = parent.target != null &&
        allowAskForNoteAtEndOfConnection(parent.target, false) &&
        (title == "Connection Error" || type == "restarting");
    final showNoteEdit = noteAllowed && !hasRetry;
    if (showNoteEdit) {
      await showConnEndAuditDialogCloseCanceled(
          ffi: parent.target!, type: type, title: title, text: text);
      closeConnection();
    } else {
      VoidCallback? onSubmit;
      if (noteAllowed && hasRetry) {
        final ffi = parent.target!;
        onSubmit = () async {
          _timer?.cancel();
          _timer = null;
          await showConnEndAuditDialogCloseCanceled(
              ffi: ffi, type: type, title: title, text: text);
          closeConnection();
        };
      }
      msgBox(sessionId, type, title, text, link, dialogManager,
          hasCancel: hasCancel,
          reconnect: hasRetry ? reconnect : null,
          reconnectTimeout: hasRetry ? _reconnects : null,
          onSubmit: onSubmit);
    }
    _timer?.cancel();
    if (hasRetry) {
      _timer = Timer(Duration(seconds: _reconnects), () {
        reconnect(dialogManager, sessionId, false);
      });
      _reconnects *= 2;
    } else {
      _reconnects = 1;
      _offlineReconnectStartTime = null;
    }
  }

  void _cancelPendingMonitorRestore() {
    _pendingRestoreTimer?.cancel();
    _pendingRestoreTimer = null;
    pendingMonitorRestore = null;
  }

  void cancelPendingRestoreTimer() {
    _pendingRestoreTimer?.cancel();
    _pendingRestoreTimer = null;
  }

  void reconnect(OverlayDialogManager dialogManager, SessionID sessionId,
      bool forceRelay) {
    // 在重连前禁用相对鼠标模式，以确保光标被释放。
    parent.target?.inputModel.setRelativeMouseMode(false);
    _cancelPendingMonitorRestore();
    bind.sessionReconnect(sessionId: sessionId, forceRelay: forceRelay);
    clearPermissions();
    dialogManager.dismissAll();
    dialogManager.showLoading(translate('Connecting...'),
        onCancel: closeConnection);
  }

  Future<void> showRelayHintDialog(
      SessionID sessionId,
      String type,
      String title,
      String text,
      OverlayDialogManager dialogManager,
      String peerId) async {
    var hint = "\n\n${translate('relay_hint_tip')}";
    if (text.contains("10054") || text.contains("104")) {
      hint = "";
    }
    final text2 = "${translate(text)}$hint";

    if (parent.target != null &&
        allowAskForNoteAtEndOfConnection(parent.target, false) &&
        pi.isSet.isTrue) {
      if (await showConnEndAuditDialogCloseCanceled(
          ffi: parent.target!, type: type, title: title, text: text2)) {
        return;
      }
      closeConnection();
      return;
    }

    dialogManager.show(tag: '$sessionId-$type', (setState, close, context) {
      onClose() {
        closeConnection();
        close();
      }

      final style =
          ElevatedButton.styleFrom(backgroundColor: Colors.green[700]);

      return CustomAlertDialog(
        title: null,
        content: msgboxContent(type, title, text2),
        actions: [
          dialogButton('Close', onPressed: onClose, isOutline: true),
          if (type == 'relay-hint')
            dialogButton('Connect via relay',
                onPressed: () => reconnect(dialogManager, sessionId, true),
                buttonStyle: style,
                isOutline: true),
          dialogButton('Retry',
              onPressed: () => reconnect(dialogManager, sessionId, false)),
          if (type == 'relay-hint2')
            dialogButton('Connect via relay',
                onPressed: () => reconnect(dialogManager, sessionId, true),
                buttonStyle: style),
        ],
        onCancel: onClose,
      );
    });
  }

  void showConnectedWaitingForImage(OverlayDialogManager dialogManager,
      SessionID sessionId, String type, String title, String text) {
    onClose() {
      closeConnection();
    }

    if (waitForFirstImage.isFalse) return;
    dialogManager.show(
      (setState, close, context) => CustomAlertDialog(
          title: null,
          content: SelectionArea(child: msgboxContent(type, title, text)),
          actions: [
            dialogButton("Cancel", onPressed: onClose, isOutline: true)
          ],
          onCancel: onClose),
      tag: '$sessionId-waiting-for-image',
    );
    waitForImageDialogShow.value = true;
    waitForImageTimer = Timer(Duration(milliseconds: 1500), () {
      if (waitForFirstImage.isTrue && !isRefreshing) {
        bind.sessionInputOsPassword(sessionId: sessionId, value: '');
      }
    });
    bind.sessionOnWaitingForImageDialogShow(sessionId: sessionId);
  }

  void showPrivacyFailedDialog(
      SessionID sessionId,
      String type,
      String title,
      String text,
      String link,
      bool hasRetry,
      OverlayDialogManager dialogManager) {
    // 远端发生了显示器变化，
    // 这会导致某些消息刷新画布并关闭对话框。
    // 所以这里加一点延迟，确保对话框能够显示出来。
    Future.delayed(Duration(milliseconds: 3000), () {
      showMsgBox(sessionId, type, title, text, link, hasRetry, dialogManager);
    });
  }

  _updateSessionWidthHeight(SessionID sessionId) {
    if (_rect == null) return;
    if (_rect!.width <= 0 || _rect!.height <= 0) {
      debugPrintStack(
          label: 'invalid display size (${_rect!.width},${_rect!.height})');
    } else {
      final displays = _pi.getCurDisplays();
      if (displays.length == 1) {
        bind.sessionSetSize(
          sessionId: sessionId,
          display:
              pi.currentDisplay == kAllDisplayValue ? 0 : pi.currentDisplay,
          width: displays[0].width,
          height: displays[0].height,
        );
      } else {
        for (int i = 0; i < displays.length; ++i) {
          bind.sessionSetSize(
            sessionId: sessionId,
            display: i,
            width: displays[i].width,
            height: displays[i].height,
          );
        }
      }
    }
  }

  void _queryAuditGuid(String peerId) async {
    try {
      if (bind.isDisableAccount()) {
        return;
      }
      if (bind
          .sessionGetAuditServerSync(sessionId: sessionId, typ: "conn/active")
          .isEmpty) {
        return;
      }
      if (!mainGetLocalBoolOptionSync(
          kOptionAllowAskForNoteAtEndOfConnection)) {
        return;
      }
      if (bind.sessionGetAuditGuid(sessionId: sessionId).isNotEmpty) {
        debugPrint('Get cached audit GUID');
        return;
      }
      final url = bind.sessionGetAuditServerSync(
          sessionId: sessionId, typ: "conn/active");
      if (url.isEmpty) {
        return;
      }
      final initialConnSessionId =
          bind.sessionGetConnSessionId(sessionId: sessionId);
      final connType = switch (parent.target?.connType) {
        ConnType.defaultConn => 0,
        ConnType.fileTransfer => 1,
        ConnType.portForward => 2,
        ConnType.rdp => 2,
        ConnType.viewCamera => 3,
        ConnType.terminal => 4,
        _ => 0,
      };

      const retryIntervals = [1, 1, 2, 2, 3, 3];

      for (int attempt = 1; attempt <= retryIntervals.length; attempt++) {
        final currentConnSessionId =
            bind.sessionGetConnSessionId(sessionId: sessionId);
        if (currentConnSessionId != initialConnSessionId) {
          debugPrint('connSessionId changed, stopping audit GUID query');
          return;
        }

        final fullUrl =
            '$url?id=$peerId&session_id=$currentConnSessionId&conn_type=$connType';

        debugPrint(
            'Querying audit GUID, attempt $attempt/${retryIntervals.length}');
        try {
          var headers = getHttpHeaders();
          headers['Content-Type'] = "application/json";

          final response = await http.get(
            Uri.parse(fullUrl),
            headers: headers,
          );

          if (response.statusCode == 200) {
            final guid = jsonDecode(response.body) as String?;
            if (guid != null && guid.isNotEmpty) {
              bind.sessionSetAuditGuid(sessionId: sessionId, guid: guid);
              debugPrint('Successfully retrieved audit GUID');
              return;
            }
          } else {
            debugPrint(
                'Failed to query audit GUID. Status: ${response.statusCode}, Body: ${response.body}');
            return;
          }
        } catch (e) {
          debugPrint('Error querying audit GUID (attempt $attempt): $e');
        }

        if (attempt < retryIntervals.length) {
          await Future.delayed(Duration(seconds: retryIntervals[attempt - 1]));
        }
      }

      debugPrint(
          'Failed to retrieve audit GUID after ${retryIntervals.length} attempts');
    } catch (e) {
      debugPrint('Error in _queryAuditGuid: $e');
    }
  }

  /// 根据 [evt] 处理设备信息事件。
  handlePeerInfo(Map<String, dynamic> evt, String peerId, bool isCache) async {
    parent.target?.chatModel.voiceCallStatus.value = VoiceCallStatus.notStarted;

    _queryAuditGuid(peerId);

    // 这里必须克隆 Map，否则 "evt" 可能会被其他线程通过引用修改。
    // 因为该函数是异步的，里面有一个 "await"。
    cachedPeerData.peerInfo = {...evt};
    // 不要缓存分辨率，因为新的显示器连接可能有不同的分辨率。
    cachedPeerData.peerInfo.remove('resolutions');

    // 最近使用的设备由 handle_peer_info(ui_session_interface.rs) --> handle_peer_info(client.rs) --> save_config(client.rs) 更新
    bind.mainLoadRecentPeers();

    parent.target?.dialogManager.dismissAll();
    _pi.version = evt['version'];
    // 注意：连接时不会自动启用相对鼠标模式。
    // 用户必须通过工具栏或键盘快捷键（Ctrl+Alt+Shift+M）手动启用。
    //
    // 对于桌面端/webDesktop，键盘模式的初始化稍后由
    // checkDesktopKeyboardMode() 处理，如果当前模式不受支持它可能会改变模式，
    // 随后由 updateKeyboardMode() 同步 InputModel.keyboardMode。
    // 对于移动端，updateKeyboardMode() 目前是空操作（只在桌面端/web 上执行），
    // 但这里仍然调用它，以保持一致性并为将来做准备。
    if (isMobile) {
      parent.target?.inputModel.updateKeyboardMode();
    }
    _pi.isSupportMultiUiSession =
        bind.isSupportMultiUiSession(version: _pi.version);
    _pi.username = evt['username'];
    _pi.hostname = evt['hostname'];
    _pi.platform = evt['platform'];
    _pi.sasEnabled = evt['sas_enabled'] == 'true';
    final currentDisplay = int.parse(evt['current_display']);
    if (_pi.primaryDisplay == kInvalidDisplayIndex) {
      _pi.primaryDisplay = currentDisplay;
    }

    if (bind.peerGetSessionsCount(
            id: peerId, connType: parent.target!.connType.index) <=
        1) {
      _pi.currentDisplay = currentDisplay;
    }

    try {
      CurrentDisplayState.find(peerId).value = _pi.currentDisplay;
    } catch (e) {
      //
    }

    final connType = parent.target?.connType;
    if (isPeerAndroid) {
      _touchMode = true;
    } else {
      // `kOptionTouchMode` 原本是对端选项，但后来被移到了本地选项。
      // 我们先检查本地选项，如果没设置，再检查对端选项。
      // 因为如果本地选项不为空：
      // 1. 用户显式设置了触摸模式。
      // 2. 高级选项（自定义客户端）已设置。
      //    那么我们就选择使用本地选项。
      final optLocal = bind.mainGetLocalOption(key: kOptionTouchMode);
      if (optLocal != '') {
        _touchMode = optLocal == 'Y';
      } else {
        final optSession = await bind.sessionGetOption(
            sessionId: sessionId, arg: kOptionTouchMode);
        _touchMode = optSession != '';
      }
    }
    if (isMobile) {
      virtualMouseMode.loadOptions();
    }
    if (connType == ConnType.fileTransfer) {
      parent.target?.fileModel.onReady();
    } else if (connType == ConnType.terminal) {
      // 对所有已注册的终端模型调用 onReady
      final models = parent.target?._terminalModels.values ?? [];
      for (final model in models) {
        model.onReady();
      }
    } else if (connType == ConnType.defaultConn ||
        connType == ConnType.viewCamera) {
      List<Display> newDisplays = [];
      List<dynamic> displays = json.decode(evt['displays']);
      for (int i = 0; i < displays.length; ++i) {
        newDisplays.add(evtToDisplay(displays[i]));
      }
      _pi.displays.value = newDisplays;
      _pi.displaysCount.value = _pi.displays.length;
      if (_pi.currentDisplay < _pi.displays.length) {
        // 现在已改为 _updateCurDisplay
        updateCurDisplay(sessionId);
      }
      // 重连后，等画布就绪再恢复上次选择的显示器。
      // 如果显示器尺寸不同，提前切换会导致视图偏移。
      // 新的显示器窗口在 currentDisplay 中保留各自的选择。
      final last = lastUserDisplay ?? _pi.currentDisplay;
      pendingMonitorRestore = (!isCache &&
              last != currentDisplay &&
              bind.sessionGetUseAllMyDisplaysForTheRemoteSession(
                      sessionId: sessionId) !=
                  'Y' &&
              ((last == kAllDisplayValue && _pi.displays.isNotEmpty) ||
                  (last >= 0 && last < _pi.displays.length)))
          ? last
          : null;
      // 如果第一个图像事件始终没有到达该标签页（多 UI）时的回退处理。
      _pendingRestoreTimer?.cancel();
      if (pendingMonitorRestore != null) {
        _pendingRestoreTimer = Timer(const Duration(milliseconds: 1500),
            () => parent.target?._applyPendingMonitorRestore());
      }
      if (displays.isNotEmpty) {
        _reconnects = 1;
        _offlineReconnectStartTime = null;
        resetRestartReconnectState();
        waitForFirstImage.value = true;
        isRefreshing = false;
      }
      Map<String, dynamic> features = json.decode(evt['features']);
      _pi.features.privacyMode = features['privacy_mode'] == true;
      if (!isCache) {
        handleResolutions(peerId, evt["resolutions"]);
      }
      parent.target?.elevationModel.onPeerInfo(_pi);
    }
    if (connType == ConnType.defaultConn) {
      setViewOnly(
          peerId,
          bind.sessionGetToggleOptionSync(
              sessionId: sessionId, arg: kOptionToggleViewOnly));
      setShowMyCursor(bind.sessionGetToggleOptionSync(
          sessionId: sessionId, arg: kOptionToggleShowMyCursor));
    }
    if (connType == ConnType.defaultConn || connType == ConnType.viewCamera) {
      final platformAdditions = evt['platform_additions'];
      if (platformAdditions != null && platformAdditions != '') {
        try {
          _pi.platformAdditions = json.decode(platformAdditions);
        } catch (e) {
          debugPrint('Failed to decode platformAdditions $e');
        }
      }
    }

    _pi.isSet.value = true;
    stateGlobal.resetLastResolutionGroupValues(peerId);

    if (isDesktop || isWebDesktop) {
      // 如果当前键盘模式不受支持，checkDesktopKeyboardMode 可能会改变键盘模式。
      // 之后重新同步 InputModel.keyboardMode。
      // 注意：updateKeyboardMode() 在移动端是空操作（提前返回）。
      await checkDesktopKeyboardMode();
      await parent.target?.inputModel.updateKeyboardMode();
    }

    notifyListeners();

    if (!isCache) {
      tryUseAllMyDisplaysForTheRemoteSession(peerId);
    }
  }

  checkDesktopKeyboardMode() async {
    if (isInputSourceFlutter) {
      // 本地端，flutter 键盘输入源
      // 目前只支持 map 模式，legacy 模式用于兼容。
      for (final mode in [kKeyMapMode, kKeyLegacyMode]) {
        if (bind.sessionIsKeyboardModeSupported(
            sessionId: sessionId, mode: mode)) {
          await bind.sessionSetKeyboardMode(sessionId: sessionId, value: mode);
          break;
        }
      }
    } else {
      final curMode = await bind.sessionGetKeyboardMode(sessionId: sessionId);
      if (curMode != null) {
        if (bind.sessionIsKeyboardModeSupported(
            sessionId: sessionId, mode: curMode)) {
          return;
        }
      }

      // 如果当前键盘模式不受支持，就切换到另一个。
      for (final mode in [kKeyMapMode, kKeyTranslateMode, kKeyLegacyMode]) {
        if (bind.sessionIsKeyboardModeSupported(
            sessionId: sessionId, mode: mode)) {
          bind.sessionSetKeyboardMode(sessionId: sessionId, value: mode);
          break;
        }
      }
    }
  }

  tryUseAllMyDisplaysForTheRemoteSession(String peerId) async {
    if (bind.sessionGetUseAllMyDisplaysForTheRemoteSession(
            sessionId: sessionId) !=
        'Y') {
      return;
    }

    if (!_pi.isSupportMultiDisplay || _pi.displays.length <= 1) {
      return;
    }

    final screenRectList = await getScreenRectList();
    if (screenRectList.length <= 1) {
      return;
    }

    // 待办：对端的 currentDisplay 是主显示器，但主显示器未必是第一块显示器。
    // 本地的主显示器同样未必是第一块。
    //
    // 目前这里假定 0 是主显示器。

    // 移动到第一块显示器并设为全屏
    bind.sessionSwitchDisplay(
      isDesktop: isDesktop,
      sessionId: sessionId,
      value: Int32List.fromList([0]),
    );
    _pi.currentDisplay = 0;
    try {
      CurrentDisplayState.find(peerId).value = _pi.currentDisplay;
    } catch (e) {
      //
    }
    await tryMoveToScreenAndSetFullscreen(screenRectList[0]);

    final length = _pi.displays.length < screenRectList.length
        ? _pi.displays.length
        : screenRectList.length;
    for (var i = 1; i < length; i++) {
      openMonitorInNewTabOrWindow(i, peerId, _pi,
          screenRect: screenRectList[i]);
    }
  }

  tryShowAndroidActionsOverlay({int delayMSecs = 10}) {
    if (isPeerAndroid) {
      if (parent.target?.connType == ConnType.defaultConn &&
          parent.target != null &&
          parent.target!.ffiModel.permissions['keyboard'] != false) {
        Timer(Duration(milliseconds: delayMSecs), () {
          if (parent.target!.dialogManager.mobileActionsOverlayVisible.isTrue) {
            parent.target!.dialogManager
                .showMobileActionsOverlay(ffi: parent.target!);
          }
        });
      }
    }
  }

  handleResolutions(String id, dynamic resolutions) {
    try {
      final resolutionsObj = json.decode(resolutions as String);
      late List<dynamic> dynamicArray;
      if (resolutionsObj is Map) {
        // web 版本
        dynamicArray = (resolutionsObj as Map<String, dynamic>)['resolutions']
            as List<dynamic>;
      } else {
        // rust 版本
        dynamicArray = resolutionsObj as List<dynamic>;
      }
      List<Resolution> arr = List.empty(growable: true);
      for (int i = 0; i < dynamicArray.length; i++) {
        var width = dynamicArray[i]["width"];
        var height = dynamicArray[i]["height"];
        if (width is int && width > 0 && height is int && height > 0) {
          arr.add(Resolution(width, height));
        }
      }
      arr.sort((a, b) {
        if (b.width != a.width) {
          return b.width - a.width;
        } else {
          return b.height - a.height;
        }
      });
      _pi.resolutions = arr;
    } catch (e) {
      debugPrint("Failed to parse resolutions:$e");
    }
  }

  Display evtToDisplay(Map<String, dynamic> evt) {
    var d = Display();
    d.x = evt['x']?.toDouble() ?? d.x;
    d.y = evt['y']?.toDouble() ?? d.y;
    d.width = evt['width'] ?? d.width;
    d.height = evt['height'] ?? d.height;
    d.cursorEmbedded = evt['cursor_embedded'] == 1;
    d.originalWidth = evt['original_width'] ?? kInvalidResolutionValue;
    d.originalHeight = evt['original_height'] ?? kInvalidResolutionValue;
    d._scale = 1.0;
    final scaledWidth = evt['scaled_width'];
    if (scaledWidth != null) {
      final sw = int.tryParse(scaledWidth.toString());
      if (sw != null && sw > 0 && d.width > 0) {
        d._scale = max(d.width.toDouble() / sw, 1.0);
      } else {
        debugPrint(
            "Invalid scaled_width ($scaledWidth) or width (${d.width}), using default scale 1.0");
      }
    }
    return d;
  }

  updateLastCursorId(Map<String, dynamic> evt) {
    // int.parse(evt['id']) 可能导致 FormatException
    // Unhandled Exception: FormatException: Positive input exceeds the limit of integer 18446744071749110741
    parent.target?.cursorModel.id = evt['id'];
  }

  handleCursorId(Map<String, dynamic> evt) {
    cachedPeerData.lastCursorId = evt;
    parent.target?.cursorModel.updateCursorId(evt);
  }

  /// 到达的形状就是正在使用的形状，就像 cursor_id 一样。
  handleCursorData(String id, int hotx, int hoty, int width, int height,
      Uint8List colors) async {
    // 无论形状以什么顺序重放，重放时最后选中的都是这个。
    cachedPeerData.lastCursorId = {'id': id};
    parent.target?.cursorModel.id = id;
    await parent.target?.cursorModel
        .updateCursorData(id, hotx, hoty, width, height, colors);
  }

  /// 根据 [evt] 处理设备信息同步事件。
  handleSyncPeerInfo(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) async {
    if (evt['displays'] != null) {
      cachedPeerData.peerInfo['displays'] = evt['displays'];
      List<dynamic> displays = json.decode(evt['displays']);
      List<Display> newDisplays = [];
      for (int i = 0; i < displays.length; ++i) {
        newDisplays.add(evtToDisplay(displays[i]));
      }
      _pi.displays.value = newDisplays;
      _pi.displaysCount.value = _pi.displays.length;

      if (_pi.currentDisplay == kAllDisplayValue) {
        updateCurDisplay(sessionId);
        // 待办：如果显示器发生变化怎么办？
      } else {
        if (_pi.currentDisplay >= 0 &&
            _pi.currentDisplay < _pi.displays.length) {
          updateCurDisplay(sessionId);
        } else {
          if (_pi.displays.isNotEmpty) {
            // 通知切换显示器
            msgBox(sessionId, 'custom-nook-nocancel-hasclose-info', 'Prompt',
                'display_is_plugged_out_msg', '', parent.target!.dialogManager);
            final isPeerPrimaryDisplayValid =
                pi.primaryDisplay == kInvalidDisplayIndex ||
                    pi.primaryDisplay >= pi.displays.length;
            final newDisplay =
                isPeerPrimaryDisplayValid ? 0 : pi.primaryDisplay;
            bind.sessionSwitchDisplay(
              isDesktop: isDesktop,
              sessionId: sessionId,
              value: Int32List.fromList([newDisplay]),
            );

            if (_pi.isSupportMultiUiSession) {
              // 如果对端支持 multi-ui-session，就不会回传 switch display 消息。
              // 我们需要手动更新显示器。
              switchToNewDisplay(newDisplay, sessionId, peerId);
            }
          } else {
            msgBox(sessionId, 'nocancel-error', 'Prompt', 'No Displays', '',
                parent.target!.dialogManager);
          }
        }
      }
    }
    parent.target!.canvasModel
        .tryUpdateScrollStyle(Duration(milliseconds: 300), null);
    notifyListeners();
  }

  handlePlatformAdditions(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) async {
    final updateData = evt['platform_additions'] as String?;
    if (updateData == null) {
      return;
    }

    if (updateData.isEmpty) {
      _pi.platformAdditions.remove(kPlatformAdditionsRustDeskVirtualDisplays);
      _pi.platformAdditions.remove(kPlatformAdditionsAmyuniVirtualDisplays);
    } else {
      try {
        final updateJson = json.decode(updateData) as Map<String, dynamic>;
        for (final key in updateJson.keys) {
          _pi.platformAdditions[key] = updateJson[key];
        }
        if (!updateJson
            .containsKey(kPlatformAdditionsRustDeskVirtualDisplays)) {
          _pi.platformAdditions
              .remove(kPlatformAdditionsRustDeskVirtualDisplays);
        }
        if (!updateJson.containsKey(kPlatformAdditionsAmyuniVirtualDisplays)) {
          _pi.platformAdditions.remove(kPlatformAdditionsAmyuniVirtualDisplays);
        }
      } catch (e) {
        debugPrint('Failed to decode platformAdditions $e');
      }
    }

    cachedPeerData.peerInfo['platform_additions'] =
        json.encode(_pi.platformAdditions);
  }

  handleFollowCurrentDisplay(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) async {
    if (evt['display_idx'] != null) {
      if (pi.currentDisplay == kAllDisplayValue) {
        return;
      }
      _pi.currentDisplay = int.parse(evt['display_idx']);
      try {
        CurrentDisplayState.find(peerId).value = _pi.currentDisplay;
      } catch (e) {
        //
      }
      bind.sessionSwitchDisplay(
        isDesktop: isDesktop,
        sessionId: sessionId,
        value: Int32List.fromList([_pi.currentDisplay]),
      );
    }
    notifyListeners();
  }

  // 不等待响应，直接切换到新显示器。
  switchToNewDisplay(int display, SessionID sessionId, String peerId,
      {bool updateCursorPos = false}) {
    // 无需等待响应
    pi.currentDisplay = display;
    updateCurDisplay(sessionId, updateCursorPos: updateCursorPos);
    try {
      CurrentDisplayState.find(peerId).value = display;
    } catch (e) {
      //
    }
  }

  updateBlockInputState(Map<String, dynamic> evt, String peerId) {
    _inputBlocked = evt['input_state'] == 'on';
    notifyListeners();
    try {
      BlockInputState.find(peerId).value = evt['input_state'] == 'on';
    } catch (e) {
      //
    }
  }

  updatePrivacyMode(
      Map<String, dynamic> evt, SessionID sessionId, String peerId) async {
    notifyListeners();
    try {
      final isOn = bind.sessionGetToggleOptionSync(
          sessionId: sessionId, arg: 'privacy-mode');
      if (isOn) {
        var privacyModeImpl = await bind.sessionGetOption(
            sessionId: sessionId, arg: 'privacy-mode-impl-key');
        // 为了兼容版本 < 1.2.4，默认值为 'privacy_mode_impl_mag'。
        final initDefaultPrivacyMode = 'privacy_mode_impl_mag';
        PrivacyModeState.find(peerId).value =
            privacyModeImpl ?? initDefaultPrivacyMode;
      } else {
        PrivacyModeState.find(peerId).value = '';
      }
    } catch (e) {
      //
    }
  }

  void setViewOnly(String id, bool value) {
    if (versionCmp(_pi.version, '1.2.0') < 0) return;
    // 针对 https://github.com/rustdesk/rustdesk/pull/3706#issuecomment-1481242389 的临时修复
    // 因为下面的 rx 在移动端版本中未被使用，所以没有初始化，下面的代码会导致崩溃
    // 目前我们的 flutter 代码质量简直烂透了。!!!!!!!!!!!!!!!!
    try {
      if (value) {
        ShowRemoteCursorState.find(id).value = value;
      } else {
        ShowRemoteCursorState.find(id).value = bind.sessionGetToggleOptionSync(
            sessionId: sessionId, arg: 'show-remote-cursor');
      }
    } catch (e) {
      //
    }
    if (_viewOnly != value) {
      _viewOnly = value;
      notifyListeners();
    }
  }

  void setShowMyCursor(bool value) {
    if (_showMyCursor != value) {
      _showMyCursor = value;
      notifyListeners();
    }
  }
}

class VirtualMouseMode with ChangeNotifier {
  bool _showVirtualMouse = false;
  double _virtualMouseScale = 1.0;
  bool _showVirtualJoystick = false;

  bool get showVirtualMouse => _showVirtualMouse;
  double get virtualMouseScale => _virtualMouseScale;
  bool get showVirtualJoystick => _showVirtualJoystick;

  FfiModel ffiModel;

  VirtualMouseMode(this.ffiModel);

  bool _shouldShow() => !ffiModel.isPeerAndroid;

  setShowVirtualMouse(bool b) {
    if (b == _showVirtualMouse) return;
    if (_shouldShow()) {
      _showVirtualMouse = b;
      notifyListeners();
    }
  }

  setVirtualMouseScale(double s) {
    if (s <= 0) return;
    if (s == _virtualMouseScale) return;
    _virtualMouseScale = s;
    bind.mainSetLocalOption(key: kOptionVirtualMouseScale, value: s.toString());
    notifyListeners();
  }

  setShowVirtualJoystick(bool b) {
    if (b == _showVirtualJoystick) return;
    if (_shouldShow()) {
      _showVirtualJoystick = b;
      notifyListeners();
    }
  }

  void loadOptions() {
    _showVirtualMouse =
        bind.mainGetLocalOption(key: kOptionShowVirtualMouse) == 'Y';
    _virtualMouseScale = double.tryParse(
            bind.mainGetLocalOption(key: kOptionVirtualMouseScale)) ??
        1.0;
    _showVirtualJoystick =
        bind.mainGetLocalOption(key: kOptionShowVirtualJoystick) == 'Y';
    notifyListeners();
  }

  Future<void> toggleVirtualMouse() async {
    await bind.mainSetLocalOption(
        key: kOptionShowVirtualMouse, value: showVirtualMouse ? 'N' : 'Y');
    setShowVirtualMouse(
        bind.mainGetLocalOption(key: kOptionShowVirtualMouse) == 'Y');
  }

  Future<void> toggleVirtualJoystick() async {
    await bind.mainSetLocalOption(
        key: kOptionShowVirtualJoystick,
        value: showVirtualJoystick ? 'N' : 'Y');
    setShowVirtualJoystick(
        bind.mainGetLocalOption(key: kOptionShowVirtualJoystick) == 'Y');
  }
}

class ImageModel with ChangeNotifier {
  ui.Image? _image;

  ui.Image? get image => _image;

  String id = '';

  late final SessionID sessionId;

  bool _useTextureRender = false;

  WeakReference<FFI> parent;

  final List<Function(String)> callbacksOnFirstImage = [];

  ImageModel(this.parent) {
    sessionId = parent.target!.sessionId;
  }

  get useTextureRender => _useTextureRender;

  addCallbackOnFirstImage(Function(String) cb) => callbacksOnFirstImage.add(cb);

  clearImage() => _image = null;

  bool _webDecodingRgba = false;
  final List<Uint8List> _webRgbaList = List.empty(growable: true);
  webOnRgba(int display, Uint8List rgba) async {
    // 需要深拷贝，否则会报 "instantiateCodec failed: TypeError: Cannot perform Construct on a detached ArrayBuffer"
    _webRgbaList.add(Uint8List.fromList(rgba));
    if (_webDecodingRgba) {
      return;
    }
    _webDecodingRgba = true;
    try {
      while (_webRgbaList.isNotEmpty) {
        final rgba2 = _webRgbaList.last;
        _webRgbaList.clear();
        await decodeAndUpdate(display, rgba2);
      }
    } catch (e) {
      debugPrint('onRgba error: $e');
    }
    _webDecodingRgba = false;
  }

  onRgba(int display, Uint8List rgba) async {
    try {
      await decodeAndUpdate(display, rgba);
    } catch (e) {
      debugPrint('onRgba error: $e');
    }
    platformFFI.nextRgba(sessionId, display);
  }

  // 仅 web：图像已经从解码后的 WebCodecs 帧创建好了
  Future<void> onImage(
      int display, ui.Image image, bool Function() isCurrentSession) async {
    await update(image, isCurrentSession: isCurrentSession);
  }

  decodeAndUpdate(int display, Uint8List rgba) async {
    final pid = parent.target?.id;
    final rect = parent.target?.ffiModel.pi.getDisplayRect(display);
    final image = await img.decodeImageFromPixels(
      rgba,
      rect?.width.toInt() ?? 0,
      rect?.height.toInt() ?? 0,
      isWeb | isWindows | isLinux
          ? ui.PixelFormat.rgba8888
          : ui.PixelFormat.bgra8888,
    );
    if (parent.target?.id != pid) {
      image?.dispose();
      return;
    }
    await update(image);
  }

  Future<void> update(ui.Image? image,
      {bool Function()? isCurrentSession}) async {
    if (_disposeIfStale(image, isCurrentSession)) return;
    if (_image == null && image != null) {
      if (isDesktop || isWebDesktop) {
        await parent.target?.canvasModel.updateViewStyle();
        await parent.target?.canvasModel.updateScrollStyle();
        await parent.target?.canvasModel.initializeEdgeScrollEdgeThickness();
      }
      if (parent.target != null) {
        await initializeCursorAndCanvas(parent.target!);
      }
    }
    if (_disposeIfStale(image, isCurrentSession)) return;
    _image?.dispose();
    _image = image;
    if (image != null) notifyListeners();
  }

  bool _disposeIfStale(ui.Image? image, bool Function()? isCurrentSession) {
    if (image == null || isCurrentSession == null) return false;
    if (isCurrentSession()) return false;
    image.dispose();
    return true;
  }

  // 仅移动端
  double get maxScale {
    if (_image == null) return 1.5;
    final size = parent.target!.canvasModel.getSize();
    final xscale = size.width / _image!.width;
    final yscale = size.height / _image!.height;
    return max(1.5, max(xscale, yscale));
  }

  // 仅移动端
  double get minScale {
    if (_image == null) return 1.5;
    final size = parent.target!.canvasModel.getSize();
    final xscale = size.width / _image!.width;
    final yscale = size.height / _image!.height;
    return min(xscale, yscale) / 1.5;
  }

  updateUserTextureRender() {
    final preValue = _useTextureRender;
    _useTextureRender = isDesktop && bind.mainGetUseTextureRender();
    if (preValue != _useTextureRender) {
      notifyListeners();
    }
  }

  setUseTextureRender(bool value) {
    _useTextureRender = value;
    notifyListeners();
  }

  void disposeImage() {
    _image?.dispose();
    _image = null;
  }
}

enum ScrollStyle {
  scrollbar(kRemoteScrollStyleBar),
  scrollauto(kRemoteScrollStyleAuto),
  scrolledge(kRemoteScrollStyleEdge);

  const ScrollStyle(this.stringValue);

  final String stringValue;

  String toJson() {
    return name;
  }

  static ScrollStyle fromJson(String json, [ScrollStyle? fallbackValue]) {
    switch (json) {
      case 'scrollbar':
        return scrollbar;
      case 'scrollauto':
        return scrollauto;
      case 'scrolledge':
        return scrolledge;
    }

    if (fallbackValue != null) {
      return fallbackValue;
    }

    throw ArgumentError("Unknown ScrollStyle JSON value: '$json'");
  }

  @override
  String toString() {
    return stringValue;
  }

  static ScrollStyle fromString(String string, [ScrollStyle? fallbackValue]) {
    switch (string) {
      case kRemoteScrollStyleBar:
        return scrollbar;
      case kRemoteScrollStyleAuto:
        return scrollauto;
      case kRemoteScrollStyleEdge:
        return scrolledge;
    }

    if (fallbackValue != null) {
      return fallbackValue;
    }

    throw ArgumentError("Unknown ScrollStyle string value: '$string'");
  }
}

class ViewStyle {
  final String style;
  final double width;
  final double height;
  final int displayWidth;
  final int displayHeight;
  ViewStyle({
    required this.style,
    required this.width,
    required this.height,
    required this.displayWidth,
    required this.displayHeight,
  });

  static defaultViewStyle() {
    final desktop = (isDesktop || isWebDesktop);
    final w =
        desktop ? kDesktopDefaultDisplayWidth : kMobileDefaultDisplayWidth;
    final h =
        desktop ? kDesktopDefaultDisplayHeight : kMobileDefaultDisplayHeight;
    return ViewStyle(
      style: '',
      width: w.toDouble(),
      height: h.toDouble(),
      displayWidth: w,
      displayHeight: h,
    );
  }

  static int _double2Int(double v) => (v * 100).round().toInt();

  @override
  bool operator ==(Object other) =>
      other is ViewStyle &&
      other.runtimeType == runtimeType &&
      _innerEqual(other);

  bool _innerEqual(ViewStyle other) {
    return style == other.style &&
        ViewStyle._double2Int(other.width) == ViewStyle._double2Int(width) &&
        ViewStyle._double2Int(other.height) == ViewStyle._double2Int(height) &&
        other.displayWidth == displayWidth &&
        other.displayHeight == displayHeight;
  }

  @override
  int get hashCode => Object.hash(
        style,
        ViewStyle._double2Int(width),
        ViewStyle._double2Int(height),
        displayWidth,
        displayHeight,
      ).hashCode;

  double get scale {
    double s = 1.0;
    if (style == kRemoteViewStyleAdaptive) {
      if (width != 0 &&
          height != 0 &&
          displayWidth != 0 &&
          displayHeight != 0) {
        final s1 = width / displayWidth;
        final s2 = height / displayHeight;
        s = s1 < s2 ? s1 : s2;
      }
    } else if (style == kRemoteViewStyleCustom) {
      // 自定义缩放是会话级别的，在 CanvasModel.updateViewStyle() 中应用
    }
    return s;
  }
}

enum EdgeScrollState {
  inactive,
  armed,
  active,
}

class EdgeScrollFallbackState {
  final CanvasModel _owner;

  late Ticker _ticker;

  Duration _lastTotalElapsed = Duration.zero;
  bool _nextEventIsFirst = true;
  Vector2 _encroachment = Vector2.zero();

  EdgeScrollFallbackState(this._owner, TickerProvider tickerProvider) {
    _ticker = tickerProvider.createTicker(emitTick);
  }

  void setEncroachment(Vector2 encroachment) {
    _encroachment = encroachment;
  }

  void emitTick(Duration totalElapsed) {
    if (_nextEventIsFirst) {
      _lastTotalElapsed = totalElapsed;
      _nextEventIsFirst = false;
    } else {
      final thisTickElapsed = totalElapsed - _lastTotalElapsed;

      const double kFrameTime = 1000.0 / 60.0;
      const double kSpeedFactor = 0.1;

      var delta = _encroachment *
          (kSpeedFactor * thisTickElapsed.inMilliseconds / kFrameTime);

      _owner.performEdgeScroll(delta);

      _lastTotalElapsed = totalElapsed;
    }
  }

  void start() {
    if (!_ticker.isActive) {
      _nextEventIsFirst = true;
      _ticker.start();
    }
  }

  void stop() {
    _ticker.stop();
  }
}

class CanvasModel with ChangeNotifier {
  // 画布的图像偏移
  double _x = 0;
  // 画布的图像偏移
  double _y = 0;
  // 图像缩放
  double _scale = 1.0;
  bool _locked = false;
  double _devicePixelRatio = 1.0;
  Size _size = Size.zero;
  // 位于图像上方的标签栏
  // double tabBarHeight = 0.0;
  // 窗口边框的宽度
  // double windowBorderWidth = 0.0;
  // 远端 ID
  String id = '';
  late final SessionID sessionId;
  // 滚动偏移 x 百分比
  double _scrollX = 0.0;
  // 滚动偏移 y 百分比
  double _scrollY = 0.0;
  ScrollStyle _scrollStyle = ScrollStyle.scrollauto;
  // 边缘滚动模式：当光标靠近视图边缘时触发滚动
  int _edgeScrollEdgeThickness = 100;
  // 跟踪边缘滚动是否应处于活动状态，避免在光标从外部进入视图时出现
  // 多余的滚动
  EdgeScrollState _edgeScrollState = EdgeScrollState.inactive;
  // Bump Mouse 不可用时的回退策略
  late EdgeScrollFallbackState _edgeScrollFallbackState;
  // 以免不断调用无用的 Bump Mouse
  bool _bumpMouseIsWorking = true;
  ViewStyle _lastViewStyle = ViewStyle.defaultViewStyle();

  Timer? _timerMobileFocusCanvasCursor;
  Timer? _timerMobileRestoreCanvasOffset;
  Offset? _offsetBeforeMobileSoftKeyboard;
  double? _scaleBeforeMobileSoftKeyboard;

  // `isMobileCanvasChanged` 用于避免画布被重置，
  // 即在显示软键盘之后切换输入法时。
  bool isMobileCanvasChanged = false;

  final ScrollController _horizontal = ScrollController();
  final ScrollController _vertical = ScrollController();

  final _imageOverflow = false.obs;

  WeakReference<FFI> parent;

  CanvasModel(this.parent) {
    sessionId = parent.target!.sessionId;
  }

  double get x => _x;
  double get y => _y;
  double get scale => _scale;
  bool get locked => _locked;
  double get devicePixelRatio => _devicePixelRatio;
  Size get size => _size;
  ScrollStyle get scrollStyle => _scrollStyle;
  ViewStyle get viewStyle => _lastViewStyle;
  RxBool get imageOverflow => _imageOverflow;

  void setLocked(bool value) {
    if (_locked == value) return;
    _locked = value;
    notifyListeners();
  }

  _resetScroll() => setScrollPercent(0.0, 0.0);

  void setScrollPercent(double x, double y) {
    _scrollX = x.isFinite ? x : 0.0;
    _scrollY = y.isFinite ? y : 0.0;
  }

  void pushScrollPositionToUI(double scrollPixelX, double scrollPixelY) {
    if (_horizontal.hasClients) {
      _horizontal.jumpTo(scrollPixelX);
    }
    if (_vertical.hasClients) {
      _vertical.jumpTo(scrollPixelY);
    }
  }

  ScrollController get scrollHorizontal => _horizontal;
  ScrollController get scrollVertical => _vertical;
  double get scrollX => _scrollX;
  double get scrollY => _scrollY;

  static double get leftToEdge =>
      isDesktop ? windowBorderWidth + kDragToResizeAreaPadding.left : 0;
  static double get rightToEdge =>
      isDesktop ? windowBorderWidth + kDragToResizeAreaPadding.right : 0;
  static double get topToEdge => isDesktop
      ? tabBarHeight + windowBorderWidth + kDragToResizeAreaPadding.top
      : 0;
  static double get bottomToEdge =>
      isDesktop ? windowBorderWidth + kDragToResizeAreaPadding.bottom : 0;

  Size getSize() {
    final mediaData = MediaQueryData.fromView(ui.window);
    final size = mediaData.size;
    // 如果已最小化，这里的 w 或 h 可能为负数。
    double w = size.width - leftToEdge - rightToEdge;
    double h = size.height - topToEdge - bottomToEdge;
    if (isMobile) {
      // 在两种方向下都考虑水平方向的安全区内边距。
      w = w - mediaData.padding.left - mediaData.padding.right;
      // 在垂直方向上，减去底部键盘内边距 (viewInsets.bottom) 以及任何
      // 底部浮层（例如按键帮助工具），以免画布被遮挡。
      h = h -
          mediaData.viewInsets.bottom -
          (parent.target?.cursorModel.keyHelpToolsRectToAdjustCanvas?.bottom ??
              0);
      // 按方向分别处理：
      //  - 竖屏：额外减去顶部内边距（例如状态栏 / 刘海）
      //  - 横屏：不减去 mediaData.padding.top/bottom（home indicator 会自动隐藏）
      final isPortrait = size.height > size.width;
      if (isPortrait) {
        // 在竖屏模式下，减去顶部安全区内边距（例如状态栏 / 刘海）
        // 以免远端图像被截断，同时保留底部内边距以避免
        // 在画布周围引入不必要的空白。
        //
        // iOS -> Android，竖屏，adjust 模式：
        // h = h（不减去内边距）：上下都会被截断
        // https://github.com/user-attachments/assets/30ed4559-c27e-432b-847f-8fec23c9f998
        // h = h - top - bottom：会出现额外的空白
        // https://github.com/user-attachments/assets/12a98817-3b4e-43aa-be0f-4b03cf364b7e
        // h = h - top（当前做法）：效果良好
        // https://github.com/user-attachments/assets/95f047f2-7f47-4a36-8113-5023989a0c81
        h = h - mediaData.padding.top;
      }
    }
    return Size(w < 0 ? 0 : w, h < 0 ? 0 : h);
  }

  // 仅移动端
  double getAdjustY() {
    final bottom =
        parent.target?.cursorModel.keyHelpToolsRectToAdjustCanvas?.bottom ?? 0;
    return max(bottom - MediaQueryData.fromView(ui.window).padding.top, 0);
  }

  updateSize() => _size = getSize();

  updateViewStyle({refreshMousePos = true, notify = true}) async {
    final style = await bind.sessionGetViewStyle(sessionId: sessionId);
    if (style == null) {
      return;
    }

    updateSize();
    final displayWidth = getDisplayWidth();
    final displayHeight = getDisplayHeight();
    final viewStyle = ViewStyle(
      style: style,
      width: size.width,
      height: size.height,
      displayWidth: displayWidth,
      displayHeight: displayHeight,
    );
    // 如果只有 Custom scale 百分比发生了变化，即使
    // 基本的 ViewStyle 字段相同，也继续更新。
    // 在 Custom scale 模式下，缩放百分比可以独立于其他
    // ViewStyle 字段变化，并且不会被相等性检查捕获。因此，我们必须
    // 在 style == kRemoteViewStyleCustom 时允许更新继续进行，即使
    // ViewStyle 的其余字段没有变化。
    if (_lastViewStyle == viewStyle &&
        _devicePixelRatio == ui.window.devicePixelRatio &&
        style != kRemoteViewStyleCustom) {
      return;
    }
    if (_lastViewStyle.style != viewStyle.style) {
      _resetScroll();
    }
    _lastViewStyle = viewStyle;
    _scale = viewStyle.scale;

    // 在 Custom 模式下应用自定义缩放百分比
    if (style == kRemoteViewStyleCustom) {
      try {
        _scale = await getSessionCustomScale(sessionId);
      } catch (e, stack) {
        debugPrint('Error in getSessionCustomScale: $e');
        debugPrintStack(stackTrace: stack);
        _scale = 1.0;
      }
    }

    _devicePixelRatio = ui.window.devicePixelRatio;
    if (kIgnoreDpi) {
      if (style == kRemoteViewStyleOriginal) {
        _scale = 1.0 / _devicePixelRatio;
      } else if (_scale != 0 && style == kRemoteViewStyleCustom) {
        _scale /= _devicePixelRatio;
      }
    }
    _resetCanvasOffset(displayWidth, displayHeight);
    final overflow = _x < 0 || y < 0;
    if (_imageOverflow.value != overflow) {
      _imageOverflow.value = overflow;
    }
    if (notify) {
      notifyListeners();
    }
    if (!isMobile && refreshMousePos) {
      parent.target?.inputModel.refreshMousePos();
    }
    tryUpdateScrollStyle(Duration.zero, style);
  }

  _resetCanvasOffset(int displayWidth, int displayHeight) {
    _x = (size.width - displayWidth * _scale) / 2;
    _y = (size.height - displayHeight * _scale) / 2;
    if (isMobile) {
      _moveToCenterCursor();
    }
  }

  tryUpdateScrollStyle(Duration duration, String? style) async {
    if (_scrollStyle == ScrollStyle.scrollauto) return;
    style ??= await bind.sessionGetViewStyle(sessionId: sessionId);
    if (style != kRemoteViewStyleOriginal && style != kRemoteViewStyleCustom) {
      return;
    }

    _resetScroll();

    Future.delayed(duration, () async {
      // 布局会更新滚动范围，并卸载不再需要的滚动条。
      await SchedulerBinding.instance.endOfFrame;
      updateScrollPercent();
    });
  }

  Future<void> updateScrollStyle() async {
    final style = await bind.sessionGetScrollStyle(sessionId: sessionId);

    _scrollStyle =
        style != null ? ScrollStyle.fromString(style) : ScrollStyle.scrollauto;

    if (_scrollStyle != ScrollStyle.scrollauto) {
      // 滚动条和 Scroll Edge 共享 controller，并保留各自的位置。
      updateScrollPercent();
    }

    notifyListeners();
  }

  Future<void> initializeEdgeScrollEdgeThickness() async {
    final savedValue =
        await bind.sessionGetEdgeScrollEdgeThickness(sessionId: sessionId);

    if (savedValue != null) {
      _edgeScrollEdgeThickness = savedValue;
    }
  }

  void updateEdgeScrollEdgeThickness(int newThickness) {
    _edgeScrollEdgeThickness = newThickness;
    notifyListeners();
  }

  void update(double x, double y, double scale) {
    _x = x;
    _y = y;
    _scale = scale;
    notifyListeners();
  }

  bool get cursorEmbedded =>
      parent.target?.ffiModel._pi.cursorEmbedded ?? false;

  int getDisplayWidth() {
    final defaultWidth = (isDesktop || isWebDesktop)
        ? kDesktopDefaultDisplayWidth
        : kMobileDefaultDisplayWidth;
    return parent.target?.ffiModel.rect?.width.toInt() ?? defaultWidth;
  }

  int getDisplayHeight() {
    final defaultHeight = (isDesktop || isWebDesktop)
        ? kDesktopDefaultDisplayHeight
        : kMobileDefaultDisplayHeight;
    return parent.target?.ffiModel.rect?.height.toInt() ?? defaultHeight;
  }

  static double get windowBorderWidth => stateGlobal.windowBorderWidth.value;
  static double get tabBarHeight => stateGlobal.tabBarHeight;

  void activateLocalCursor() {
    if (isDesktop || isWebDesktop) {
      try {
        RemoteCursorMovedState.find(id).value = false;
      } catch (e) {
        //
      }
    }
  }

  void updateLocalCursor(double x, double y) {
    if (parent.target?.ffiModel.viewOnly == true) return;
    // 如果没有键盘权限，鼠标移动时不要移动光标。
    if (parent.target != null && parent.target!.ffiModel.keyboard) {
      // 如果不是桌面端，则绘制光标。
      if (!(isDesktop || isWebDesktop)) {
        parent.target!.cursorModel.moveLocal(x, y);
      } else {
        try {
          RemoteCursorMovedState.find(id).value = false;
        } catch (e) {
          //
        }
      }
    }
  }

  void moveDesktopMouse(double x, double y) {
    if (size.width == 0 || size.height == 0) {
      return;
    }

    // 在移动平台上，让画布跟随光标移动。
    final dw = getDisplayWidth() * _scale;
    final dh = getDisplayHeight() * _scale;
    var dxOffset = 0;
    var dyOffset = 0;
    try {
      if (dw > size.width) {
        dxOffset = (x - dw * (x / size.width) - _x).toInt();
      }
      if (dh > size.height) {
        dyOffset = (y - dh * (y / size.height) - _y).toInt();
      }
    } catch (e) {
      debugPrintStack(
          label:
              '(x,y) ($x,$y), (_x,_y) ($_x,$_y), _scale $_scale, display size (${getDisplayWidth()},${getDisplayHeight()}), size $size, , $e');
      return;
    }

    _x += dxOffset;
    _y += dyOffset;
    if (dxOffset != 0 || dyOffset != 0) {
      notifyListeners();
    }
  }

  void initializeEdgeScrollFallback(TickerProvider tickerProvider) {
    _edgeScrollFallbackState = EdgeScrollFallbackState(this, tickerProvider);
  }

  void disableEdgeScroll() {
    _edgeScrollState = EdgeScrollState.inactive;
    cancelEdgeScroll();
  }

  void rearmEdgeScroll() {
    _edgeScrollState = EdgeScrollState.armed;
  }

  void cancelEdgeScroll() {
    _edgeScrollFallbackState.stop();
  }

  (Vector2, Vector2) getScrollInfo() {
    final scrollPixel = Vector2(
        _horizontal.hasClients ? _horizontal.position.pixels : 0,
        _vertical.hasClients ? _vertical.position.pixels : 0);

    final max = Vector2(
        _horizontal.hasClients ? _horizontal.position.maxScrollExtent : 0,
        _vertical.hasClients ? _vertical.position.maxScrollExtent : 0);

    return (scrollPixel, max);
  }

  void edgeScrollMouse(double x, double y) async {
    if ((_edgeScrollState == EdgeScrollState.inactive) ||
        (size.width == 0 || size.height == 0) ||
        !(_horizontal.hasClients || _vertical.hasClients)) {
      return;
    }

    if (_edgeScrollState == EdgeScrollState.armed) {
      // 一旦观察到光标
      // 位于边缘滚动区域矩形的内部，边缘滚动就会
      // 被激活。如果用户刚刚把
      // 光标从窗口外部移进来，边缘滚动
      // 还不会发生。
      final clientArea = Rect.fromLTWH(0, 0, size.width, size.height);

      final innerZone = clientArea.deflate(_edgeScrollEdgeThickness.toDouble());

      if (innerZone.contains(Offset(x, y))) {
        _edgeScrollState = EdgeScrollState.active;
      } else {
        // 尚未。
        return;
      }
    }

    var dxOffset = 0.0;
    var dyOffset = 0.0;

    if (x < _edgeScrollEdgeThickness) {
      dxOffset = x - _edgeScrollEdgeThickness;
    } else if (x >= size.width - _edgeScrollEdgeThickness) {
      dxOffset = x - (size.width - _edgeScrollEdgeThickness);
    }

    if (y < _edgeScrollEdgeThickness) {
      dyOffset = y - _edgeScrollEdgeThickness;
    } else if (y >= size.height - _edgeScrollEdgeThickness) {
      dyOffset = y - (size.height - _edgeScrollEdgeThickness);
    }

    var encroachment = Vector2(dxOffset, dyOffset);

    var (scrollPixel, max) = getScrollInfo();

    encroachment.clamp(-scrollPixel, max - scrollPixel);

    if (encroachment.length2 == 0) {
      _edgeScrollFallbackState.stop();
    } else {
      var bumpAmount = -encroachment;

      // 向远离 0 的方向取整：这可以确保鼠标被推离
      // 它所在的任何一个边缘滚动区域
      bumpAmount.x += bumpAmount.x.sign * 0.5;
      bumpAmount.y += bumpAmount.y.sign * 0.5;

      var bumpMouseSucceeded = _bumpMouseIsWorking &&
          (await rustDeskWinManager.call(WindowType.Main, kWindowBumpMouse,
                  {"dx": bumpAmount.x.round(), "dy": bumpAmount.y.round()}))
              .result;

      if (bumpMouseSucceeded) {
        performEdgeScroll(encroachment);
      } else {
        // 如果无法使用 BumpMouse，就切换到带自动重复的较慢滚动

        // 如果 BumpMouse 不起作用，就不要一直调用它。
        _bumpMouseIsWorking = false;

        // 只要用户还停留在某个边缘上方，就继续滚动。
        _edgeScrollFallbackState.setEncroachment(encroachment);
        _edgeScrollFallbackState.start();
      }
    }
  }

  void performEdgeScroll(Vector2 delta) {
    var (scrollPixel, max) = getScrollInfo();

    scrollPixel += delta;

    scrollPixel.clamp(Vector2.zero(), max);

    var scrollPixelPercent = scrollPixel.clone();

    scrollPixelPercent.divide(max);
    scrollPixelPercent.scale(100.0);

    setScrollPercent(scrollPixelPercent.x, scrollPixelPercent.y);
    pushScrollPositionToUI(scrollPixel.x, scrollPixel.y);

    notifyListeners();
  }

  panX(double dx) {
    _x += dx;
    if (isMobile) {
      isMobileCanvasChanged = true;
    }
    notifyListeners();
  }

  resetOffset() {
    if (isWebDesktop) {
      updateViewStyle();
    } else {
      _resetCanvasOffset(getDisplayWidth(), getDisplayHeight());
    }
    notifyListeners();
  }

  panY(double dy) {
    _y += dy;
    if (isMobile) {
      isMobileCanvasChanged = true;
    }
    notifyListeners();
  }

  // 仅移动端
  updateScale(double v, Offset focalPoint) {
    if (parent.target?.imageModel.image == null) return;
    final s = _scale;
    _scale *= v;
    final maxs = parent.target?.imageModel.maxScale ?? 1;
    final mins = parent.target?.imageModel.minScale ?? 1;
    if (_scale > maxs) _scale = maxs;
    if (_scale < mins) _scale = mins;
    // (focalPoint.dx - _x_1) / s1 + displayOriginX = (focalPoint.dx - _x_2) / s2 + displayOriginX
    // _x_2 = focalPoint.dx - (focalPoint.dx - _x_1) / s1 * s2
    _x = focalPoint.dx - (focalPoint.dx - _x) / s * _scale;
    final adjust = getAdjustY();
    // (focalPoint.dy - _y_1 - adjust) / s1 + displayOriginY = (focalPoint.dy - _y_2 - adjust) / s2 + displayOriginY
    // _y_2 = focalPoint.dy - adjust - (focalPoint.dy - _y_1 - adjust) / s1 * s2
    _y = focalPoint.dy - adjust - (focalPoint.dy - _y - adjust) / s * _scale;
    if (isMobile) {
      isMobileCanvasChanged = true;
    }
    notifyListeners();
  }

  // 用于把画布重置为上一次的视图样式
  reset() {
    _scale = _lastViewStyle.scale;
    _devicePixelRatio = ui.window.devicePixelRatio;
    if (kIgnoreDpi && _lastViewStyle.style == kRemoteViewStyleOriginal) {
      _scale = 1.0 / _devicePixelRatio;
    }
    _resetCanvasOffset(getDisplayWidth(), getDisplayHeight());
    bind.sessionSetViewStyle(sessionId: sessionId, value: _lastViewStyle.style);
    notifyListeners();
  }

  clear() {
    _x = 0;
    _y = 0;
    _scale = 1.0;
    _locked = false;
    _lastViewStyle = ViewStyle.defaultViewStyle();
    _timerMobileFocusCanvasCursor?.cancel();
    _timerMobileRestoreCanvasOffset?.cancel();
    _offsetBeforeMobileSoftKeyboard = null;
    _scaleBeforeMobileSoftKeyboard = null;
  }

  updateScrollPercent() {
    final percentX = _horizontal.hasClients
        ? _horizontal.position.extentBefore /
            (_horizontal.position.extentBefore +
                _horizontal.position.extentInside +
                _horizontal.position.extentAfter)
        : 0.0;
    final percentY = _vertical.hasClients
        ? _vertical.position.extentBefore /
            (_vertical.position.extentBefore +
                _vertical.position.extentInside +
                _vertical.position.extentAfter)
        : 0.0;
    setScrollPercent(percentX, percentY);
  }

  void mobileFocusCanvasCursor() {
    _timerMobileFocusCanvasCursor?.cancel();
    _timerMobileFocusCanvasCursor =
        Timer(Duration(milliseconds: 100), () async {
      updateSize();
      _resetCanvasOffset(getDisplayWidth(), getDisplayHeight());
      notifyListeners();
    });
  }

  void saveMobileOffsetBeforeSoftKeyboard() {
    _timerMobileRestoreCanvasOffset?.cancel();
    _offsetBeforeMobileSoftKeyboard = Offset(_x, _y);
    _scaleBeforeMobileSoftKeyboard = _scale;
  }

  void restoreMobileOffsetAfterSoftKeyboard() {
    _timerMobileRestoreCanvasOffset?.cancel();
    _timerMobileFocusCanvasCursor?.cancel();
    final targetOffset = _offsetBeforeMobileSoftKeyboard;
    final targetScale = _scaleBeforeMobileSoftKeyboard;
    if (targetOffset == null || targetScale == null) {
      return;
    }
    _timerMobileRestoreCanvasOffset = Timer(Duration(milliseconds: 100), () {
      updateSize();
      _x = targetOffset.dx;
      _y = targetOffset.dy;
      _scale = targetScale;
      _offsetBeforeMobileSoftKeyboard = null;
      _scaleBeforeMobileSoftKeyboard = null;
      notifyListeners();
    });
  }

  // 仅移动端
  // 移动画布，让光标在屏幕上可见（居中）。
  void _moveToCenterCursor() {
    Rect? imageRect = parent.target?.ffiModel.rect;
    if (imageRect == null) {
      // 不可达
      return;
    }
    final maxX = 0.0;
    final minX = _size.width + (imageRect.left - imageRect.right) * _scale;
    final maxY = 0.0;
    final minY = _size.height + (imageRect.top - imageRect.bottom) * _scale;
    Offset offsetToCenter =
        parent.target?.cursorModel.getCanvasOffsetToCenterCursor() ??
            Offset.zero;
    if (minX < 0) {
      _x = min(max(offsetToCenter.dx, minX), maxX);
    } else {
      // _size.width > (imageRect.right, imageRect.left) * _scale 时，不应改变 _x
    }
    if (minY < 0) {
      _y = min(max(offsetToCenter.dy, minY), maxY);
    } else {
      // _size.height > (imageRect.bottom - imageRect.top) * _scale 时，不应改变 _y
    }
  }
}

// 光标数据
class CursorData {
  // 最多 1 MiB 的 RGBA，包含 Linux 方形光标的填充：最大无障碍光标的两倍
  // 无障碍光标，超过这个值时光标会显示得更小而不是更大。
  static const _maxRasterSize = 512;

  final String peerId;
  final String id;
  img2.Image? _image;
  double scale;
  Uint8List? data;
  final double hotxOrigin;
  final double hotyOrigin;
  double hotx;
  double hoty;
  final int width;
  final int height;
  int _rasterWidth;
  int _rasterHeight;
  bool _scaleLimitReported = false;

  int get rasterWidth => _rasterWidth;
  int get rasterHeight => _rasterHeight;

  img2.Image get image => _image!;

  /// 对于未在使用的形状为 false；只有正在使用的形状保留像素，用于生成新的位图。
  bool get hasPixels => _image != null;

  void releasePixels() {
    _image = null;
    data = null;
  }

  CursorData({
    required this.peerId,
    required this.id,
    required img2.Image image,
    required this.scale,
    required this.data,
    required this.hotxOrigin,
    required this.hotyOrigin,
    required this.width,
    required this.height,
  })  : _image = image,
        _rasterWidth = (width * scale).ceil(),
        _rasterHeight = (height * scale).ceil(),
        hotx = hotxOrigin * scale,
        hoty = hotyOrigin * scale;

  int _doubleToInt(double v) => (v * 10e6).round().toInt();

  double _limitScale(double requestedScale) {
    final valid = requestedScale.isFinite && requestedScale > 0;
    // 无效的请求会保留上一次有效的位图和热点。
    final limitedScale = valid
        ? min(requestedScale, _maxRasterSize / max(width, height))
        : scale;
    final limited = !valid || limitedScale != requestedScale;
    if (limited && !_scaleLimitReported) {
      debugPrint(
          'Cursor $id: rejected scale $requestedScale for ${width}x$height; '
          'using $limitedScale (maximum raster side $_maxRasterSize).');
    }
    _scaleLimitReported = limited;
    return limitedScale;
  }

  double _checkUpdateScale(double scale) {
    scale = _limitScale(scale);
    if (scale != 1.0) {
      // 细长的光标不应仅仅为了让短边达到最小值而被放大。
      scale = max(scale, kMinCursorSize / max(width, height));
    }

    final targetWidth = (width * scale).ceil();
    final targetHeight = (height * scale).ceil();
    if ((_rasterWidth != targetWidth || _rasterHeight != targetHeight) &&
        !hasPixels) {
      // 没有可用于生成该位图的内容；buildCursorOfCache 会索取像素，除非它的
      // 原生光标之前已经创建过。
      data = null;
      return scale;
    }
    if (_rasterWidth != targetWidth || _rasterHeight != targetHeight) {
      if (isWindows) {
        data = img2
            .copyResize(
              image,
              width: targetWidth,
              height: targetHeight,
              interpolation: img2.Interpolation.average,
            )
            .getBytes(order: img2.ChannelOrder.bgra);
      } else if (isDesktop && scale < 1.0 && !image.hasPalette) {
        data = Uint8List.fromList(
            img2.encodePng(_resizeWithAlpha(targetWidth, targetHeight)));
      } else {
        data = Uint8List.fromList(
          img2.encodePng(
            img2.copyResize(
              image,
              width: targetWidth,
              height: targetHeight,
              interpolation: img2.Interpolation.average,
            ),
          ),
        );
      }
      _rasterWidth = targetWidth;
      _rasterHeight = targetHeight;
    }

    this.scale = scale;
    hotx = hotxOrigin * _rasterWidth / width;
    hoty = hotyOrigin * _rasterHeight / height;
    return scale;
  }

  img2.Image _resizeWithAlpha(int targetWidth, int targetHeight) {
    final resized =
        img2.Image.fromResized(image, width: targetWidth, height: targetHeight);
    final dx = image.width / targetWidth;
    final dy = image.height / targetHeight;
    // 使用均值滤波器的采样区域，但按 alpha 对 RGB 加权，这样
    // 在直通 alpha 的 PNG 中，透明像素就不会让可见边缘变暗。
    for (final pixel in resized) {
      final x = (pixel.x * dx).toInt();
      final y = (pixel.y * dy).toInt();
      final sampleWidth = ((pixel.x + 1) * dx).toInt() - x;
      final sampleHeight = ((pixel.y + 1) * dy).toInt() - y;
      final samples = image.getRange(x, y, sampleWidth, sampleHeight);
      num r = 0;
      num g = 0;
      num b = 0;
      num a = 0;
      while (samples.moveNext()) {
        final sample = samples.current;
        r += sample.r * sample.a;
        g += sample.g * sample.a;
        b += sample.b * sample.a;
        a += sample.a;
      }
      if (a == 0) {
        pixel.setRgba(0, 0, 0, 0);
        continue;
      }
      pixel.setRgba(r / a, g / a, b / a, a / (sampleWidth * sampleHeight));
    }
    return resized;
  }

  String updateGetKey(double scale) {
    scale = _checkUpdateScale(scale);
    // 是请求的位图，而不是上一次创建的：没有像素的形状会保留原生
    // 光标，以及它显示过的每个位图对应的原生光标。
    return '${peerId}_${id}_${_doubleToInt(width * scale)}_${_doubleToInt(height * scale)}_${(width * scale).ceil()}_${(height * scale).ceil()}';
  }
}

const _forbiddenCursorPng =
    'iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAMAAABEpIrGAAAAAXNSR0IB2cksfwAAAAlwSFlzAAALEwAACxMBAJqcGAAAAkZQTFRFAAAA2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4G2B4GWAwCAAAAAAAA2B4GAAAAMTExAAAAAAAA2B4G2B4G2B4GAAAAmZmZkZGRAQEBAAAA2B4G2B4G2B4G////oKCgAwMDag8D2B4G2B4G2B4Gra2tBgYGbg8D2B4G2B4Gubm5CQkJTwsCVgwC2B4GxcXFDg4OAAAAAAAA2B4G2B4Gz8/PFBQUAAAAAAAA2B4G2B4G2B4G2B4G2B4G2B4G2B4GDgIA2NjYGxsbAAAAAAAA2B4GFwMB4eHhIyMjAAAAAAAA2B4G6OjoLCwsAAAAAAAA2B4G2B4G2B4G2B4G2B4GCQEA4ODgv7+/iYmJY2NjAgICAAAA9PT0Ojo6AAAAAAAAAAAA+/v7SkpKhYWFr6+vAAAAAAAA8/PzOTk5ERER9fX1KCgoAAAAgYGBKioqAAAAAAAApqamlpaWAAAAAAAAAAAAAAAAAAAAAAAALi4u/v7+GRkZAAAAAAAAAAAAAAAAAAAAfn5+AAAAAAAAV1dXkJCQAAAAAAAAAQEBAAAAAAAAAAAA7Hz6BAAAAMJ0Uk5TAAIWEwEynNz6//fVkCAatP2fDUHs6cDD8d0mPfT5fiEskiIR584A0gejr3AZ+P4plfALf5ZiTL85a4ziD6697fzN3UYE4v/4TwrNHuT///tdRKZh///+1U/ZBv///yjb///eAVL//50Cocv//6oFBbPvpGZCbfT//7cIhv///8INM///zBEcWYSZmO7//////1P////ts/////8vBv//////gv//R/z///QQz9sevP///2waXhNO/+fc//8mev/5gAe2r90MAAAByUlEQVR4nGNggANGJmYWBpyAlY2dg5OTi5uHF6s0H78AJxRwCAphyguLgKRExcQlQLSkFLq8tAwnp6ycPNABjAqKQKNElVDllVU4OVVhVquJA81Q10BRoAkUUYbJa4Edoo0sr6PLqaePLG/AyWlohKTAmJPTBFnelAFoixmSAnNOTgsUeQZLTk4rJAXWnJw2EHlbiDyDPCenHZICe04HFrh+RydnBgYWPU5uJAWinJwucPNd3dw9GDw5Ob2QFHBzcnrD7ffx9fMPCOTkDEINhmC4+3x8Q0LDwlEDIoKTMzIKKg9SEBIdE8sZh6SAJZ6Tkx0qD1YQkpCYlIwclCng0AXLQxSEpKalZyCryATKZwkhKQjJzsnNQ1KQXwBUUVhUXBJYWgZREFJeUVmFpMKlWg+anmqgCkJq6+obkG1pLEBTENLU3NKKrIKhrb2js8u4G6Kgpze0r3/CRAZMAHbkpJDJU6ZMmTqtFbuC6TNmhsyaMnsOFlmwgrnzpsxfELJwEXZ5Bp/FS3yWLlsesmLlKuwKVk9Ys5Zh3foN0zduwq5g85atDAzbpqSGbN9RhV0FGOzctWH3lD14FOzdt3H/gQw8Cg4u2gQPAwBYDXXdIH+wqAAAAABJRU5ErkJggg==';
const _defaultCursorPng =
    'iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAAXNSR0IArs4c6QAAAARzQklUCAgICHwIZIgAAAFmSURBVFiF7dWxSlxREMbx34QFDRowYBchZSxSCWlMCOwD5FGEFHap06UI7KPsAyyEEIQFqxRaCqYTsqCJFsKkuAeRXb17wrqV918dztw55zszc2fo6Oh47MR/e3zO1/iAHWmznHKGQwx9ip/LEbCfazbsoY8j/JLOhcC6sCW9wsjEwJf483AC9nPNc1+lFRwI13d+l3rYFS799rFGxJMqARv2pBXh+72XQ7gWvklPS7TmMl9Ak/M+DqrENvxAv/guKKApuKPWl0/TROK4+LbSqzhuB+OZ3fRSeFPWY+Fkyn56Y29hfgTSpnQ+s98cvorVey66uPlNFxKwZOYLCGfCs5n9NMYVrsp6mvXSoFqpqYFDvMBkStgJJe93dZOwVXxbqUnBENulydSReqUrDhcX0PT2EXarBYS3GNXMhboinBgIl9K71kg0L3+PvyYGdVpruT2MwrF0iotiXfIwus0Dj+OOjo6Of+e7ab74RkpgAAAAAElFTkSuQmCC';

const kPreForbiddenCursorId = "-2";
final preForbiddenCursor = PredefinedCursor(
  png: _forbiddenCursorPng,
  id: kPreForbiddenCursorId,
);
const kPreDefaultCursorId = "-1";
final preDefaultCursor = PredefinedCursor(
  png: _defaultCursorPng,
  id: kPreDefaultCursorId,
  hotxGetter: (double w) => w / 2,
  hotyGetter: (double h) => h / 2,
);

class PredefinedCursor {
  ui.Image? _image;
  img2.Image? _image2;
  CursorData? _cache;
  String png;
  String id;
  double Function(double)? hotxGetter;
  double Function(double)? hotyGetter;

  PredefinedCursor(
      {required this.png, required this.id, this.hotxGetter, this.hotyGetter}) {
    init();
  }

  ui.Image? get image => _image;
  CursorData? get cache => _cache;

  init() {
    _image2 = img2.decodePng(base64Decode(png));
    if (_image2 != null) {
      // 禁止显示的光标图像的 png 类型为 `PngColorType.indexed`。
      if (id == kPreForbiddenCursorId) {
        _image2 = _image2!.convert(format: img2.Format.uint8, numChannels: 4);
      }

      () async {
        final defaultImg = _image2!;
        // 该函数只会被调用一次，无需考虑性能。
        Uint8List data = defaultImg.getBytes(order: img2.ChannelOrder.rgba);
        _image?.dispose();
        _image = await img.decodeImageFromPixels(
            data, defaultImg.width, defaultImg.height, ui.PixelFormat.rgba8888);
        if (_image == null) {
          print("decodeImageFromPixels failed, pre-defined cursor $id");
          return;
        }
        double scale = 1.0;
        if (isWindows) {
          data = _image2!.getBytes(order: img2.ChannelOrder.bgra);
        } else {
          data = Uint8List.fromList(img2.encodePng(_image2!));
        }

        _cache = CursorData(
          peerId: '',
          id: id,
          image: _image2!.clone(),
          scale: scale,
          data: data,
          hotxOrigin:
              hotxGetter != null ? hotxGetter!(_image2!.width.toDouble()) : 0,
          hotyOrigin:
              hotyGetter != null ? hotyGetter!(_image2!.height.toDouble()) : 0,
          width: _image2!.width,
          height: _image2!.height,
        );
      }();
    }
  }
}

class CursorModel with ChangeNotifier {
  ui.Image? _image;
  final _images = <String, Tuple3<ui.Image, double, double>>{};
  CursorData? _cache;
  final _cacheMap = <String, CursorData>{};
  final _cacheKeys = <String>{};
  double _x = -10000;
  double _y = -10000;
  // int.parse(evt['id']) 可能导致 FormatException
  // 所以这里使用 String。
  String _id = "-1";
  double _hotx = 0;
  double _hoty = 0;
  double _displayOriginX = 0;
  double _displayOriginY = 0;
  DateTime? _firstUpdateMouseTime;
  Rect? _windowRect;
  List<RemoteWindowCoords> _remoteWindowCoords = [];
  bool gotMouseControl = true;
  DateTime _lastPeerMouse = DateTime.now()
      .subtract(Duration(milliseconds: 3000 * kMouseControlTimeoutMSec));
  String peerId = '';
  WeakReference<FFI> parent;

  // 仅用于移动端的触摸模式
  // 用于屏蔽 KeyHelpTools 上方的触摸事件
  //
  // 更好的做法是不监听来自 KeyHelpTools 的事件。
  // 但我们现在用 Container(child: Stack(...)) 包裹 KeyHelpTools，
  // 而监听器挂在 Container 上。
  Rect? _keyHelpToolsRect;
  // `lastIsBlocked` 只在 common/widgets/remote_input.dart -> _RawTouchGestureDetectorRegionState -> onDoubleTap() 中使用
  // 因为 onDoubleTap() 没有 `event` 参数，我们无法获取触摸事件的位置。
  bool _lastIsBlocked = false;
  bool _lastKeyboardIsVisible = false;

  bool get lastKeyboardIsVisible => _lastKeyboardIsVisible;

  Rect? get keyHelpToolsRectToAdjustCanvas =>
      _lastKeyboardIsVisible ? _keyHelpToolsRect : null;
  // 被屏蔽的矩形用于在远端页面中屏蔽指针/触摸事件。
  final List<Rect> _blockedRects = [];
  // 在 shouldBlock() 中使用。
  // _blockEvents 是一个标志，用于屏蔽远端图像上的指针/触摸事件。
  // 在以下场景中把它设为 true 以防止误触事件：
  //   1. 在浮动鼠标模式下，显示滚动圆圈时。
  //   2. 在浮动鼠标组件模式下，移动左键/右键按钮时。
  //   3. 在浮动鼠标组件模式下，使用虚拟摇杆时。
  // 当 _blockEvents 为 true 时，无论 _blockedRects 的内容如何，所有指针/触摸事件都会被屏蔽。
  // _blockedRects 包含需要屏蔽事件的具体矩形区域；当 _blockEvents 为 false 时会检查这些区域。
  // 总结：_blockEvents 相当于全局屏蔽，而 _blockedRects 提供细粒度的屏蔽。
  bool _blockEvents = false;
  List<Rect> get blockedRects => List.unmodifiable(_blockedRects);

  set blockEvents(bool v) => _blockEvents = v;

  keyHelpToolsVisibilityChanged(Rect? rect, bool keyboardIsVisible) {
    _keyHelpToolsRect = rect;
    if (rect == null) {
      _lastIsBlocked = false;
    } else {
      // 在这里屏蔽触摸事件是安全的。
      // `lastIsBlocked` 只在 onDoubleTap() 中使用，用于屏蔽来自 KeyHelpTools 的触摸事件。
      // 当光标移动或触摸到其他位置时，会设置 `lastIsBlocked`。
      _lastIsBlocked = true;
    }
    if (isMobile && _lastKeyboardIsVisible != keyboardIsVisible) {
      if (keyboardIsVisible) {
        parent.target?.canvasModel.saveMobileOffsetBeforeSoftKeyboard();
        parent.target?.canvasModel.mobileFocusCanvasCursor();
        parent.target?.canvasModel.isMobileCanvasChanged = false;
      } else {
        parent.target?.canvasModel.restoreMobileOffsetAfterSoftKeyboard();
      }
    }
    _lastKeyboardIsVisible = keyboardIsVisible;
  }

  addBlockedRect(Rect rect) {
    _blockedRects.add(rect);
  }

  removeBlockedRect(Rect rect) {
    _blockedRects.remove(rect);
  }

  get lastIsBlocked => _lastIsBlocked;

  /// 正在使用的形状的图像，或在其回来之前显示的上一个图像：切换会
  /// 只有对核心无法提供的形状才会回退到默认光标。它会向核心请求
  /// 松开时正在使用的那个形状。
  ui.Image? get image {
    if (_images[_id] == null && _cacheMap.containsKey(_id)) {
      restorePixels(_id);
    }
    return _image;
  }

  CursorData? get cache => _cache;

  double get x => _x - _displayOriginX;
  double get y => _y - _displayOriginY;

  double get devicePixelRatio => parent.target!.canvasModel.devicePixelRatio;

  Offset get offset => Offset(_x, _y);

  double get hotx => _hotx;
  double get hoty => _hoty;

  set id(String id) {
    if (_id != id) _unavailable = null;
    _id = id;
  }

  bool get isPeerControlProtected =>
      DateTime.now().difference(_lastPeerMouse).inMilliseconds <
      kMouseControlTimeoutMSec;

  bool isConnIn2Secs() {
    if (_firstUpdateMouseTime == null) {
      _firstUpdateMouseTime = DateTime.now();
      return true;
    } else {
      return DateTime.now().difference(_firstUpdateMouseTime!).inSeconds < 2;
    }
  }

  CursorModel(this.parent) {
    // 现在才创建，而不是首次绘制时创建：解码完成后不会再重绘，所以第一次
    // 需要它的构建会先显示之前的形状来占位。
    preDefaultCursor.cache;
    preForbiddenCursor.cache;
  }

  Set<String> get cachedKeys => _cacheKeys;
  addKey(String key) => _cacheKeys.add(key);

  // 远端物理显示器坐标
  // 用于 update pan（移动端）、onOneFingerPanStart、onOneFingerPanUpdate、onHoldDragUpdate
  Rect getVisibleRect() {
    final size = parent.target?.canvasModel.getSize() ??
        MediaQueryData.fromView(ui.window).size;
    final xoffset = parent.target?.canvasModel.x ?? 0;
    final yoffset = parent.target?.canvasModel.y ?? 0;
    final scale = parent.target?.canvasModel.scale ?? 1;
    final x0 = _displayOriginX - xoffset / scale;
    final y0 = _displayOriginY - yoffset / scale;
    return Rect.fromLTWH(x0, y0, size.width / scale, size.height / scale);
  }

  Offset getCanvasOffsetToCenterCursor() {
    // 光标应位于可见矩形的中心。
    // _x = rect.left + rect.width / 2
    // _y = rect.right + rect.height / 2
    // 参见 `getVisibleRect()`
    // _x = _displayOriginX - xoffset / scale + size.width / scale * 0.5;
    // _y = _displayOriginY - yoffset / scale + size.height / scale * 0.5;
    final size = parent.target?.canvasModel.getSize() ??
        MediaQueryData.fromView(ui.window).size;
    final xoffset = (_displayOriginX - _x) * scale + size.width * 0.5;
    final yoffset = (_displayOriginY - _y) * scale + size.height * 0.5;
    return Offset(xoffset, yoffset);
  }

  get scale => parent.target?.canvasModel.scale ?? 1.0;

  // 移动端软键盘，屏蔽来自 KeyHelpTools 的触摸事件
  shouldBlock(double x, double y) {
    if (_blockEvents) {
      return true;
    }
    final offset = Offset(x, y);
    for (final rect in _blockedRects) {
      if (isPointInRect(offset, rect)) {
        return true;
      }
    }

    // 对于帮助工具矩形，只在触摸模式下屏蔽触摸事件。
    if (!(parent.target?.ffiModel.touchMode ?? false)) {
      return false;
    }
    if (_keyHelpToolsRect != null &&
        isPointInRect(offset, _keyHelpToolsRect!)) {
      return true;
    }
    return false;
  }

  // 用于触摸模式
  Future<bool> move(double x, double y) async {
    if (shouldBlock(x, y)) {
      _lastIsBlocked = true;
      return false;
    }
    _lastIsBlocked = false;
    if (!_moveLocalIfInRemoteRect(x, y)) {
      return false;
    }
    await parent.target?.inputModel.moveMouse(_x, _y);
    return true;
  }

  Future<void> syncCursorPosition() async {
    await parent.target?.inputModel.moveMouse(_x, _y);
  }

  bool isInRemoteRect(Offset offset) {
    return getRemotePosInRect(offset) != null;
  }

  Offset? getRemotePosInRect(Offset offset) {
    final adjust = parent.target?.canvasModel.getAdjustY() ?? 0;
    final newPos = _getNewPos(offset.dx, offset.dy, adjust);
    final visibleRect = getVisibleRect();
    if (!isPointInRect(newPos, visibleRect)) {
      return null;
    }
    final rect = parent.target?.ffiModel.rect;
    if (rect != null) {
      if (!isPointInRect(newPos, rect)) {
        return null;
      }
    }
    return newPos;
  }

  Offset _getNewPos(double x, double y, double adjust) {
    final xoffset = parent.target?.canvasModel.x ?? 0;
    final yoffset = parent.target?.canvasModel.y ?? 0;
    final newX = (x - xoffset) / scale + _displayOriginX;
    final newY = (y - yoffset - adjust) / scale + _displayOriginY;
    return Offset(newX, newY);
  }

  bool _moveLocalIfInRemoteRect(double x, double y) {
    final newPos = getRemotePosInRect(Offset(x, y));
    if (newPos == null) {
      return false;
    }
    _x = newPos.dx;
    _y = newPos.dy;
    notifyListeners();
    return true;
  }

  moveLocal(double x, double y, {double adjust = 0}) {
    final newPos = _getNewPos(x, y, adjust);
    _x = newPos.dx;
    _y = newPos.dy;
    notifyListeners();
  }

  reset() {
    _x = _displayOriginX;
    _y = _displayOriginY;
    parent.target?.inputModel.moveMouse(_x, _y);
    parent.target?.canvasModel.reset();
    notifyListeners();
  }

  updatePan(Offset delta, Offset localPosition, bool touchMode) async {
    if (touchMode) {
      await _handleTouchMode(delta, localPosition);
      return;
    }
    double dx = delta.dx;
    double dy = delta.dy;
    if (parent.target?.imageModel.image == null) return;
    final scale = parent.target?.canvasModel.scale ?? 1.0;
    dx /= scale;
    dy /= scale;
    final r = getVisibleRect();
    var cx = r.center.dx;
    var cy = r.center.dy;
    var tryMoveCanvasX = false;
    final displayRect = parent.target?.ffiModel.rect;
    if (dx > 0) {
      final maxCanvasCanMove = _displayOriginX +
          (displayRect?.width ?? 1280) -
          r.right.roundToDouble();
      tryMoveCanvasX = _x + dx > cx && maxCanvasCanMove > 0;
      if (tryMoveCanvasX) {
        dx = min(dx, maxCanvasCanMove);
      } else {
        final maxCursorCanMove = r.right - _x;
        dx = min(dx, maxCursorCanMove);
      }
    } else if (dx < 0) {
      final maxCanvasCanMove = _displayOriginX - r.left.roundToDouble();
      tryMoveCanvasX = _x + dx < cx && maxCanvasCanMove < 0;
      if (tryMoveCanvasX) {
        dx = max(dx, maxCanvasCanMove);
      } else {
        final maxCursorCanMove = r.left - _x;
        dx = max(dx, maxCursorCanMove);
      }
    }
    var tryMoveCanvasY = false;
    if (dy > 0) {
      final mayCanvasCanMove = _displayOriginY +
          (displayRect?.height ?? 720) -
          r.bottom.roundToDouble();
      tryMoveCanvasY = _y + dy > cy && mayCanvasCanMove > 0;
      if (tryMoveCanvasY) {
        dy = min(dy, mayCanvasCanMove);
      } else {
        final mayCursorCanMove = r.bottom - _y;
        dy = min(dy, mayCursorCanMove);
      }
    } else if (dy < 0) {
      final mayCanvasCanMove = _displayOriginY - r.top.roundToDouble();
      tryMoveCanvasY = _y + dy < cy && mayCanvasCanMove < 0;
      if (tryMoveCanvasY) {
        dy = max(dy, mayCanvasCanMove);
      } else {
        final mayCursorCanMove = r.top - _y;
        dy = max(dy, mayCursorCanMove);
      }
    }

    if (dx == 0 && dy == 0) return;

    Point<double>? newPos;
    final rect = parent.target?.ffiModel.rect;
    if (rect == null) {
      // 不可达
      return;
    }
    newPos = InputModel.getPointInRemoteRect(
        false,
        parent.target?.ffiModel.pi.platform,
        kPointerEventKindMouse,
        kMouseEventTypeDefault,
        _x + dx,
        _y + dy,
        rect,
        buttons: kPrimaryButton);
    if (newPos == null) {
      return;
    }
    dx = newPos.x - _x;
    dy = newPos.y - _y;
    _x = newPos.x;
    _y = newPos.y;
    if (tryMoveCanvasX && dx != 0) {
      parent.target?.canvasModel.panX(-dx * scale);
    }
    if (tryMoveCanvasY && dy != 0) {
      parent.target?.canvasModel.panY(-dy * scale);
    }

    parent.target?.inputModel.moveMouse(_x, _y);
    notifyListeners();
  }

  bool _isInCurrentWindow(double x, double y) {
    final w = _windowRect!.width / devicePixelRatio;
    final h = _windowRect!.width / devicePixelRatio;
    return x >= 0 && y >= 0 && x <= w && y <= h;
  }

  _handleTouchMode(Offset delta, Offset localPosition) async {
    bool isMoved = false;
    if (_remoteWindowCoords.isNotEmpty &&
        _windowRect != null &&
        !_isInCurrentWindow(localPosition.dx, localPosition.dy)) {
      final coords = InputModel.findRemoteCoords(localPosition.dx,
          localPosition.dy, _remoteWindowCoords, devicePixelRatio);
      if (coords != null) {
        double x2 =
            (localPosition.dx - coords.relativeOffset.dx / devicePixelRatio) /
                coords.canvas.scale;
        double y2 =
            (localPosition.dy - coords.relativeOffset.dy / devicePixelRatio) /
                coords.canvas.scale;
        x2 += coords.cursor.offset.dx;
        y2 += coords.cursor.offset.dy;
        await parent.target?.inputModel.moveMouse(x2, y2);
        isMoved = true;
      }
    }
    if (!isMoved) {
      final rect = parent.target?.ffiModel.rect;
      if (rect == null) {
        // 不可达
        return;
      }

      Offset? movementInRect(double x, double y, Rect r) {
        final isXInRect = x >= r.left && x <= r.right;
        final isYInRect = y >= r.top && y <= r.bottom;
        if (!(isXInRect || isYInRect)) {
          return null;
        }
        if (x < r.left) {
          x = r.left;
        } else if (x > r.right) {
          x = r.right;
        }
        if (y < r.top) {
          y = r.top;
        } else if (y > r.bottom) {
          y = r.bottom;
        }
        return Offset(x, y);
      }

      final scale = parent.target?.canvasModel.scale ?? 1.0;
      var movement =
          movementInRect(_x + delta.dx / scale, _y + delta.dy / scale, rect);
      if (movement == null) {
        return;
      }
      movement = movementInRect(movement.dx, movement.dy, getVisibleRect());
      if (movement == null) {
        return;
      }

      _x = movement.dx;
      _y = movement.dy;
      await parent.target?.inputModel.moveMouse(_x, _y);
    }
    notifyListeners();
  }

  disposeImages() {
    _images.forEach((_, v) => v.item1.dispose());
    _images.clear();
  }

  /// 它的图像与最近使用的那些图像一起保留，参见 [_imageLimit]；无法解码的形状
  /// 会被标记为核心无法提供的形状。
  Future<void> updateCursorData(String id, int hotxInt, int hotyInt, int width,
      int height, Uint8List rgba) async {
    final generation = _generation;
    if (_unavailable == id) _unavailable = null;
    final hotx = hotxInt.toDouble();
    final hoty = hotyInt.toDouble();
    final image = await img.decodeImageFromPixels(
        rgba, width, height, ui.PixelFormat.rgba8888);
    if (image == null) {
      // 它没有解码成功；绘制时不能每帧都去索取它。
      _markUnavailable(generation, id);
      return;
    }
    if (!await _updateCache(
        generation, rgba, image, id, hotx, hoty, width, height)) {
      // 没有保留，或者在它解码期间会话被清除了。
      image.dispose();
      _markUnavailable(generation, id);
      return;
    }
    final old = _images.remove(id);
    _images[id] = Tuple3(image, hotx, hoty);
    if (old != null) {
      if (identical(old.item1, _image)) _image = image;
      old.item1.dispose();
    }

    // 更新上一次的光标数据。
    // 不要使用上一次的 `image` 和 `id`，因为 `_id` 可能已经改变。
    _updateCurData();
    final cache = _cacheMap[id];
    if (id != _id && cache != null) _switchedAway(cache);
  }

  Future<bool> _updateCache(
    int generation,
    Uint8List rgba,
    ui.Image image,
    String id,
    double hotx,
    double hoty,
    int w,
    int h,
  ) async {
    Uint8List? data;
    img2.Image imgOrigin = img2.Image.fromBytes(
        width: w,
        height: h,
        bytes: rgba.buffer,
        bytesOffset: rgba.offsetInBytes,
        order: img2.ChannelOrder.rgba);
    if (isWindows) {
      data = imgOrigin.getBytes(order: img2.ChannelOrder.bgra);
    } else {
      ByteData? imgBytes =
          await image.toByteData(format: ui.ImageByteFormat.png);
      if (imgBytes == null) {
        return false;
      }
      data = imgBytes.buffer.asUint8List();
      if (isDesktop &&
          (parent.target?.ffiModel.isPeerLinux == true ||
              parent.target?.ffiModel.isPeerWindows == true)) {
        // PNG 解码为 Linux/Windows 光标缩放提供直通 alpha。
        final decoded = img2.decodePng(data);
        if (decoded == null) {
          debugPrint('Unable to decode cursor $id PNG for resizing');
          return false;
        }
        imgOrigin = decoded;
      }
    }
    final cache = CursorData(
      peerId: peerId,
      id: id,
      image: imgOrigin,
      scale: 1.0,
      data: data,
      hotxOrigin: hotx,
      hotyOrigin: hoty,
      width: w,
      height: h,
    );
    if (generation != _generation) {
      return false;
    }
    _cacheMap[id] = cache;
    return true;
  }

  @visibleForTesting
  Iterable<String> get shapeIds => _images.keys;

  @visibleForTesting
  CursorData? cachedShape(String id) => _cacheMap[id];

  final _restoring = <String>{};

  // 同一窗口内的标签页共享该引擎的原生光标，包括预定义的那些。
  static int _nextKeyScope = 0;
  final int _keyScope = _nextKeyScope++;

  String nativeKey(CursorData cache, double scale) {
    final key = '${_keyScope}_${cache.updateGetKey(scale)}';
    // 另一个位图对应的原生光标并不持有构成当前光标的像素。
    if (!_cacheKeys.contains(key)) _nativeIds.remove(cache.id);
    return key;
  }

  // 原生光标在整个会话期间保留，每个形状的每个位图一个，而且只有 [clear]
  // 会删除它们，与之前一样；与绘制的图像不同，它们目前没有 LRU。一次删除
  // 在 Windows 上什么也不会释放：引擎的 `deleteCustomCursor/windows` 只是用
  // `DeleteObject` 释放 HCURSOR，而 `DeleteObject` 并不能处理光标。在 Windows 上用 500 个光标实测，
  // 像引擎那样由 `CreateIconIndirect` 创建：`DeleteObject` 对全部 500 个都失败，
  // 留下 500 个 USER 对象未释放，而 `DestroyCursor` 或 `DestroyIcon` 释放了全部 500 个。每次
  // 删除都会泄漏其句柄直到进程退出，而为某个位图再次创建光标
  // 或再次显示某个形状又会泄漏一个，所以 LRU 泄漏得比一直保留还多。
  //
  // 修复引擎的代价不低。x64 在 Flutter 3.24 上运行我们的分支 rustdesk/engine；
  // 这个修复意味着再多一个需要重新构建、发布并在每次 Flutter 升级时同步的补丁。arm64
  // 运行的是更新版本 Flutter 的官方引擎，代码是一样的：修复它意味着
  // 把分支移植到那个版本，并且还要构建和发布 arm64 引擎，或者
  // 推动上游修复并等待它进入稳定版。一旦两个引擎都能销毁光标，就可以
  // 恢复像绘制图像那样的 LRU。内容 id 让数量只限于对端真正显示的形状，
  // 也就几十到几百个。

  /// 正在使用的形状保留其像素，因此新的位图立即由它们生成；其他形状
  /// 一旦原生光标持有它们，就不再保留像素。
  void registered(CursorData cache, String key) {
    _nativeIds.add(cache.id);
    _awaitingNative.remove(cache.id);
    if (cache.id != _id && identical(_cacheMap[cache.id], cache)) {
      cache.releasePixels();
    }
  }

  // 原生光标持有的形状，以及那些在获得原生光标之前保留像素的形状，最旧的
  // 排在最前：在对端已经切换之后才解码的形状，在再次显示时仍必须获得原生光标，
  // 而不是每次回来都重新解码。
  final _nativeIds = <String>{};
  final _awaitingNative = <String>{};

  void _switchedAway(CursorData cache) {
    _awaitingNative.remove(cache.id);
    // 移动端只根据图像绘制光标。
    if (isMobile || _nativeIds.contains(cache.id)) {
      cache.releasePixels();
      return;
    }
    _awaitingNative.add(cache.id);
    while (_awaitingNative.length > kRecentShapes) {
      final oldest = _awaitingNative.first;
      _awaitingNative.remove(oldest);
      if (oldest != _id) _cacheMap[oldest]?.releasePixels();
    }
  }

  /// 对端最近使用且保留绘制图像的形状，以及保留像素等待原生光标的最多个，
  /// 核心会重建其他的。动画光标每一帧都是一个形状，
  /// Windows 的忙碌光标是 18 个，KDE 的是 23 个，而比这个上限更长的循环
  /// 会导致每帧都重建。
  static const kRecentShapes = 64;

  /// 上一次显示的形状对应的原生光标，在正在使用的形状生成期间保持显示。
  String? shownKey;

  void shown(CursorData cache, String key) {
    // 禁止显示的光标只在输入关闭时显示；它并不是某个形状的替身。
    if (cache.id != kPreForbiddenCursorId) shownKey = key;
  }

  /// 桌面端是否把对端的光标绘制在远端图像之上。
  @protected
  bool get showsRemoteCursor {
    final tag = ShowRemoteCursorState.tag(parent.target?.id ?? '');
    return Get.isRegistered<RxBool>(tag: tag) &&
        Get.find<RxBool>(tag: tag).value;
  }

  // 保留绘制图像：移动端总是绘制光标，桌面端只在显示
  // 远端光标时才绘制，否则由原生光标显示形状。
  int get _imageLimit => isMobile || showsRemoteCursor ? kRecentShapes : 1;

  // `_images` 按使用顺序排列。被绘制的图像会保留，无论它属于谁。
  void _evictImages() {
    var excess = _images.length - _imageLimit;
    _images.removeWhere((id, shape) {
      if (excess <= 0 || id == _id || identical(shape.item1, _image)) {
        return false;
      }
      excess--;
      shape.item1.dispose();
      return true;
    });
  }

  /// 核心会保存对端发送的每个形状（压缩后）；参见 `Session::cursor_shapes`。
  @protected
  Future<CursorShape?> fetchCursorShape(String id) {
    final ffi = parent.target;
    if (ffi == null) return Future.value(null);
    return bind.sessionGetCursorShape(sessionId: ffi.sessionId, id: id);
  }

  // 核心无法提供的形状，在它正在使用时不会再次被索取。一个就够，
  // 因为只有正在使用的形状才会每帧被索取，而且对端的 id 不能
  // 在这里堆积。
  String? _unavailable;
  // 统计会话清理次数，以便在清理之后才完成的解码不会保留任何东西。
  int _generation = 0;

  /// 再次从核心解码正在使用的形状，用于其原生光标缺少的位图，用于
  /// 绘制它，或用于标签页移动到的窗口。
  void restorePixels(String id) {
    if (id != _id || _unavailable == id || !_restoring.add(id)) return;
    final generation = _generation;
    () async {
      try {
        // 抛出的异常会变成稍后由 await 交出的错误，而不是在可能
        // 发起请求的那次构建中发生：失败会通知监听器。
        final shape = await Future.sync(() => fetchCursorShape(id));
        if (generation != _generation) {
          return;
        } else if (shape == null) {
          _markUnavailable(generation, id);
          debugPrint('Cursor $id is not kept by the core');
        } else {
          // 即使对端已经切换也会解码：动画光标会回到它。
          await updateCursorData(id, shape.hotx, shape.hoty, shape.width,
              shape.height, shape.colors);
        }
      } catch (e) {
        _markUnavailable(generation, id);
        debugPrint('Failed to fetch cursor $id: $e');
      } finally {
        _restoring.remove(id);
      }
    }();
  }

  void _markUnavailable(int generation, String id) {
    if (generation != _generation || id != _id) return;
    _unavailable = id;
    _updateCurData();
  }

  bool _updateCurData() {
    final previous = _cache;
    final cache = _cacheMap[_id];
    // 尚未解码的形状会先让之前显示的形状留在原位，直到它解码完成；核心
    // 无法提供的形状则不留下任何内容，于是显示默认光标，就像从未发送过的形状一样。
    if (cache != null || _unavailable == _id) _cache = cache;
    if (previous != null && !identical(previous, _cache)) {
      _switchedAway(previous);
    }
    final tmp = _images.remove(_id);
    if (tmp != null) {
      _images[_id] = tmp;
      _image = tmp.item1;
      _hotx = tmp.item2;
      _hoty = tmp.item3;
    } else if (_unavailable == _id) {
      _image = null;
    }
    _evictImages();
    try {
      // 可能会抛出异常，因为监听器可能已经被 dispose
      notifyListeners();
    } catch (e) {
      debugPrint('WARNING: updateCursorId $_id, without notifyListeners(). $e');
    }
    return tmp != null || cache != null;
  }

  updateCursorId(Map<String, dynamic> evt) {
    if (!_updateCurData()) {
      restorePixels(_id);
    }
  }

  /// 更新光标位置。
  updateCursorPosition(Map<String, dynamic> evt, String id) async {
    if (!isConnIn2Secs()) {
      gotMouseControl = false;
      _lastPeerMouse = DateTime.now();
    }
    _x = double.parse(evt['x']);
    _y = double.parse(evt['y']);
    try {
      RemoteCursorMovedState.find(id).value = true;
    } catch (e) {
      //
    }
    notifyListeners();
  }

  updateDisplayOrigin(double x, double y, {updateCursorPos = true}) {
    _displayOriginX = x;
    _displayOriginY = y;
    if (updateCursorPos) {
      _x = x + 1;
      _y = y + 1;
      parent.target?.inputModel.moveMouse(x, y);
    }
    parent.target?.canvasModel.resetOffset();
    notifyListeners();
  }

  updateDisplayOriginWithCursor(
      double x, double y, double xCursor, double yCursor) {
    _displayOriginX = x;
    _displayOriginY = y;
    _x = xCursor;
    _y = yCursor;
    parent.target?.inputModel.moveMouse(x, y);
    notifyListeners();
  }

  clear() {
    _x = -10000;
    _x = -10000;
    _image = null;
    _firstUpdateMouseTime = null;
    gotMouseControl = true;
    disposeImages();

    _clearCache();
    _cache = null;
    _cacheMap.clear();
    _nativeIds.clear();
    _awaitingNative.clear();
    _unavailable = null;
    _generation++;
  }

  _clearCache() {
    final keys = {...cachedKeys};
    for (var k in keys) {
      debugPrint("deleting cursor with key $k");
      deleteCustomCursor(k);
    }
    _cacheKeys.clear();
    shownKey = null;
    resetSystemCursor();
  }

  trySetRemoteWindowCoords() {
    Future.delayed(Duration.zero, () async {
      _windowRect =
          await InputModel.fillRemoteCoordsAndGetCurFrame(_remoteWindowCoords);
    });
  }

  clearRemoteWindowCoords() {
    _windowRect = null;
    _remoteWindowCoords.clear();
  }
}

class QualityMonitorData {
  String? speed;
  String? fps;
  String? delay;
  String? targetBitrate;
  String? codecFormat;
  String? chroma;
}

class QualityMonitorModel with ChangeNotifier {
  WeakReference<FFI> parent;

  QualityMonitorModel(this.parent);
  var _show = false;
  final _data = QualityMonitorData();

  bool get show => _show;
  QualityMonitorData get data => _data;

  // 只有 web 上的 WebRTC 会话会在这里标注其传输方式：web 没有
  // 会话标签页可以展示它（桌面端标签页的提示已经做到了），而且
  // WebRTC 是唯一可能走直连或 TURN 的路径。
  String? get webrtcTransport {
    if (!isWeb) return null;
    final ffiModel = parent.target?.ffiModel;
    if (ffiModel == null) return null;
    final streamType = ffiModel.cachedPeerData.streamType;
    if (!streamType.startsWith('WebRTC')) return null;
    return ffiModel.direct == false ? '$streamType (TURN)' : streamType;
  }

  checkShowQualityMonitor(SessionID sessionId) async {
    final show = await bind.sessionGetToggleOption(
            sessionId: sessionId, arg: 'show-quality-monitor') ==
        true;
    if (_show != show) {
      _show = show;
      notifyListeners();
    }
  }

  updateQualityStatus(Map<String, dynamic> evt) {
    try {
      if (evt.containsKey('speed') && (evt['speed'] as String).isNotEmpty) {
        _data.speed = evt['speed'];
      }
      if (evt.containsKey('fps') && (evt['fps'] as String).isNotEmpty) {
        final fps = jsonDecode(evt['fps']) as Map<String, dynamic>;
        final pi = parent.target?.ffiModel.pi;
        if (pi != null) {
          final currentDisplay = pi.currentDisplay;
          if (currentDisplay != kAllDisplayValue) {
            final fps2 = fps[currentDisplay.toString()];
            if (fps2 != null) {
              _data.fps = fps2.toString();
            }
          } else if (fps.isNotEmpty) {
            final fpsList = [];
            for (var i = 0; i < pi.displays.length; i++) {
              fpsList.add((fps[i.toString()] ?? 0).toString());
            }
            _data.fps = fpsList.join(' ');
          }
        } else {
          _data.fps = null;
        }
      }
      if (evt.containsKey('delay') && (evt['delay'] as String).isNotEmpty) {
        _data.delay = evt['delay'];
      }
      if (evt.containsKey('target_bitrate') &&
          (evt['target_bitrate'] as String).isNotEmpty) {
        _data.targetBitrate = evt['target_bitrate'];
      }
      if (evt.containsKey('codec_format') &&
          (evt['codec_format'] as String).isNotEmpty) {
        _data.codecFormat = evt['codec_format'];
      }
      if (evt.containsKey('chroma') && (evt['chroma'] as String).isNotEmpty) {
        _data.chroma = evt['chroma'];
      }
      notifyListeners();
    } catch (e) {
      //
    }
  }
}

class RecordingModel with ChangeNotifier {
  WeakReference<FFI> parent;
  RecordingModel(this.parent);
  bool _start = false;
  bool get start => _start;

  toggle() async {
    if (isIOS) return;
    final sessionId = parent.target?.sessionId;
    if (sessionId == null) return;
    final pi = parent.target?.ffiModel.pi;
    if (pi == null) return;
    bool value = !_start;
    if (value) {
      await sessionRefreshVideo(sessionId, pi);
    }
    await bind.sessionRecordScreen(sessionId: sessionId, start: value);
  }

  updateStatus(bool status) {
    _start = status;
    notifyListeners();
  }
}

class ElevationModel with ChangeNotifier {
  WeakReference<FFI> parent;
  ElevationModel(this.parent);
  bool _running = false;
  bool _canElevate = false;
  bool get showRequestMenu => _canElevate && !_running;
  onPeerInfo(PeerInfo pi) {
    _canElevate = pi.platform == kPeerPlatformWindows && pi.sasEnabled == false;
    _running = false;
  }

  onPortableServiceRunning(bool running) => _running = running;
}

// `ConnType` 的索引值与 rust protobuf 相同。
enum ConnType {
  defaultConn,
  fileTransfer,
  portForward,
  rdp,
  viewCamera,
  terminal
}

/// Flutter 的状态管理器以及与 Rust 核心的数据通信。
class FFI {
  var id = '';
  var version = '';
  var connType = ConnType.defaultConn;
  var closed = false;

  /// dialogManager 使用 late，以确保在主页面绑定 [globalKey] 之后才初始化
  late final dialogManager = OverlayDialogManager();

  late final SessionID sessionId;
  late final ImageModel imageModel; // session
  late final FfiModel ffiModel; // session
  late final CursorModel cursorModel; // session
  late final CanvasModel canvasModel; // session
  late final ServerModel serverModel; // global
  late final ChatModel chatModel; // session
  late final FileModel fileModel; // session
  late final AbModel abModel; // global
  late final GroupModel groupModel; // global
  late final UserModel userModel; // global
  late final PeerTabModel peerTabModel; // global
  late final QualityMonitorModel qualityMonitorModel; // session
  late final RecordingModel recordingModel; // session
  late final InputModel inputModel; // session
  late final ElevationModel elevationModel; // session
  late final CmFileModel cmFileModel; // cm
  late final TextureModel textureModel; //session
  late final ShortcutModel shortcutModel; // session
  late final Peers recentPeersModel; // global
  late final Peers favoritePeersModel; // global
  late final Peers lanPeersModel; // global

  // 多终端的终端模型注册表
  final Map<int, TerminalModel> _terminalModels = {};

  // 终端模型的 Getter
  Map<int, TerminalModel> get terminalModels => _terminalModels;

  FFI(SessionID? sId) {
    sessionId = sId ?? (isDesktop ? Uuid().v4obj() : _constSessionId);
    imageModel = ImageModel(WeakReference(this));
    ffiModel = FfiModel(WeakReference(this));
    cursorModel = CursorModel(WeakReference(this));
    canvasModel = CanvasModel(WeakReference(this));
    serverModel = ServerModel(WeakReference(this));
    chatModel = ChatModel(WeakReference(this));
    fileModel = FileModel(WeakReference(this));
    userModel = UserModel(WeakReference(this));
    peerTabModel = PeerTabModel(WeakReference(this));
    abModel = AbModel(WeakReference(this));
    groupModel = GroupModel(WeakReference(this));
    qualityMonitorModel = QualityMonitorModel(WeakReference(this));
    recordingModel = RecordingModel(WeakReference(this));
    inputModel = InputModel(WeakReference(this));
    elevationModel = ElevationModel(WeakReference(this));
    cmFileModel = CmFileModel(WeakReference(this));
    textureModel = TextureModel(WeakReference(this));
    shortcutModel = ShortcutModel(WeakReference(this));
    recentPeersModel = Peers(
        name: PeersModelName.recent,
        loadEvent: LoadEvent.recent,
        getInitPeers: null);
    favoritePeersModel = Peers(
        name: PeersModelName.favorite,
        loadEvent: LoadEvent.favorite,
        getInitPeers: null);
    lanPeersModel = Peers(
        name: PeersModelName.lan, loadEvent: LoadEvent.lan, getInitPeers: null);
  }

  /// 移动端复用 FFI
  void mobileReset() {
    ffiModel.resetRestartReconnectState();
    ffiModel.waitForFirstImage.value = true;
    ffiModel.isRefreshing = false;
    ffiModel.waitForImageDialogShow.value = true;
    ffiModel.waitForImageTimer?.cancel();
    ffiModel.waitForImageTimer = null;
  }

  /// 用给定的 [id] 启动。仅当 [isFileTransfer] 时传输文件，仅当 [isViewCamera] 时查看摄像头，仅当 [isPortForward] 时做端口转发。
  void start(
    String id, {
    bool isFileTransfer = false,
    bool isViewCamera = false,
    bool isPortForward = false,
    bool isRdp = false,
    bool isTerminal = false,
    String? switchUuid,
    String? password,
    bool? isSharedPassword,
    String? connToken,
    bool? forceRelay,
    int? tabWindowId,
    int? display,
    List<int>? displays,
  }) {
    closed = false;
    if (isMobile) mobileReset();
    assert(
        (!(isPortForward && isViewCamera)) &&
            (!(isViewCamera && isPortForward)) &&
            (!(isPortForward && isFileTransfer)) &&
            (!(isTerminal && isFileTransfer)) &&
            (!(isTerminal && isViewCamera)) &&
            (!(isTerminal && isPortForward)),
        'more than one connect type');
    if (isFileTransfer) {
      connType = ConnType.fileTransfer;
    } else if (isViewCamera) {
      connType = ConnType.viewCamera;
    } else if (isPortForward) {
      connType = ConnType.portForward;
    } else if (isTerminal) {
      connType = ConnType.terminal;
    } else {
      chatModel.resetClientMode();
      connType = ConnType.defaultConn;
      canvasModel.id = id;
      imageModel.id = id;
      cursorModel.peerId = id;
    }

    final isNewPeer = tabWindowId == null;
    // 如果 tabWindowId != null，该会话就是 "tab -> window" 的那种。
    // 否则该会话是一个新的会话。
    if (isNewPeer) {
      // ignore: unused_local_variable
      final addRes = bind.sessionAddSync(
        sessionId: sessionId,
        id: id,
        isFileTransfer: isFileTransfer,
        isViewCamera: isViewCamera,
        isPortForward: isPortForward,
        isRdp: isRdp,
        isTerminal: isTerminal,
        switchUuid: switchUuid ?? '',
        forceRelay: forceRelay ?? false,
        password: password ?? '',
        isSharedPassword: isSharedPassword ?? false,
        connToken: connToken,
      );
    } else if (display != null) {
      if (displays == null) {
        debugPrint(
            'Unreachable, failed to add existed session to $id, the displays is null while display is $display');
        return;
      }
      final addRes = bind.sessionAddExistedSync(
          id: id,
          sessionId: sessionId,
          displays: Int32List.fromList(displays),
          isViewCamera: isViewCamera);
      if (addRes != '') {
        debugPrint(
            'Unreachable, failed to add existed session to $id, $addRes');
        return;
      }
      ffiModel.pi.currentDisplay = display;
    }
    if (isDesktop && connType == ConnType.defaultConn) {
      textureModel.updateCurrentDisplay(display ?? 0);
    }
    // FIXME: separate cameras displays or shift all indices.
    if (isDesktop && connType == ConnType.viewCamera) {
      // FIXME: currently the default 0 is not used.
      textureModel.updateCurrentDisplay(display ?? 0);
    }

    if (isDesktop) {
      inputModel.updateTrackpadSpeed();
    }

    // 注意：`sessionStart()` 和 `sessionStartWithDisplays()` 是异步函数。
    // 虽然流是立即返回的，但它可能还没准备好。
    // 任何依赖该流的操作都应谨慎处理。
    late final Stream<EventToUI> stream;
    if (isNewPeer || display == null || displays == null) {
      stream = bind.sessionStart(sessionId: sessionId, id: id);
    } else {
      // 必须把显示器放进 `sessionStart()`，以确保流已准备好，
      // 这样才能发送显示器的采集请求。
      stream = bind.sessionStartWithDisplays(
          sessionId: sessionId, id: id, displays: Int32List.fromList(displays));
    }

    if (isWeb) {
      platformFFI.setCursorDataCallback(ffiModel.handleCursorData);
      platformFFI.setRgbaCallback((int display, Uint8List data) {
        onEvent2UIRgba();
        imageModel.onRgba(display, data);
      });
      platformFFI.setVideoFrameCallback((int display, ui.Image image,
          bool Function() isCurrentSession) async {
        if (!isCurrentSession()) {
          image.dispose();
          return;
        }
        await onEvent2UIRgba();
        await imageModel.onImage(display, image, isCurrentSession);
      });
      this.id = id;
      return;
    }

    final cb = ffiModel.startEventListener(sessionId, id);

    imageModel.updateUserTextureRender();
    final hasGpuTextureRender = bind.mainHasGpuTextureRender();
    final SimpleWrapper<bool> isToNewWindowNotified = SimpleWrapper(false);
    // 为 rgba 数据保留。
    stream.listen((message) {
      if (closed) return;
      if (tabWindowId != null && !isToNewWindowNotified.value) {
        // 会话被读取以移动到新窗口。
        // 获取缓存数据并处理该缓存数据。
        Future.delayed(Duration.zero, () async {
          final args = jsonEncode({'id': id, 'close': display == null});
          final cachedData = await DesktopMultiWindow.invokeMethod(
              tabWindowId, kWindowEventGetCachedSessionData, args);
          if (cachedData == null) {
            // 不可达
            debugPrint('Unreachable, the cached data is empty.');
            return;
          }
          final data = CachedPeerData.fromString(cachedData);
          if (data == null) {
            debugPrint('Unreachable, the cached data cannot be decoded.');
            return;
          }
          ffiModel.setPermissions(data.permissions);
          await ffiModel.handleCachedPeerData(data, id);
          await sessionRefreshVideo(sessionId, ffiModel.pi);
          await bind.sessionRequestNewDisplayInitMsgs(
              sessionId: sessionId, display: ffiModel.pi.currentDisplay);
        });
        isToNewWindowNotified.value = true;
      }
      () async {
        if (message is EventToUI_Event) {
          if (message.field0 == "close") {
            closed = true;
            debugPrint('Exit session event loop');
            return;
          }

          Map<String, dynamic>? event;
          try {
            event = json.decode(message.field0);
          } catch (e) {
            debugPrint('json.decode fail1(): $e, ${message.field0}');
          }
          if (event != null) {
            await cb(event);
          }
        } else if (message is EventToUI_Rgba) {
          final display = message.field0;
          // 从 rust 代码获取图像缓冲区。
          final sz = platformFFI.getRgbaSize(sessionId, display);
          if (sz == 0) {
            platformFFI.nextRgba(sessionId, display);
            return;
          }
          final rgba = platformFFI.getRgba(sessionId, display, sz);
          if (rgba != null) {
            onEvent2UIRgba();
            await imageModel.onRgba(display, rgba);
          } else {
            platformFFI.nextRgba(sessionId, display);
          }
        } else if (message is EventToUI_Cursor) {
          await ffiModel.handleCursorData(message.id, message.hotx,
              message.hoty, message.width, message.height, message.colors);
        } else if (message is EventToUI_Texture) {
          final display = message.field0;
          final gpuTexture = message.field1;
          debugPrint(
              "EventToUI_Texture display:$display, gpuTexture:$gpuTexture");
          if (gpuTexture && !hasGpuTextureRender) {
            debugPrint('the gpuTexture is not supported.');
            return;
          }
          textureModel.setTextureType(display: display, gpuTexture: gpuTexture);
          onEvent2UIRgba();
        }
      }();
    });
    // 每个实例都会绑定一个流
    this.id = id;
  }

  Future<void> onEvent2UIRgba() async {
    if (ffiModel.waitForImageDialogShow.isTrue) {
      ffiModel.waitForImageDialogShow.value = false;
      ffiModel.waitForImageTimer?.cancel();
      clearWaitingForImage(dialogManager, sessionId);
    }
    if (ffiModel.waitForFirstImage.value == true) {
      ffiModel.waitForFirstImage.value = false;
      ffiModel.cancelPendingRestoreTimer();
      ffiModel.resetRestartReconnectState();
      dialogManager.dismissAll();
      try {
        await canvasModel.updateViewStyle();
        await canvasModel.updateScrollStyle();
        await canvasModel.initializeEdgeScrollEdgeThickness();
        for (final cb in imageModel.callbacksOnFirstImage) {
          cb(id);
        }
      } finally {
        _applyPendingMonitorRestore();
      }
    }
  }

  void _applyPendingMonitorRestore() {
    final restore = ffiModel.pendingMonitorRestore;
    ffiModel._cancelPendingMonitorRestore();
    if (restore == null || closed) return;
    // 自恢复操作入队以来，显示器列表可能已经发生变化。
    final displays = ffiModel.pi.displays;
    if ((restore == kAllDisplayValue && displays.isNotEmpty) ||
        (restore >= 0 && restore < displays.length)) {
      openMonitorInTheSameTab(restore, this, ffiModel.pi,
          recordSelection: false, updateCursorPos: false);
    }
  }

  /// 用 [password] 登录，选择客户端是否要 [remember] 它。
  void login(String osUsername, String osPassword, SessionID sessionId,
      String password, bool remember) {
    bind.sessionLogin(
        sessionId: sessionId,
        osUsername: osUsername,
        osPassword: osPassword,
        password: password,
        remember: remember);
  }

  void send2FA(SessionID sessionId, String code, bool trustThisDevice) {
    bind.sessionSend2Fa(
        sessionId: sessionId, code: code, trustThisDevice: trustThisDevice);
  }

  /// 关闭远端会话。
  Future<void> close({bool closeSession = true}) async {
    closed = true;
    if (isWeb) {
      platformFFI.clearVideoFrameCallback();
    }
    chatModel.close();
    // 关闭所有终端模型
    for (final model in _terminalModels.values) {
      model.dispose();
    }
    _terminalModels.clear();
    if (imageModel.image != null && !isWebDesktop) {
      await setCanvasConfig(
          sessionId,
          cursorModel.x,
          cursorModel.y,
          canvasModel.x,
          canvasModel.y,
          canvasModel.scale,
          ffiModel.pi.currentDisplay);
    }
    imageModel.callbacksOnFirstImage.clear();
    shortcutModel.clear();
    await imageModel.update(null);
    cursorModel.clear();
    ffiModel.clear();
    canvasModel.clear();
    inputModel.resetModifiers();
    // 释放相对鼠标模式的资源，以确保光标被恢复
    inputModel.disposeRelativeMouseMode();
    inputModel.disposeSideButtonTracking();
    if (closeSession) {
      await bind.sessionClose(sessionId: sessionId);
    }
    debugPrint('model $id closed');
    id = '';
  }

  void setMethodCallHandler(FMethod callback) {
    platformFFI.setMethodCallHandler(callback);
  }

  Future<bool> invokeMethod(String method, [dynamic arguments]) async {
    return await platformFFI.invokeMethod(method, arguments);
  }

  Future<T?> invokeMethodWithResult<T>(String method,
      [dynamic arguments]) async {
    return await platformFFI.invokeMethodWithResult<T>(method, arguments);
  }

  // 终端模型管理
  void registerTerminalModel(int terminalId, TerminalModel model) {
    debugPrint('[FFI] Registering terminal model for terminal $terminalId');
    _terminalModels[terminalId] = model;
  }

  void unregisterTerminalModel(int terminalId) {
    debugPrint('[FFI] Unregistering terminal model for terminal $terminalId');
    _terminalModels.remove(terminalId);
  }

  void routeTerminalResponse(Map<String, dynamic> evt) {
    final int terminalId = TerminalModel.getTerminalIdFromEvt(evt);

    // 如果存在特定的终端模型，则路由到它
    final model = _terminalModels[terminalId];
    if (model != null) {
      model.handleTerminalResponse(evt);
    }
  }
}

const kInvalidResolutionValue = -1;
const kVirtualDisplayResolutionValue = 0;

class Display {
  double x = 0;
  double y = 0;
  int width = 0;
  int height = 0;
  bool cursorEmbedded = false;
  int originalWidth = kInvalidResolutionValue;
  int originalHeight = kInvalidResolutionValue;
  double _scale = 1.0;
  double get scale => _scale > 1.0 ? _scale : 1.0;

  Display() {
    width = (isDesktop || isWebDesktop)
        ? kDesktopDefaultDisplayWidth
        : kMobileDefaultDisplayWidth;
    height = (isDesktop || isWebDesktop)
        ? kDesktopDefaultDisplayHeight
        : kMobileDefaultDisplayHeight;
  }

  @override
  bool operator ==(Object other) =>
      other is Display &&
      other.runtimeType == runtimeType &&
      _innerEqual(other);

  bool _innerEqual(Display other) =>
      other.x == x &&
      other.y == y &&
      other.width == width &&
      other.height == height &&
      other.cursorEmbedded == cursorEmbedded;

  bool get isOriginalResolutionSet =>
      originalWidth != kInvalidResolutionValue &&
      originalHeight != kInvalidResolutionValue;
  bool get isVirtualDisplayResolution =>
      originalWidth == kVirtualDisplayResolutionValue &&
      originalHeight == kVirtualDisplayResolutionValue;
  bool get isOriginalResolution =>
      width == (originalWidth * scale).round() &&
      height == (originalHeight * scale).round();
}

class Resolution {
  int width = 0;
  int height = 0;
  Resolution(this.width, this.height);

  @override
  String toString() {
    return 'Resolution($width,$height)';
  }
}

class Features {
  bool privacyMode = false;
}

const kInvalidDisplayIndex = -1;

class PeerInfo with ChangeNotifier {
  String version = '';
  String username = '';
  String hostname = '';
  String platform = '';
  bool sasEnabled = false;
  bool isSupportMultiUiSession = false;
  int currentDisplay = 0;
  int primaryDisplay = kInvalidDisplayIndex;
  RxList<Display> displays = <Display>[].obs;
  Features features = Features();
  List<Resolution> resolutions = [];
  Map<String, dynamic> platformAdditions = {};

  RxInt displaysCount = 0.obs;
  RxBool isSet = false.obs;

  bool get isWayland => platformAdditions[kPlatformAdditionsIsWayland] == true;
  bool get isInstalled =>
      platform != kPeerPlatformWindows ||
      platformAdditions[kPlatformAdditionsIsInstalled] == true;
  List<int> get RustDeskVirtualDisplays => List<int>.from(
      platformAdditions[kPlatformAdditionsRustDeskVirtualDisplays] ?? []);
  int get amyuniVirtualDisplayCount =>
      platformAdditions[kPlatformAdditionsAmyuniVirtualDisplays] ?? 0;

  bool get isSupportMultiDisplay =>
      (isDesktop || isWebDesktop) && isSupportMultiUiSession;
  bool get forceTextureRender => currentDisplay == kAllDisplayValue;

  bool get cursorEmbedded => tryGetDisplay()?.cursorEmbedded ?? false;

  bool get isRustDeskIdd =>
      platformAdditions[kPlatformAdditionsIddImpl] == 'rustdesk_idd';
  bool get isAmyuniIdd =>
      platformAdditions[kPlatformAdditionsIddImpl] == 'amyuni_idd';

  Display? tryGetDisplay({int? display}) {
    if (displays.isEmpty) {
      return null;
    }
    display ??= currentDisplay;
    if (display == kAllDisplayValue) {
      return displays[0];
    } else {
      if (display > 0 && display < displays.length) {
        return displays[display];
      } else {
        return displays[0];
      }
    }
  }

  Display? tryGetDisplayIfNotAllDisplay({int? display}) {
    if (displays.isEmpty) {
      return null;
    }
    display ??= currentDisplay;
    if (display == kAllDisplayValue) {
      return null;
    }
    if (display >= 0 && display < displays.length) {
      return displays[display];
    } else {
      return null;
    }
  }

  List<Display> getCurDisplays() {
    if (currentDisplay == kAllDisplayValue) {
      return displays;
    } else {
      if (currentDisplay >= 0 && currentDisplay < displays.length) {
        return [displays[currentDisplay]];
      } else {
        return [];
      }
    }
  }

  double scaleOfDisplay(int display) {
    if (display >= 0 && display < displays.length) {
      return displays[display].scale;
    }
    return 1.0;
  }

  Rect? getDisplayRect(int display) {
    final d = tryGetDisplayIfNotAllDisplay(display: display);
    if (d == null) return null;
    return Rect.fromLTWH(d.x, d.y, d.width.toDouble(), d.height.toDouble());
  }
}

const canvasKey = 'canvas';

Future<void> setCanvasConfig(
    SessionID sessionId,
    double xCursor,
    double yCursor,
    double xCanvas,
    double yCanvas,
    double scale,
    int currentDisplay) async {
  final p = <String, dynamic>{};
  p['xCursor'] = xCursor;
  p['yCursor'] = yCursor;
  p['xCanvas'] = xCanvas;
  p['yCanvas'] = yCanvas;
  p['scale'] = scale;
  p['currentDisplay'] = currentDisplay;
  await bind.sessionSetFlutterOption(
      sessionId: sessionId, k: canvasKey, v: jsonEncode(p));
}

Future<Map<String, dynamic>?> getCanvasConfig(SessionID sessionId) async {
  if (!isWebDesktop) return null;
  var p =
      await bind.sessionGetFlutterOption(sessionId: sessionId, k: canvasKey);
  if (p == null || p.isEmpty) return null;
  try {
    Map<String, dynamic> m = json.decode(p);
    return m;
  } catch (e) {
    return null;
  }
}

Future<void> initializeCursorAndCanvas(FFI ffi) async {
  var p = await getCanvasConfig(ffi.sessionId);
  int currentDisplay = 0;
  if (p != null) {
    currentDisplay = p['currentDisplay'];
  }
  if (p == null || currentDisplay != ffi.ffiModel.pi.currentDisplay) {
    ffi.cursorModel.updateDisplayOrigin(
        ffi.ffiModel.rect?.left ?? 0, ffi.ffiModel.rect?.top ?? 0);
    return;
  }
  double xCursor = p['xCursor'];
  double yCursor = p['yCursor'];
  double xCanvas = p['xCanvas'];
  double yCanvas = p['yCanvas'];
  double scale = p['scale'];
  ffi.cursorModel.updateDisplayOriginWithCursor(ffi.ffiModel.rect?.left ?? 0,
      ffi.ffiModel.rect?.top ?? 0, xCursor, yCursor);
  ffi.canvasModel.update(xCanvas, yCanvas, scale);
}

clearWaitingForImage(OverlayDialogManager? dialogManager, SessionID sessionId) {
  dialogManager?.dismissByTag('$sessionId-waiting-for-image');
}
