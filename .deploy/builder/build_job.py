#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""RustDesk 客户端构建任务执行器。

用法: python3 build_job.py <job.json>

同一份脚本跑在两个地方:
  * 麒麟服务器  -> 直接构建 Linux 客户端
  * Windows 构建机 (agent) -> 构建 Windows 客户端

职责:
  1. 把品牌资源写进源码树 (logo / 图标)
  2. 把服务器配置预置成客户端默认值 (装完即用, 不需要手动配服务器)
  3. 跑 cargo build + flutter build
  4. 打包产物
"""

import base64
import errno
import json
import os
import shutil
import subprocess
import sys
import time

BUILDER_ROOT = os.environ.get("RUSTDESK_BUILDER_ROOT", "/var/lib/rustdesk-builder")
SRC_DIR = os.environ.get("RUSTDESK_SRC_DIR", "/opt/rustdesk-src")
ARTIFACT_DIR = os.path.join(BUILDER_ROOT, "artifacts")
LOCK_FILE = os.path.join(BUILDER_ROOT, "build.lock")


class Job:
    def __init__(self, path):
        self.path = path
        self.dir = os.path.dirname(os.path.abspath(path))
        with open(path, "r", encoding="utf-8") as f:
            self.data = json.load(f)
        self.id = self.data.get("id", "unknown")
        self.log_path = os.path.join(self.dir, "build.log")
        self.status_path = os.path.join(self.dir, "status.json")
        self._log = None

    def log(self, msg):
        line = "[%s] %s" % (time.strftime("%H:%M:%S"), msg)
        print(line, flush=True)
        if self._log:
            self._log.write(line + "\n")
            self._log.flush()

    def status(self, phase, **extra):
        st = {"id": self.id, "phase": phase, "updated_at": time.strftime("%Y-%m-%d %H:%M:%S")}
        st.update(extra)
        tmp = self.status_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(st, f, ensure_ascii=False, indent=2)
        os.replace(tmp, self.status_path)


def run(cmd, job, cwd=None, env=None):
    """跑一条命令, 输出实时写进日志。返回 (returncode, output)。"""
    job.log("$ %s" % (" ".join(cmd) if isinstance(cmd, list) else cmd))
    merged = dict(os.environ)
    if env:
        merged.update(env)
    try:
        proc = subprocess.Popen(
            cmd, cwd=cwd, env=merged, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, bufsize=1, universal_newlines=True)
    except OSError as exc:
        job.log("!! 无法启动: %s" % exc)
        return 127, str(exc)
    collected = []
    try:
        for line in proc.stdout:
            collected.append(line)
            if job._log:
                job._log.write(line)
                job._log.flush()
        proc.wait()
    except Exception as exc:  # noqa: BLE001
        proc.kill()
        job.log("!! 异常: %s" % exc)
        return 1, str(exc)
    return proc.returncode, "".join(collected)


def tool(name, job):
    """在 PATH 里找可执行文件。

    Windows 上 flutter 是 flutter.bat, 而 CreateProcess **不认 PATHEXT** ——
    直接 subprocess 调 "flutter" 会报 [WinError 2]。shutil.which 认 PATHEXT。
    """
    path = shutil.which(name)
    if not path:
        job.log("!! 找不到 %s —— 不在 PATH 里" % name)
    return path or name


def apply_branding(job, src):
    """把品牌资源和服务器配置写进源码树。"""
    assets = job.data.get("assets") or {}
    for name, b64 in assets.items():
        try:
            payload = base64.b64decode(b64)
        except Exception as exc:  # noqa: BLE001
            job.log("!! 资源 %s 解码失败: %s" % (name, exc))
            continue
        if name.endswith(".ico"):
            targets = [os.path.join(src, "flutter", "windows", "runner", "resources", "app_icon.ico"),
                       os.path.join(src, "res", "icon.ico")]
        else:
            targets = [os.path.join(src, "flutter", "assets", name)]
        for t in targets:
            os.makedirs(os.path.dirname(t), exist_ok=True)
            with open(t, "wb") as f:
                f.write(payload)
            job.log("资源 %s -> %s (%d B)" % (name, os.path.relpath(t, src), len(payload)))

    srv = job.data.get("server") or {}
    host = (srv.get("host") or "").strip()
    relay = (srv.get("relay") or "").strip()
    api = (srv.get("api") or "").strip()
    key = (srv.get("key") or "").strip()
    if host or key:
        payload = {"host": host, "relay": relay, "api": api, "key": key}
        raw = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
        b64 = base64.urlsafe_b64encode(raw.encode("utf-8")).decode("ascii")[::-1]

        # 服务器配置预置必须落在 Rust 侧: 真正去连 hbbs 的是 Rust 核心,
        # Dart 改运行期配置来不及。src/server_preset.txt 是编译期常量。
        # 文件两行: 第 1 行应用名, 第 2 行服务器配置串。
        app_name = (job.data.get("app_name") or "RustDesk").replace("\\", "\\\\").replace("'", "\\'")
        preset_txt = os.path.join(src, "src", "server_preset.txt")
        with open(preset_txt, "w", encoding="utf-8", newline="\n") as f:
            f.write(app_name + "\n" + b64 + "\n")
        job.log("服务器配置已预置 -> %s" % os.path.relpath(preset_txt, src))
        job.log("  应用名   = %s" % app_name)
        job.log("  ID Server = %s" % host)
        job.log("  Relay     = %s" % relay)
        job.log("  API       = %s" % api)
        job.log("  Key       = %s" % key)

        # Dart 侧只管界面上的应用名
        target = os.path.join(src, "flutter", "lib", "common", "server_preset.dart")
        content = (
            "// Generated by the RustDesk build service -- do not edit by hand, a\n"
            "// rebuild overwrites it. An empty value means no preset, so the client\n"
            "// behaves exactly like upstream.\n"
            "//\n"
            "// The server address preset lives on the Rust side (src/server_preset.rs),\n"
            "// because the Rust core is what actually connects to hbbs, not Dart.\n"
            "const String kPresetAppName = '%s';\n" % app_name
        )
        with open(target, "w", encoding="utf-8", newline="\n") as f:
            f.write(content)
        job.log("应用名已预置 -> %s (%s)" % (os.path.relpath(target, src), app_name))


def acquire_lock(job):
    """源码树是共享的, 同一时刻只允许一个构建。"""
    lock = open(LOCK_FILE, "w")
    try:
        import fcntl
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return lock
    except ImportError:
        return lock          # Windows: 没有 fcntl, 靠 agent 侧单任务保证
    except OSError as exc:
        if exc.errno not in (errno.EACCES, errno.EAGAIN):
            raise
        job.log("已有构建任务在跑, 排队等待")
        job.status("queued", target=job.data.get("target"), log=job.log_path)
        import fcntl
        fcntl.flock(lock, fcntl.LOCK_EX)
        return lock


def build_linux(job, src, env):
    feats = "flutter,linux-pkg-config"
    if (job.data.get("options") or {}).get("hwcodec"):
        feats += ",hwcodec"
    job.status("building-rust", target=job.data.get("target"), log=job.log_path)
    rc, out = run([tool("cargo", job), "build", "--locked", "--features", feats,
                   "--lib", "--release"], job, cwd=src, env=env)
    if rc != 0:
        job.status("failed", error="cargo build 失败", log=job.log_path)
        return None

    job.status("building-flutter", target=job.data.get("target"), log=job.log_path)
    rc, out = run([tool("flutter", job), "build", "linux", "--release"], job,
                  cwd=os.path.join(src, "flutter"), env=env)
    if rc != 0:
        job.status("failed", error="flutter build 失败", log=job.log_path)
        return None
    arch = "arm64" if "arm64" in (job.data.get("target") or "") else "x64"
    return os.path.join(src, "flutter", "build", "linux", arch, "release", "bundle")


def build_windows(job, src, env):
    feats = "flutter"
    if (job.data.get("options") or {}).get("hwcodec"):
        feats += ",hwcodec"
    job.status("building-rust", target=job.data.get("target"), log=job.log_path)
    rc, out = run([tool("cargo", job), "build", "--locked", "--features", feats,
                   "--lib", "--release"], job, cwd=src, env=env)
    if rc != 0:
        job.status("failed", error="cargo build 失败", log=job.log_path)
        return None

    job.status("building-flutter", target=job.data.get("target"), log=job.log_path)
    rc, out = run([tool("flutter", job), "build", "windows", "--release"], job,
                  cwd=os.path.join(src, "flutter"), env=env)
    if rc != 0:
        job.status("failed", error="flutter build 失败", log=job.log_path)
        return None
    return os.path.join(src, "flutter", "build", "windows", "x64", "runner", "Release")


def main():
    if len(sys.argv) < 2:
        print("用法: build_job.py <job.json>")
        return 2
    job = Job(sys.argv[1])
    os.makedirs(ARTIFACT_DIR, exist_ok=True)
    os.makedirs(job.dir, exist_ok=True)
    job._log = open(job.log_path, "a", encoding="utf-8")
    job.log("===== 任务 %s 开始 (目标 %s) =====" % (job.id, job.data.get("target")))

    lock = acquire_lock(job)
    src = SRC_DIR
    if not os.path.isdir(src):
        job.log("!! 源码树不存在: %s" % src)
        job.status("failed", error="源码树不存在", log=job.log_path)
        return 1

    out_dir = os.path.join(ARTIFACT_DIR, job.id)
    os.makedirs(out_dir, exist_ok=True)
    target = job.data.get("target", "linux-x64")
    env = dict(os.environ)
    env["PATH"] = env.get("PATH", "")

    try:
        job.status("baking", target=target, log=job.log_path)
        apply_branding(job, src)

        if target.startswith("linux"):
            bundle = build_linux(job, src, env)
        elif target.startswith("windows"):
            bundle = build_windows(job, src, env)
        else:
            job.log("!! 未知目标: %s" % target)
            job.status("failed", error="未知目标 " + target, log=job.log_path)
            return 1
        if not bundle or not os.path.isdir(bundle):
            job.status("failed", error="找不到构建输出目录", log=job.log_path)
            return 1

        job.status("packaging", target=target, log=job.log_path)
        if target.startswith("windows"):
            artifact = os.path.join(out_dir, "rustdesk-client-%s.zip" % target)
            rc, out = run([sys.executable, "-c",
                           "import shutil,sys;shutil.make_archive(sys.argv[1],'zip',sys.argv[2])",
                           artifact[:-4], bundle], job)
        else:
            artifact = os.path.join(out_dir, "rustdesk-client-%s.tar.gz" % target)
            rc, out = run(["tar", "-czf", artifact, "-C", bundle, "."], job)
        if rc != 0:
            job.status("failed", error="打包失败", log=job.log_path)
            return 1
        size = os.path.getsize(artifact)
        job.log("产物: %s (%.1f MB)" % (artifact, size / 1024.0 / 1024.0))

        with open(os.path.join(out_dir, "client-config.json"), "w", encoding="utf-8") as f:
            json.dump(job.data.get("server") or {}, f, ensure_ascii=False, indent=2)

        job.status("done", target=target, log=job.log_path,
                   artifact=os.path.basename(artifact), size=size)
        job.log("===== 任务 %s 完成 =====" % job.id)
        return 0
    except Exception as exc:  # noqa: BLE001
        job.log("!! 未捕获异常: %s" % exc)
        job.status("failed", error=str(exc), log=job.log_path)
        return 1
    finally:
        try:
            lock.close()
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(main())
