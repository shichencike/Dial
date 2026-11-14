#!/usr/bin/env python3
"""自研后端（--backend=asm）vs LLVM 后端 性能对跑器。

目标
----
把「自研后端能不能比 LLVM 后端快」变成一个**可复现、可回归**的数字，
而不是靠感觉判断。测法沿用 tools/bench_harness.py 的口径（同一批
28 项跨语言等价微基准 + 内嵌校验和锚点 + 校准重复数 + 扣启动基线 +
多次取最小），因此结果与既有 bench 网页的数据可对照。

公平性铁律
----------
* **同一份源码**：只有 `--backend` 一个参数不同，其余编译选项完全一致。
* **校验和锚点**：每项任务在两个后端都必须算出与真值表相同的十进制校验和，
  否则立即失败——保证比的是**等价计算**，而不是「一边算错所以更快」。
* **同口径计时**：校准重复数 → 扣启动基线 → 多次取最小，与 harness 完全一致。

用法
----
    python3 tools/bench_asm_vs_llvm.py                    # 全量 28 项
    python3 tools/bench_asm_vs_llvm.py --quick            # 快速模式（每项 2 采样）
    python3 tools/bench_asm_vs_llvm.py --tasks b01,b09     # 只测指定项
    python3 tools/bench_asm_vs_llvm.py --backend-flags "--fast"

退出码：0 = 全部通过（含校验和）；1 = 有失败项。
"""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import platform
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench"

# ---------------------------------------------------------------------------
# 28 项元数据与校验和：与 tools/bench_harness.py **同一份真相源**。
# 直接 import 复用，避免两处各写一份导致漂移。
# ---------------------------------------------------------------------------
sys.path.insert(0, str(ROOT / "tools"))
import bench_harness as H  # noqa: E402

TASKS = H.TASKS
CHECKSUMS = H.CHECKSUMS
TARGET_MS = H.TARGET_MS
CALIB_MIN_REP = H.CALIB_MIN_REP
CALIB_MAX_REP = H.CALIB_MAX_REP
FLOOR_MS = H.FLOOR_MS
STARTUP_SAMPLES = H.STARTUP_SAMPLES


def log(msg: str) -> None:
    print(msg, flush=True)


def run_capture(argv: list[str], env: dict | None = None, cwd: str | None = None):
    """返回 (墙钟秒, stdout 文本, returncode)。"""
    t0 = time.perf_counter()
    p = subprocess.run(argv, capture_output=True, text=True,
                       env=env, cwd=cwd, timeout=600)
    dt = time.perf_counter() - t0
    return dt, p.stdout, p.returncode


class Backend:
    """一个后端的可执行产物 + 其运行入口。"""

    def __init__(self, name: str, argv_fn, artifact: pathlib.Path,
                 compile_ms: float, flags: str, env: dict):
        self.name = name
        self.argv_fn = argv_fn
        self.artifact = artifact
        self.compile_ms = compile_ms
        self.flags = flags
        self.env = env
        self.startup_ms = 0.0
        self.size_bytes = 0

    def run(self, tid: str, rep: int):
        return run_capture(self.argv_fn(tid, rep), env=self.env)


def build_backend(dialc: pathlib.Path, tmp: pathlib.Path, backend: str,
                  extra_flags: list[str]) -> Backend:
    """编译一份 bench.dial 到指定后端。"""
    out = tmp / f"bench_{backend}"
    argv = [str(dialc), "build", str(BENCH / "bench.dial"),
            f"--backend={backend}", "-o", str(out)] + extra_flags
    log(f"编译 {backend}: {' '.join(argv)}")
    env = dict(os.environ)
    env.setdefault("LLVM_SYS_181_PREFIX", "/usr/lib/llvm-18")
    t0 = time.perf_counter()
    p = subprocess.run(argv, capture_output=True, text=True, env=env, timeout=900)
    c_ms = (time.perf_counter() - t0) * 1000.0
    if p.returncode != 0:
        log(p.stdout[-2000:])
        log(p.stderr[-2000:])
        raise RuntimeError(f"{backend} 后端编译失败 (rc={p.returncode})")
    size = out.stat().st_size if out.exists() else 0
    log(f"  {backend}: 编译 {c_ms:.1f} ms，产物 {size} 字节")
    return Backend(backend, lambda t, r: [str(out), t, str(r)], out,
                   c_ms, " ".join(extra_flags) or "（默认优化）", env)


