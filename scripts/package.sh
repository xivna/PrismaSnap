#!/usr/bin/env bash
# PrismaSnap 便携版打包脚本（WSL2 交叉编译，环境见 AGENTS.md 5.1）
#
# 产出（默认输出到 /mnt/d/Download，可用参数 1 覆盖）：
#   1. PrismaSnap_<版本>_portable.zip   主包：PrismaSnap.exe + 使用说明.txt
#   2. PrismaSnap_<版本>_OCR插件包.zip  可选插件：plugins/ocr/ 四件套 + 安装说明.txt
#
# 用法：
#   scripts/package.sh [输出目录]
#
# 说明：
#   - 打包前自动复核 det/rec 模型的 SHA256（期望值取自 src/ocr/download.rs）。
#   - ZIP 使用 deflate 压缩；条目路径相对包根（插件包内为 plugins/ocr/...）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
[ -n "$VERSION" ] || { echo "错误：无法从 Cargo.toml 读取版本号" >&2; exit 1; }

OUT_DIR="${1:-/mnt/d/Download}"
[ -d "$OUT_DIR" ] || { echo "错误：输出目录不存在：$OUT_DIR" >&2; exit 1; }

EXE="target/x86_64-pc-windows-msvc/release/prismsnap.exe"
PLUGIN_DIR="plugins/ocr"

echo "==> [1/5] 构建 release（目标 x86_64-pc-windows-msvc）"
cargo build --target x86_64-pc-windows-msvc --release
[ -f "$EXE" ] || { echo "错误：未找到 $EXE" >&2; exit 1; }

echo "==> [2/5] 校验 OCR 插件模型哈希"
str_of() { grep -A1 "$1" src/ocr/download.rs | grep -o '[0-9a-f]\{64\}' | head -n1; }
check_hash() {
    local file="$1" expect="$2" actual
    actual="$(sha256sum "$file" | cut -d' ' -f1)"
    if [ "$actual" != "$expect" ]; then
        echo "错误：$file SHA256 不匹配" >&2
        echo "  期望 $expect" >&2
        echo "  实际 $actual" >&2
        exit 1
    fi
    echo "    OK  $file"
}
check_hash "$PLUGIN_DIR/det.onnx" "$(str_of DET_SHA256)"
check_hash "$PLUGIN_DIR/rec.onnx" "$(str_of REC_SHA256)"
for f in keys.txt onnxruntime.dll; do
    [ -s "$PLUGIN_DIR/$f" ] || { echo "错误：缺少插件文件 $PLUGIN_DIR/$f" >&2; exit 1; }
    echo "    OK  $PLUGIN_DIR/$f（仅校验非空）"
done

make_zip() {
    # make_zip <输出.zip> <基准目录> <相对路径...>
    python3 - "$@" <<'PY'
import os, sys, zipfile

out, base = sys.argv[1], sys.argv[2]
paths = sys.argv[3:]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for p in paths:
        full = os.path.join(base, p)
        if os.path.isdir(full):
            for root, _, files in os.walk(full):
                for name in sorted(files):
                    fp = os.path.join(root, name)
                    z.write(fp, os.path.relpath(fp, base))
        else:
            z.write(full, p)
PY
}

STAGE="$(mktemp -d /tmp/prismsnap_pkg.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT

echo "==> [3/5] 主包"
mkdir -p "$STAGE/main"
cp "$EXE" "$STAGE/main/PrismaSnap.exe"
cp "docs/使用说明.md" "$STAGE/main/使用说明.txt"
MAIN_ZIP="$OUT_DIR/PrismaSnap_${VERSION}_portable.zip"
rm -f "$MAIN_ZIP"
make_zip "$MAIN_ZIP" "$STAGE/main" PrismaSnap.exe 使用说明.txt
echo "    $MAIN_ZIP ($(du -h "$MAIN_ZIP" | cut -f1))"

echo "==> [4/5] OCR 插件包"
mkdir -p "$STAGE/plugin/plugins/ocr"
cp "$PLUGIN_DIR/det.onnx" "$PLUGIN_DIR/rec.onnx" "$PLUGIN_DIR/keys.txt" \
   "$PLUGIN_DIR/onnxruntime.dll" "$STAGE/plugin/plugins/ocr/"
cat > "$STAGE/plugin/安装说明.txt" <<'TXT'
PrismaSnap OCR 插件包（RapidOCR PP-OCRv6）
=========================================

安装步骤：
  1. 退出 PrismaSnap；
  2. 将本压缩包内的 plugins 文件夹解压到 PrismaSnap 程序目录
     （与 PrismaSnap.exe 同级，提示覆盖时选“合并/覆盖”）；
  3. 确认目录结构为：
       <程序目录>\plugins\ocr\det.onnx
       <程序目录>\plugins\ocr\rec.onnx
       <程序目录>\plugins\ocr\keys.txt
       <程序目录>\plugins\ocr\onnxruntime.dll
  4. 重新启动 PrismaSnap，在 设置 → AI 接口 → OCR 引擎 处
     确认状态为“插件已加载”。

说明：
  - 不安装本插件不影响基础截图/标注/翻译（识别自动使用系统 OCR）；
  - 替换过 onnxruntime.dll 后必须重启程序（其余文件即放即用）；
  - 对话框底部的“官方下载/手动下载帮助”提供在线获取渠道与哈希对照。
TXT
PLUGIN_ZIP="$OUT_DIR/PrismaSnap_${VERSION}_OCR插件包.zip"
rm -f "$PLUGIN_ZIP"
make_zip "$PLUGIN_ZIP" "$STAGE/plugin" plugins 安装说明.txt
echo "    $PLUGIN_ZIP ($(du -h "$PLUGIN_ZIP" | cut -f1))"

echo "==> [5/5] 完成（版本 $VERSION）"
sha256sum "$MAIN_ZIP" "$PLUGIN_ZIP"
