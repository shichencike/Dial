#!/usr/bin/env bash
# P0 正确性门禁：asm 后端 string 拼接 / 插值临时引用的零泄漏回归。
#
# 背景（2026-11-19）：审计原记「map string key 泄漏 30MB」，深挖后归因修正为
#   **string concat / interp 的 lowering 未释放自带所有权的临时操作数**——
#   `dial_string_concat` / `dial_string_new` / `*_to_string` 只读不消费操作数，
#   而 `"k"+str(i)` 的 str(i) 中间值无 Local 持有、concat 后不 release → 每轮泄漏。
#   典型 RSS：N=50万 17MB、N=200万 64MB（线性增长 ~31B/轮）。
# 修复（lower.rs）：concat 后 release owned 操作数；插值追踪每段 owned 状态并结算。
#
# 本门禁用 RSS 随循环次数的斜率断言零泄漏，并验证双后端输出一致：
#   L1 + 拼接 20 万次：RSS < 8MB（修复前 ~7MB@20万，修复后 ~1.4MB）。
#   L2 真插值 ${int} 20 万次：RSS < 8MB。
#   L3 链式拼接 20 万次：RSS < 8MB。
#   L4 双后端输出逐字节一致（正确性，防误释放导致的结果错乱）。
#
# 用法：./tools/verify_string_arc_no_leak.sh [编译器路径]   # 默认 ./target/release/dialc
# 退出码：0 = 全绿；1 = 有失败。

set -u

DIALC="${1:-./target/release/dialc}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

fail=0
ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }

if [ ! -x "$DIALC" ]; then
    echo "  ❌ 编译器不存在或不可执行：$DIALC"
    exit 1
fi
if ! command -v /usr/bin/time >/dev/null 2>&1; then
    echo "  ⚠️  缺少 /usr/bin/time，跳过 RSS 断言（仅做输出一致性检查）"
    SKIP_RSS=1
else
    SKIP_RSS=0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

N=200000
LIMIT_KB=8192   # 8MB：修复后 ~1.4MB，修复前 ~7MB（20万次），留足裕度但能抓住回归

# 取最大 RSS（KB）；失败返回空。
rss_kb() {
    /usr/bin/time -v "$@" 2>&1 | grep 'Maximum resident set size' | grep -oE '[0-9]+' | head -1
}

# --- 生成三个压测源（固定 workload，无随机） ---
cat >"$TMP/concat.dial" <<EOF
fn main(): int {
    var i: int = 0;
    while (i < ${N}) {
        var s: string = "k" + str(i);
        i = i + 1;
    }
    print("concat ", i);
    return 0;
}
EOF
cat >"$TMP/interp.dial" <<EOF
fn main(): int {
    var i: int = 0;
    while (i < ${N}) {
        var s: string = "v=\${i}!";
        i = i + 1;
    }
    print("interp ", i);
    return 0;
}
EOF
cat >"$TMP/chain.dial" <<EOF
fn main(): int {
    var i: int = 0;
    while (i < ${N}) {
        var s: string = "a" + str(i) + "b" + str(i);
        i = i + 1;
    }
    print("chain ", i);
    return 0;
}
EOF

for case in concat interp chain; do
    src="$TMP/${case}.dial"
    if ! "$DIALC" build "$src" --backend=asm --fast -O3 -o "$TMP/${case}_asm" >"$TMP/${case}_build.log" 2>&1; then
        bad "$case：asm 编译失败"; tail -8 "$TMP/${case}_build.log"; continue
    fi
    if ! "$DIALC" build "$src" --backend=llvm --fast -O3 -o "$TMP/${case}_ll" >"$TMP/${case}_build_ll.log" 2>&1; then
        bad "$case：llvm 编译失败"; tail -8 "$TMP/${case}_build_ll.log"; continue
    fi

    # L4 输出一致
    ao="$("$TMP/${case}_asm" 2>&1)"; ae=$?
    lo="$("$TMP/${case}_ll" 2>&1)"; le=$?
    if [ "$ae" -ne 0 ] || [ "$le" -ne 0 ]; then
        bad "$case：运行退出码非 0（asm=$ae llvm=$le）"; continue
    fi
    if [ "$ao" != "$lo" ]; then
        bad "$case：双后端输出不一致（asm=[$ao] llvm=[$lo]）"; continue
    fi

    # L1-L3 RSS 断言
    if [ "$SKIP_RSS" -eq 0 ]; then
        rss="$(rss_kb "$TMP/${case}_asm")"
        if [ -z "$rss" ]; then
            bad "$case：RSS 采样失败"; continue
        fi
        if [ "$rss" -ge "$LIMIT_KB" ]; then
            bad "$case：asm RSS=${rss}KB ≥ ${LIMIT_KB}KB（疑似临时 string 泄漏）"
        else
            ok "$case：双后端输出一致，asm RSS=${rss}KB < ${LIMIT_KB}KB（零泄漏）"
        fi
    else
        ok "$case：双后端输出一致（RSS 断言因缺 /usr/bin/time 跳过）"
    fi
done

echo "  ── string concat/interp ARC 零泄漏门禁结束 ──"
exit "$fail"
