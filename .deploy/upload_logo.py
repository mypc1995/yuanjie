#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把一张真实 logo 传上��, 验证上传 -> 生效 -> 导出整条链路。"""
import io
import json
import re
import sys
import zipfile

import requests

BASE = "http://127.0.0.1:21120"
USER, PASS = sys.argv[1], sys.argv[2]
LOGO = sys.argv[3]      # 服务器上的 png 路径
ICON = sys.argv[4]      # 服务器上的方形 png 路径


def csrf_of(html):
    m = re.search(r'name="csrf" value="([^"]+)"', html)
    return m.group(1) if m else ""


s = requests.Session()
r = s.get(BASE + "/login", timeout=10)
s.post(BASE + "/login", data={"username": USER, "password": PASS,
                              "csrf": csrf_of(r.text), "next": "/"}, timeout=10)

page = s.get(BASE + "/branding", timeout=10)
token = csrf_of(page.text)

with open(LOGO, "rb") as f:
    logo_bytes = f.read()
with open(ICON, "rb") as f:
    icon_bytes = f.read()

r = s.post(BASE + "/branding",
           data={"csrf": token, "app_name": "测试远控", "primary_color": "#0ea5e9",
                 "apply_all": "1"},
           files={
               "asset_logo": ("logo.png", io.BytesIO(logo_bytes), "image/png"),
               "asset_icon": ("icon.png", io.BytesIO(icon_bytes), "image/png"),
           },
           timeout=30, allow_redirects=True)
print("上传:", "OK" if "品牌已保存" in r.text else "FAIL")

for name in ("logo.png", "logo_light.png", "logo_dark.png", "icon.png"):
    rr = s.get(BASE + "/api/branding/" + name, timeout=10)
    print("  %-16s %s  %d bytes" % (name, rr.status_code, len(rr.content)))

zf = zipfile.ZipFile(io.BytesIO(s.get(BASE + "/client/bundle", timeout=20).content))
print("品牌包:", sorted(n for n in zf.namelist() if not n.endswith(".txt")))
print("logo.png in zip: %d bytes" % len(zf.read("logo.png")))
