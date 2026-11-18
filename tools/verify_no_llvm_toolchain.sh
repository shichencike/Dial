#!/usr/bin/env bash
# 唯一后端哲学 · 第 1 项：证明 asm 后端零 LLVM 工具依赖（gcc-only 独立性回归）。
#
# 核心命题（见 rationale/roadmap-gap-analysis.md 总纲执行附则第 1 项）：
#   asm 后端在「完全无 LLVM 工具（无 clang / zig / lld / llvm-*）」的 GNU 工具链下，
#   必须能独立走完 lower → pass → .s → cc -c → 链接 → 运行，且产物语义正确。
#
# 本脚本只验证 asm 后端本体，**不以 LLVM 后端作基准**（LLVM 后端在 gcc 下个别用例
# 存在 PIE relocation 兼容问题，与 asm 独立性无关）。正确性基准取自「默认工具链下
# 的 asm 产物」——同一个后端、两种工具链，输出必须逐字节一致，从而把工具链作为
# 唯一变量隔离出来。
#
# 三层证据：
#   L1 环境自检：gcc / cc / ar / as / ld 均存在且可执行。
#   L2 官方开关：DIAL_C_COMPILER=gcc 下全部 golden 用例的 asm 产物，与默认工具链
#       下同一 asm 后端的产物输出逐字节一致；且确实生成了 .s（真走汇编发射路径）。
#   L3 兜底选择：构造屏蔽 clang/zig 的隔离 PATH（只留 GNU cc/as/ld + 脚本工具），
#       不设 DIAL_C_COMPILER 时 cc_command() 必须自动落到 GNU cc，asm 仍编译运行成功。
#
# 用法：./tools/verify_no_llvm_toolchain.sh [编译器路径]   # 默认 ./target/release/dialc
# 退出码：0 = 三层证据全绿；1 = 有失败。

set -u

DIALC="${1:-./target/release/dialc}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CASES="$ROOT/tests/asm_golden"
cd "$ROOT" || exit 1

BASH_BIN="/usr/bin/bash"          # 绝对路径：构造隔离 PATH 后仍需可执行
[ -x "$BASH_BIN" ] || BASH_BIN="$(command -v bash)"

fail=0
ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }
skip() { echo "  ⚠️  $1"; }

if [ ! -x "$DIALC" ]; then
    echo "  ❌ 编译器不存在或不可执行: $DIALC"
    exit 1
fi

# ---------- L1 环境自检 ----------
echo "== L1) GNU 工具链自检 =="
for t in gcc cc ar as ld; do
    if command -v "$t" >/dev/null 2>&1; then
        ok "$t 可用 ($(command -v "$t"))"
    else
        bad "$t 不可用，无法进行 gcc-only 验证"
    fi