def verify_checksums(bk: Backend, tid: str, rep: int = 1) -> None:
    """校验和必须与真值表一致——保证比的是等价计算。"""
    _dt, out, rc = bk.run(tid, rep)
    got = out.strip().split("\n")[-1].strip()
    if rc != 0 or got != str(CHECKSUMS[tid]):
        raise RuntimeError(
            f"{bk.name}/{tid}: 期望校验和 {CHECKSUMS[tid]} 实得 {got!r} (rc={rc})")


def measure_startup(bk: Backend) -> None:
    """noop 任务的进程墙钟（含运行时初始化），采中位数。"""
    vals = []
    for _ in range(STARTUP_SAMPLES):
        dt, _out, _rc = bk.run("noop", 1)
        vals.append(dt * 1000.0)
    bk.startup_ms = statistics.median(vals)


def calibrate(bk: Backend, tid: str) -> int:
    """找到使单次进程运行接近 TARGET_MS 的重复数（与 harness 同口径）。"""
    rep = 1
    dt, _out, _rc = bk.run(tid, rep)
    ms = dt * 1000.0
    for _ in range(12):
        if ms <= 0:
            ms = 0.001
        want = max(CALIB_MIN_REP, int(TARGET_MS / ms * rep))
        want = min(want, CALIB_MAX_REP)
        if want == rep:
            break
        rep = want
        dt, _out, _rc = bk.run(tid, rep)
        ms = dt * 1000.0
    return max(CALIB_MIN_REP, min(rep, CALIB_MAX_REP))


def measure(bk: Backend, tid: str, samples: int):
    """返回单次任务的净耗时（毫秒，已扣启动基线）。"""
    rep = calibrate(bk, tid)
    best = None
    for _ in range(samples):
        dt, out, rc = bk.run(tid, rep)
        got = out.strip().split("\n")[-1].strip()
        if got != str(CHECKSUMS[tid]):
            raise RuntimeError(f"{bk.name}/{tid} rep={rep} 校验和漂移: {got!r}")
        total = dt * 1000.0
        if best is None or total < best:
            best = total
    unit = (best - bk.startup_ms) / rep
    return max(unit, FLOOR_MS), rep


