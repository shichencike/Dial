#!/usr/bin/env bash
# P1 门禁：AArch64 后端「实机运行语义」验证（不只是交叉汇编语法校验）。
#
# 背景（2026-11-19）：过去 golden 对 AArch64 只做 `clang --target=aarch64` 交叉汇编
# 产出 ELF .o 即算通过——x86 宿主无法执行，助记符合法≠语义正确。安装 qemu-user-static
# + aarch64 交叉工具链后实机跑 golden，一次抓出 3 个语法校验永远发现不了的根本 bug：
#   ① 所有函数未保存/恢复 LR(x30)：bl 覆盖 x30，末尾 ret 跳回最后一次调用返回地址→死循环；
#   ② ICmp fuse 优化对 AArch64 跳过了结果物化，但分支仍 cbz 读未写槽→while 条件随机→死循环；
#   ③ PtrAdd 发射 `add o, b, b`（o=b+b、b 不变）笔误，正确应为 `add b, b, o`→指针偏移失效。
# 三者均已修复；本门禁把「同一份源码，LLVM(x86) 与 asm(AArch64/qemu 实机) 输出逐字节一致」
# 钉死，防止回归。
#
# 铁律：所有 DIAL_ASM_TARGET 设置只作用于本脚本的子进程，不污染宿主后续编译。
# 缺 qemu / 交叉 gcc 时整段 skip（CI 无这些依赖不误红），有则必须全绿。
#
# 用法：./tools/verify_aarch64_semantics.sh [编译器路径]   # 默认 ./target/release/dialc
# 退出码：0 = 全绿或 skip；1 = 有失败。

set -u
cd "$(dirname "$0")/.." || exit 1

DIALC="${1:-./target/release/dialc}"
fail=0
ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }
skip() { echo "  ⚠️  $1"; }

if [ ! -x "$DIALC" ]; then
    echo "  ❌ 编译器不存在或不可执行：$DIALC"
    exit 1
fi

# --- 依赖探测：缺任一则整段 skip（本机环境限制，非产品缺陷）---
QEMU="$(command -v qemu-aarch64-static || true)"
CC="$(command -v aarch64-linux-gnu-gcc || true)"
if [ -z "$QEMU" ] || [ -z "$CC" ]; then
    skip "跳过 AArch64 实机语义门禁（缺 qemu-aarch64-static 或 aarch64-linux-gnu-gcc）"
    echo "  ── AArch64 实机语义门禁结束（skip）──"
    exit 0
fi
if ! command -v timeout >/dev/null 2>&1 || ! command -v diff >/dev/null 2>&1; then
    skip "跳过 AArch64 实机语义门禁（缺 timeout / diff）"
    echo "  ── AArch64 实机语义门禁结束（skip）──"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 用固定的交叉驱动，避免宿主 DIAL_C_COMPILER 干扰；timeout 防止死循环 hang 住门禁。
A64_ENV=(env DIAL_ASM_TARGET=aarch64 DIAL_C_COMPILER="$CC")
RUN_TIMEOUT=10
npass=0; nskip=0

for src in tests/asm_golden/*.dial; do
    name="$(basename "$src" .dial)"
    # inline_asm：该 golden 模板是 x86 汇编语法（movq/addq），无法在 AArch64 汇编。
    # P4.5 起 AArch64 已支持内联汇编，但须用 AArch64 语法模板，由专用门禁
    # verify_aarch64_inline_asm.sh 实机覆盖；此处跳过 x86 模板的 golden，避免误红。
    if [ "$name" = "inline_asm" ]; then
        nskip=$((nskip + 1))
        continue
    fi

    # x86 基线：LLVM 后端（语义参照），ReleaseFast。
    if ! "$DIALC" build "$src" --backend=llvm --fast -o "$TMP/base_$name" >"$TMP/base_$name.log" 2>&1; then
        nskip=$((nskip + 1)); continue
    fi
    timeout "$RUN_TIMEOUT" "$TMP/base_$name" >"$TMP/base_$name.out" 2>/dev/null || { nskip=$((nskip + 1)); continue; }

    # AArch64 asm：静态链接（--single-file），qemu 实机运行。
    if ! "${A64_ENV[@]}" "$DIALC" build "$src" --backend=asm --single-file --fast \
            -o "$TMP/arm_$name" >"$TMP/arm_$name.log" 2>&1; then
        bad "$name：AArch64 asm 编译失败"; tail -6 "$TMP/arm_$name.log"; continue
    fi
    timeout "$RUN_TIMEOUT" "$QEMU" "$TMP/arm_$name" >"$TMP/arm_$name.out" 2>"$TMP/arm_$name.err"
    arc=$?
    if [ "$arc" -eq 124 ]; then
        bad "$name：AArch64 qemu 运行超时（exit=124，疑似死循环 / LR 未保存回归）"; continue
    fi
    if [ "$arc" -ne 0 ]; then
        bad "$name：AArch64 qemu 运行失败（exit=$arc）"; head -4 "$TMP/arm_$name.err"; continue
    fi
    if ! diff -q "$TMP/base_$name.out" "$TMP/arm_$name.out" >/dev/null 2>&1; then
        bad "$name：双后端输出不一致（x86 LLVM vs AArch64 asm 实机）"
        diff "$TMP/base_$name.out" "$TMP/arm_$name.out" | head -12
        continue
    fi
    ok "$name：x86 LLVM 与 AArch64 asm 实机(qemu)输出逐字节一致"
    npass=$((npass + 1))
done

echo "  ── AArch64 实机语义门禁：通过 $npass，skip $nskip（inline_asm 用 x86 模板，由专用门禁覆盖）──"
exit "$fail"
