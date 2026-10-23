#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""面板端到端功能测试: 登录 / 各页面 / 品牌上传 / 品牌包导出 / 配置串校验。"""
import io
import json
import re
import sys
import zipfile

import requests

BASE = "http://127.0.0.1:21120"
USER = sys.argv[1]
PASS = sys.argv[2]

FAILS = []


def check(label, cond, detail=""):
    if cond:
        print("  OK   %s" % label)
    else:
        print("  FAIL %s  %s" % (label, detail))
        FAILS.append(label)


def csrf_of(html):
    m = re.search(r'name="csrf" value="([^"]+)"', html)
    return m.group(1) if m else ""


print("=== 1. 登录 ===")
s = requests.Session()
r = s.get(BASE + "/login", timeout=10)
check("GET /login -> 200", r.status_code == 200, r.status_code)
check("登录页有账号输入框", 'name="username"' in r.text)
check("登录页有密码输入框", 'name="password"' in r.text)
token = csrf_of(r.text)

r = s.post(BASE + "/login",
           data={"username": USER, "password": PASS, "csrf": token, "next": "/"},
           timeout=10, allow_redirects=True)
check("POST /login -> 落到仪表盘", "仪表盘" in r.text, r.status_code)

# 错误密码
s2 = requests.Session()
r2 = s2.get(BASE + "/login", timeout=10)
t2 = csrf_of(r2.text)
r2 = s2.post(BASE + "/login",
             data={"username": USER, "password": "wrong-password", "csrf": t2},
             timeout=10, allow_redirects=True)
check("错误密码被拒", "账号或密码不正确" in r2.text)

print()
print("=== 2. 各页面 ===")
for path, label, needle in [
    ("/", "仪表盘", "服务状态"),
    ("/config", "基础配置", "hbbs"),
    ("/branding", "品牌定制", "上传"),
    ("/client", "客户端集成", "ID Server"),
    ("/keys", "密钥", "公钥"),
    ("/lists", "黑白名单", "blacklist"),
    ("/logs", "日志", "log"),
]:
    r = s.get(BASE + path, timeout=15)
    check("%-10s %s" % (label, path), r.status_code == 200 and needle in r.text,
          "status=%s" % r.status_code)

print()
print("=== 3. 未登录访问受保护页面应跳登录 ===")
s3 = requests.Session()
r3 = s3.get(BASE + "/config", timeout=10, allow_redirects=False)
check("未登录 -> 302 跳登录", r3.status_code == 302 and "/login" in r3.headers.get("Location", ""),
      "%s %s" % (r3.status_code, r3.headers.get("Location")))

print()
print("=== 4. 品牌上传 ===")
# 造一张最小 PNG (1x1 红点)
PNG = bytes(bytearray([
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0x60, 0x60, 0x00, 0x00,
    0x00, 0x04, 0x00, 0x01, 0x8D, 0x0D, 0x0D, 0x8B, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82]))
page = s.get(BASE + "/branding", timeout=10)
token = csrf_of(page.text)
files = {
    "asset_logo": ("logo.png", io.BytesIO(PNG), "image/png"),
    "asset_logo_light": ("logo.png", io.BytesIO(PNG), "image/png"),
    "asset_logo_dark": ("logo.png", io.BytesIO(PNG), "image/png"),
    "asset_icon": ("icon.png", io.BytesIO(PNG), "image/png"),
}
r = s.post(BASE + "/branding",
           data={"csrf": token, "app_name": "测试远控", "primary_color": "#0ea5e9",
                 "apply_all": "1"},
           files=files, timeout=20, allow_redirects=True)
check("POST /branding 成功", "品牌已保存" in r.text, r.status_code)

r = s.get(BASE + "/api/branding", timeout=10)
brand = r.json()
check("api/branding 应用名已改", brand.get("app_name") == "测试远控", brand.get("app_name"))
check("api/branding 主题色已改", brand.get("primary_color") == "#0ea5e9", brand.get("primary_color"))
check("api/branding logo 已记录", brand.get("logo") == "logo.png", brand.get("logo"))

r = s.get(BASE + "/api/branding/logo.png", timeout=10)
check("logo.png 可下载", r.status_code == 200 and r.content[:4] == b"\x89PNG",
      "status=%s len=%s" % (r.status_code, len(r.content)))

print()
print("=== 5. 面板自身吃 branding ===")
r = s.get(BASE + "/", timeout=10)
check("面板头部显示新应用名", "测试远控" in r.text)
check("面板用新主题色", "#0ea5e9" in r.text)

print()
print("=== 6. 客户端配置 ===")
page = s.get(BASE + "/client", timeout=10)
token = csrf_of(page.text)
r = s.post(BASE + "/client",
           data={"csrf": token, "host": "203.0.113.10", "relay": "203.0.113.10",
                 "api": "", "key": "TESTKEY123"},
           timeout=10, allow_redirects=True)
check("POST /client 保存成功", "客户端服务器配置已保存" in r.text)

r = s.get(BASE + "/api/client-config", timeout=10)
data = r.json()
enc = data.get("config", "")
check("api/client-config 返回配置串", bool(enc), enc[:40])


def client_decode(msg):
    """复刻 flutter/lib/common.dart ServerConfig.decode 的 base64 分支。"""
    import base64 as b64
    try:
        return json.loads(msg)
    except ValueError:
        pass
    inp = "".join(reversed(list(msg)))
    norm = inp.replace("-", "+").replace("_", "/") + "=" * (-len(inp) % 4)
    return json.loads(b64.b64decode(norm).decode("utf-8", "replace"))


dec = client_decode(enc)
expect = {"host": "203.0.113.10", "relay": "203.0.113.10", "api": "", "key": "TESTKEY123"}
check("客户端逻辑解码一致", dec == expect, "%s" % dec)

print()
print("=== 7. 品牌包导出 ===")
r = s.get(BASE + "/client/bundle", timeout=20)
check("品牌包可下载", r.status_code == 200, r.status_code)
zf = zipfile.ZipFile(io.BytesIO(r.content))
names = zf.namelist()
for want in ["branding.json", "client-config.json", "server-config.b64", "logo.png",
             "logo_light.png", "logo_dark.png", "icon.png", "README.txt"]:
    check("包里有 %s" % want, want in names)
b = json.loads(zf.read("branding.json"))
check("包内 branding.json 应用名", b.get("app_name") == "测试远控", b.get("app_name"))
b64str = zf.read("server-config.b64").decode().strip()
check("包内 server-config.b64 可被客户端解码", client_decode(b64str) == expect)

print()
if FAILS:
    print("=== 失败 %d 项: %s" % (len(FAILS), ", ".join(FAILS)))
    sys.exit(1)
print("=== 全部通过 ===")
sys.exit(0)
