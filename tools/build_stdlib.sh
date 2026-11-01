#!/usr/bin/env bash
# ===========================================================================
# build_stdlib.sh —— 把 runtime/ + stdlib/ 编译为单份静态库 libdial_std.a
#
# 用法:
#   tools/build_stdlib.sh <输出 .a 路径> [安全等级]
#   例: tools/build_stdlib.sh dist-stdlib/x86_64-linux-gnu/libdial_std.a 1
#
# 默认安全等级 = 1（ReleaseSafe）：保留边界/空指针检查作为保底防线，
# 关闭 Debug 级冗余断言。官方标准库以此级别预编译发布。
#
# 这是三平台 CI 共用的"编译标准库"一步；各作业用各自的 C 工具链跑它，
# 产出的 .a 由 make_stdlib_pkg.py 收齐打包成 stdlib.dialpkg。
# ===========================================================================
set -euo pipefail

OUT_LIB="${1:?用法: build_stdlib.sh <输出 .a 路径> [安全等级]}"
SAFETY="${2:-1}"

# 定位仓库根目录（脚本在 tools/ 下）
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

RUNTIME_DIR="$REPO_ROOT/runtime"
STDLIB_DIR="$REPO_ROOT/stdlib"

if [ ! -d "$STDLIB_DIR" ] || [ ! -d "$RUNTIME_DIR" ]; then
    echo "❌ 找不到 stdlib/ 或 runtime/ 目录（应在仓库根下）" >&2
    exit 1
fi

# ---- 选择 C 编译器 ----
# 顺序：显式 $CC → zig cc（本地开发首选，跨平台一致）→ gcc → clang → cc。
# 注意：clang 在部分平台会动态加载 LLVM（Termux 容器内删掉 libffi.so 后失效；
# 而 Windows 的 MSVC clang 缺 unistd.h），故 gcc 优先于 clang，让 stdlib 用各平台
# 自包含的 GNU C 编译器编译。这与 link() 回退编译路径（gcc 优先）保持一致。
CC=""
for cand in "${CC:-}" "zig cc" gcc clang cc; do
    [ -z "$cand" ] && continue
    if command -v "${cand%% *}" >/dev/null 2>&1; then
        CC="$cand"
        break
    fi
done
if [ -z "$CC" ]; then
    echo "❌ 找不到可用的 C 编译器（zig cc / clang / gcc / cc）" >&2
    exit 1
fi

# ar：Windows(MinGW) 与 Linux/Alpine 均提供；优先 $AR 否则取第一个可用的 ar。
AR="${AR:-}"
for ac in ar llvm-ar gcc-ar; do
    if command -v "$ac" >/dev/null 2>&1; then
        AR="$ac"
        break
    fi
done
if [ -z "$AR" ]; then
    echo "❌ 找不到 ar（需 binutils/llvm-ar）" >&2
    exit 1
fi

BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$BUILD_DIR"' EXIT

echo "ℹ️  C 编译器: $CC"
echo "ℹ️  归档器:   $AR"
echo "ℹ️  安全等级: -DDIAL_SAFETY=$SAFETY"

# ---- 编译所有 C 源文件 -> .o ----
OBJ_FILES=()
for dir in "$RUNTIME_DIR" "$STDLIB_DIR"; do
    for cfile in "$dir"/*.c; do
        [ -e "$cfile" ] || continue
        base="$(basename "$cfile" .c)"
        out_o="$BUILD_DIR/$base.o"
        echo "  编译 $cfile"
        # shellcheck disable=SC2086
        $CC -O2 -pthread "-DDIAL_SAFETY=$SAFETY" -I "$RUNTIME_DIR" -I "$STDLIB_DIR" -c "$cfile" -o "$out_o"
        OBJ_FILES+=("$out_o")
    done
done

if [ "${#OBJ_FILES[@]}" -eq 0 ]; then
    echo "❌ 没有编译出任何 .o（stdlib/runtime 为空？）" >&2
    exit 1
fi

# ---- 归档为静态库 ----
mkdir -p "$(dirname "$OUT_LIB")"
rm -f "$OUT_LIB"
$AR rcs "$OUT_LIB" "${OBJ_FILES[@]}"

echo "✅ 已生成 $OUT_LIB ($(stat -c%s "$OUT_LIB" 2>/dev/null || wc -c < "$OUT_LIB") 字节)"
