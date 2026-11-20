#!/usr/bin/env bash
# P4.5 门禁：AArch64 后端内联汇编 @asm/@asm_intel「实机」验证。
#
# 背景（2026-11-20）：此前 codegen_asm.rs 对 AArch64 含 InlineAsm 的程序直接
# Fail-Fast（"仅支持 x86-64"），是 LLVM 有而 asm 无的 A 类能力缺口。P4.5 把
# inst_inline_asm / render_asm_template 改成 target-aware：
#   - 操作数池：x86 rdi/rsi/rdx/rcx/r8/r9 → AArch64 x0-x5；
#   - i8/i1 零扩展：x86 movzbq → AArch64 uxtb wN,wN；
#   - 模板占位符：AArch64 不加 ATT 的 % 前缀（GAS 用裸名 x0/w0）；
#   - Intel 方言切换（.intel_syntax）仅 x86 发射，AArch64 忽略（单一语法）。
# 模板按 AArch64 汇编语法书写（与 LLVM 一致：用户按目标写汇编，后端只渲染占位符）。
#
# 证据（全部 qemu-aarch64-static 实机运行）：
#   L1 identity：mov $0,$1 → mov x0,x1
#   L2 add7 原地累加（绑定约束 "=r,0"）：add $0,$0,#7 → add x0,x0,#7
#   L3 byte 宽度 C ABI 适配：uxtb 零扩展 + mov w0,w1 → 200/255 正确
#   L4 @asm_intel 入口在 AArch64 不误发 .intel_syntax，模板按 AArch64 语法运行
#   L5 无 asm 的普通程序 AArch64 行为零变化（golden 子集 qemu 实机）
#
# 铁律：所有 DIAL_ASM_TARGET 设置只作用于本脚本子进程，不污染宿主后续编译。
# 缺 qemu / 交叉 gcc 时整段 skip（CI 无这些依赖不误红），有则必须全绿。
#
# 用法：./tools/verify_aarch64_inline_asm.sh [编译器路径]   # 默认 ./target/release/dialc
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

QEMU="$(command -v qemu-aarch64-static || true)"
CC="$(command -v aarch64-linux-gnu-gcc || true)"
if [ -z "$QEMU" ] || [ -z "$CC" ]; then
    skip "跳过 AArch64 内联汇编门禁（缺 qemu-aarch64-static / aarch64-linux-gnu-gcc）"
    echo "  ── AArch64 内联汇编门禁结束（skip）──"
    exit 0
fi
if ! command -v timeout >/dev/null 2>&1 || ! command -v diff >/dev/null 2>&1; then
    skip "跳过 AArch64 内联汇编门禁（缺 timeout / diff）"
    echo "  ── AArch64 内联汇编门禁结束（skip）──"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
A64_ENV=(env DIAL_ASM_TARGET=aarch64 DIAL_C_COMPILER="$CC")
RUN_TIMEOUT=15

# AArch64 汇编语法模板（占位符 $0/$1 由后端渲染成 x0/x1 等裸名）。
SRC="$TMP/a64_asm.dial"
cat > "$SRC" <<'DIALEOF'
fn asm_identity(x: int): int {
    return @asm<int>("mov $0, $1", "=r,r", x);
}
fn asm_add7(x: int): int {
    return @asm<int>("add $0, $0, #7", "=r,0", x);
}
fn asm_byte(x: int): int {
    return byte_to_int(@asm<byte>("mov $0, $1", "=r,r", int_to_byte(x)));
}
fn asm_intel_id(x: int): int {
    return @asm_intel<int>("mov $0, $1", "=r,r", x);
}
fn asm_add2(a: int, b: int): int {
    return @asm<int>("add $0, $1, $2", "=r,r,r", a, b);
}
fn main(): int {
    print(asm_identity(42));   // 42
    print(asm_add7(35));       // 42
    print(asm_byte(200));      // 200
    print(asm_byte(255));      // 255
    print(asm_intel_id(88));   // 88
    print(asm_add2(100, 23));  // 123
    return 0;
}
DIALEOF

EXPECTED=$'42\n42\n200\n255\n88\n123'

echo "== 编译 + qemu 实机运行 AArch64 内联汇编 =="
if ! "${A64_ENV[@]}" "$DIALC" build "$SRC" --backend=asm --single-file -o "$TMP/a64_asm" >"$TMP/build.log" 2>&1; then
    bad "AArch64 内联汇编编译失败"; tail -10 "$TMP/build.log"
else
    ok "AArch64 内联汇编编译成功"
fi
if [ -f "$TMP/a64_asm" ]; then
    timeout "$RUN_TIMEOUT" "$QEMU" "$TMP/a64_asm" >"$TMP/out" 2>"$TMP/err"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        bad "qemu 运行失败（exit=$rc）"; head -5 "$TMP/err"
    elif printf '%s\n' "$EXPECTED" | diff -q - "$TMP/out" >/dev/null 2>&1; then
        ok "identity/add7/byte/intel/双输入 输出逐值正确 ($(tr '\n' ' ' < "$TMP/out"))"
    else
        bad "内联汇编输出不一致"; printf '%s\n' "$EXPECTED" | diff -u - "$TMP/out" | head -12
    fi
fi

echo "== 形态断言：渲染出 AArch64 寄存器（裸名，无 % 前缀 / 无 intel_syntax）=="
S="$TMP/a64_asm.s"
if [ -f "$S" ]; then
    grep -q 'mov x0, x1' "$S" && ok "identity 渲染为 mov x0,x1" || bad "identity 渲染错误"
    grep -q 'add x0, x0, #7' "$S" && ok "add7 渲染为 add x0,x0,#7" || bad "add7 渲染错误"
    grep -q 'add x0, x1, x2' "$S" && ok "双输入渲染为 add x0,x1,x2" || bad "双输入渲染错误"
    if grep -q 'intel_syntax\|att_syntax' "$S"; then
        bad "AArch64 误发射 x86 方言切换指令"
    else
        ok "无 x86 方言切换指令"
    fi
    if grep -q '%[xw][0-9]' "$S"; then
        bad "AArch64 寄存器误加 % 前缀"
    else
        ok "AArch64 寄存器用裸名（无 %）"
    fi
    grep -q 'uxtb' "$S" && ok "byte 宽度用 uxtb 零扩展" || bad "byte 宽度未用 uxtb"
else
    bad "未生成 .s"
fi

echo "== 零回归：无 asm 的普通程序 AArch64 行为不变 =="
PLAIN="$TMP/plain.dial"
cat > "$PLAIN" <<'DIALEOF'
fn main(): int { var s: int = 0; for (i in 0..10) { s = s + i; } print(s); return 0; }
DIALEOF
if "${A64_ENV[@]}" "$DIALC" build "$PLAIN" --backend=asm --single-file -o "$TMP/plain" >/dev/null 2>&1; then
    timeout "$RUN_TIMEOUT" "$QEMU" "$TMP/plain" >"$TMP/plain.out" 2>/dev/null
    [ "$(cat "$TMP/plain.out")" = "45" ] && ok "无 asm 程序输出正确 (45)" || bad "无 asm 程序输出错误: $(cat "$TMP/plain.out")"
else
    bad "无 asm 程序编译失败"
fi

if [ "$fail" -eq 0 ]; then
    echo "== AArch64 内联汇编门禁：全部通过 ✅ =="
else
    echo "== AArch64 内联汇编门禁：存在失败 ❌ =="
fi
exit "$fail"
