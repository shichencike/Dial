#!/usr/bin/env python3
"""标量 ISA 扩展（LZCNT/TZCNT/BMI1/BMI2/CMOV）ROI 诊断工具。

用途：在投入 targeted 指令融合之前，静态扫描 asm 后端生成的 x86-64 汇编，
量化各类高级标量指令「可替代惯用法」的出现频次，作为 ROI 决策依据。
延续 e3x（自动向量化证伪）/e3at（caller-saved GRA 否决）的「先量化再投入」纪律。

用法：
    dialc build bench/bench.dial --backend=asm --target-cpu=baseline -O2 -o /tmp/diag/bin
    python3 tools/diag_scalar_isa.py /tmp/diag/bench.s

判据（当前 x86-64 baseline 汇编）：
  - TZCNT/LZCNT：源码里的 ctz/clz 惯用法（x&(x-1)、位扫描 bsf/bsr）在 Dial 中
    没有内建，只能靠编译器识别循环惯用法；统计 bsf/bsr/lzcnt/tzcnt 与 \\& 1 循环。
  - BMI2 shlx/shrx/sarx：替代「变量移位」shlq/sarq %cl（固定移位 $N 不适用）。
  - BMI1 andn：替代 ~x & y，近似统计 notq 指令。
  - CMOV：只在「无条件计算两个值再选择」时可安全替换；cmp+jcc+mov 只是粗上界，
    需人工确认分支体无副作用/无循环/无递归。
"""
import re
import sys
from collections import Counter


def scan(path: str) -> None:
    lines = open(path, encoding="utf-8", errors="ignore").read().splitlines()
    pats = {
        "变量移位 %cl (BMI2 shlx/shrx/sarx)": r"\b(shlq|shrq|sarq|salq)\s+%cl\b",
        "位扫描 bsf/bsr (LZCNT/TZCNT)": r"\b(bsf|bsr|lzcnt|tzcnt)\b",
        "notq (BMI1 andn 粗上界)": r"\bnotq\b",
        "固定移位 $N (非 BMI2 目标)": r"\b(shlq|shrq|sarq)\s+\$",
        "popcnt (e3y 已融合)": r"\bpopcnt\b",
    }
    print(f"=== {path}: {len(lines)} 行 ===")
    for k, p in pats.items():
        print(f"  {k:<34}{sum(bool(re.search(p, l)) for l in lines)}")

    cand = 0
    dist: Counter = Counter()
    for i in range(len(lines) - 2):
        if lines[i].strip().startswith("cmp") and lines[i + 1].strip().startswith("j"):
            if any(x.strip().startswith("mov") for x in lines[i + 2 : i + 5]):
                cand += 1
                fn = "?"
                for j in range(i, -1, -1):
                    m = re.search(r"\.Ldial_([A-Za-z0-9_]+)_b\d+", lines[j])
                    if m:
                        fn = m.group(1)
                        break
                dist[fn] += 1
    print(f"  {'cmp+jcc+mov 候选 (CMOV 粗上界)':<32}{cand}")
    print("    按函数分布（多数应为循环边界/递归基，非 CMOV 目标）:")
    for fn, n in dist.most_common(6):
        print(f"      {fn:<16}{n}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    scan(sys.argv[1])
