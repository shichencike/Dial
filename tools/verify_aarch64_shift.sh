#!/usr/bin/env bash
# P4.6 门禁：AArch64 后端「常量移位立即数折叠」验证。
#
# 背景（2026-11-20）：x86-64 后端早在 M7-c1 就把 [0,63] 的整数常量移位量折叠为
# `shlq/sarq $n, reg`（跳过 %cl 装载），但同一 shift_imm() 判定被 target 门控只在
# x86 生效，AArch64 始终发射 `lslv/asrv reg, reg, reg`——必须先把移位量装进寄存器。
# AArch64 立即数移位 `lsl/asr reg, reg, #n` 同样只接受 [0,63]，与 x86 完全同构，
# 因此 P4.6 去掉 target 门控，让两个 ISA 共用同一折叠判定，AArch64 每条常量移位
# 省掉 1 条装载指令。这是「平台对等」的对等补齐，不是性能竞赛（战略：够用即可）。
#
# 证据：
#   L1 结构：常量 << 3 / >> 2 折叠为 `lsl x9,x9,#3` / `asr x9,x9,#2`
#   L2 边界：<< 0 / << 63 也折叠为立即数（shift_imm 的 [0,63] 闭区间）
#   L3 非常量：x << n（变量移位量）仍走 `lslv` 寄存器形式，不误折叠
#   L4 qemu 实机：常量/边界/非常量输出与 x86 原生逐字节一致
#   L5 x86 零回归：x86 .s 仍是 `shlq/sarq $n`、无 %cl 变址
#
# 铁律：所有 DIAL_ASM_TARGET 设置只作用于本脚本子进程，不污染宿主后续编译。
# 缺 qemu / 交叉 gcc 时整段 skip（CI 无这些依赖不误红），有则必须全绿。
#
# 用法：./tools/verify_aarch64_shift.sh [编译器路径]   # 默认 ./target/release/dialc
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
    skip "跳过 AArch64 常量移位门禁（缺 qemu-aarch64-static / aarch64-linux-gnu-gcc）"
    echo "  ── AArch64 常量移位门禁结束（skip）──"
    exit 0
fi
if ! command -v timeout >/dev/null 2>&1 || ! command -v diff >/dev/null 2>&1; then
    skip "跳过 AArch64 常量移位门禁（缺 timeout / diff）"
    echo "  ── AArch64 常量移位门禁结束（skip）──"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
A64_ENV=(env DIAL_ASM_TARGET=aarch64 DIAL_C_COMPILER="$CC")
RUN_TIMEOUT=15

SRC="$TMP/shift.dial"
cat > "$SRC" <<'DIALEOF'
export fn shl_const(x: int): int { return x << 3; }
export fn shr_const(x: int): int { return x >> 2; }
export fn shl0(x: int): int { return x << 0; }
export fn shl63(x: int): int { return x << 63; }
export fn shl_var(x: int, n: int): int { return x << n; }
fn main(): int {
    print(shl_const(1));    // 8
    print(shr_const(16));   // 4
    print(shl0(7));         // 7
    print(shl63(0));        // 0
    print(shl_var(3, 2));   // 12
    return 0;
}
DIALEOF
EXPECTED=$'8\n4\n7\n0\n12'

echo "== 编译 AArch64 目标（保留 .s 观察发射形态）=="
if ! "${A64_ENV[@]}" "$DIALC" build "$SRC" --backend=asm -O2 --no-link -o "$TMP/a64" >"$TMP/a64build.log" 2>&1; then
    bad "AArch64 编译失败"; tail -10 "$TMP/a64build.log"
else
    ok "AArch64 编译成功"
fi