def main() -> int:
    ap = argparse.ArgumentParser(description="asm 后端 vs LLVM 后端 性能对跑")
    ap.add_argument("--dialc", default=str(ROOT / "target" / "release" / "dialc"))
    ap.add_argument("--quick", action="store_true", help="每项 2 采样（快速）")
    ap.add_argument("--tasks", default="", help="只测指定任务（逗号分隔，如 b01,b09）")
    ap.add_argument("--backend-flags", default="", help="额外的编译选项（空格分隔）")
    ap.add_argument("--json", default="", help="把结果写入该 JSON 文件")
    args = ap.parse_args()

    dialc = pathlib.Path(args.dialc)
    if not dialc.exists():
        log(f"❌ 找不到 dialc: {dialc}")
        return 1
    extra_flags = args.backend_flags.split() if args.backend_flags else []

    tasks = TASKS
    if args.tasks:
        want = {t.strip() for t in args.tasks.split(",") if t.strip()}
        tasks = [t for t in TASKS if t[0] in want]
        if not tasks:
            log(f"❌ 没有匹配的任务: {args.tasks}")
            return 1

    samples = 2 if args.quick else H.SAMPLES

    with tempfile.TemporaryDirectory(prefix="dialc-asm-vs-llvm-") as td:
        tmp = pathlib.Path(td)
        log("== 编译两个后端（除 --backend 外其余选项完全一致）==")
        bk_asm = build_backend(dialc, tmp, "asm", extra_flags)
        bk_llvm = build_backend(dialc, tmp, "llvm", extra_flags)

        log("== 校验 28 项校验和锚点（保证比的是等价计算）==")
        for tid, *_ in tasks:
            verify_checksums(bk_asm, tid)
            verify_checksums(bk_llvm, tid)
        log(f"  ✅ {len(tasks)}×2 校验和全部一致")

        log("== 采集启动基线 ==")
        measure_startup(bk_asm)
        measure_startup(bk_llvm)
        log(f"  asm:  {bk_asm.startup_ms:.3f} ms")
        log(f"  llvm: {bk_llvm.startup_ms:.3f} ms")

        log("== 逐项测量 ==")
        rows = []
        for tid, tname, cat, _desc in tasks:
            a_ms, a_rep = measure(bk_asm, tid, samples)
            l_ms, l_rep = measure(bk_llvm, tid, samples)
            ratio = a_ms / l_ms if l_ms > 0 else float("inf")
            rows.append(dict(task=tid, name=tname, category=cat,
                             asm_ms=a_ms, llvm_ms=l_ms, ratio=ratio,
                             asm_rep=a_rep, llvm_rep=l_rep))
            mark = "🟢 快" if ratio < 0.95 else ("🔴 慢" if ratio > 1.05 else "⚪ 持平")
            log(f"  {tid} {tname:<18} [{cat}] asm={a_ms:8.3f}ms "
                f"llvm={l_ms:8.3f}ms  比值={ratio:5.3f}x {mark}")

        # 汇总
        log("")
        log("== 汇总 ==")
        wins = sum(1 for r in rows if r["ratio"] < 0.95)
        ties = sum(1 for r in rows if 0.95 <= r["ratio"] <= 1.05)
        losses = sum(1 for r in rows if r["ratio"] > 1.05)
        # 几何平均比值：各项比值的几何平均（对数尺度上的平均倍率）
        import math
        gm = math.exp(sum(math.log(r["ratio"]) for r in rows) / len(rows))
        # 总耗时比：两后端各项净耗时之和的比（更贴近「跑完全套要多久」）
        total_a = sum(r["asm_ms"] for r in rows)
        total_l = sum(r["llvm_ms"] for r in rows)
        log(f"  逐项胜/平/负：{wins} / {ties} / {losses}（共 {len(rows)} 项）")
        log(f"  几何平均比值（asm/llvm）：{gm:.4f}x  "
            f"（<1 表示 asm 更快）")
        log(f"  总净耗时：asm {total_a:.3f} ms vs llvm {total_l:.3f} ms "
            f"= {total_a / total_l:.4f}x")
        log(f"  产物大小：asm {bk_asm.size_bytes} B vs llvm {bk_llvm.size_bytes} B")
        log(f"  启动耗时：asm {bk_asm.startup_ms:.3f} ms vs llvm {bk_llvm.startup_ms:.3f} ms")

        # 按类别汇总，定位短板
        log("")
        log("== 按类别 ==")
        cats = {}
        for r in rows:
            cats.setdefault(r["category"], []).append(r["ratio"])
        for cat, rs in cats.items():
            cgm = math.exp(sum(math.log(x) for x in rs) / len(rs))
            log(f"  {cat:<12} 几何平均 {cgm:.4f}x  （{len(rs)} 项）")

        if args.json:
            out = dict(
                host=platform.platform(),
                tasks=[dict(task=t[0], name=t[1], category=t[2], desc=t[3]) for t in tasks],
                rows=rows,
                summary=dict(wins=wins, ties=ties, losses=losses,
                             geomean_ratio=gm,
                             total_asm_ms=total_a, total_llvm_ms=total_l,
                             total_ratio=total_a / total_l,
                             asm_size=bk_asm.size_bytes, llvm_size=bk_llvm.size_bytes,
                             asm_startup_ms=bk_asm.startup_ms,
                             llvm_startup_ms=bk_llvm.startup_ms),
            )
            pathlib.Path(args.json).write_text(
                json.dumps(out, ensure_ascii=False, indent=2), encoding="utf-8")
            log(f"\n结果已写入 {args.json}")

    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RuntimeError as e:
        log(f"❌ {e}")
        sys.exit(1)
