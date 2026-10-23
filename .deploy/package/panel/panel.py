#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""RustDesk 服务端管理面板 (hbbs / hbbr) —— 离线内网部署版。

功能:
  * 账号密码登录 (session 认证)
  * 服务启停 / 重启 / 开机自启
  * 编辑 .env (端口、绑定、relay 地址、强制中继、带宽 QoS ...)
  * 管理密钥 (查看公钥、轮换密钥对)
  * 维护 blacklist.txt / blocklist.txt
  * 通过 loopback console 实时读取与调整
  * 查看 hbbs/hbbr 日志
  * **品牌定制**: 上传 logo / 图标 / 应用名, 面板自身与客户端都用它
  * **客户端集成**: 生成客户端可导入的配置串, 打包下发 branding 资源

客户端拉取接口 (无需登录, 只读):
  GET /api/branding              -> branding.json
  GET /api/branding/<asset>      -> logo.png / logo_light.png / logo_dark.png / icon.png / app_icon.ico
  GET /api/client-config         -> base64 配置串 (RustDesk ServerConfig 格式)

依赖: Flask 及其纯 Python 依赖 (已随包 vendor 在 ./vendor, 无需联网安装)
"""

import base64
import io
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import time
import zipfile
from functools import wraps

# vendor 目录: 离线包里带着 Flask 及其依赖, 直接加进 sys.path, 不碰系统 site-packages
_VENDOR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vendor")
if os.path.isdir(_VENDOR):
    import sys
    sys.path.insert(0, _VENDOR)

from flask import (Flask, abort, flash, jsonify, redirect, render_template,
                   request, session, url_for)

# --------------------------------------------------------------------------
# 配置
# --------------------------------------------------------------------------

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
SERVER_DIR = os.environ.get("RUSTDESK_SERVER_DIR", "/var/lib/rustdesk-server")
LOG_DIR = os.environ.get("RUSTDESK_LOG_DIR", "/var/log/rustdesk-server")
PANEL_DIR = os.environ.get("RUSTDESK_PANEL_DIR", "/var/lib/rustdesk-panel")
BRAND_DIR = os.path.join(PANEL_DIR, "branding")
CONF_PATH = os.path.join(SERVER_DIR, ".env")
PANEL_CONF = os.environ.get("RUSTDESK_PANEL_CONF", "/etc/rustdesk-panel/panel.conf")

HBBS_UNIT = "rustdesk-hbbs"
HBBR_UNIT = "rustdesk-hbbr"
DEFAULT_PORT = 21116

BLACKLIST = os.path.join(SERVER_DIR, "blacklist.txt")
BLOCKLIST = os.path.join(SERVER_DIR, "blocklist.txt")
PUBKEY_FILE = os.path.join(SERVER_DIR, "id_ed25519.pub")
PRIVKEY_FILE = os.path.join(SERVER_DIR, "id_ed25519")

BRANDING_JSON = os.path.join(BRAND_DIR, "branding.json")
CLIENT_CONFIG_JSON = os.path.join(BRAND_DIR, "client-config.json")

# 构建服务
BUILD_ROOT = os.environ.get("RUSTDESK_BUILDER_ROOT", "/var/lib/rustdesk-builder")
JOBS_DIR = os.path.join(BUILD_ROOT, "jobs")
ARTIFACT_DIR = os.path.join(BUILD_ROOT, "artifacts")
BUILD_SCRIPT = os.environ.get("RUSTDESK_BUILD_SCRIPT", "/opt/rustdesk-builder/build_job.py")

# 可选构建目标。Windows 目标不能在 Linux 上编 (Flutter 不支持),
# 走 Windows agent 路径: 面板派发 -> Windows 构建机编译 -> 产物回传。
BUILD_TARGETS = [
    ("linux-x64", "Linux x64", "在麒麟服务器上直接构建（适合麒麟/统信等国产系统客户端）"),
    ("linux-arm64", "Linux arm64", "飞腾 / 鲲鹏 / 龙芯等 ARM 服务器", "cross"),
    ("windows-x64", "Windows x64", "需要一台 Windows 构建机（agent 模式）", "agent"),
]

# branding 资源: 允许上传的类型与落盘名
BRAND_ASSETS = {
    "logo": ("logo.png", "png"),
    "logo_light": ("logo_light.png", "png"),
    "logo_dark": ("logo_dark.png", "png"),
    "icon": ("icon.png", "png"),
    "app_icon": ("app_icon.ico", "ico"),
}

SHARED_FIELDS = [
    ("PORT", "主端口", "hbbs 监听此端口；hbbr 监听 PORT+1。默认 21116 -> 21116/21117", "21116"),
    ("BIND", "绑定地址", "留空表示所有网卡。1.1.17+ 生效", ""),
]

HBBS_FIELDS = [
    ("RELAY-SERVERS", "中继服务器", "逗号分隔的 host 或 host:port。客户端会拿到这个地址", "203.0.113.10"),
    ("ALWAYS_USE_RELAY", "强制走中继", "Y / N。为 Y 时所有会话都走中继，不做打洞", "N"),
    ("RMEM", "UDP 接收缓冲", "字节。0 表示用系统默认。调大前先 sysctl net.core.rmem_max", "0"),
    ("DB_URL", "数据库路径", "SQLite 文件路径，相对路径相对于工作目录", "./db_v2.sqlite3"),
    ("MAX_PUNCH_REQUESTS", "打洞请求记录数", "内存中保留的记录条数，0 表示不记录", "100000"),
    ("TEST_HBBS", "UDP 自测目标", "host:port。留空自动检测，NAT 后建议显式指定", ""),
]

HBBR_FIELDS = [
    ("SINGLE_BANDWIDTH", "单连接带宽", "Mb/s", "128"),
    ("TOTAL_BANDWIDTH", "总带宽", "Mb/s", "1024"),
    ("LIMIT_SPEED", "限速", "Mb/s，命中黑名单或降级后的速度", "32"),
    ("DOWNGRADE_THRESHOLD", "降级阈值", "0-1 的比例，超过后触发降速", "0.66"),
    ("DOWNGRADE_START_CHECK", "降级检查延迟", "秒", "1800"),
]

ALL_FIELDS = SHARED_FIELDS + HBBS_FIELDS + HBBR_FIELDS

app = Flask(__name__)
app.secret_key = os.environ.get("RUSTDESK_PANEL_SECRET") or secrets.token_hex(32)
app.config["SEND_FILE_MAX_AGE_DEFAULT"] = 0
app.config["ASSET_VERSION"] = os.environ.get("RUSTDESK_PANEL_ASSET_V", "3")
app.config["MAX_CONTENT_LENGTH"] = 32 * 1024 * 1024


# --------------------------------------------------------------------------
# 基础工具
# --------------------------------------------------------------------------

def _read(path, default=""):
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return default


def _write(path, content, mode=0o640):
    directory = os.path.dirname(path)
    if directory and not os.path.isdir(directory):
        os.makedirs(directory)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(content)
    os.chmod(tmp, mode)
    os.replace(tmp, path)


def parse_env(text):
    data = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith(";"):
            continue
        if "=" not in line:
            continue
        key, _, value = line.partition("=")
        data[key.strip()] = value.strip()
    return data


def write_env(values, original):
    seen = set()
    out = []
    for line in original.splitlines():
        stripped = line.strip()
        if stripped and not stripped.startswith("#") and not stripped.startswith(";") and "=" in stripped:
            key = stripped.partition("=")[0].strip()
            if key in values:
                out.append("%s=%s" % (key, values[key]))
                seen.add(key)
                continue
        out.append(line)
    for key, value in values.items():
        if key not in seen:
            out.append("%s=%s" % (key, value))
    return "\n".join(out).rstrip() + "\n"


def systemctl(*args):
    try:
        p = subprocess.run(["systemctl"] + list(args), stdout=subprocess.PIPE,
                           stderr=subprocess.STDOUT, timeout=20)
        return p.returncode, p.stdout.decode("utf-8", "replace").strip()
    except Exception as exc:  # noqa: BLE001
        return 1, str(exc)


def unit_state(unit):
    code, out = systemctl("is-active", unit)
    active = (out or "").strip() == "active"
    _, enabled = systemctl("is-enabled", unit)
    return {"unit": unit, "active": active, "state": (out or "unknown").strip(),
            "enabled": (enabled or "unknown").strip()}


def console(port, command, timeout=2.0):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=5)
    except OSError as exc:
        return "连接失败: %s" % exc
    try:
        s.settimeout(timeout)
        s.sendall((command + "\n").encode("utf-8"))
        buf = b""
        while True:
            try:
                chunk = s.recv(4096)
            except socket.timeout:
                break
            if not chunk:
                break
            buf += chunk
        return buf.decode("utf-8", "replace").strip()
    finally:
        s.close()


def tail(path, lines=200):
    if not os.path.exists(path):
        return ["(文件不存在: %s)" % path]
    try:
        with open(path, "rb") as f:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            data = b""
            while size > 0 and data.count(b"\n") <= lines:
                step = min(65536, size)
                size -= step
                f.seek(size)
                data = f.read(step) + data
        return data.decode("utf-8", "replace").splitlines()[-lines:]
    except OSError as exc:
        return ["读取失败: %s" % exc]


def server_port():
    try:
        return int(parse_env(_read(CONF_PATH)).get("PORT", DEFAULT_PORT))
    except ValueError:
        return DEFAULT_PORT


def hbbs_console_port():
    return server_port() - 1


def hbbr_console_port():
    return server_port() + 1


def public_key():
    return _read(PUBKEY_FILE).strip()


def client_config_string(host, relay, api, key):
    """与 flutter/lib/common.dart 的 ServerConfig.encode() 完全一致:
    base64UrlEncode(json) 之后把整个字符串反转。"""
    payload = {"host": (host or "").strip(), "relay": (relay or "").strip(),
               "api": (api or "").strip(), "key": (key or "").strip()}
    raw = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
    return base64.urlsafe_b64encode(raw.encode("utf-8")).decode("ascii")[::-1]


def decode_client_config(text):
    text = (text or "").strip()
    try:
        return json.loads(text)
    except ValueError:
        pass
    try:
        raw = base64.urlsafe_b64decode(text[::-1] + "=" * (-len(text) % 4))
        return json.loads(raw.decode("utf-8", "replace"))
    except Exception:  # noqa: BLE001
        return None


# --------------------------------------------------------------------------
# 品牌 (branding)
# --------------------------------------------------------------------------

def load_json_file(path, default=None):
    """读 JSON 文件。注意不能用 parse_env —— 那是给 .env (INI) 用的,
    而 JSON 里没有 '='，整行会被跳过，读出来永远是空。"""
    raw = _read(path).strip()
    if not raw:
        return dict(default or {})
    try:
        data = json.loads(raw)
    except ValueError:
        return dict(default or {})
    return data if isinstance(data, dict) else dict(default or {})


def default_branding():
    return {"app_name": "RustDesk", "primary_color": "#2563eb",
            "logo": "", "logo_light": "", "logo_dark": "", "icon": "", "app_icon": "",
            "updated_at": ""}


def load_branding():
    data = default_branding()
    data.update(load_json_file(BRANDING_JSON, {}))
    return data


def save_branding(data):
    if not os.path.isdir(BRAND_DIR):
        os.makedirs(BRAND_DIR, exist_ok=True)
    data["updated_at"] = time.strftime("%Y-%m-%d %H:%M:%S")
    _write(BRANDING_JSON, json.dumps(data, ensure_ascii=False, indent=2))


def branding_asset_path(name):
    """按名字定位品牌资源。名字可以带不带扩展名:
    /api/branding/logo 和 /api/branding/logo.png 都指向同一个文件。"""
    if name in BRAND_ASSETS:
        key = name
    else:
        key = os.path.splitext(name)[0]
    if key not in BRAND_ASSETS:
        return None
    path = os.path.join(BRAND_DIR, BRAND_ASSETS[key][0])
    return path if os.path.exists(path) else None


def load_client_config():
    data = load_json_file(CLIENT_CONFIG_JSON, {})
    if not data:
        data = {"host": "", "relay": "", "api": "", "key": public_key()}
    if not data.get("key"):
        data["key"] = public_key()
    return data


def save_client_config(data):
    if not os.path.isdir(BRAND_DIR):
        os.makedirs(BRAND_DIR, exist_ok=True)
    _write(CLIENT_CONFIG_JSON, json.dumps(data, ensure_ascii=False, indent=2))


# --------------------------------------------------------------------------
# 认证
# --------------------------------------------------------------------------

def panel_credentials():
    conf = parse_env(_read(PANEL_CONF))
    return conf.get("user", "admin"), conf.get("password", "")


def check_auth(username, password):
    want_user, want_pass = panel_credentials()
    if not want_pass:
        return False
    return (secrets.compare_digest(username or "", want_user)
            and secrets.compare_digest(password or "", want_pass))


def requires_login(func):
    @wraps(func)
    def wrapper(*args, **kwargs):
        if not session.get("logged_in"):
            return redirect(url_for("login", next=request.path))
        return func(*args, **kwargs)
    return wrapper


@app.before_request
def csrf_guard():
    if request.method in ("POST", "PUT", "DELETE"):
        # agent 接口用 X-Agent-Token 鉴权, 不走网页 session, 不需要 CSRF
        if request.path.startswith("/api/agent/"):
            return
        token = session.get("csrf")
        sent = request.form.get("csrf") or request.headers.get("X-CSRF-Token")
        if not token or sent != token:
            abort(400, "CSRF 校验失败，请刷新页面重试")


@app.context_processor
def inject_globals():
    if "csrf" not in session:
        session["csrf"] = secrets.token_urlsafe(24)
    branding = load_branding()
    return {
        "csrf_token": session.get("csrf"),
        "asset_version": app.config.get("ASSET_VERSION", "1"),
        "hbbs": unit_state(HBBS_UNIT),
        "hbbr": unit_state(HBBR_UNIT),
        "server_dir": SERVER_DIR,
        "branding": branding,
        "logged_in": session.get("logged_in", False),
    }


# --------------------------------------------------------------------------
# 路由: 登录
# --------------------------------------------------------------------------

@app.route("/login", methods=["GET", "POST"])
def login():
    if session.get("logged_in"):
        return redirect(url_for("index"))
    error = None
    if request.method == "POST":
        user = request.form.get("username", "")
        password = request.form.get("password", "")
        if check_auth(user, password):
            session["logged_in"] = True
            session.permanent = False
            nxt = request.form.get("next") or url_for("index")
            if not nxt.startswith("/"):
                nxt = url_for("index")
            return redirect(nxt)
        error = "账号或密码不正确"
    return render_template("login.html", error=error,
                           next=request.args.get("next", ""))


@app.route("/logout", methods=["POST"])
def logout():
    session.clear()
    return redirect(url_for("login"))


# --------------------------------------------------------------------------
# 路由: 面板
# --------------------------------------------------------------------------

@app.route("/")
@requires_login
def index():
    data = parse_env(_read(CONF_PATH))
    port = server_port()
    live = {
        "always_use_relay": console(hbbs_console_port(), "aur"),
        "relay_servers": console(hbbs_console_port(), "rs"),
        "total_bandwidth": console(hbbr_console_port(), "tb"),
        "single_bandwidth": console(hbbr_console_port(), "sb"),
        "limit_speed": console(hbbr_console_port(), "ls"),
    }
    listeners = []
    code, out = systemctl("status", "rustdesk-hbbs", "--no-pager", "-l")
    for line in out.splitlines():
        if re.search(r"^[\s\d]*[Ll][Ii][Ss][Tt][Ee][Nn]", line):
            listeners.append(line.strip())
    return render_template("index.html", conf=data, live=live, port=port,
                           pubkey=public_key(), listeners=listeners)


@app.route("/config", methods=["GET", "POST"])
@requires_login
def config():
    original = _read(CONF_PATH)
    if request.method == "POST":
        values = {}
        for key, _l, _h, _d in ALL_FIELDS:
            values[key] = request.form.get(key, "").strip()
        for key, value in parse_env(original).items():
            values.setdefault(key, value)
        _write(CONF_PATH, write_env(values, original))
        code, out = systemctl("restart", HBBS_UNIT)
        systemctl("restart", HBBR_UNIT)
        flash("配置已保存，hbbs/hbbr 已重启。" if code == 0
              else "配置已保存，但重启失败: %s" % out, "ok" if code == 0 else "err")
        return redirect(url_for("config"))
    return render_template("config.html", conf=parse_env(original), fields=ALL_FIELDS,
                           shared=SHARED_FIELDS, hbbs=HBBS_FIELDS, hbbr=HBBR_FIELDS,
                           raw=original)


@app.route("/keys", methods=["GET", "POST"])
@requires_login
def keys():
    if request.method == "POST":
        if request.form.get("action") == "rotate":
            for path in (PUBKEY_FILE, PRIVKEY_FILE):
                if os.path.exists(path):
                    os.remove(path)
            systemctl("restart", HBBS_UNIT)
            systemctl("restart", HBBR_UNIT)
            time.sleep(1.5)
            flash("密钥已轮换，服务已重启。新公钥: %s" % public_key(), "ok")
        return redirect(url_for("keys"))
    return render_template("keys.html", pubkey=public_key(),
                           priv_exists=os.path.exists(PRIVKEY_FILE))


@app.route("/lists", methods=["GET", "POST"])
@requires_login
def lists():
    if request.method == "POST":
        which = request.form.get("which", "")
        content = request.form.get("content", "")
        target = BLACKLIST if which == "blacklist" else BLOCKLIST
        lines = [ln.strip() for ln in content.splitlines()]
        lines = [ln for ln in lines if ln and not ln.startswith("#")]
        _write(target, "\n".join(lines) + ("\n" if lines else ""))
        flash("%s 已保存。" % ("限速黑名单" if which == "blacklist" else "拒绝黑名单"), "ok")
        return redirect(url_for("lists"))
    return render_template("lists.html", blacklist=_read(BLACKLIST),
                           blocklist=_read(BLOCKLIST),
                           console_blacklist=console(hbbr_console_port(), "b"),
                           console_blocklist=console(hbbr_console_port(), "B"))


@app.route("/logs")
@requires_login
def logs():
    which = request.args.get("which", "hbbs")
    name = "hbbs" if which == "hbbs" else "hbbr"
    return render_template("logs.html", which=which, name=name,
                           log=tail(os.path.join(LOG_DIR, "%s.log" % name), 300),
                           err=tail(os.path.join(LOG_DIR, "%s.error" % name), 60))


@app.route("/action/<what>", methods=["POST"])
@requires_login
def action(what):
    if what in ("start", "stop", "restart"):
        failed = False
        for unit in (HBBS_UNIT, HBBR_UNIT):
            code, out = systemctl(what, unit)
            if code != 0:
                flash("%s %s 失败: %s" % (unit, what, out), "err")
                failed = True
        if not failed:
            flash("已执行 %s。" % what, "ok")
    elif what == "enable":
        for unit in (HBBS_UNIT, HBBR_UNIT):
            systemctl("enable", unit)
        flash("已设置开机自启。", "ok")
    elif what == "disable":
        for unit in (HBBS_UNIT, HBBR_UNIT):
            systemctl("disable", unit)
        flash("已取消开机自启。", "ok")
    else:
        abort(404)
    return redirect(request.referrer or url_for("index"))


# --------------------------------------------------------------------------
# 路由: 品牌定制
# --------------------------------------------------------------------------

@app.route("/branding", methods=["GET", "POST"])
@requires_login
def branding():
    if request.method == "POST":
        # 落盘图片前先确保目录存在, 否则新装的机器上这里会 FileNotFoundError
        if not os.path.isdir(BRAND_DIR):
            os.makedirs(BRAND_DIR, exist_ok=True)
        data = load_branding()
        data["app_name"] = request.form.get("app_name", "").strip() or "RustDesk"
        color = request.form.get("primary_color", "").strip() or "#2563eb"
        if not re.match(r"^#[0-9a-fA-F]{6}$", color):
            color = "#2563eb"
        data["primary_color"] = color

        # 每个上传流只读一次。FileStorage.read() 会耗尽流,
        # 第二次 read() 拿到的是空 bytes —— 之前 apply_all 那段就是这么把
        # 三个 logo 覆盖成 0 字节的。
        payloads = {}
        for name in BRAND_ASSETS:
            upload = request.files.get("asset_%s" % name)
            if upload and upload.filename:
                payloads[name] = upload.read()

        apply_all = request.form.get("apply_all") == "1"
        logo_payload = payloads.get("logo")
        for name, (filename, _ext) in BRAND_ASSETS.items():
            payload = payloads.get(name)
            if not payload and apply_all and logo_payload \
                    and name in ("logo_light", "logo_dark"):
                payload = logo_payload
            if payload:
                with open(os.path.join(BRAND_DIR, filename), "wb") as f:
                    f.write(payload)
                data[name] = filename

        save_branding(data)
        flash("品牌已保存。客户端下次启动会拉取新资源（客户端需启用「从服务器拉取品牌」）。", "ok")
        return redirect(url_for("branding"))
    data = load_branding()
    return render_template("branding.html", branding=data,
                           client_cfg=load_client_config())


@app.route("/branding/preview/<name>")
@requires_login
def branding_preview(name):
    path = branding_asset_path(name)
    if not path:
        abort(404)
    with open(path, "rb") as f:
        payload = f.read()
    mime = "image/x-icon" if path.endswith(".ico") else "image/png"
    return app.response_class(payload, mimetype=mime)


# --------------------------------------------------------------------------
# 路由: 客户端集成
# --------------------------------------------------------------------------

@app.route("/client", methods=["GET", "POST"])
@requires_login
def client():
    if request.method == "POST":
        data = {
            "host": request.form.get("host", "").strip(),
            "relay": request.form.get("relay", "").strip(),
            "api": request.form.get("api", "").strip(),
            "key": request.form.get("key", "").strip() or public_key(),
        }
        save_client_config(data)
        flash("客户端服务器配置已保存。", "ok")
        return redirect(url_for("client"))
    cfg = load_client_config()
    encoded = client_config_string(cfg.get("host", ""), cfg.get("relay", ""),
                                   cfg.get("api", ""), cfg.get("key", ""))
    return render_template("client.html", cfg=cfg, encoded=encoded,
                           decoded=decode_client_config(encoded),
                           branding=load_branding())


@app.route("/client/decode", methods=["POST"])
@requires_login
def client_decode():
    decoded = decode_client_config(request.form.get("text", ""))
    flash("解析成功: %s" % json.dumps(decoded, ensure_ascii=False) if decoded
          else "无法解析这段配置串。", "ok" if decoded else "err")
    return redirect(url_for("client"))


@app.route("/client/bundle")
@requires_login
def client_bundle():
    """打包下发: 品牌资源 + 客户端服务器配置, 给 Windows 打包脚本用。"""
    cfg = load_client_config()
    brand = load_branding()
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
        zf.writestr("branding.json", json.dumps(brand, ensure_ascii=False, indent=2))
        zf.writestr("client-config.json", json.dumps(cfg, ensure_ascii=False, indent=2))
        zf.writestr("server-config.b64", client_config_string(
            cfg.get("host", ""), cfg.get("relay", ""), cfg.get("api", ""), cfg.get("key", "")))
        zf.writestr("README.txt", CLIENT_BUNDLE_README % {
            "host": cfg.get("host", ""), "relay": cfg.get("relay", ""),
            "api": cfg.get("api", ""), "key": cfg.get("key", ""),
            "app": brand.get("app_name", "RustDesk")})
        for name, (filename, _ext) in BRAND_ASSETS.items():
            path = branding_asset_path(name)
            if path:
                zf.write(path, filename)
    return app.response_class(
        buf.getvalue(), mimetype="application/zip",
        headers={"Content-Disposition": "attachment; filename=rustdesk-client-bundle.zip"})


CLIENT_BUNDLE_README = """RustDesk 客户端品牌包
========================

