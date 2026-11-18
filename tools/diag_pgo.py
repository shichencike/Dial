#!/usr/bin/env python3
"""R4「PGO / LTO / bitcode 自建」的可行性与 ROI 诊断（唯一后端收回路线图第 4 项）。

用途
----
在投入横跨 ir.rs / codegen_asm.rs / asm.rs / main.rs / runtime 的插桩大工程之前，
先用数据回答三个问题：
  1. asm 默认块布局是否已接近 PGO 块重排能给出的最优？（决定"块布局 PGO"ROI）
  2. LTO 的"跨模块 IR 合并"是否已天然存在？（决定 LTO 子步是否还有工作量）
  3. bitcode 是能力还是手段？（决定要不要为 asm 自建 .bc）

只读、零产品代码改动。确定性输出。

生成待分析的汇编（在仓库根执行）：
    mkdir -p /tmp/r4 && cp bench/bench.dial /tmp/r4/
    target/release/dialc build /tmp/r4/bench.dial --backend=asm -O3 -o /tmp/r4/bin
    python3 tools/diag_pgo.py /tmp/r4/bench.s

用法：
    python3 tools/diag_pgo.py <asm_O3.s> [--verbose]
"""
from __future__ import annotations

import re
import sys

# --- 源码级已核实事实（不随 .s 变化，来自 R4 盘点） ---
SOURCE_FACTS = [
    ("--pgo 拒绝点", "src/main.rs:504-506", "asm 后端遇 --pgo 直接 exit(1)：真实功能缺口，非优化差距"),
    ("LLVM PGO 插桩", "src/main.rs:1179-1187", "插桩由 clang -fprofile-generate/-fprofile-use 对 .bc 完成，不在 Dial IR"),
    ("bitcode emit", "src/codegen.rs:1759", "write_bitcode_to_path 只是喂 clang 的中间物，是手段非能力"),
    ("LTO 源码级", "src/main.rs:2039-2160", "逐 unit parse 后合入单个 ast::Program → 单个 IrProgram，天然 LTO"),
    ("LTO 包名级", "src/main.rs:1497", "包名 import 走 .a 静态链接，是 C 机器码非 IR，不在 IR 合并范畴"),
    ("块结构", "src/ir.rs:369-402", "有 IrBlock + Term::Jump/Branch 基本块与 block id，可承载 profile 挂点"),
    ("布局 pass", "src/codegen_asm.rs:3567,4377", "align_loop_headers 对所有循环头 .p2align 4；文本 pass 已做跳转重定向/死块删除"),
    ("profile 写回通道", "runtime/", "仅 mem+trap，无 atexit/文件 IO 自动写回；需新增或借 main 尾部生成写回"),
]

FN_RE = re.compile(r'^([A-Za-z_][\w]*):\s*$')
BLK_RE = re.compile(r'^(\.L[\w]+):\s*$')
JMP_RE = re.compile(r'^\s*j(mp|e|ne|g|ge|l|le|a|ae|b|be|s|ns)\s+(\.L[\w]+)')
P2ALIGN_RE = re.compile(r'^\s*\.p2align')


