#!/usr/bin/env bash
# 唯一后端哲学 · 第 4 项：自研 PGO 闭环门禁（R4，收回路线图最后一项）。
#
# 核心命题（见 rationale/roadmap-gap-analysis.md 总纲执行附则第 4 项）：
#   --pgo 在 asm 后端可用、零 bitcode/.bc 依赖——插桩编译 → 训练运行写 .dialprof
#   → 读回重编（热函数聚拢 .text.hot + 冷函数甩 .text.unlikely + 冷循环省对齐 padding）。
#   LLVM 路径、baseline（无 --pgo）、AArch64、-g 一律不受影响。
#
# 四层证据：
#   L1 功能闭环：自包含固定 workload（无随机/无 args），baseline 与 PGO 版输出
#       逐字节一致；训练产出合法 .dialprof（magic/计数正确反映真实执行）。
#   L2 布局生效：优化版 .s 出现 .text.hot / .text.unlikely 分段；热函数（含高频
#       循环）入 .text.hot、冷函数入 .text.unlikely。
#   L3 确定性：固定 workload 下重复编译，profile 内容逐字节一致。
#   L4 零回归：同一源码无 --pgo 的两次编译 .s 一致（profile=None 全短路）。
#
# 用法：./tools/verify_asm_pgo.sh [编译器路径]   # 默认 ./target/release/dialc
# 退出码：0 = 全绿；1 = 有失败。

set -u

DIALC="${1:-./target/release/dialc}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

fail=0
ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }

if [ ! -x "$DIALC" ]; then
    echo "  ❌ 编译器不存在或不可执行: $DIALC"
    exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 固定 workload：3 个热函数（数据依赖循环，防常量折叠）+ 2 个冷函数。
# 循环计数写死，训练运行即真实 workload，profile 内容确定。
SRC="$TMP/pgo_verify.dial"
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
    for (r in 0..20) {
        s = s + work1(r + 1);
        s = s + work2(r + 1);
    }
    s = s + cold1();
    s = s + cold2();
    print(s);
    return 0;
}
DIALEOF

echo "== L1) 功能闭环：baseline 与 PGO 输出一致 + profile 合法 =="
if ! "$DIALC" build "$SRC" --backend=asm -O2 -o "$TMP/base" >"$TMP/base.log" 2>&1; then
    bad "baseline 编译失败"; tail -5 "$TMP/base.log"
else
    "$TMP/base" > "$TMP/base.out" 2>&1
    ok "baseline 编译+运行"
fi
if ! "$DIALC" build "$SRC" --backend=asm -O2 --pgo -o "$TMP/pgo" >"$TMP/pgo.log" 2>&1; then
    bad "PGO 编译失败"; tail -8 "$TMP/pgo.log"
else
    "$TMP/pgo" > "$TMP/pgo.out" 2>&1
    ok "PGO 两段式编译+运行"
fi
if [ -f "$TMP/base.out" ] && [ -f "$TMP/pgo.out" ] && cmp -s "$TMP/base.out" "$TMP/pgo.out"; then
    ok "baseline 与 PGO 输出逐字节一致 (result=$(cat "$TMP/base.out"))"
else
    bad "baseline 与 PGO 输出不一致"; diff "$TMP/base.out" "$TMP/pgo.out" | head -6
fi

# 找到本次 profile（stem=pgo_verify），校验格式与计数。
PROF=$(find "$ROOT/.dialc_build" -path '*pgo-asm-pgo_verify*' -name profile.dialprof 2>/dev/null | head -1)
if [ -z "$PROF" ]; then
    bad "未生成 .dialprof"
else
    head1=$(sed -n '1p' "$PROF")
    if [ "$head1" = "DIALPGO1" ]; then
        ok ".dialprof magic 正确 (DIALPGO1)"
    else
        bad ".dialprof magic 错误: $head1"
    fi
    # 循环头计数应 > 0（work1/work2 各执行 20*300=6000 次）
    if awk 'NR>2 && $2>100 {found=1} END{exit !found}' "$PROF"; then
        ok ".dialprof 计数反映真实热路径"
    else
        bad ".dialprof 无高频计数（插桩可能失效）"; head -8 "$PROF"
    fi
fi

echo "== L2) 布局生效：优化版 .s 出现冷热分段且归属正确 =="
S="$TMP/pgo_verify.s"
if [ ! -f "$S" ]; then
    bad "未生成 PGO 版 .s"
elif ! grep -q '\.section \.text\.hot' "$S" || ! grep -q '\.section \.text\.unlikely' "$S"; then
    bad ".s 缺 .text.hot / .text.unlikely 分段"
else
    ok "优化版 .s 出现冷热分段"
    # 热函数 work1/work2 应紧跟 .text.hot，冷函数 cold1/cold2 应跟 .text.unlikely。
    hot_ok=$(awk '/\.section \.text\.hot/{sec="hot"} /\.section \.text\.unlikely/{sec="cold"} /^work1:|^work2:/{if(sec!="hot")bad=1} END{exit bad+0}' "$S"; echo $?)
    cold_ok=$(awk '/\.section \.text\.hot/{sec="hot"} /\.section \.text\.unlikely/{sec="cold"} /^cold1:|^cold2:/{if(sec!="cold")bad=1} END{exit bad+0}' "$S"; echo $?)
    if [ "$hot_ok" -eq 0 ] && [ "$cold_ok" -eq 0 ]; then
        ok "热函数入 .text.hot、冷函数入 .text.unlikely"
    else
        bad "函数冷热分段归属错误"
    fi
fi

echo "== L3) 确定性：固定 workload 重复编译，profile 逐字节一致 =="
if [ -n "$PROF" ]; then
    cp "$PROF" "$TMP/prof1"
    "$DIALC" build "$SRC" --backend=asm -O2 --pgo -o "$TMP/pgo2" >/dev/null 2>&1
    PROF2=$(find "$ROOT/.dialc_build" -path '*pgo-asm-pgo_verify*' -name profile.dialprof 2>/dev/null | head -1)
    if [ -n "$PROF2" ] && cmp -s "$TMP/prof1" "$PROF2"; then
        ok "两次训练 profile 逐字节一致"
    else
        bad "profile 不确定"
    fi
fi

echo "== L4) 零回归：无 --pgo 的 .s 两次编译一致（profile=None 全短路）=="
"$DIALC" build "$SRC" --backend=asm -O2 --no-link -o "$TMP/b1.o" >/dev/null 2>&1 && cp "$TMP/pgo_verify.s" "$TMP/nopgo1.s"
"$DIALC" build "$SRC" --backend=asm -O2 --no-link -o "$TMP/b2.o" >/dev/null 2>&1 && cp "$TMP/pgo_verify.s" "$TMP/nopgo2.s"
if [ -f "$TMP/nopgo1.s" ] && cmp -s "$TMP/nopgo1.s" "$TMP/nopgo2.s"; then
    ok "无 --pgo 两次 .s 逐字节一致"
else
    bad "无 --pgo .s 不稳定"
fi

if [ "$fail" -eq 0 ]; then
    echo "== 自研 PGO 闭环门禁：全部通过 ✅ =="
else
    echo "== 自研 PGO 闭环门禁：存在失败 ❌ =="
fi
exit "$fail"
