#!/usr/bin/env bash
# P4.5 门禁：AArch64 自研 PGO「实机两段式」验证（插桩→qemu 训练写 .dialprof→读回重编）。
#
# 背景（2026-11-20）：此前 verify_asm_pgo.sh 只在 x86-64 验证自研 PGO；AArch64 侧
# codegen 的 pgo_active 被硬门控，插桩全 x86 汇编（pushfq/%rip/incq），且训练二进制
# 是 aarch64 ELF 无法在 x86 宿主执行。P4.5 把插桩改成 target-aware：
#   - counter_inc：AArch64 用 adrp+add(:lo12) 取 .comm 符号地址 + ldr/add/str（add 不改
#     NZCV，无需保 flags），scratch x9/x10/x11 在驻留开启前无活跃值；
#   - dump：AArch64 用 stp/ldp 保护 x0-x17 + LR(x30)（bl 覆盖 LR，x86 call 不会），
#     静态栈 160 字节保持 16 对齐；
#   - main.rs：pgo_active 放开双目标，AArch64 交叉目标训练二进制改 -static 链接 + qemu
#     -aarch64-static 执行，缺 qemu 则 Fail-Fast（不静默降级）。
#
# 六层证据：
#   L1 两段式编译成功 + profile magic 合法
#   L2 计数反映真实执行（work 入口=调用数、循环头=迭代数，证明插桩真自增）
#   L3 冷热分段出现且热函数入 .text.hot
#   L4 baseline 与 PGO 版 qemu 实机输出逐字节一致（PGO 不改语义）
#   L5 profile 确定性（重复编译逐字节一致）
#   L6 Fail-Fast：屏蔽 qemu 时 AArch64 PGO 当场拒绝（不伪装成功）
#
# 铁律：所有 DIAL_ASM_TARGET 设置只作用于本脚本子进程，不污染宿主后续编译。
# 缺 qemu / 交叉 gcc 时整段 skip（CI 无这些依赖不误红），有则必须全绿。
#
# 用法：./tools/verify_aarch64_pgo.sh [编译器路径]   # 默认 ./target/release/dialc
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

# --- 依赖探测 ---
QEMU="$(command -v qemu-aarch64-static || true)"
CC="$(command -v aarch64-linux-gnu-gcc || true)"
if [ -z "$QEMU" ] || [ -z "$CC" ]; then
    skip "跳过 AArch64 PGO 门禁（缺 qemu-aarch64-static / aarch64-linux-gnu-gcc）"
    echo "  ── AArch64 PGO 门禁结束（skip）──"
    exit 0
fi
if ! command -v timeout >/dev/null 2>&1 || ! command -v diff >/dev/null 2>&1; then
    skip "跳过 AArch64 PGO 门禁（缺 timeout / diff）"
    echo "  ── AArch64 PGO 门禁结束（skip）──"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 固定 workload：与 x86 门禁 verify_asm_pgo.sh 同款（2 热函数 + 2 冷函数 + main 外层循环）。
# 计数写死，profile 确定；已由 x86 门禁证明能稳定分出冷热，此处复用只验证 AArch64 链路。
SRC="$TMP/pgo_a64.dial"
cat > "$SRC" <<'DIALEOF'
fn work1(x: int): int {
    var s: int = x;
    for (i in 0..300) { s = (s * 31 + i) % 1000000007; }
    return s;
}
fn work2(x: int): int {
    var s: int = x + 7;
    for (i in 0..300) { s = (s * 3 + 7) % 1000000007; }
    return s;
}
fn cold1(): int { return 111; }
fn cold2(): int { return 222; }
fn main(): int {
    var s: int = 0;
    for (r in 0..20) { s = s + work1(r + 1); s = s + work2(r + 1); }
    s = s + cold1(); s = s + cold2();
    print(s);
    return 0;
}
DIALEOF

# 子进程环境变量：只作用于本脚本内的 dialc 调用。
A64_ENV=(env DIAL_ASM_TARGET=aarch64 DIAL_C_COMPILER="$CC")
PROF_DIR=".dialc_build/safety-2/pgo-asm-pgo_a64"
PROF="$PROF_DIR/profile.dialprof"
S="$TMP/pgo_a64.s"   # .s 用源码 stem（output_paths → obj_path = <源码目录>/<源码stem>.o → .s）

# 每次门禁先清掉本门禁专属的 profile 与 .s，避免命中历史产物。
rm -rf "$PROF_DIR" "$S"

echo "== L1) AArch64 两段式编译 + profile magic =="
if ! "${A64_ENV[@]}" "$DIALC" build "$SRC" --backend=asm -O2 --pgo --single-file -o "$TMP/pgo" >"$TMP/pgo.log" 2>&1; then
    bad "AArch64 PGO 编译失败"; tail -8 "$TMP/pgo.log"
