#!/bin/sh
set -eu

PROJECT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
cd "$PROJECT_DIR"

PATH="$HOME/.cargo/bin:$PATH"
export PATH

if ! command -v rustup >/dev/null 2>&1; then
    if ! command -v curl >/dev/null 2>&1; then
        printf '%s\n' '错误：未找到 curl，请先安装 curl 后重试。' >&2
        exit 1
    fi

    printf '%s\n' '正在安装 Rustup...'
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
    PATH="$HOME/.cargo/bin:$PATH"
    export PATH
    if ! command -v rustup >/dev/null 2>&1; then
        printf '%s\n' '错误：Rustup 安装未完成，请检查网络后重试。' >&2
        exit 1
    fi
fi

if ! command -v cargo >/dev/null 2>&1; then
    printf '%s\n' '错误：安装后仍找不到 cargo，请检查 Rustup 安装或 PATH。' >&2
    exit 1
fi

printf '%s\n' '正在准备项目工具链并构建 release...'
cargo build --release
TEMP_BIN=$(mktemp "$PROJECT_DIR/.ncm-audio.XXXXXX")
trap 'rm -f "$TEMP_BIN"' EXIT HUP INT TERM
cp "$PROJECT_DIR/target/release/ncm-audio" "$TEMP_BIN"
chmod 755 "$TEMP_BIN"
mv -f "$TEMP_BIN" "$PROJECT_DIR/ncm-audio"
trap - EXIT HUP INT TERM
printf '构建完成：%s/ncm-audio\n' "$PROJECT_DIR"
