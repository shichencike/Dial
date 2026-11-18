#!/usr/bin/env python3
"""caller-saved 寄存器分配收益诊断（静态分析 asm 后端生成的 x86-64 汇编）。

用途：在投入「把 GRA 重写为 live-range 分配以支持 caller-saved」这一内核级工程之前，
先用数据判断收益上限是否抵得过成本。不改任何产品代码，只读 .s。

生成待分析的汇编（在仓库根执行，普通 build 会在 .dial 同目录留下 .s）：

    mkdir -p /tmp/diag && cp bench/bench.dial /tmp/diag/
    target/release/dialc build /tmp/diag/bench.dial --backend asm -O2 -o /tmp/diag/bin
    python3 tools/diag_gra.py /tmp/diag/bench.s

三个判据：

  1) 上界比 S/(8*C)：把 caller-saved 加入「全函数固定绑定」池时，每个 call 点须
     save/restore 4 个 caller-saved 驻留值 = 8 条 mov，成本 8*C。收益乐观上界是该函数
     全部栈访问 S（实际远小于 S，因为多数是 ABI 流量）。S < 8*C => 朴素方案必负收益。
  2) 栈槽访问频次：只访问 1~2 次的栈槽是 call 结果落栈 / 参数中转，被 C ABI 强制，
     跟寄存器数量无关；只有高频(>=4)栈槽才可能是寄存器压力导致的 spill。
  3) 综合：若多数栈访问是低频 ABI 流量、且上界比 < 1，则 live-range 重写即便成功，
     可消除量也受限于「值活不过下一次 call」这一 ABI 事实，收益不足以抵消重写成本。

用法：
    python3 tools/diag_gra.py <bench.s>
"""
from __future__ import annotations

import collections
import re
import sys


def analyze(path: str) -> None:
    lines = open(path, encoding="utf-8").read().splitlines()

    funcs: dict[str, dict] = {}
    cur: str | None = None
    fn_slots: dict[str, collections.Counter] = {}

    for ln in lines:
        m = re.match(r"^([A-Za-z_][A-Za-z0-9_]*):\s*$", ln)
        if m:
            cur = m.group(1)
            funcs[cur] = dict(calls=0, frame=0, cs_save=0, stack_rw=0)
            fn_slots[cur] = collections.Counter()
            continue
        if cur is None:
            continue
        d = funcs[cur]
        if re.search(r"\bcall(q|l)?\b", ln):
            d["calls"] += 1
        m = re.match(r"\s*subq\s+\$(\d+),\s*%rsp", ln)
        if m:
            d["frame"] = max(d["frame"], int(m.group(1)))
        m = re.match(r"\s*movq\s+%(rbx|r12|r13|r14|r15|rbp),\s*(\d+)\(%rsp\)", ln)
        if m and int(m.group(2)) <= 48:
            d["cs_save"] += 1
        if re.search(r"\(%rsp\)", ln) and re.match(r"\s*mov", ln):
            d["stack_rw"] += 1
            for mm in re.finditer(r"(?<![\w])(-?\d+)\(%rsp\)", ln):
                off = int(mm.group(1))
                if not (0 <= off <= 48):  # 排除 prologue callee-saved 保存区
                    fn_slots[cur][off] += 1

    rows = [(k, v) for k, v in funcs.items() if v["calls"] > 0]
    rows.sort(key=lambda kv: -kv[1]["calls"])

    print(f"{'函数':<12}{'calls':>7}{'frameB':>9}{'csSave':>8}{'stackRW':>9}{'8*C成本':>9}{'S上界':>8}{'上界/成本':>10}")
    print("-" * 74)
    tot_c = tot_s = tot_cost = 0
    for k, v in rows:
        cost = 8 * v["calls"]
        ratio = v["stack_rw"] / cost if cost else 0
        print(f"{k:<12}{v['calls']:>7}{v['frame']:>9}{v['cs_save']:>8}{v['stack_rw']:>9}{cost:>9}{v['stack_rw']:>8}{ratio:>10.3f}")
        tot_c += v["calls"]
        tot_s += v["stack_rw"]
        tot_cost += cost
    print("-" * 74)
    print(f"{'合计':<12}{tot_c:>7}{'':>9}{'':>8}{tot_s:>9}{tot_cost:>9}{tot_s:>8}{tot_s/tot_cost if tot_cost else 0:>10.3f}")

    print("\n栈槽访问频次分布（每函数独立统计，排除 prologue 保存区）:")
    hist: collections.Counter = collections.Counter()
    for slots in fn_slots.values():
        for cnt in slots.values():
            hist[cnt] += 1
    total_slots = sum(hist.values())
    total_access = sum(c * n for c, n in hist.items())
    print(f"  唯一栈槽 {total_slots}，总访问 {total_access}")
    for c in sorted(hist):
        n = hist[c]
        print(f"  {c:>3} 次: {n:>4} 槽 ({n/total_slots*100:>5.1f}%), 贡献 {c*n:>4} 访问")
    for th in (3, 4, 8):
        hot = sum(n for c, n in hist.items() if c >= th)
        hotacc = sum(c * n for c, n in hist.items() if c >= th)
        print(f"  >={th} 次: {hot} 槽 ({hot/total_slots*100:.1f}%), 贡献 {hotacc/total_access*100:.1f}% 访问")

    print("\n结论指引：")
    print("  * 上界/成本 < 1  => 朴素 caller-saved 全绑定方案必负收益；")
    print("  * 低频(<=2次)栈槽占绝大多数 => 多为 C ABI 强制的 call 结果落栈/参数中转，")
    print("    值活不过下一次 call，live-range 重写可消除量有限，不足以抵消内核级重写成本。")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        raise SystemExit(2)
    analyze(sys.argv[1])