生成时间: 由服务端管理面板导出

应用名     : %(app)s
ID Server  : %(host)s
Relay      : %(relay)s
API        : %(api)s
Key        : %(key)s

内容:
  branding.json        品牌元数据 (应用名、主题色)
  client-config.json   服务器地址与 Key (结构化)
  server-config.b64    客户端可直接导入的配置串 (base64url + 反转)
  logo.png             界面主 logo
  logo_light.png       浅色主题 logo
  logo_dark.png        深色主题 logo
  icon.png             应用图标
  app_icon.ico         Windows exe 图标

用法 (Windows 打包脚本 package.ps1):
  1. 把本包解压到 branding/ 目录
  2. 运行 package.ps1 -BrandingDir branding
  脚本会把 logo/icon 替换进 flutter/assets 和 res/，并把 server-config.b64
  预置进客户端配置。

也可以让客户端在运行时从服务器拉:
  客户端启动时访问
    http://<服务器>:21120/api/branding
    http://<服务器>:21120/api/client-config
"""


# --------------------------------------------------------------------------
# 路由: 客户端构建
# --------------------------------------------------------------------------

def _agent_token():
    return parse_env(_read(PANEL_CONF)).get("agent_token", "")


def _agent_authorized():
    token = _agent_token()
    if not token:
        return False
    sent = request.headers.get("X-Agent-Token", "")
    return secrets.compare_digest(sent or "", token)


def _job_dir(job_id):
    return os.path.join(JOBS_DIR, job_id)


def _read_status(job_id):
    raw = _read(os.path.join(_job_dir(job_id), "status.json")).strip()
    if not raw:
        return {"id": job_id, "phase": "pending"}
    try:
        return json.loads(raw)
    except ValueError:
        return {"id": job_id, "phase": "unknown"}


def _list_jobs():
    if not os.path.isdir(JOBS_DIR):
        return []
    out = []
    for name in os.listdir(JOBS_DIR):
        d = os.path.join(JOBS_DIR, name)
        if not os.path.isdir(d):
            continue
        st = _read_status(name)
        job = _read(os.path.join(d, "job.json"))
        try:
            meta = json.loads(job) if job else {}
        except ValueError:
            meta = {}
        st["app_name"] = meta.get("app_name", "")
        st["target"] = meta.get("target", st.get("target", ""))
        st["created"] = os.path.getmtime(os.path.join(d, "job.json")) if os.path.exists(os.path.join(d, "job.json")) else 0
        out.append(st)
    out.sort(key=lambda s: s.get("created", 0), reverse=True)
    return out


@app.route("/build", methods=["GET", "POST"])
@requires_login
def build():
    if request.method == "POST":
        target = request.form.get("target", "linux-x64")
        if not any(t[0] == target for t in BUILD_TARGETS):
            flash("未知目标: %s" % target, "err")
            return redirect(url_for("build"))
        job_id = "job-%s-%s" % (time.strftime("%Y%m%d-%H%M%S"), secrets.token_hex(3))
        job_dir = _job_dir(job_id)
        os.makedirs(job_dir, exist_ok=True)

        brand = load_branding()
        cfg = load_client_config()
        assets = {}
        for name in BRAND_ASSETS:
            path = branding_asset_path(name)
            if path:
                with open(path, "rb") as f:
                    assets[BRAND_ASSETS[name][0]] = base64.b64encode(f.read()).decode("ascii")

        job = {
            "id": job_id,
            "target": target,
            "app_name": brand.get("app_name", "RustDesk"),
            "primary_color": brand.get("primary_color", "#2563eb"),
            "server": {"host": cfg.get("host", ""), "relay": cfg.get("relay", ""),
                       "api": cfg.get("api", ""), "key": cfg.get("key", "")},
            "assets": assets,
            "options": {"hwcodec": request.form.get("hwcodec") == "1"},
            "created_at": time.strftime("%Y-%m-%d %H:%M:%S"),
        }
        with open(os.path.join(job_dir, "job.json"), "w", encoding="utf-8") as f:
            json.dump(job, f, ensure_ascii=False, indent=2)

        if target.startswith("windows"):
            # 派发给 Windows 构建机, agent 会来取
            with open(os.path.join(job_dir, "status.json"), "w", encoding="utf-8") as f:
                json.dump({"id": job_id, "phase": "pending-agent",
                           "target": target, "updated_at": time.strftime("%Y-%m-%d %H:%M:%S")},
                          f, ensure_ascii=False, indent=2)
            flash("任务已派发，等待 Windows 构建机接单。", "ok")
        else:
            with open(os.path.join(job_dir, "status.json"), "w", encoding="utf-8") as f:
                json.dump({"id": job_id, "phase": "starting", "target": target,
                           "updated_at": time.strftime("%Y-%m-%d %H:%M:%S")},
                          f, ensure_ascii=False, indent=2)
            # 后台跑, 不阻塞面板
            try:
                subprocess.Popen([sys.executable, BUILD_SCRIPT, os.path.join(job_dir, "job.json")],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True)
                flash("构建任务已提交 (Linux 目标在服务器上直接编译)。", "ok")
            except OSError as exc:
                flash("无法启动构建进程: %s" % exc, "err")
        return redirect(url_for("build", job=job_id))

    jobs = _list_jobs()
    current = request.args.get("job", "")
    detail = None
    log = []
    if current:
        detail = _read_status(current)
        d = _job_dir(current)
        log = tail(os.path.join(d, "build.log"), 300)
    brand = load_branding()
    cfg = load_client_config()
    return render_template("build.html", jobs=jobs, detail=detail, log=log,
                           targets=BUILD_TARGETS, branding=brand, cfg=cfg,
                           job_id=current)


@app.route("/build/<job_id>/artifact")
@requires_login
def build_artifact(job_id):
    st = _read_status(job_id)
    name = st.get("artifact")
    path = os.path.join(ARTIFACT_DIR, job_id, name) if name else ""
    if not path or not os.path.exists(path):
        abort(404)
    with open(path, "rb") as f:
        payload = f.read()
    return app.response_class(payload, mimetype="application/octet-stream",
                              headers={"Content-Disposition":
                                       "attachment; filename=%s" % os.path.basename(path)})


# --------------------------------------------------------------------------
# 路由: 对外 API (客户端拉取, 只读, 不需要登录)
# --------------------------------------------------------------------------

@app.route("/api/branding")
def api_branding():
    return jsonify(load_branding())


@app.route("/api/branding/<name>")
def api_branding_asset(name):
    path = branding_asset_path(name)
    if not path:
        abort(404)
    with open(path, "rb") as f:
        payload = f.read()
    mime = "image/x-icon" if path.endswith(".ico") else "image/png"
    return app.response_class(payload, mimetype=mime,
                              headers={"Cache-Control": "no-cache"})


@app.route("/api/client-config")
def api_client_config():
    cfg = load_client_config()
    encoded = client_config_string(cfg.get("host", ""), cfg.get("relay", ""),
                                   cfg.get("api", ""), cfg.get("key", ""))
    return jsonify({"config": encoded, "plain": cfg,
                    "branding_url": request.url_root.rstrip("/") + "/api/branding"})


@app.route("/api/status")
@requires_login
def api_status():
    return jsonify({"hbbs": unit_state(HBBS_UNIT), "hbbr": unit_state(HBBR_UNIT),
                    "port": server_port(), "public_key": public_key(),
                    "conf": parse_env(_read(CONF_PATH)),
                    "branding": load_branding()})


# --------------------------------------------------------------------------
# 路由: 构建 agent 接口 (用 X-Agent-Token 鉴权, 不走网页登录)
# --------------------------------------------------------------------------

@app.route("/api/agent/register", methods=["POST"])
def api_agent_register():
    if not _agent_authorized():
        return jsonify({"error": "agent token 无效"}), 401
    info = request.get_json(silent=True) or {}
    with open(os.path.join(BUILD_ROOT, "agents.json"), "w", encoding="utf-8") as f:
        json.dump(info, f, ensure_ascii=False, indent=2)
    return jsonify({"ok": True, "agent_id": info.get("agent_id")})


@app.route("/api/agent/jobs/next")
def api_agent_jobs_next():
    if not _agent_authorized():
        return jsonify({"error": "agent token 无效"}), 401
    if not os.path.isdir(JOBS_DIR):
        return jsonify({}), 204
    for name in sorted(os.listdir(JOBS_DIR)):
        d = os.path.join(JOBS_DIR, name)
        if not os.path.isdir(d):
            continue
        raw = _read(os.path.join(d, "status.json")).strip()
        if not raw:
            continue
        try:
            st = json.loads(raw)
        except ValueError:
            continue
        if st.get("phase") != "pending-agent":
            continue
        job_raw = _read(os.path.join(d, "job.json"))
        try:
            job = json.loads(job_raw)
        except ValueError:
            continue
        st["phase"] = "running-agent"
        st["updated_at"] = time.strftime("%Y-%m-%d %H:%M:%S")
        with open(os.path.join(d, "status.json"), "w", encoding="utf-8") as f:
            json.dump(st, f, ensure_ascii=False, indent=2)
        return jsonify(job)
    return jsonify({}), 204


@app.route("/api/agent/jobs/<job_id>/status", methods=["POST"])
def api_agent_job_status(job_id):
    if not _agent_authorized():
        return jsonify({"error": "agent token 无效"}), 401
    st = request.get_json(silent=True) or {}
    st["id"] = job_id
    st.setdefault("updated_at", time.strftime("%Y-%m-%d %H:%M:%S"))
    d = _job_dir(job_id)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, "status.json"), "w", encoding="utf-8") as f:
        json.dump(st, f, ensure_ascii=False, indent=2)
    return jsonify({"ok": True})


@app.route("/api/agent/jobs/<job_id>/artifact", methods=["PUT"])
def api_agent_job_artifact(job_id):
    if not _agent_authorized():
        return jsonify({"error": "agent token 无效"}), 401
    name = request.headers.get("X-Filename") or ("artifact-%s.bin" % job_id)
    name = os.path.basename(name)
    out_dir = os.path.join(ARTIFACT_DIR, job_id)
    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, name)
    with open(path, "wb") as f:
        f.write(request.get_data())
    size = os.path.getsize(path)
    st = _read_status(job_id)
    st["phase"] = "done"
    st["artifact"] = name
    st["size"] = size
    st["updated_at"] = time.strftime("%Y-%m-%d %H:%M:%S")
    with open(os.path.join(_job_dir(job_id), "status.json"), "w", encoding="utf-8") as f:
        json.dump(st, f, ensure_ascii=False, indent=2)
    return jsonify({"ok": True, "size": size})


@app.route("/api/agent/jobs/<job_id>/log", methods=["PUT"])
def api_agent_job_log(job_id):
    if not _agent_authorized():
        return jsonify({"error": "agent token 无效"}), 401
    job_dir = _job_dir(job_id)
    if not os.path.isdir(job_dir):
        abort(404)
    payload = request.get_data()
    with open(os.path.join(job_dir, "build.log"), "wb") as f:
        f.write(payload)
    return jsonify({"ok": True, "bytes": len(payload)})


@app.errorhandler(400)
@app.errorhandler(401)
@app.errorhandler(404)
@app.errorhandler(500)
def handle_error(err):
    return render_template("error.html", error=err), getattr(err, "code", 500)


if __name__ == "__main__":
    app.run(host="0.0.0.0", port=int(os.environ.get("RUSTDESK_PANEL_PORT", "21120")),
            threaded=True)
