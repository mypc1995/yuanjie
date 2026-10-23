#!/bin/bash
echo "=== dnf 源可达性 ==="
timeout 60 curl -sS -o /dev/null -w "  update.cs2c.com.cn: %{http_code}\n" \
  "http://update.cs2c.com.cn:8080/NS/V10/V10SP2/os/adv/lic/base/x86_64/repodata/repomd.xml" 2>&1

echo "=== 需要��包是否存在 ==="
PKGS="clang clang-devel ninja-build gtk3-devel libX11-devel libXext-devel libXrandr-devel libxinerama-devel libXi-devel mesa-libGL-devel alsa-lib-devel libstdc++-devel zlib-devel liblzma-devel libvpx-devel libyuv-devel libaom-devel opus-devel libjpeg-turbo-devel"
for p in $PKGS; do
  hit=$(timeout 40 dnf --noplugins --setopt=timeout=8 --setopt=retries=0 list --available "$p" 2>/dev/null | grep -c "^$p")
  if [ "$hit" -gt 0 ]; then
    printf "  %-24s OK\n" "$p"
  else
    printf "  %-24s MISSING\n" "$p"
  fi
done

echo "=== 已有 rpm 里的候选 (已装) ==="
rpm -qa 2>/dev/null | grep -iE "^(clang|ninja|gtk3|libvpx|libyuv|libaom|opus)" | head -10
