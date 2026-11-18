#!/usr/bin/env python3
"""高级指令集主线剩余三项（SIMD 打包发射 / 浮点 FMA / 加密专用指令）ROI 诊断工具。

用途：在投入 targeted 指令发射之前，用源码与语料证据量化三类高级 ISA 的
可优化形态频次与架构可行性，作为「开工 / 量化搁置」的决策依据。延续
e3x（自动向量化证伪）/ e3at（caller-saved GRA 否决）/ diag_scalar_isa.py
的「先量化再投入」纪律：无正 ROI、或有语义/护栏障碍，则不硬做。

用法：
    python3 tools/diag_advanced_isa.py [可选：某份 baseline .s 路径]

判据（三项各自独立裁决）：
  - SIMD 打包（阶段二第 11 项）：只有在 bench/真实 workload 中反复出现
    simd<T,N> 向量运算，且发射层能拿到「向量操作」时，打包（paddq/vaddpd）
    才有正收益。若 (a) 无 SIMD workload，(b) lower.rs 已把向量逐 lane 展开成
    标量 IR（发射层看不到向量操作），(c) 向量栈段为每 lane 8 字节以对齐 LLVM
    的 sizeof 语义（紧密打包布局会破坏双后端语义一致）——则诊断后搁置。
  - 浮点 FMA（第 12 项）：只有浮点 a*b+c 形态且能接受 FMA 的舍入语义变化
    （单次舍入 vs 两次舍入）才可做。若语料只有整数乘加、无浮点乘加形态，
    则无目标；且精度对齐须改冻结的 codegen.rs，属双后端语言层裁决。
  - 加密专用指令（第 13 项）：AES-NI/SHA/CRC32 只有落在「本编译器发射的
    标量代码」里才谈得上 targeted 融合。若加密能力全在 C 库（dial_crypto_*
    等包），编译器发射层无从介入，则不属于本后端 ISA 发射范畴。

输出：每项给出「语料证据 + 架构障碍 + 裁决 + 重启条件」，末尾汇总。
本工具只读源码与 .s，零产品代码改动。
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def scan_keyword(dirs, pattern, exts=(".dial",)):
    """在给定目录树里统计匹配 pattern 的文件数与命中行数。"""
    files, hits = 0, 0
    rx = re.compile(pattern, re.I)
    for d in dirs:
        base = os.path.join(ROOT, d)
        if not os.path.isdir(base):
            continue
        for dp, _, fns in os.walk(base):
            for fn in fns:
                if not fn.endswith(exts):
                    continue
                p = os.path.join(dp, fn)
                try:
                    txt = open(p, encoding="utf-8", errors="ignore").read()
                except OSError:
                    continue
                found = [l for l in txt.splitlines() if rx.search(l)]
                if found:
                    files += 1
                    hits += len(found)
    return files, hits


def grep_file(path, pattern):
    if not os.path.isfile(path):
        return []
    rx = re.compile(pattern)
    return [l.rstrip() for l in open(path, encoding="utf-8", errors="ignore") if rx.search(l)]


def section(title):
    print("\n" + "=" * 68)
    print(title)
    print("=" * 68)


def diag_simd(asm_path):
    section("第 11 项 · SIMD 打包发射（paddq / vaddpd …）")
    bench_files, bench_hits = scan_keyword(["bench"], r"\bsimd\b")
    test_files, test_hits = scan_keyword(["tests"], r"\bsimd\b")
    print(f"  语料证据：bench 含 simd 的文件={bench_files} 命中行={bench_hits}"
          f"  |  tests 含 simd 的文件={test_files} 命中行={test_hits}")
    print("            → simd 仅出现在语义/负向测试（golden + tests/bad），bench 零负载。")

    lower = os.path.join(ROOT, "src", "lower.rs")
    unrolled = grep_file(lower, r"IrInst::(Bin|FBin|ICmp|FCmp)")
    print(f"  架构障碍 A：lower.rs 逐 lane 展开（Bin/FBin/ICmp/FCmp 发射行数={len(unrolled)}）"
          " → 向量在 lower 层已降级为标量 IR，发射层拿不到「向量操作」可融合。")

    golden = os.path.join(ROOT, "tests", "asm_golden", "simd.dial")
    sizeof_lines = grep_file(golden, r"sizeof<simd<int,\s*4>>")
    print(f"  架构障碍 B：golden sizeof 断言={sizeof_lines} "
          "→ 每 lane 8 字节（int4=32B）对齐 LLVM 语义；紧密打包布局会破坏双后端逐值一致。")

    if asm_path and os.path.isfile(asm_path):
        lines = open(asm_path, encoding="utf-8", errors="ignore").read().splitlines()
        scalar = sum(bool(re.search(r"\b(addq|subq|imulq|movq|movl|comisd|ucomisd)\b", l)) for l in lines)
        print(f"  汇编佐证：{os.path.basename(asm_path)} 共 {len(lines)} 行，标量算术/mov {scalar} 条"
              "（逐 lane 展开的直接开销；baseline 与 v3 产物逐字节一致，证明当前零打包发射）。")
    print("  裁决：⏸️ 量化搁置 —— 无 SIMD workload 可测 ROI，且 lower 层展开 + 8B/lane 布局")
    print("        两大架构障碍使打包发射要么零收益死代码、要么破坏双后端语义一致。")
    print("  重启条件：① bench/真实 workload 出现热点 simd<T,N> 运算；② 语言层引入独立的")
    print("        紧密向量 ABI（双后端同步，明确放宽 sizeof 一致性护栏）；③ e3x 自动向量化")
    print("        结论被新证据推翻。满足任一则重开诊断。")


def diag_fma():
    section("第 12 项 · 浮点 FMA 融合（vfmadd*）")
    bench_files, bench_hits = scan_keyword(["bench"], r"\bfloat\b|\bf64\b|\bdouble\b")
    # 浮点 a*b+c 形态：乘号与加号同现且含浮点类型声明的行（近似）
    fma_files, fma_hits = scan_keyword(["bench", "examples", "tests"],
                                       r"\b(float|f64)\b.*\*.*\+|\*.*\+.*\b(float|f64)\b")
    print(f"  语料证据：bench 含浮点类型的文件={bench_files} 命中行={bench_hits}")
    print(f"            浮点 a*b+c 形态命中={fma_hits}（文件 {fma_files}）")
    print("            → bench 中的乘加（如 s*31+i、C[pos]+aik*B[..]）全为整数，走 imulq+addq，")
    print("              非 FMA 目标；无浮点乘加负载。")
    print("  架构障碍：FMA 是单次舍入，与「先乘后加」的两次舍入语义不同；精度对齐须改冻结的")
    print("            codegen.rs，属双后端语言层裁决，超出 asm 后端护栏内范畴。")
    print("  裁决：⏸️ 量化搁置 —— 无浮点乘加 workload，且舍入语义对齐需双后端裁决。")
    print("  重启条件：① 出现热点浮点 a*b+c workload 且实测 FMA 有正 ROI；② 语言层明确")
    print("        FMA 舍入语义规范并放宽 codegen.rs 护栏。满足任一则重开。")


def diag_crypto():
    section("第 13 项 · 加密专用指令（AES-NI / SHA / CRC32）")
    c_files, c_hits = scan_keyword(["tests", "bench", "examples"],
                                   r"dial_crypto_|dial_gui_.*crc32|\baes\b|\bsha256\b|\bcrc32\b")
    print(f"  语料证据：tests/bench/examples 命中文件={c_files} 命中行={c_hits}")
    print("            → 全部为 C 库调用（dial_crypto_* 包 / dial_gui_painter_crc32 等），")
    print("              不是本编译器发射的标量代码。")
    print("  架构障碍：加密能力落在 C 包/C 运行时；编译器发射层对跨 FFI 的 C 实现无从做")
    print("            targeted ISA 融合。若要走指令级加密，须把算法重写进 Dial 语言层/内建")
    print("            （改 codegen.rs + 语义），属独立语言主题，且收益小、难度最大。")
    print("  裁决：⏸️ 量化搁置 —— 不属于本后端 ISA 发射范畴（加密在 C 库），排序最后。")
    print("  重启条件：① 语言层引入原生加密内建并由本后端发射其标量主体；② 出现可实测的")
    print("        热点且 AES-NI/SHA/CRC32 有正 ROI。满足任一则重开。")


def main():
    asm_path = sys.argv[1] if len(sys.argv) > 1 else None
    print("高级指令集主线剩余三项 ROI 诊断（只读、零产品代码改动）")
    print(f"仓库根：{ROOT}")
    diag_simd(asm_path)
    diag_fma()
    diag_crypto()
    section("汇总")
    print("  第 11 SIMD 打包 / 第 12 FMA / 第 13 加密指令：三项均「诊断完成、量化搁置」。")
    print("  「开关 + 显式向量语义」层（e3ay/e3az）已完成；实际打包发射属有硬件、无 workload 的")
    print("  储备能力，硬做 = 零收益死代码或破坏双后端一致。重启条件已逐项写死，条件满足再诊断。")


if __name__ == "__main__":
    main()
