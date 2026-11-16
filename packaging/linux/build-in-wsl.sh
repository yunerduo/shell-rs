#!/usr/bin/env bash
# 在 Windows 的 WSL（Ubuntu 22.04）里构建 Linux x86_64 的 AppImage。
#
#   packaging/linux/build-in-wsl.sh [输出目录]
#
# 产物默认写到仓库的 dist/，文件名与 release workflow 一致。
# 注意：Ubuntu 22.04 的 glibc 是 2.35，AppImage 因此要求 glibc 2.35
# 及以上（Ubuntu 22.04+、Debian 12+）。要与 release workflow 一样兼容
# Ubuntu 20.04，把构建放进 ubuntu:20.04 容器里做。
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
out=${1:-$root/dist}
build=/root/shellrs-linux-build
log() { printf '==> %s\n' "$*"; }

export DEBIAN_FRONTEND=noninteractive
missing=()
for package in build-essential cmake clang pkg-config libfontconfig-dev \
    libfreetype-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
    libx11-xcb-dev libxcb1-dev libvulkan-dev libdbus-1-dev libssl-dev curl file; do
    dpkg -s "$package" >/dev/null 2>&1 || missing+=("$package")
done
if [ ${#missing[@]} -gt 0 ]; then
    log "安装构建依赖：${missing[*]}"
    sudo apt-get update -qq
    sudo apt-get install -y -qq --no-install-recommends "${missing[@]}"
fi

# 在 ext4 上构建：直接编译 /mnt/e 慢很多。
log "复制源码到 $build"
mkdir -p "$build"
tar -C "$root" -czf /tmp/shellrs-src.tgz \
    --exclude=./target --exclude=./.git --exclude=./.screenshots \
    --exclude=./server/target --exclude=./server/data .
tar -C "$build" -xzf /tmp/shellrs-src.tgz

# Windows 上的检出会把脚本和 .desktop 换成 CRLF，Linux 里执行不了。
find "$build" -type f \( -name '*.sh' -o -name '*.desktop' \) -print0 \
    | xargs -0 sed -i 's/\r$//'

version=$(grep -m1 '^version = ' "$root/Cargo.toml" | cut -d'"' -f2)
log "构建 ShellRS $version"
cd "$build"
SHELLRS_BUILD_DATE=$(date -u +%Y-%m-%d) cargo build --release --locked --bin shellrs

tool=/root/appimagetool
if [ ! -x "$tool" ]; then
    log "下载 appimagetool"
    curl -fsSL -o "$tool" \
        https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$tool"
fi

log "打包 AppImage 到 $out"
mkdir -p "$out"
APPIMAGETOOL="$tool" APPIMAGE_EXTRACT_AND_RUN=1 \
    packaging/linux/appimage.sh "$build/target/release/shellrs" "$version" "$out"
ls -l "$out"/*.AppImage
