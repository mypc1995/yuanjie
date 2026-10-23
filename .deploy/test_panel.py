#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""按客户端真实的 decode 逻辑校验面板生成的配置串。"""
import base64
import json
import re
import sys
import urllib.request
import urllib.parse
import base64 as b64

BASE = "http://127.0.0.1:21120"
USER = sys.argv[1]
PASS = sys.argv[2]

# 打开带认证的连接
mgr = urllib.request.HTTPPasswordMgrWithDefaultRealm()
mgr.add_password(None, BASE, USER, PASS)
opener = urllib.request.build_opener(urllib.request.HTTPBasicAuthHandler(mgr))


def get(path):
    with opener.open(BASE + path, timeout=10) as r:
        return r.read().decode("utf-8", "replace")


def client_decode(msg):
    """完全复刻 flutter/lib/common.dart ServerConfig.decode 的 base64 分支。"""
    try:
        return json.loads(msg)  # back compatible: 直接是 JSON
    except Exception:
        pass
    # msg.split('').reversed.join('')
    inp = "".join(reversed(list(msg)))
    # base64.normalize: 把 URL-safe 字符转回标准字母表，并补/去 padding
    normalized = inp.replace("-", "+").replace("_", "/")
    normalized += "=" * (-len(normalized) % 4)
    raw = b64.b64decode(normalized)
    return json.loads(raw.decode("utf-8", "replace"))


print("=== 各页面 HTTP 状态 ===")
for path, label in [("/", "仪表盘"), ("/config", "基础配置"), ("/keys", "密钥"),
                    ("/lists", "黑白名单"), ("/logs", "日志"), ("/client", "客户端配置"),
                    ("/api/status", "状态 API")]:
    try:
        code = urllib.request.urlopen.__self__ if False else None
        with opener.open(BASE + path, timeout=10) as r:
            print("  %-14s %s  %d" % (label, path, r.status))
    except Exception as exc:
        print("  %-14s %s  FAIL %s" % (label, path, exc))

print()
print("=== 客户端配置串校验 ===")
page = get("/client?host=203.0.113.10&relay=203.0.113.10&api=&key=TESTKEY123")
m = re.search(r'<pre class="mono" id="cfg">([^<]+)</pre>', page)
if not m:
    print("  没抓到配置串！")
    sys.exit(1)
encoded = m.group(1).strip()
print("  生成的串: %s" % encoded)

decoded = client_decode(encoded)
print("  客户端解码结果: %s" % json.dumps(decoded, ensure_ascii=False))
expect = {"host": "203.0.113.10", "relay": "203.0.113.10", "api": "", "key": "TESTKEY123"}
ok = decoded == expect
print("  与预期一致: %s" % ("是 ✓" if ok else "否 ✗ -> 期望 %s" % expect))

print()
print("=== 反向：把客户端 decode 过的结果重新 encode 是否能还原 ===")
# 客户端 decode 之后再 encode 应该回到原串
raw = json.dumps(expect, separators=(",", ":"), ensure_ascii=False)
re_encoded = b64.urlsafe_b64encode(raw.encode("utf-8")).decode("ascii")
re_encoded = re_encoded[::-1]
print("  还原一致: %s" % ("是 ✓" if re_encoded == encoded else "否 ✗\n    %s\n    %s" % (re_encoded, encoded)))

print()
print("=== /api/status ===")
print(get("/api/status")[:400])

sys.exit(0 if ok else 1)