elif [ ! -f "$PROF" ]; then
    bad "未生成 profile.dialprof"; tail -8 "$TMP/pgo.log"
else
    ok "AArch64 PGO 两段式编译成功"
    if [ "$(sed -n '1p' "$PROF")" = "DIALPGO1" ]; then
        ok ".dialprof magic 正确 (DIALPGO1)"
    else
        bad ".dialprof magic 错误: $(sed -n '1p' "$PROF")"
    fi
fi

echo "== L2) 插桩计数反映真实执行 =="
# work1/work2 内层循环各 20 轮 × 300 = 6000 → 循环头计数 > 5000。
if [ -f "$PROF" ] && awk 'NR>2 && $2>5000 {found=1} END{exit !found}' "$PROF"; then
    ok ".dialprof 循环头计数 >5000（AArch64 插桩真自增）"
else
    bad ".dialprof 无高频计数（AArch64 插桩失效）"; [ -f "$PROF" ] && head -10 "$PROF"
fi

echo "== L3) 冷热分段 + 归属 =="
if [ -f "$S" ] && grep -q '\.section \.text\.hot' "$S" && grep -q '\.section \.text\.unlikely' "$S"; then
    ok "优化版 .s 出现 .text.hot / .text.unlikely"
    hot_ok=$(awk '/\.section \.text\.hot/{sec="hot"} /\.section \.text\.unlikely/{sec="cold"} /^work1:|^work2:/{if(sec!="hot")bad=1} END{exit bad+0}' "$S"; echo $?)
    cold_ok=$(awk '/\.section \.text\.hot/{sec="hot"} /\.section \.text\.unlikely/{sec="cold"} /^cold1:|^cold2:/{if(sec!="cold")bad=1} END{exit bad+0}' "$S"; echo $?)
    if [ "$hot_ok" -eq 0 ] && [ "$cold_ok" -eq 0 ]; then
        ok "热函数入 .text.hot、冷函数入 .text.unlikely"
    else
        bad "函数冷热分段归属错误"
    fi
else
    bad "优化版 .s 缺冷热分段"
fi

echo "== L4) baseline 与 PGO 版 qemu 实机输出一致 =="
if "${A64_ENV[@]}" "$DIALC" build "$SRC" --backend=asm -O2 --single-file -o "$TMP/base" >"$TMP/base.log" 2>&1; then
    timeout 15 "$QEMU" "$TMP/base" >"$TMP/base.out" 2>/dev/null
    timeout 15 "$QEMU" "$TMP/pgo"  >"$TMP/pgo.out"  2>/dev/null
    if [ -f "$TMP/base.out" ] && [ -f "$TMP/pgo.out" ] && cmp -s "$TMP/base.out" "$TMP/pgo.out"; then
        ok "qemu 实机 baseline==PGO 输出 (result=$(cat "$TMP/base.out"))"
    else
        bad "qemu 实机 baseline/PGO 输出不一致"; diff "$TMP/base.out" "$TMP/pgo.out" | head -6
    fi
else
    bad "baseline 编译失败"; tail -6 "$TMP/base.log"
fi

echo "== L5) profile 确定性（重复编译逐字节一致）=="
if [ -f "$PROF" ]; then
    cp "$PROF" "$TMP/prof1"
    "${A64_ENV[@]}" "$DIALC" build "$SRC" --backend=asm -O2 --pgo --single-file -o "$TMP/pgo2" >/dev/null 2>&1
    if [ -f "$PROF" ] && cmp -s "$TMP/prof1" "$PROF"; then
        ok "两次训练 profile 逐字节一致"
    else
        bad "profile 不确定"
    fi
fi

echo "== L6) Fail-Fast：缺 qemu 时 AArch64 PGO 当场拒绝 =="
# 用空 PATH 子目录屏蔽 qemu（保留 HOME 供工具链，DIAL 环境变量显式给）。
EMPTY="$(mktemp -d)"
FF_LOG="$TMP/ff.log"
env -i PATH="$EMPTY" HOME="$HOME" DIAL_ASM_TARGET=aarch64 DIAL_C_COMPILER="$CC" \
    "$DIALC" build "$SRC" --backend=asm -O2 --pgo -o "$TMP/ff" >"$FF_LOG" 2>&1
FF_RC=$?
rm -rf "$EMPTY"
if [ "$FF_RC" -ne 0 ] && grep -qi "qemu" "$FF_LOG"; then
    ok "缺 qemu → Fail-Fast 拒绝（rc=$FF_RC，明确提示）"
else
    bad "缺 qemu 未 Fail-Fast（rc=$FF_RC）"; tail -4 "$FF_LOG"
fi

if [ "$fail" -eq 0 ]; then
    echo "== AArch64 PGO 门禁：全部通过 ✅ =="
else
    echo "== AArch64 PGO 门禁：存在失败 ❌ =="
fi
exit "$fail"
