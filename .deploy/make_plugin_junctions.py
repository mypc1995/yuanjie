import os
import re
import subprocess
import sys

FL = r"D:\Software\RustDesk\flutter"
PLUGINS = os.path.join(FL, ".flutter-plugins")
LINKDIR = os.path.join(FL, "windows", "flutter", "ephemeral", ".plugin_symlinks")

# 解析 .flutter-plugins (key=value 格式, 路径里的 \ 是转义过的)
pairs = []
with open(PLUGINS, "r", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, _, path = line.partition("=")
        path = path.replace("\\\\", "\\")
        pairs.append((name, path))

os.makedirs(LINKDIR, exist_ok=True)
made, skipped = [], []
for name, path in pairs:
    src = os.path.join(path, "windows")
    if not os.path.isdir(src):
        continue
    dst = os.path.join(LINKDIR, name)
    if os.path.exists(dst):
        skipped.append(name)
        continue
    try:
        # 目录联接 (junction) 不需要管理员权限
        subprocess.run(["powershell", "-NoProfile", "-Command",
                        "New-Item -ItemType Junction -Path '%s' -Target '%s' | Out-Null" % (dst, src)],
                       check=True)
        made.append(name)
    except subprocess.CalledProcessError as exc:
        print("  !! %s 建联接失败: %s" % (name, exc))

print("已建联接 %d 个:" % len(made))
for n in made:
    print("   ", n)
if skipped:
    print("已存在跳过 %d 个: %s" % (len(skipped), ", ".join(skipped)))
print("联接目录: %s" % LINKDIR)