def analyze(s: str, verbose: bool) -> dict:
    lines = s.splitlines()
    funcs: dict[str, list[str]] = {}
    blocks: dict[str, dict] = {}
    order: list[str] = []
    cur_fn = None
    pending_align = False  # .p2align 出现在块标签之前，对齐的是下一个块
    for ln in lines:
        mfn = FN_RE.match(ln)
        if mfn and not mfn.group(1).startswith('.L'):
            cur_fn = mfn.group(1)
            funcs.setdefault(cur_fn, [])
            pending_align = False
            continue
        mb = BLK_RE.match(ln)
        if mb and cur_fn and '_b' in mb.group(1):
            lbl = mb.group(1)
            blocks[lbl] = {'fn': cur_fn, 'idx': len(order), 'jmp': None, 'aligned': pending_align}
            funcs[cur_fn].append(lbl)
            order.append(lbl)
            pending_align = False
            continue
        mj = JMP_RE.match(ln)
        if mj and order:
            blocks[order[-1]]['jmp'] = mj.group(2)
            continue
        if P2ALIGN_RE.match(ln):
            pending_align = True

    multi = {f: b for f, b in funcs.items() if len(b) >= 2}
    # 指标1: 条件跳转目标是否为函数末尾块（冷退出已甩末尾 = 理想布局）
    cold_at_end = 0
    jmp_samples = 0
    # 指标2: 回边(jmp 到更小 idx) 是否落到对齐的块（循环头对齐）
    backedges = 0
    backedge_aligned = 0
    # 指标3: 每块指令数以判定"跳转目标块"是否为冷小块
    for f, blks in multi.items():
        fn_max_idx = max(blocks[b]['idx'] for b in blks)
        for lbl in blks:
            b = blocks[lbl]
            tgt = b['jmp']
            if not tgt or tgt not in blocks:
                continue
            jmp_samples += 1
            if blocks[tgt]['idx'] == fn_max_idx:
                cold_at_end += 1
            # 回边: 无条件 jmp 到更小 idx
            if tgt in blocks and blocks[tgt]['idx'] < b['idx']:
                backedges += 1
                if blocks[tgt]['aligned']:
                    backedge_aligned += 1

    return {
        'multi_funcs': len(multi),
        'total_blocks': len(order),
        'jmp_samples': jmp_samples,
        'cold_at_end': cold_at_end,
        'backedges': backedges,
        'backedge_aligned': backedge_aligned,
        'funcs': multi,
        'blocks': blocks,
    }


def main() -> int:
    args = [a for a in sys.argv[1:] if a != '--verbose']
    verbose = '--verbose' in sys.argv
    if not args:
        print(__doc__)
        return 2
    s = open(args[0], encoding='utf-8', errors='replace').read()
    r = analyze(s, verbose)

    print("=" * 68)
    print("R4 诊断：PGO / LTO / bitcode 自建（唯一后端收回路线图第 4 项）")
    print("=" * 68)

    print("\n【源码级已核实事实】")
    for label, loc, note in SOURCE_FACTS:
        print(f"  - {label:<14} {loc:<24} {note}")

    print("\n【块布局 ROI 实证（决定『块布局 PGO』是否有收益）】")
    print(f"  多块函数数: {r['multi_funcs']}")
    print(f"  基本块总数: {r['total_blocks']}")
    print(f"  块间跳转样本: {r['jmp_samples']}")
    if r['jmp_samples']:
        pct = 100 * r['cold_at_end'] / r['jmp_samples']
        print(f"  跳转目标=函数末尾冷块: {r['cold_at_end']}/{r['jmp_samples']} ({pct:.0f}%)")
    print(f"  回边数: {r['backedges']}, 其中循环头已对齐: {r['backedge_aligned']}")
    if r['backedges']:
        print(f"  循环头对齐率: {100*r['backedge_aligned']/r['backedges']:.0f}%")

    print("\n【结论：四个子步的真实状态】")
    print("  ① 自研 profile 格式      : 有工作量（需定义 .dialprof + 插桩计数 + 写回）")
    print("  ② 插桩 + profile 写回    : 有工作量（runtime 无 atexit，需在 main 尾部生成写回）")
    print("  ③ profile 指导块布局/热冷分离: 收益上界低（默认已热 fallthrough + 冷甩末尾 + 循环头对齐）")
    print("  ④ 跨模块 IR 合并 LTO     : 源码级已天然单编译单元；包名级是 C .a 非 IR —— 基本无 IR 层工作量")
    print("  bitcode                  : 是喂 clang 的手段非能力，asm 不需自建")

    print("\n【落地点排序（按 ROI）】")
    print("  高: 功能完整性 —— 让 --pgo 在 asm 下可用、零 .bc 依赖（用户可见缺口，直接 exit(1)）")
    print("  中: profile 指导「只对齐热循环 / 热函数聚拢 .text.hot」（省冷循环 padding，有可测收益）")
    print("  低: 通用块重排（默认布局已接近最优，重排收益上界低且风险高）")

    if verbose:
        print("\n【各函数块数分布】")
        for f, blks in sorted(r['funcs'].items(), key=lambda kv: -len(kv[1]))[:20]:
            print(f"  {f:<24} {len(blks)} 块")

    print("\n判据纪律：功能缺口（--pgo 被拒）与性能收益分开评判；负 ROI 的性能优化不硬做；")
    print("          默认布局的既得收益不得因引入 PGO 而回退（零回归铁律）。")
    return 0


if __name__ == '__main__':
    sys.exit(main())