S="$TMP/shift.s"
echo "== L1/L2/L3 结构断言：常量折叠、边界折叠、非常量保留变址 =="
if [ -f "$S" ]; then
    grep -qE 'lsl x9, x9, #3' "$S" && ok "常量 << 3 折叠为 lsl x9,x9,#3" || bad "常量左移未折叠为立即数"
    grep -qE 'asr x9, x9, #2' "$S" && ok "常量 >> 2 折叠为 asr x9,x9,#2" || bad "常量右移未折叠为立即数"
    grep -qE 'lsl x9, x9, #0' "$S" && ok "边界 << 0 折叠为 lsl x9,x9,#0" || bad "<< 0 未折叠"
    grep -qE 'lsl x9, x9, #63' "$S" && ok "边界 << 63 折叠为 lsl x9,x9,#63" || bad "<< 63 未折叠"
    grep -qE 'lslv x9, x9, x10' "$S" && ok "非常量 x << n 仍走 lslv 变址（未误折叠）" || bad "非常量移位未走 lslv"
else
    bad "未生成 .s 供结构断言"
fi

echo "== L4 qemu 实机：与 x86 原生输出逐字节一致 =="
if "${A64_ENV[@]}" "$DIALC" build "$SRC" --backend=asm -O2 --single-file -o "$TMP/a64_run" >"$TMP/a64run.log" 2>&1; then
    timeout "$RUN_TIMEOUT" "$QEMU" "$TMP/a64_run" >"$TMP/a64.out" 2>"$TMP/a64.err"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        bad "qemu 运行失败（exit=$rc）"; head -5 "$TMP/a64.err"
    elif printf '%s\n' "$EXPECTED" | diff -q - "$TMP/a64.out" >/dev/null 2>&1; then
        ok "AArch64 qemu 输出逐值正确 ($(tr '\n' ' ' < "$TMP/a64.out"))"
    else
        bad "AArch64 输出不一致"; printf '%s\n' "$EXPECTED" | diff -u - "$TMP/a64.out" | head -12
    fi
else
    bad "AArch64 single-file 编译失败"; tail -6 "$TMP/a64run.log"
fi
if "$DIALC" build "$SRC" --backend=asm -O2 --single-file -o "$TMP/x86_run" >"$TMP/x86run.log" 2>&1; then
    "$TMP/x86_run" >"$TMP/x86.out" 2>/dev/null
    if diff -q "$TMP/x86.out" "$TMP/a64.out" >/dev/null 2>&1; then
        ok "x86 原生与 AArch64 qemu 输出逐字节一致"
    else
        bad "x86/AArch64 输出不一致"; diff -u "$TMP/x86.out" "$TMP/a64.out" | head -12
    fi
else
    bad "x86 single-file 编译失败"; tail -6 "$TMP/x86run.log"
fi

echo '== L5 x86 零回归：常量仍为 shlq/sarq $n 立即数、非常量仍为 %cl 变址 =='
X86S="$TMP/shift.s"
# x86 复用同名 .s（重新编译覆盖），断言立即数/变址各归其位、形态不变。
# shift 函数用 export fn（External linkage）：不会被内联/死函数消除删除，
# .s 里保留独立函数体供形态断言（否则 O2 内联后消除，形态断言落空）。
if "$DIALC" build "$SRC" --backend=asm -O2 --no-link -o "$TMP/x86nl" >"$TMP/x86nl.log" 2>&1 && [ -f "$X86S" ]; then
    grep -qE 'shlq \$3, ' "$X86S" && ok "x86 常量 << 3 仍为 shlq \$3" || bad "x86 常量左移形态回归"
    grep -qE 'sarq \$2, ' "$X86S" && ok "x86 常量 >> 2 仍为 sarq \$2" || bad "x86 常量右移形态回归"
    grep -qE 'shlq %cl' "$X86S" && ok "x86 非常量 x << n 仍为 %cl 变址（未误折叠）" || bad "x86 非常量移位形态回归"
else
    bad "x86 no-link 编译失败或未生成 .s"
fi

if [ "$fail" -eq 0 ]; then
    echo "== AArch64 常量移位门禁：全部通过 ✅ =="
else
    echo "== AArch64 常量移位门禁：存在失败 ❌ =="
fi
exit "$fail"
