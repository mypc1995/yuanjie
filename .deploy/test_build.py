#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""构建服务 + agent 接口的端到端测试。"""
import io
import json
import os
import re
import sys
import time
import zipfile

import requests

BASE = "http://127.0.0.1:21120"
USER, PASS = sys.argv[1], sys.argv[2]
TOKEN = sys.argv[3]
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


s = requests.Session()
r = s.get(BASE + "/login", timeout=10)
s.post(BASE + "/login", data={"username": USER, "password": PASS,
                              "csrf": csrf_of(r.text), "next": "/"}, timeout=10)

print("=== 1. 构建页 ===")
r = s.get(BASE + "/build", timeout=15)
check("GET /build -> 200", r.status_code == 200, r.status_code)
check("页面有目标平台选择", "linux-x64" in r.text and "windows-x64" in r.text)
check("页面显示要烘焙的服务器配置", "ID Server" in r.text)

print()
print("=== 2. agent 鉴权 ===")
r = requests.get(BASE + "/api/agent/jobs/next", timeout=10)
check("无 token -> 401", r.status_code == 401, r.status_code)
r = requests.get(BASE + "/api/agent/jobs/next", headers={"X-Agent-Token": "wrong"}, timeout=10)
check("错 token -> 401", r.status_code == 401, r.status_code)
h = {"X-Agent-Token": TOKEN}
r = requests.post(BASE + "/api/agent/register", headers=h,
                  json={"agent_id": "test-agent", "platform": "windows"}, timeout=10)
check("注册 agent -> 200", r.status_code == 200, r.status_code)

print()
print("=== 3. 派发一个 windows 任务 ===")
page = s.get(BASE + "/build", timeout=10)
token = csrf_of(page.text)
r = s.post(BASE + "/build", data={"csrf": token, "target": "windows-x64"}, timeout=15,
           allow_redirects=True)
check("创建任务成功", "任务已派发" in r.text, r.text[:200])
m = re.search(r"job-\d{8}-\d{6}-[0-9a-f]{6}", r.text)
job_id = m.group(0) if m else None
check("拿到任务号", job_id is not None, r.text[:200])

print()
print("=== 4. agent 领单 ===")
r = requests.get(BASE + "/api/agent/jobs/next", headers=h, timeout=30)
check("领到任务 -> 200", r.status_code == 200, r.status_code)
if r.status_code == 200:
    job = r.json()
    check("领到的就是刚派的那个", job.get("id") == job_id, "%s vs %s" % (job.get("id"), job_id))
    check("任务带上了服务器配置", bool((job.get("server") or {}).get("host")),
          json.dumps(job.get("server")))
    check("任务带上了品牌资源", len(job.get("assets") or {}) >= 1,
          list((job.get("assets") or {}).keys()))
    check("任务带上了应用名", job.get("app_name") == "测试远控", job.get("app_name"))
    # 领过的任务不该再被领一次
    r2 = requests.get(BASE + "/api/agent/jobs/next", headers=h, timeout=30)
    check("同一个任务不会被领两次", r2.status_code == 204 or r2.json().get("id") != job_id,
          r2.status_code)

print()
print("=== 5. agent 回传状态 + 上传产物 ===")
r = requests.post(BASE + "/api/agent/jobs/%s/status" % job_id, headers=h,
                  json={"phase": "building-rust", "agent_id": "test-agent"}, timeout=10)
check("状态回传 -> 200", r.status_code == 200, r.status_code)

buf = io.BytesIO()
with zipfile.ZipFile(buf, "w") as zf:
    zf.writestr("rustdesk.exe", "fake")
    zf.writestr("data/flutter_assets/assets/logo.png", "fake-logo")
payload = buf.getvalue()
r = requests.put(BASE + "/api/agent/jobs/%s/artifact" % job_id, headers=h, data=payload,
                 timeout=60)
check("产物上传 -> 200", r.status_code == 200, r.status_code)
check("回传了大小", (r.json() or {}).get("size") == len(payload), r.text[:200])

print()
print("=== 6. 面板上能下载 ===")
r = s.get(BASE + "/build?job=%s" % job_id, timeout=15)
check("任务详情页 200", r.status_code == 200, r.status_code)
check("详情页显示完成", "done" in r.text)
check("详情页有下载链接", "artifact" in r.text)
r = s.get(BASE + "/build/%s/artifact" % job_id, timeout=30)
check("下载产物 -> 200", r.status_code == 200, r.status_code)
check("下载的是 zip", r.content[:2] == b"PK", r.content[:8])

print()
print("=== 7. 状态持久化 ===")
st = requests.get(BASE + "/api/status", timeout=10)
check("status 接口仍然正常 (未登录 302/401 均可)", st.status_code in (200, 302, 401), st.status_code)

print()
if FAILS:
    print("=== 失败 %d 项: %s" % (len(FAILS), ", ".join(FAILS)))
    sys.exit(1)
print("=== 全部通过 ===")
sys.exit(0)
