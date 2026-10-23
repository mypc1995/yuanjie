#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端联调: 面板建任务 -> Windows 构建机 agent 领单编译 -> 产物回传 -> 面板下载。

只用标准库 (urllib), 内网机上不用装 requests。

用法:
    python test_e2e_build.py <面板地址> <用户名> <密码> [等待秒数]

前置: 已经在本机起了 agent:
    python agent/agent.py --panel http://<面板地址> --token <agent_token> --src <源码目录>
"""
import http.cookiejar
import os
import re
import sys
import time
import urllib.parse
import urllib.request

BASE = sys.argv[1].rstrip("/")
USER, PASS = sys.argv[2], sys.argv[3]
WAIT = int(sys.argv[4]) if len(sys.argv) > 4 else 3600

FAILS = []
# 面板把 phase 渲染成 <span class="pill ...">building-rust</span>
PHASE_RE = re.compile(r'<span class="pill[^"]*">\s*([a-zA-Z\-_]+)\s*</span>')


def check(label, cond, detail=""):
    if cond:
        print("  OK   %s" % label, flush=True)
    else:
        print("  FAIL %s  %s" % (label, detail), flush=True)
        FAILS.append(label)


class Session:
    def __init__(self):
        self.jar = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(self.jar))

    def get(self, path):
        with self.opener.open(BASE + path, timeout=60) as r:
            return r.getcode(), r.read().decode("utf-8", "replace")

    def post(self, path, data):
        body = urllib.parse.urlencode(data).encode("utf-8")
        req = urllib.request.Request(BASE + path, data=body, method="POST")
        with self.opener.open(req, timeout=60) as r:
            return r.getcode(), r.geturl(), r.read().decode("utf-8", "replace")

    def download(self, path, out):
        with self.opener.open(BASE + path, timeout=900) as r:
            payload = r.read()
        with open(out, "wb") as f:
            f.write(payload)
        return len(payload)


def csrf_of(html):
    m = re.search(r'name="csrf" value="([^"]+)"', html)
    return m.group(1) if m else ""


s = Session()

print("=== 1. 登录面板 ===", flush=True)
code, html = s.get("/login")
check("GET /login -> 200", code == 200, code)
code, url, html = s.post("/login", {"username": USER, "password": PASS,
                                    "csrf": csrf_of(html), "next": "/"})
check("登录成功", code == 200 and "/login" not in url, url)

print("\n=== 2. 派发 windows 构建任务 ===", flush=True)
code, html = s.get("/build")
check("GET /build -> 200", code == 200, code)
token = csrf_of(html)
check("拿到 CSRF", bool(token))
code, url, html = s.post("/build", {"csrf": token, "target": "windows-x64"})
m = re.search(r"job=([^&\"']+)", url)
job_id = m.group(1) if m else ""
check("POST /build -> 200", code == 200, code)
check("任务号已生成", bool(job_id), url)
print("  任务号: %s" % job_id)

print("\n=== 3. 等 agent 接单并编译 ===", flush=True)
deadline = time.time() + WAIT
phase = ""
last = ""
while time.time() < deadline:
    code, html = s.get("/build?job=%s" % job_id)
    m = PHASE_RE.search(html)
    p = m.group(1) if m else "unknown"
    if p != phase:
        phase = p
        print("  [%s] phase = %s" % (time.strftime("%H:%M:%S"), p), flush=True)
    if p in ("done", "failed", "error"):
        last = p
        break
    time.sleep(10)
check("任务走到终态", last in ("done", "failed", "error"), last)

print("\n=== 4. 产物可下载 ===", flush=True)
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "e2e-artifact.zip")
try:
    size = s.download("/build/%s/artifact" % job_id, out)
    check("下载产物 -> 200", True)
    check("产物非空", size > 1000, size)
    print("  产物已保存: %s (%.1f MB)" % (out, size / 1024.0 / 1024.0))
except urllib.error.HTTPError as e:
    check("下载产物 -> 200", False, "%s %s" % (e.code, e.reason))

print("\n=== 5. 构建日志尾部 ===", flush=True)
code, html = s.get("/build?job=%s" % job_id)
tail = re.findall(r"<pre[^>]*>(.*?)</pre>", html, re.S)
if tail:
    for line in tail[-1].splitlines()[-30:]:
        print("  | " + re.sub(r"<[^>]+>", "", line))

print("")
if FAILS:
    print("失败 %d 项: %s" % (len(FAILS), FAILS))
    sys.exit(1)
print("端到端联调全部通过")
