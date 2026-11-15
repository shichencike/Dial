#!/usr/bin/env python3
"""A/B 对跑：OLD(M7-e3a @3180271) vs NEW(M7-e3b) 自研 asm 后端。

与 bench_asm_vs_llvm.py 同口径（校准重复数 → 扣启动基线 → 多次取最小）。
关键差异：同一任务在 OLD / NEW 两个二进制间**交替采样**，抵消整机噪声
（跨时段绝对读数不可直接相减，以相对改善为准）。

用法：
    python3 tools/ab_asm_m7e3b.py                 # 全量 28 项，每项 5 采样
    python3 tools/ab_asm_m7e3b.py --quick         # 每项 2 采样
    python3 tools/ab_asm_m7e3b.py --tasks b01,b09
"""
from __future__ import annotations

import argparse
import os
import pathlib
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import bench_harness as H  # noqa: E402

TASKS = H.TASKS
CHECKSUMS = H.CHECKSUMS
TARGET_MS = H.TARGET_MS
CALIB_MIN_REP = H.CALIB_MIN_REP
CALIB_MAX_REP = H.CALIB_MAX_REP
FLOOR_MS = H.FLOOR_MS
STARTUP_SAMPLES = H.STARTUP_SAMPLES

OLD = pathlib.Path("/tmp/dialc_old/target/release/dialc")
NEW = ROOT / "target" / "release" / "dialc"


def log(m: str) -> None:
    print(m, flush=True)


def run(argv, env):
    t0 = time.perf_counter()
    p = subprocess.run(argv, capture_output=True, text=True, env=env, timeout=600)
    return time.perf_counter() - t0, p.stdout, p.returncode


def build(dialc, out):
    argv = [str(dialc), "build", str(ROOT / "bench" / "bench.dial"),
            "--backend", "asm", "-o", str(out)]
    env = dict(os.environ)
    env.setdefault("LLVM_SYS_181_PREFIX", "/usr/lib/llvm-18")
    t0 = time.perf_counter()
    p = subprocess.run(argv, capture_output=True, text=True, env=env, timeout=900)
    if p.returncode != 0:
        log(p.stdout[-2000:]); log(p.stderr[-2000:])
        raise RuntimeError(f"{dialc} 编译失败 rc={p.returncode}")
    return (time.perf_counter() - t0) * 1000.0


def measure_startup(exe):
    vals = []
    for _ in range(STARTUP_SAMPLES):
        dt, _o, _r = run([str(exe), "noop", "1"], dict(os.environ))
        vals.append(dt * 1000.0)
    return statistics.median(vals)


def calibrate(exe, tid):
    rep = 1
    dt, _o, _r = run([str(exe), tid, str(rep)], dict(os.environ))
    ms = dt * 1000.0
    for _ in range(12):
        if ms <= 0:
            ms = 0.001
        want = max(CALIB_MIN_REP, int(TARGET_MS / ms * rep))
        want = min(want, CALIB_MAX_REP)
        if want == rep:
            break
        rep = want
        dt, _o, _r = run([str(exe), tid, str(rep)], dict(os.environ))
        ms = dt * 1000.0
    return max(CALIB_MIN_REP, min(rep, CALIB_MAX_REP))


def ab_main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--tasks", default="")
    args = ap.parse_args()

    tasks = args.tasks.split(",") if args.tasks else TASKS
    samples = 2 if args.quick else 5

    tmp = pathlib.Path(tempfile.mkdtemp(prefix="ab_m7e3b_"))
    old_exe = tmp / "bench_old"
    new_exe = tmp / "bench_new"
    log(f"编译 OLD ({OLD.name}) …")
    build(OLD, old_exe)
    log(f"编译 NEW ({NEW.name}) …")
    build(NEW, new_exe)

    startup_old = measure_startup(old_exe)
    startup_new = measure_startup(new_exe)
    log(f"startup: old={startup_old:.3f}ms new={startup_new:.3f}ms")

    log(f"{'task':5} {'old_ms':>10} {'new_ms':>10} {'ratio':>7}  cks")
    ratios = []
    for t in tasks:
        tid = t[0]  # TASKS 为 (name, 标题, 分类, 说明) 四元组
        rep_o = calibrate(old_exe, tid)
        rep_n = calibrate(new_exe, tid)
        best_o = best_n = None
        for _ in range(samples):
            dt, o, rc = run([str(old_exe), tid, str(rep_o)], dict(os.environ))
            co = o.strip().split("\n")[-1].strip()
            if rc == 0 and co == str(CHECKSUMS[tid]):
                best_o = dt * 1000.0 if best_o is None else min(best_o, dt * 1000.0)
            dt, o, rc = run([str(new_exe), tid, str(rep_n)], dict(os.environ))
            co = o.strip().split("\n")[-1].strip()
            if rc == 0 and co == str(CHECKSUMS[tid]):
                best_n = dt * 1000.0 if best_n is None else min(best_n, dt * 1000.0)
        if best_o is None or best_n is None:
            log(f"{tid:5}  ⚠ 校验和失败 old={best_o} new={best_n}")
            continue
        uo = max((best_o - startup_old) / rep_o, FLOOR_MS)
        un = max((best_n - startup_new) / rep_n, FLOOR_MS)
        r = uo / un  # >1 表示新更快
        ratios.append(r)
        log(f"{tid:5} {uo:10.4f} {un:10.4f} {r:7.4f}  ok")

    if ratios:
        geomean = statistics.geometric_mean(ratios)
        log(f"\n几何平均 ratio(old/new) = {geomean:.4f}  （{len(ratios)} 项；>1 新更快）")
        log(f"即 NEW 相对 OLD 约 {geomean-1:+.2%}")
    else:
        log("无有效项")


if __name__ == "__main__":
    ab_main()
