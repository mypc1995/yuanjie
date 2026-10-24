import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/desktop/pages/remote_tab_page.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:provider/provider.dart';

/// 多标签页的桌面远控界面
class DesktopRemoteScreen extends StatelessWidget {
  final Map<String, dynamic> params;

  DesktopRemoteScreen({Key? key, required this.params}) : super(key: key) {
      bind.mainInitInputSource();
      stateGlobal.getInputSource(force: true);
  }

  @override
  Widget build(BuildContext context) {
    return MultiProvider(
        providers: [
          ChangeNotifierProvider.value(value: gFFI.ffiModel),
          ChangeNotifierProvider.value(value: gFFI.imageModel),
          ChangeNotifierProvider.value(value: gFFI.cursorModel),
          ChangeNotifierProvider.value(value: gFFI.canvasModel),
        ],
        child: Scaffold(
          // 设置透明背景，把缩放区域填充到 flutter 视图之外。
          // 这样壁纸就能透过我们的缩放区域显示。（目前仅 Linux）
          backgroundColor: isLinux ? Colors.transparent : null,
          body: ConnectionTabPage(
            params: params,
          ),
        ));
  }
}
