#!/bin/bash
echo "=== 试各个 libyuv / aom 下载源 ==="
try() {
  local label="$1" url="$2"
  code=$(timeout 45 curl -sS -o /dev/null -w "%{http_code}" -L "$url" 2>/dev/null || echo ERR)
  printf "  %-46s %s\n" "$label" "$code"
}
try "chromium.googlesource libyuv(+archive sha)" \
  "https://chromium.googlesource.com/libyuv/libyuv/+archive/d98915a654d3564e4802a0004add46221c4e4348.tar.gz"
try "chromium.googlesource libyuv(+archive main)" \
  "https://chromium.googlesource.com/libyuv/libyuv/+archive/refs/heads/main.tar.gz"
try "aomedia.googlesource aom(+archive v3.13.3)" \
  "https://aomedia.googlesource.com/aom/+archive/v3.13.3.tar.gz"
try "codeload leMenkov/libyuv main" \
  "https://codeload.github.com/leMenkov/libyuv/tar.gz/refs/heads/main"
try "codeload ultralytics/libyuv master" \
  "https://codeload.github.com/ultralytics/libyuv/tar.gz/refs/heads/master"
try "codeload AOM v3.13.3" \
  "https://codeload.github.com/Alliance-for-Open-Media/AOM/archive/refs/tags/v3.13.3.tar.gz"
try "codeload AOM v3.9.1" \
  "https://codeload.github.com/Alliance-for-Open-Media/AOM/archive/refs/tags/v3.9.1.tar.gz"
try "codeload libyuv (wg) master" \
  "https://codeload.github.com/webkitgtk/libyuv/tar.gz/refs/heads/main"
try "storage.googleapis chromium-bazel libyuv" \
  "https://storage.googleapis.com/chromium-bazel/chromium/libyuv/archive/d98915a654d3564e4802a0004add46221c4e4348.tar.gz"

echo "=== dnf 里搜 yuv / aom / av1 ==="
timeout 60 dnf --noplugins --setopt=timeout=8 --setopt=retries=0 list --available 2>/dev/null | grep -iE "yuv|aom|av1" | head -10
