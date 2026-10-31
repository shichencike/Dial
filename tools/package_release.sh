#!/usr/bin/env bash
#
# 打包「单平台」发布压缩包（与 tools/package.sh 的全量源码包不同，这里是精简的
# 二进制分发包，每个平台一个，供 install-dial(.sh/.ps1) 下载安装）。
#
# 包名固定为：dialc-v<版本>-<slug>.zip
# 包内根目录固定为 Dial/，含：
#   dialc            （或 dialc.exe，编译器本体）
#   stdlib/ runtime/ （编译器按「可执行文件位置」向上查找，必须随包分发）
#   LICENSE*         （三重许可说明 + 三份协议原文，硬性要求）
#   install-dial.sh  （Linux / Termux 一键安装；Windows 包额外含 install-dial.ps1）
#   BUILD_INFO.txt   （本平台运行环境说明）
#
# 用法：bash tools/package_release.sh <slug> <编译好的二进制路径>
#   slug: linux-x86_64 | windows-x86_64 | termux-aarch64
set -euo pipefail

if [ $# -lt 2 ]; then
  echo "用法: bash tools/package_release.sh <slug> <binary-path>" >&2
  exit 2
fi
SLUG="$1"
BIN="$2"

cd "$(dirname "$0")/.." || exit 1            # 进入仓库根
if [ ! -x "$BIN" ]; then
  echo "❌ 二进制不存在或不可执行: $BIN" >&2
  exit 1
fi

# ---- 版本号：三段式 -> 短式 ----
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')
MAJOR=$(echo "$VERSION" | cut -d. -f1)
MINOR=$(echo "$VERSION" | cut -d. -f2)
PATCH=$(echo "$VERSION" | cut -d. -f3)
if [ "$PATCH" = "0" ]; then SHORT="${MAJOR}.${MINOR}"; else SHORT="${VERSION}"; fi

PKG_ROOT="Dial"
OUT="$(pwd)/dialc-v${SHORT}-${SLUG}.zip"
echo "== 打包 dialc v${VERSION} [${SLUG}] -> ${OUT} =="

# ---- 硬性校验：四份许可文件必须存在 ----
echo "-- 校验开源许可文件 --"
MISSING=0
for f in LICENSE LICENSE-GPLv3 LICENSE-MIT LICENSE-APACHE; do
  if [ -f "$f" ]; then
    printf '   ✅ %-18s %s 字节\n' "$f" "$(wc -c <"$f" | tr -d ' ')"
  else
    printf '   ❌ %-18s 缺失！\n' "$f"; MISSING=1
  fi
done
[ "$MISSING" -eq 0 ] || { echo "许可文件不全，拒绝打包" >&2; exit 1; }

# ---- 暂存目录 ----
STAGE="$(mktemp -d)"; trap 'rm -rf "$STAGE"' EXIT
DEST="${STAGE}/${PKG_ROOT}"; mkdir -p "$DEST"

# 编译器二进制
BASENAME=$(basename "$BIN")
cp "$BIN" "$DEST/$BASENAME"

# 标准库 / 运行时 / 许可 / 安装脚本
cp -r stdlib runtime "$DEST/"
for f in LICENSE LICENSE-GPLv3 LICENSE-MIT LICENSE-APACHE; do cp "$f" "$DEST/"; done
cp tools/install-dial.sh "$DEST/install-dial.sh"
if [ "$SLUG" = "windows-x86_64" ]; then
  cp tools/install-dial.ps1 "$DEST/install-dial.ps1"
  # LLVM-C.dll：dialc.exe 运行期依赖（与官方 MSVC LLVM 18 的 C 接口配套）
  LLVM_DLL="${LLVM18_DIR:-C:/llvm18}/bin/LLVM-C.dll"
  if [ -f "$LLVM_DLL" ]; then
    cp "$LLVM_DLL" "$DEST/LLVM-C.dll"
    echo "   ✅ 已随包附带 LLVM-C.dll"
  else
    echo "   ⚠️  未找到 LLVM-C.dll（LLVM18_DIR=$LLVM18_DIR）；dialc.exe 运行期将缺该 DLL" >&2
  fi
fi

# 平台运行环境说明
{
  echo "dialc v${VERSION}（由 GitHub Actions 构建，平台 slug=${SLUG}）"
  echo
  case "$SLUG" in
    linux-x86_64)
      echo "运行环境：Linux x86_64，动态链接 glibc，运行期需要系统装有 libLLVM-18"
      echo "（apt 源 apt.llvm.org 的 llvm-18，或自行设置 LD_LIBRARY_PATH 指向 libLLVM-18.so）。";;
    windows-x86_64)
      echo "运行环境：Windows x64（MinGW 目标）。dialc.exe 运行期需要同目录的 LLVM-C.dll"
      echo "（已随包分发）。用 dialc 编译你的 .dial 程序时，需要 MinGW-w64 工具链"
      echo "（gcc + winpthreads），产物为单文件 .exe，不依赖 LLVM。";;
    termux-aarch64)
      echo "运行环境：Termux / 任意 ARM64 Linux，完全静态链接（musl），无外部依赖。"
      echo "用 dialc 编译 .dial 程序时仅需本机装有 gcc（或 clang），产物同样无外部依赖。";;
  esac
  echo
  echo "版本:       ${VERSION}"
  echo "构建提交:   ${GITHUB_SHA:-（本地构建）}"
  echo "构建时间:   $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
} > "$DEST/BUILD_INFO.txt"

# ---- 打包 ----
rm -f "$OUT"
( cd "$STAGE" && zip -r -q "$OUT" "$PKG_ROOT" )

# ---- 自检 ----
echo "-- 自检 --"
FAIL=0
# 先把清单读进变量再 grep，避免 pipefail 下 grep -q 提前关闭管道导致 unzip 收 SIGPIPE 的假阴性
LISTING=$(unzip -l "$OUT")
for f in LICENSE LICENSE-GPLv3 LICENSE-MIT LICENSE-APACHE "$BASENAME" stdlib runtime install-dial.sh; do
  if echo "$LISTING" | grep -q "${PKG_ROOT}/$f"; then
    echo "   ✅ 包内含 $f"
  else
    echo "   ❌ 包内缺少 $f"; FAIL=1
  fi
done
if [ "$SLUG" = "windows-x86_64" ]; then
  echo "$LISTING" | grep -q "${PKG_ROOT}/install-dial.ps1" && echo "   ✅ 包内含 install-dial.ps1" || { echo "   ❌ 缺少 install-dial.ps1"; FAIL=1; }
fi

echo
if [ "$FAIL" -eq 0 ]; then
  echo "== 打包完成 ✅ : ${OUT} ($(du -h "$OUT" | cut -f1)) =="
else
  echo "== 打包失败 ❌ =="; exit 1
fi
