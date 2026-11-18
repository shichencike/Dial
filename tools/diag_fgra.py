#!/usr/bin/env python3
"""浮点寄存器分配（fGRA）收益诊断（只读地量化 asm 后端浮点栈访问开销）。

用途：诊断「浮点值不驻留、每次 FBin 都 load/store 栈」造成的性能缺口。本工具
**不改任何产品代码**，只做两件事：① 静态统计 asm 生成的 .s 里浮点栈访问密度；
② 若给出编译器路径，则编译一个自带的内联浮点热循环 workload，与 LLVM 后端交替
计时，量化 asm/LLVM 耗时比。

生成待分析的汇编（在仓库根执行）：

    mkdir -p /tmp/diagf && cp tests/asm_golden/core1.dial /tmp/diagf/
    target/release/dialc build /tmp/diagf/core1.dial --backend asm -O2 -o /tmp/diagf/bin
    # 对含浮点的程序：
    python3 tools/diag_fgra.py --asm /tmp/diagf/fp.s

完整诊断（编译自带 workload + 双后端计时）：

    python3 tools/diag_fgra.py --dialc target/release/dialc

判据：
  - 浮点运算 (addsd/subsd/mulsd/divsd) 与浮点栈访问 (movsd (%rsp)) 的比值。
    理想驻留下比值应接近 1:1（仅在函数入/出处访问栈）；若达到 5x 以上，说明每个
    中间结果都被强制落栈，是浮点驻留的直接收益空间。
  - asm/LLVM 耗时比 > 1 表示 asm 更慢；显著 > 1 且伴随高栈访问比，则浮点驻留值得做。

与 diag_gra.py 的区别：diag_gra 诊断含 call 整数函数的 caller-saved（结论：ROI 负，
被 ABI 强制落栈救不了）；本工具诊断无 call 的纯浮点热循环（x86-64 有 16 个 xmm
闲置，落栈纯属编译器未利用寄存器），结论方向可能相反。
"""
from __future__ import annotations

import argparse
import re
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

# 自带的浮点密集 workload：纯计算、无 call，依赖浮点值驻留。
FP_WORKLOAD = r"""
fn parseRep(s: string): int { var n: int = 0; for (i in 0..s.length()) { n = n * 10 + (s.get(i) - 48); } return n; }
fn fp_work(n: int): float {
    var x: float = 1.000001;
    var acc: float = 0.0;
    var i: int = 0;
    while (i < n) {
        x = x * 1.0000001 + 0.0000003;
        acc = acc + x * x - x / 2.0 + 3.14159265;
        i = i + 1;
    }
    return acc;
}
fn main(): int {
    var av: string[] = args();
    var n: int = 200000;
    if (av.length() > 0) { n = parseRep(av[0]); }
    print(float_to_int(fp_work(n)));
    return 0;
}
"""


def static_stats(asm_path: str) -> None:
    lines = Path(asm_path).read_text(encoding="utf-8").splitlines()
    fp_op = [ln for ln in lines if re.search(r"\b(addsd|subsd|mulsd|divsd)\b", ln)]
    fp_stack = [ln for ln in lines if re.search(r"\bmovs[sd]\b.*\(%rsp\)", ln)]
    movabs = [ln for ln in lines if re.search(r"\bmovabsq\b", ln)]
    n_op = len(fp_op)
    n_stack = len(fp_stack)
    print(f"浮点运算指令 (addsd/subsd/mulsd/divsd): {n_op}")
    print(f"浮点栈访问 (movsd/movss ...(%rsp))    : {n_stack}")
    print(f"浮点常量物化 (movabsq)                 : {len(movabs)}")
    if n_op:
        print(f"栈访问 / 运算 比                        : {n_stack / n_op:.2f}x")
        if n_stack / n_op >= 5:
            print("  => 高栈访问比：每个中间结果被强制落栈，浮点驻留有明确收益空间")
        else:
            print("  => 栈访问比低：驻留收益空间有限")


def measure(binpath: str, n: str, rounds: int = 7) -> float:
    subprocess.run([binpath, n], capture_output=True)
    ts = []
    for _ in range(rounds):
        t = time.perf_counter()
        subprocess.run([binpath, n], capture_output=True)
        ts.append(time.perf_counter() - t)
    return min(ts)


def dynamic_stats(dialc: str) -> None:
    with tempfile.TemporaryDirectory(prefix="dialc_fgra_") as td:
        src = Path(td) / "fp.dial"
        src.write_text(FP_WORKLOAD, encoding="utf-8")
        bins = {}
        for backend in ("llvm", "asm"):
            out = Path(td) / f"fp_{backend}"
            p = subprocess.run(
                [dialc, "build", str(src), f"--backend={backend}", "-O2", "-o", str(out)],
                capture_output=True, text=True,
            )
            if p.returncode != 0:
                print(f"{backend} 编译失败: {p.stderr[-500:]}")
                return
            bins[backend] = str(out)
        # 输出一致性校验
        o_llvm = subprocess.run([bins["llvm"], "1000"], capture_output=True).stdout
        o_asm = subprocess.run([bins["asm"], "1000"], capture_output=True).stdout
        if o_llvm != o_asm:
            print(f"警告：双后端输出不一致 ({o_llvm!r} vs {o_asm!r})，计时不可比")
            return
        n = "500000"
        # 交替测，减少系统抖动
        ratios = []
        for _ in range(3):
            lm = measure(bins["llvm"], n)
            am = measure(bins["asm"], n)
            ratios.append(am / lm)
        r = statistics.median(ratios)
        print(f"双后端输出一致 ✅  校验值={o_llvm.decode().strip()}")
        print(f"LLVM min / asm min 交替计时：asm/LLVM 耗时比 = {r:.3f}x")
        if r >= 1.15:
            print(f"  => asm 显著慢于 LLVM ({r:.2f}x)，浮点驻留是追平的关键方向")
        elif r <= 0.85:
            print("  => asm 反而更快，浮点驻留无必要")
        else:
            print("  => 两者基本持平")


def main() -> None:
    ap = argparse.ArgumentParser(description="浮点寄存器分配收益诊断")
    ap.add_argument("--asm", metavar="FILE", help="分析一个已有的 asm .s 文件")
    ap.add_argument("--dialc", metavar="PATH", help="编译器路径，编译自带 workload 做双后端计时")
    args = ap.parse_args()
    if not args.asm and not args.dialc:
        ap.error("至少给出 --asm 或 --dialc 之一")
    if args.asm:
        print(f"=== 静态统计: {args.asm} ===")
        static_stats(args.asm)
    if args.dialc:
        print(f"\n=== 动态计时: {args.dialc} ===")
        dynamic_stats(args.dialc)


if __name__ == "__main__":
    main()
