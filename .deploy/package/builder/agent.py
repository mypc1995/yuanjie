#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""RustDesk 客户端构建 agent (跑在 Windows 构建机上)。

面板不直接编 Windows 客户端 —— Flutter 不支持从 Linux 交叉构建 Windows。
所以 Windows 目标走 agent: 面板派发任务, 这台机器编译, 产物回传给面板,
用户再从面板下载。

用法:
    python agent.py --panel http://<服务器IP>:21120 --user admin --password xxx
"""

import argparse
import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
BUILDER_ROOT = os.environ.get("RUSTDESK_BUILDER_ROOT", r"D:\Software\RustDesk\.deploy\builder")
SRC_DIR = os.environ.get("RUSTDESK_SRC_DIR", r"D:\Software\RustDesk")
POLL_SECONDS = int(os.environ.get("RUSTDESK_AGENT_POLL", "10"))


def refresh_windows_env():
    """把用户级环境变量补进当前进程。

    install-offline.ps1 把 PATH / CARGO_HOME / VCPKG_ROOT 这些写进**用户**环境变量,
    但已经开着的命令行窗口不会自动更新。agent 若从旧窗口启动, 就找不到 cargo /
    flutter。这里直接从注册表 HKCU\\Environment 读一遍补上, 免得 agent 起来了
    却因为找不到工具链而失败。
    """
    if os.name != "nt":
        return
    try:
        import winreg
    except ImportError:
        return
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment") as k:
            wanted = ["Path", "CARGO_HOME", "VCPKG_ROOT", "VCPKG_INSTALLED_ROOT",
                      "LIBCLANG_PATH", "PUB_CACHE", "FLUTTER_CMAKE_PATH"]
            for name in wanted:
                try:
                    val, _ = winreg.QueryValueEx(k, name)
                except OSError:
                    continue
                if not val:
                    continue
                if name == "Path":
                    cur = os.environ.get("PATH", "")
                    # 用户 PATH 排前面, 保证 RustHome\bin 里的 rustc 压过 rustup shim
                    if val not in cur.split(";"):
                        os.environ["PATH"] = val + ";" + cur
                elif not os.environ.get(name):
                    os.environ[name] = val
    except OSError:
        pass



class Panel:
    def __init__(self, base, token):
        self.base = base.rstrip("/")
        self.token = token

    def _req(self, method, path, data=None, raw=False, timeout=120):
        url = self.base + path
        body = None
        headers = {"X-Agent-Token": self.token}
        if data is not None:
            body = json.dumps(data).encode("utf-8")
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                payload = r.read()
                if raw:
                    return r.status, payload, dict(r.headers)
                return r.status, json.loads(payload.decode("utf-8")) if payload else None
        except urllib.error.HTTPError as e:
            payload = e.read()
            if raw:
                return e.code, payload, dict(e.headers)
            try:
                return e.code, json.loads(payload.decode("utf-8"))
            except Exception:
                return e.code, None

    def register(self, info):
        return self._req("POST", "/api/agent/register", info)

    def next_job(self):
        st, data = self._req("GET", "/api/agent/jobs/next", timeout=60)
        if st == 200 and data:
            return data
        return None

    def report(self, job_id, status):
        return self._req("POST", "/api/agent/jobs/%s/status" % job_id, status)

    def upload(self, job_id, path):
        with open(path, "rb") as f:
            payload = f.read()
        url = "%s/api/agent/jobs/%s/artifact" % (self.base, job_id)
        req = urllib.request.Request(
            url, data=payload, method="PUT",
            headers={"X-Agent-Token": self.token,
                     "Content-Type": "application/octet-stream",
                     "X-Filename": os.path.basename(path)})
        try:
            with urllib.request.urlopen(req, timeout=900) as r:
                return r.status
        except urllib.error.HTTPError as e:
            print("上传失败: %s %s" % (e.code, e.read()[:200]))
            return e.code

    def upload_log(self, job_id, path):
        """把构建日志回传给面板 —— 面板的「构建日志」面板靠它。"""
        try:
            with open(path, "rb") as f:
                payload = f.read()
        except OSError:
            return None
        url = "%s/api/agent/jobs/%s/log" % (self.base, job_id)
        req = urllib.request.Request(
            url, data=payload, method="PUT",
            headers={"X-Agent-Token": self.token,
                     "Content-Type": "text/plain; charset=utf-8"})
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                return r.status
        except urllib.error.HTTPError as e:
            print("日志上传失败: %s %s" % (e.code, e.read()[:200]))
            return e.code


def tail_log(path, n=40):
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            return f.read().splitlines()[-n:]
    except OSError:
        return []


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--panel", required=True)
    ap.add_argument("--token", required=True, help="面板的 agent_token")
    ap.add_argument("--once", action="store_true", help="只取一个任务就退出")
    ap.add_argument("--src", default=SRC_DIR)
    args = ap.parse_args()

    panel = Panel(args.panel, args.token)
    refresh_windows_env()
    agent_id = "win-%s" % uuid.uuid4().hex[:8]
    st, data = panel.register({
        "agent_id": agent_id,
        "platform": "windows",
        "host": os.environ.get("COMPUTERNAME", "unknown"),
        "src_dir": args.src,
    })
    print("注册: %s %s" % (st, data))
    if st != 200:
        return 1

    while True:
        job = panel.next_job()
        if not job:
            if args.once:
                return 0
            time.sleep(POLL_SECONDS)
            continue

        job_id = job.get("id")
        print("=== 接到任务 %s (%s) ===" % (job_id, job.get("target")))
        job_dir = os.path.join(BUILDER_ROOT, "jobs", job_id)
        os.makedirs(job_dir, exist_ok=True)
        job_path = os.path.join(job_dir, "job.json")
        with open(job_path, "w", encoding="utf-8") as f:
            json.dump(job, f, ensure_ascii=False, indent=2)

        env = dict(os.environ)
        env["RUSTDESK_SRC_DIR"] = args.src
        env["RUSTDESK_BUILDER_ROOT"] = BUILDER_ROOT
        script = os.path.join(HERE, "build_job.py")
        proc = subprocess.Popen([sys.executable, script, job_path],
                                env=env, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, universal_newlines=True)
        # build_job.py 自己已经往 build.log 写了一份, 这里另开一个文件,
        # 否则同一个日志会被写两遍。
        log_path = os.path.join(job_dir, "agent-console.log")
        with open(log_path, "a", encoding="utf-8") as lf:
            for line in proc.stdout:
                print(line, end="")
                lf.write(line)
                lf.flush()
        proc.wait()

        # 构建日志回传面板, 面板的「构建日志」面板靠它显示
        panel.upload_log(job_id, os.path.join(job_dir, "build.log"))

        status = {"phase": "done" if proc.returncode == 0 else "failed",
                  "agent_id": agent_id, "log": os.path.join(job_dir, "build.log")}
        if proc.returncode != 0:
            status["error"] = "构建失败, 见日志"
        panel.report(job_id, status)

        if proc.returncode == 0:
            out_dir = os.path.join(BUILDER_ROOT, "artifacts", job_id)
            if os.path.isdir(out_dir):
                for name in os.listdir(out_dir):
                    if name.endswith((".zip", ".tar.gz")):
                        print("上传产物 %s" % name)
                        panel.upload(job_id, os.path.join(out_dir, name))
        print("=== 任务 %s 结束 (rc=%s) ===" % (job_id, proc.returncode))

        if args.once:
            return 0


if __name__ == "__main__":
    sys.exit(main())
