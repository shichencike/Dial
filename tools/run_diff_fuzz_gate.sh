#!/usr/bin/env bash
# 差分 fuzz 门禁：把「双后端实现一致性」变成可复现、确定性的 CI 检查。
#
# 三段检查：
#   1. golden 自检：现有 tests/asm_golden/*.dial 双后端差分应全 OK；
#   2. 固化语料：tests/diff/*.dial（从 fuzz 生成器中挑选、覆盖高风险路径的
#      确定性程序）双后端差分应全 OK；
#   3. 在线 fuzz：固定 seed 区间 + 固定优化级别，生成 N 个程序做差分，要求
#      0 不一致。固定 seed 保证每次 CI 跑的是同一批程序，结果可复现。
#
# 用法：
#   bash tools/run_diff_fuzz_gate.sh [DIALC] [--quick]
#     DIALC     编译器路径，默认 target/release/dialc
#     --quick   在线 fuzz 只跑 60 seeds（默认 150）
#
# 退出码：0 = 全通过；非 0 = 有不一致（详细用例落盘到 .dialc_fuzz/failures/）。
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

DIALC="${1:-$ROOT/target/release/dialc}"
QUICK=0
for arg in "$@"; do [ "$arg" = "--quick" ] && QUICK=1; done
if [ "$QUICK" -eq 1 ]; then SEEDS=60; else SEEDS=150; fi

if [ ! -x "$DIALC" ]; then
    echo "  ❌ 找不到编译器：$DIALC"
    exit 1
fi
if ! python3 -c 'import sys' >/dev/null 2>&1; then
    echo "  ❌ 缺少 python3"
    exit 1
fi

fail=0

echo "== 差分门禁 1/3：golden 自检 =="
if python3 tools/diff_fuzz.py --golden-check --dialc "$DIALC"; then
    :
else
    echo "  ❌ golden 自检发现不一致"
    fail=1
fi

echo "== 差分门禁 2/3：固化语料 tests/diff/ =="
if python3 tools/diff_fuzz.py --golden-check --diff-corpus tests/diff --dialc "$DIALC"; then
    :
else
    echo "  ❌ 固化语料差分发现不一致"
    fail=1
fi

echo "== 差分门禁 3/3：在线 fuzz（固定 seed，${SEEDS} 个 × O0/O2/O3）=="
OUT="$ROOT/.dialc_fuzz_gate"
rm -rf "$OUT"
if python3 tools/diff_fuzz.py --seeds "$SEEDS" --start 40000 --jobs 2 \
       --opt-levels O0 O2 O3 --out "$OUT" --dialc "$DIALC"; then
    :
else
    echo "  ❌ 在线 fuzz 发现不一致（用例见 $OUT/failures/）"
    fail=1
fi

if [ "$fail" -eq 0 ]; then
    echo "== 差分门禁全部通过 ✅ =="
else
    echo "== 差分门禁存在失败 ❌ =="
fi
exit "$fail"
