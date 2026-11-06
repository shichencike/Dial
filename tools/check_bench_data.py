#!/usr/bin/env python3
"""校验性能基准数据的完整性，以及网页是否与数据同步。

CI 与本地通用。检查：
  * data.json 结构：28 项 × 4 语言、6 大工程维度、6 雷达轴；
  * 每项 4 语言校验和都与采集器内嵌真值表一致（等价计算的锚点）；
  * 绝对值恒为正、比值以 C=1.00 为基准、雷达得分每语言 6 个；
  * （可选）网页包含全部校验和、维度标签、雷达 SVG 与 4 语言多边形，
    即 web/bench.html 与 bench/data.json 是配套生成的。

用法：
    python3 tools/check_bench_data.py                       # 校验仓库交付数据+网页
    python3 tools/check_bench_data.py --data /tmp/x.json --html /tmp/x.html
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LANGS = ["C", "Go", "Dial", "Python"]
DIM_KEYS = ["speed", "memory", "compile", "startup", "binary", "loc"]


def load_checksums() -> dict:
    """从采集器复用唯一真相源 CHECKSUMS，避免两处硬编码漂移。"""
    spec = importlib.util.spec_from_file_location(
        "bench_harness", ROOT / "tools" / "bench_harness.py")
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m.CHECKSUMS, m.TASKS


def check(data: dict, html_text: str | None) -> list[str]:
    errs: list[str] = []
    checksums, tasks = load_checksums()

    meta = data.get("meta", {})
    if meta.get("langs") != LANGS:
        errs.append(f"meta.langs 不符: {meta.get('langs')}")
    if meta.get("task_count") != 28:
        errs.append(f"meta.task_count 应为 28，实为 {meta.get('task_count')}")

    benches = data.get("benchmarks", [])
    if len(benches) != 28:
        errs.append(f"benchmarks 应为 28 项，实为 {len(benches)}")

    ids = [t[0] for t in tasks]
    seen = set()
    for b in benches:
        tid = b.get("id")
        seen.add(tid)
        if tid not in checksums:
            errs.append(f"未知任务 id: {tid}")
            continue
        if b.get("checksum") != checksums[tid]:
            errs.append(f"{tid} 校验和错误: {b.get('checksum')} != {checksums[tid]}")
        a = b.get("abs_ms", {})
        r = b.get("ratio", {})
        for n in LANGS:
            if n not in a or a[n] <= 0:
                errs.append(f"{tid}/{n} 绝对值缺失或非正: {a.get(n)}")
            if n not in r or r[n] <= 0:
                errs.append(f"{tid}/{n} 比值缺失或非正: {r.get(n)}")
        if abs(r.get("C", 0) - 1.0) > 1e-6:
            errs.append(f"{tid} C 比值基准应为 1.00，实为 {r.get('C')}")
    for tid in ids:
        if tid not in seen:
            errs.append(f"缺少任务 {tid}")

    dims = data.get("dimensions", {})
    for k in DIM_KEYS:
        if k not in dims:
            errs.append(f"缺少工程维度 {k}")
        elif set(dims[k].get("abs", {})) != set(LANGS):
            errs.append(f"维度 {k} 缺少语言绝对值")

    radar = data.get("radar", {})
    if len(radar.get("axes", [])) != 6:
        errs.append("雷达图轴数应为 6")
    if len(radar.get("keys", [])) != 6:
        errs.append("雷达图 keys 应为 6")
    for n in LANGS:
        sc = radar.get("score", {}).get(n, [])
        if len(sc) != 6:
            errs.append(f"{n} 雷达得分应有 6 个，实为 {len(sc)}")

    # 网页同步性
    if html_text is not None:
        for b in benches:
            if str(b.get("checksum")) not in html_text:
                errs.append(f"网页缺少 {b.get('id')} 的校验和 {b.get('checksum')}")
        for k in DIM_KEYS:
            label = dims.get(k, {}).get("label", k)
            if label not in html_text:
                errs.append(f"网页缺少维度标签 {label}")
        if "<svg" not in html_text:
            errs.append("网页缺少雷达图 SVG")
        if html_text.count("<polygon") < 8:  # 4 网格 + 4 语言
            errs.append("网页雷达图多边形不足（应≥8：4 网格 + 4 语言）")
        for n in LANGS:
            color = meta.get("colors", {}).get(n)
            if color and color not in html_text:
                errs.append(f"网页缺少语言 {n} 的配色 {color}")
        if "C = 1.00" not in html_text:
            errs.append("网页缺少比值基准说明")

    return errs


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=str(ROOT / "bench" / "data.json"))
    ap.add_argument("--html", default=str(ROOT / "web" / "bench.html"))
    ap.add_argument("--no-html", action="store_true", help="只校验数据，不校验网页")
    args = ap.parse_args()

    dp = pathlib.Path(args.data)
    if not dp.exists():
        print(f"❌ 数据文件不存在: {dp}")
        return 1
    data = json.loads(dp.read_text(encoding="utf-8"))
    html_text = None
    if not args.no_html:
        hp = pathlib.Path(args.html)
        if not hp.exists():
            print(f"❌ 网页不存在: {hp}")
            return 1
        html_text = hp.read_text(encoding="utf-8")

    errs = check(data, html_text)
    if errs:
        print(f"❌ 性能数据校验失败（{len(errs)} 处）：")
        for e in errs[:40]:
            print("   -", e)
        return 1
    print(f"✅ 性能数据校验通过：{len(data['benchmarks'])} 项 × {len(LANGS)} 语言 / "
          f"6 工程维度 / 6 雷达轴 / 校验和全部一致"
          + ("" if html_text is None else " / 网页与数据同步"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