done
[ "$fail" -eq 0 ] || { echo "工具链不完整，终止"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# ---------- L2 官方开关：DIAL_C_COMPILER=gcc 下 asm 语义一致 ----------
echo
echo "== L2) DIAL_C_COMPILER=gcc：全部 golden 用例 asm 产物双工具链逐字节一致 =="
l2_count=0
for src in "$CASES"/*.dial; do
    [ -f "$src" ] || continue
    name="$(basename "$src" .dial)"
    work="$TMP/l2_$name"
    mkdir -p "$work"
    cp "$src" "$work/$name.dial"
    src2="$work/$name.dial"

    # 基准：默认工具链下的 asm 产物
    if ! "$DIALC" build "$src2" --backend=asm -o "$work/asm_default" >"$work/def.log" 2>&1; then
        bad "$name：默认工具链 asm 编译失败"; tail -6 "$work/def.log" | sed 's/^/       /'; continue
    fi
    "$work/asm_default" >"$work/def.out" 2>&1 || { bad "$name：默认工具链 asm 运行退出码非 0"; continue; }

    # 待测：强制 GNU gcc 工具链下的 asm 产物
    if ! DIAL_C_COMPILER=gcc "$DIALC" build "$src2" --backend=asm -o "$work/asm_gcc" >"$work/gcc.log" 2>&1; then
        bad "$name：gcc-only asm 编译失败"; tail -6 "$work/gcc.log" | sed 's/^/       /'; continue
    fi
    # .s 是「真走汇编发射路径」的硬证据
    if ! [ -s "$work/$name.s" ]; then
        bad "$name：未生成 .s 汇编产物"; continue
    fi
    "$work/asm_gcc" >"$work/gcc.out" 2>&1 || { bad "$name：gcc-only asm 运行退出码非 0"; sed 's/^/       /' "$work/gcc.out" | head -6; continue; }

    if diff -u "$work/def.out" "$work/gcc.out" >"$work/diff" 2>&1; then
        ok "$name：asm 产物默认工具链 ≡ gcc-only（含 .s 产物）"
        l2_count=$((l2_count + 1))
    else
        bad "$name：asm 产物双工具链输出不一致"; sed 's/^/       /' "$work/diff" | head -20
    fi
done

# ---------- L3 兜底选择：屏蔽 clang/zig 后 cc_command() 自动落 GNU cc ----------
echo
echo "== L3) 隔离 PATH 屏蔽 clang/zig：cc_command() 自动兜底选 GNU cc =="
ISO="$TMP/isolated_bin"
mkdir -p "$ISO"
# 只放 GNU 编译器 + 汇编器/链接器 + 脚本所需工具；绝不软链 clang/zig/llvm-*/lld
for t in gcc ar as ld mktemp rm mkdir cp diff tail head sed grep cat basename dirname sort wc tr cut env uname; do
    case "$t" in
        diff) p="/usr/bin/diff" ;;
        *)    p="$(command -v "$t" 2>/dev/null)" ;;
    esac
    [ -n "$p" ] && [ -x "$p" ] && ln -sf "$p" "$ISO/$t"
done
ln -sf "$(command -v cc)" "$ISO/cc"

# 自检：隔离 PATH 下 clang/zig 必须不可见
iso_invisible=1
for banned in clang zig ld.lld llvm-ar; do
    if env PATH="$ISO" "$BASH_BIN" -c "command -v $banned >/dev/null 2>&1"; then
        bad "隔离 PATH 仍能看到 $banned（隔离失败）"; iso_invisible=0
    fi
done
[ "$iso_invisible" -eq 1 ] && ok "clang/zig/lld/llvm-ar 在隔离 PATH 中均不可见"

# 用现成 golden 源码验证：不设 DIAL_C_COMPILER，dialc 必须选到 GNU cc 并编出可运行 asm
# （直接复用 core1.dial，避免自造 Dial 语法；core1 输出含 "asm backend hello"）
iso_src="$TMP/iso.dial"
cp "$CASES/core1.dial" "$iso_src"
iso_log="$TMP/iso.log"
unset DIAL_C_COMPILER
if env PATH="$ISO" "$BASH_BIN" -c \
     "cd '$ROOT' && '$DIALC' build '$iso_src' --backend=asm -o '$TMP/iso_bin'" \
     >"$iso_log" 2>&1; then
    if grep -qiE 'C 编译器: (cc|gcc)' "$iso_log"; then
        ok "cc_command() 自动选中 GNU cc/gcc（无 LLVM 工具可选）"
    else
        bad "编译成功但未识别到 GNU cc/gcc 选择"; grep -i 'C 编译器' "$iso_log" | sed 's/^/       /'
    fi
    if out="$("$TMP/iso_bin" 2>&1)" && printf '%s' "$out" | grep -qF 'asm backend hello'; then
        ok "隔离工具链下 asm 产物运行正确"
    else
        bad "隔离工具链下 asm 产物运行输出异常"; printf '%s\n' "$out" | head -6 | sed 's/^/       /'
    fi
else
    bad "隔离 PATH（无 LLVM 工具）下 asm 编译失败"; tail -10 "$iso_log" | sed 's/^/       /'
fi

# ---------- 汇总 ----------
echo
echo "  L2 双工具链一致用例: $l2_count"
if [ "$fail" -eq 0 ]; then
    echo "== 零 LLVM 工具链独立性验证通过 ✅ =="
    exit 0
else
    echo "== 零 LLVM 工具链独立性验证失败 ❌ =="
    exit 1
fi
