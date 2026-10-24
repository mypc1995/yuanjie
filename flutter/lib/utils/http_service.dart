import 'dart:convert';
import 'package:flutter/foundation.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:http/http.dart' as http;
import '../models/platform_model.dart';
import 'package:flutter_hbb/common.dart';
export 'package:http/http.dart' show Response;

enum HttpMethod { get, post, put, delete }

class HttpService {
  Future<http.Response> sendRequest(
    Uri url,
    HttpMethod method, {
    Map<String, String>? headers,
    dynamic body,
  }) async {
    headers ??= {'Content-Type': 'application/json'};

    // 在非 Web 平台上使用 Rust 的 HTTP 实现以保持一致性。
    var useFlutterHttp = (isWeb || kIsWeb);
    if (!useFlutterHttp) {
      final enableFlutterHttpOnRust =
          mainGetLocalBoolOptionSync(kOptionEnableFlutterHttpOnRust);
      // 在以下情况使用 Flutter 的 http：
      // 未设置 `enableFlutterHttpOnRust` 且没有设置代理
      useFlutterHttp =
          !(enableFlutterHttpOnRust || await bind.mainGetProxyStatus());
    }

    if (useFlutterHttp) {
      return await _pollFlutterHttp(url, method, headers: headers, body: body);
    }

    String headersJson = jsonEncode(headers);
    String methodName = method.toString().split('.').last;
    await bind.mainHttpRequest(
        url: url.toString(),
        method: methodName.toLowerCase(),
        body: body,
        header: headersJson);

    var resJson = await _pollForResponse(url.toString());
    return _parseHttpResponse(resJson);
  }

  // 仅对下面的纯 Dart 分支设置界限，否则操作系统会
  // 让它永久挂起（例如被黑洞掉的 TLS 握手），参见 #15700。
  // Rust 分支自带每次尝试 12s 的超时，并且必须
  // 被完整 await：Dart 侧的超时会与
  // 被放弃请求的、以 URL 为键的 ASYNC_HTTP_STATUS 条目产生竞态。
  static const _requestTimeout = Duration(seconds: 30);

  Future<http.Response> _pollFlutterHttp(
    Uri url,
    HttpMethod method, {
    Map<String, String>? headers,
    dynamic body,
  }) async {
    final client = http.Client();
    try {
      var response = http.Response('', 400);

      switch (method) {
        case HttpMethod.get:
          response =
              await client.get(url, headers: headers).timeout(_requestTimeout);
          break;
        case HttpMethod.post:
          response = await client
              .post(url, headers: headers, body: body)
              .timeout(_requestTimeout);
          break;
        case HttpMethod.put:
          response = await client
              .put(url, headers: headers, body: body)
              .timeout(_requestTimeout);
          break;
        case HttpMethod.delete:
          response = await client
              .delete(url, headers: headers, body: body)
              .timeout(_requestTimeout);
          break;
        default:
          throw Exception('Unsupported HTTP method');
      }

      return response;
    } finally {
      client.close();
    }
  }

  Future<String> _pollForResponse(String url) async {
    String? responseJson = " ";
    while (responseJson == " ") {
      responseJson = await bind.mainGetHttpStatus(url: url);
      if (responseJson == null) {
        throw Exception('The HTTP request failed');
      }
      if (responseJson == " ") {
        await Future.delayed(const Duration(milliseconds: 100));
      }
    }
    return responseJson!;
  }

  http.Response _parseHttpResponse(String responseJson) {
    try {
      var parsedJson = jsonDecode(responseJson);
      String body = parsedJson['body'];
      Map<String, String> headers = {};
      for (var key in parsedJson['headers'].keys) {
        headers[key] = parsedJson['headers'][key];
      }
      int statusCode = parsedJson['status_code'];
      return http.Response(body, statusCode, headers: headers);
    } catch (e) {
      print('Failed to parse response\n$responseJson\nError:\n$e');
      throw Exception('Failed to parse response.\n$responseJson');
    }
  }
}

Future<http.Response> get(Uri url, {Map<String, String>? headers}) async {
  return await HttpService().sendRequest(url, HttpMethod.get, headers: headers);
}

Future<http.Response> post(Uri url,
    {Map<String, String>? headers, Object? body, Encoding? encoding}) async {
  return await HttpService()
      .sendRequest(url, HttpMethod.post, body: body, headers: headers);
}

Future<http.Response> put(Uri url,
    {Map<String, String>? headers, Object? body, Encoding? encoding}) async {
  return await HttpService()
      .sendRequest(url, HttpMethod.put, body: body, headers: headers);
}

Future<http.Response> delete(Uri url,
    {Map<String, String>? headers, Object? body, Encoding? encoding}) async {
  return await HttpService()
      .sendRequest(url, HttpMethod.delete, body: body, headers: headers);
}
