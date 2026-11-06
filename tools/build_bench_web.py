#!/usr/bin/env python3
"""由 bench/data.json 生成独立性能数据网页 web/bench.html。

设计
----
* 纯静态 HTML：所有表格与六维雷达图都在生成时渲染完成，不依赖 JavaScript
  或任何外部 CDN，双击 file:// 也能完整查看（离线可复现）。
* 复用官网 assets/style.css 保持视觉一致；补充样式内联，避免改动官网样式表。
* 本文件**只生成 web/bench.html**，不触碰 build_web.py 负责的
  index/spec/changelog/assets，因此 CI 的「官网产物同步校验」不受影响。
* 4 个板块：概览与口径 → 六维工程维度 → 28 项绝对值/绝对比值明细 →
  底部六维雷达图（4 语言叠加）。

用法：
    python3 tools/build_bench_web.py [--data bench/data.json] [--out web/bench.html]
"""
from __future__ import annotations

import argparse
import datetime
import html
import json
import math
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent


def esc(s) -> str:
    return html.escape(str(s), quote=True)


def fmt(v: float) -> str:
    """绝对值格式化：小于 1 用 3 位小数，否则 2 位。"""
    if v < 1:
        return f"{v:.3f}"
    if v < 100:
        return f"{v:.2f}"
    return f"{v:.1f}"


def fmt_ratio(v: float) -> str:
    return f"{v:.2f}×"


# ---------------------------------------------------------------------------
# 六维雷达图：手写 SVG，6 轴等分 60°，4 语言多边形叠加
# ---------------------------------------------------------------------------
def radar_svg(axes, score, colors, langs) -> str:
    cx, cy, R = 320, 300, 210
    n = len(axes)
    rings = [0.25, 0.5, 0.75, 1.0]

    def pt(i: int, frac: float):
        ang = -math.pi / 2 + 2 * math.pi * i / n
        return cx + R * frac * math.cos(ang), cy + R * frac * math.sin(ang)

    parts = [
        f'<svg viewBox="0 0 {2*cx} {2*cy}" width="100%" '
        f'style="max-width:640px;height:auto;display:block;margin:0 auto" '
        f'xmlns="http://www.w3.org/2000/svg" role="img" '
        f'aria-label="C / Python / Go / Dial 六维工程能力雷达图">'
    ]
    parts.append('<defs><style>'
                 '.rg{stroke:#3a4152;stroke-width:1;fill:none}'
                 '.rax{stroke:#3a4152;stroke-width:1}'
                 '.rlb{fill:#c6cede;font-size:15px;font-weight:600}'
                 '.rlv{fill:#7d8798;font-size:11px}'
                 '.rpl{fill-opacity:0.10;stroke-width:2.2;stroke-linejoin:round}'
                 '</style></defs>')
    parts.append(f'<rect x="0" y="0" width="{2*cx}" height="{2*cy}" fill="transparent"/>')

    # 同心网格（多边形，不是圆，更贴合雷达图语义）
    for f in rings:
        pts = " ".join(f"{x:.1f},{y:.1f}" for x, y in (pt(i, f) for i in range(n)))
        parts.append(f'<polygon class="rg" points="{pts}"/>')
        # 环刻度标签
        lx, ly = pt(0, f)
        parts.append(f'<text class="rlv" x="{lx+4:.1f}" y="{ly-2:.1f}">{int(f*100)}</text>')

    # 轴线 + 轴标签
    for i, ax in enumerate(axes):
        x, y = pt(i, 1.0)
        parts.append(f'<line class="rax" x1="{cx}" y1="{cy}" x2="{x:.1f}" y2="{y:.1f}"/>')
        lx, ly = pt(i, 1.16)
        # 依据方位调整锚点，避免文字压线
        ang = -math.pi / 2 + 2 * math.pi * i / n
        ca = math.cos(ang)
        anchor = "middle" if abs(ca) < 0.3 else ("start" if ca > 0 else "end")
        dy = 5 if abs(ca) < 0.3 else (0 if ca > 0 else 0)
        parts.append(f'<text class="rlb" x="{lx:.1f}" y="{ly+dy:.1f}" '
                     f'text-anchor="{anchor}">{esc(ax)}</text>')

    # 4 语言多边形：Dial 最后画（在最上层），并加粗描边突出主角
    order = [l for l in langs if l != "Dial"] + ["Dial"]
    for name in order:
        sc = score[name]
        pts = " ".join(
            f"{x:.1f},{y:.1f}"
            for x, y in (pt(i, max(sc[i], 1.0) / 100.0) for i in range(n))
        )
        c = colors[name]
        sw = 3.0 if name == "Dial" else 2.2
        parts.append(f'<polygon class="rpl" points="{pts}" fill="{c}" stroke="{c}" '
                     f'stroke-width="{sw}"/>')
        # 顶点小圆
        for i in range(n):
            x, y = pt(i, max(sc[i], 1.0) / 100.0)
            r = 4 if name == "Dial" else 3
            parts.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r}" fill="{c}" '
                         f'stroke="#0f1117" stroke-width="1"/>')
    parts.append('</svg>')
    return "\n".join(parts)


def legend(langs, colors) -> str:
    items = []
    for n in langs:
        items.append(
            f'<span style="display:inline-flex;align-items:center;gap:7px;margin:4px 16px 4px 0">'
            f'<span style="width:16px;height:16px;border-radius:4px;background:{colors[n]};'
            f'border:1px solid {colors[n]};display:inline-block"></span>'
            f'<b style="color:var(--fg)">{esc(n)}</b></span>')
    return '<div style="display:flex;flex-wrap:wrap;justify-content:center;margin:6px 0 0">' + \
        "".join(items) + "</div>"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=str(ROOT / "bench" / "data.json"))
    ap.add_argument("--out", default=str(ROOT / "web" / "bench.html"))
    args = ap.parse_args()

    dp = pathlib.Path(args.data)
    if not dp.exists():
        raise SystemExit(f"❌ 找不到数据文件 {dp}；先运行 tools/bench_harness.py")
    data = json.loads(dp.read_text(encoding="utf-8"))

    meta = data["meta"]
    langs = meta["langs"]
    colors = meta["colors"]
    host = meta["host"]
    benches = data["benchmarks"]
    dims = data["dimensions"]
    radar = data["radar"]
    axes = radar["axes"]
    score = radar["score"]

    # 各语言表头色
    def th_lang(n):
        return f'<th style="color:{colors[n]};border-bottom:2px solid {colors[n]}">{esc(n)}</th>'

    # ---------- 概览 KPI ----------
    total_abs = sum(dims["speed"]["abs"].values())
    kpis = [
        (str(meta["task_count"]), "对比维度"),
        (str(len(langs)), "对比语言"),
        ("112/112", "校验和一致"),
        ("C = 1.00×", "比值基准"),
    ]
    kpi_html = "".join(
        f'<div class="kpi"><b>{esc(b)}</b><span>{esc(s)}</span></div>'
        for b, s in kpis)

    # ---------- 环境信息 ----------
    env_rows = "".join(
        f"<tr><td>{esc(k)}</td><td><code>{esc(v)}</code></td></tr>"
        for k, v in host.items())
    flag_rows = "".join(
        f'<tr><td style="color:{colors[n]};font-weight:600">{esc(n)}</td>'
        f'<td><code>{esc(meta["flags"][n])}</code></td></tr>'
        for n in langs)

    # ---------- 六维工程维度表 ----------
    dim_head = '<tr><th>工程维度</th><th>单位 / 口径</th>' + \
        "".join(th_lang(n) for n in langs) + "<th>说明</th></tr>"
    dim_body = []
    for k, d in dims.items():
        vals = d["abs"]
        best = min(vals.values())
        cells = []
        for n in langs:
            v = vals[n]
            is_best = (v == best)
            style = "color:var(--accent);font-weight:700" if is_best else ""
            mark = " ★" if is_best else ""
            cells.append(f'<td style="{style}">{fmt(v)}{mark}</td>')
        dim_body.append(
            f'<tr><td><b>{esc(d["label"])}</b></td>'
            f'<td class="muted" style="font-size:12.5px">{esc(d["unit"])}</td>'
            + "".join(cells) +
            f'<td class="muted" style="font-size:12.5px">{esc(d["note"])}</td></tr>')
    dim_table = f'<table class="bt"><thead>{dim_head}</thead><tbody>{"".join(dim_body)}</tbody></table>'

    # ---------- 28 项绝对值 + 比值明细表 ----------
    abs_head = '<tr><th>#</th><th>维度</th><th>分类</th>' + \
        "".join(th_lang(n) for n in langs) + "<th>校验和</th></tr>"
    ratio_head = '<tr><th>#</th><th>维度</th><th>分类</th>' + \
        "".join(th_lang(n) for n in langs) + "</tr>"
    floor = meta.get("floor_ms", 0.005)
    floor_tag = '<sup style="color:#f7768e;font-size:10px">†</sup>'
    abs_body, ratio_body = [], []
    for b in benches:
        a = b["abs_ms"]
        r = b["ratio"]
        abest = min(a.values())
        rbest = min(r.values())
        acells, rcells = [], []
        for n in langs:
            is_floor = a[n] <= floor * 1.0001  # 触及计时下限/被编译器折叠
            ac = "color:var(--accent);font-weight:700" if a[n] == abest else ""
            rc = "color:var(--accent);font-weight:700" if r[n] == rbest else ""
            amark = " ★" if a[n] == abest else ""
            rmark = " ★" if r[n] == rbest else ""
            ft = floor_tag if is_floor else ""
            acells.append(f'<td style="{ac}">{fmt(a[n])}{amark}{ft}</td>')
            rcells.append(f'<td style="{rc}">{fmt_ratio(r[n])}{rmark}{ft}</td>')
        common = (f'<td><code>{esc(b["id"])}</code></td>'
                  f'<td>{esc(b["name"])}</td>'
                  f'<td class="muted" style="font-size:12.5px">{esc(b["category"])}</td>')
        abs_body.append(f'<tr>{common}' + "".join(acells) +
                        f'<td class="muted" style="font-size:12px">{b["checksum"]}</td></tr>')
        ratio_body.append(f'<tr>{common}' + "".join(rcells) + '</tr>')
    abs_table = (f'<table class="bt" id="tbl-abs"><thead>{abs_head}</thead>'
                 f'<tbody>{"".join(abs_body)}</tbody></table>')
    ratio_table = (f'<table class="bt" id="tbl-ratio"><thead>{ratio_head}</thead>'
                   f'<tbody>{"".join(ratio_body)}</tbody></table>')

    # ---------- 雷达图得分表 ----------
    sc_head = '<tr><th>语言 / 轴</th>' + \
        "".join(f'<th>{esc(ax)}</th>' for ax in axes) + "</tr>"
    sc_body = []
    for n in langs:
        cells = "".join(f'<td style="color:{colors[n]};font-weight:600">{s:.0f}</td>'
                        for s in score[n])
        sc_body.append(f'<tr><td style="color:{colors[n]};font-weight:700">{esc(n)}</td>{cells}</tr>')
    sc_table = (f'<table class="bt"><thead>{sc_head}</thead>'
                f'<tbody>{"".join(sc_body)}</tbody></table>')

    svg = radar_svg(axes, score, colors, langs)
    leg = legend(langs, colors)

    # ---------- 组装 ----------
    css_extra = """
<style>
.hero .tag b{color:var(--fg)}
.bkpis{display:flex;gap:14px;flex-wrap:wrap;justify-content:center;margin:22px 0 4px}
.kpi{background:var(--panel);border:1px solid var(--border);border-radius:12px;
     padding:14px 22px;min-width:150px;text-align:center}
.kpi b{display:block;font-size:24px;color:var(--accent);margin-bottom:3px}
.kpi span{color:var(--muted);font-size:13px}
.bsec{max-width:1200px;margin:0 auto;padding:0 20px}
.bsec h2{font-size:22px;margin:40px 0 12px;padding-top:12px;border-top:1px solid var(--border)}
.bsec h2 small{color:var(--muted);font-size:13px;font-weight:400;margin-left:10px}
.bsec p.bd{color:var(--muted);font-size:14px;line-height:1.75;max-width:980px}
.bt{width:100%;border-collapse:collapse;margin:14px 0 8px;font-size:13.5px}
.bt th,.bt td{border:1px solid var(--border);padding:7px 10px;text-align:right;white-space:nowrap}
.bt th:first-child,.bt td:first-child,.bt th:nth-child(2),.bt td:nth-child(2),
.bt th:nth-child(3),.bt td:nth-child(3){text-align:left}
.bt thead th{background:var(--panel-2);position:sticky;top:0;z-index:2}
.bt tbody tr:nth-child(even){background:rgba(255,255,255,.018)}
.bt tbody tr:hover{background:rgba(124,162,247,.08)}
.bt td.muted,.bt th.muted{color:var(--muted)}
.tw{overflow-x:auto;border-radius:10px}
.tabs{display:flex;gap:8px;margin:16px 0 4px}
.tab{cursor:pointer;background:var(--panel);border:1px solid var(--border);color:var(--muted);
     padding:7px 16px;border-radius:8px;font-size:14px;user-select:none}
.tab.on{background:var(--accent);color:#06201f;border-color:var(--accent);font-weight:700}
.env2{display:grid;grid-template-columns:1fr 1fr;gap:20px}
@media(max-width:820px){.env2{grid-template-columns:1fr}}
.note{background:var(--panel);border:1px solid var(--border);border-left:3px solid var(--accent);
      border-radius:0 10px 10px 0;padding:12px 18px;color:var(--muted);font-size:13.5px;line-height:1.75;margin:14px 0}
.radar-box{background:var(--panel);border:1px solid var(--border);border-radius:14px;padding:20px;margin:14px 0}
.site-footer a{color:var(--accent)}
</style>
<script>
function showTab(which){
  document.getElementById('tbl-abs').style.display   = which==='abs'?'':'none';
  document.getElementById('tbl-ratio').style.display= which==='ratio'?'':'none';
  document.getElementById('tab-abs').className   = 'tab'+(which==='abs'?' on':'');
  document.getElementById('tab-ratio').className= 'tab'+(which==='ratio'?' on':'');
}
window.addEventListener('DOMContentLoaded',function(){showTab('abs');});
</script>
"""

    nav = """
<header class="site-header">
  <div class="wrap">
    <a class="brand" href="index.html"><span class="logo">◆</span> Dial</a>
    <nav class="topnav">
      <a href="index.html">首页</a>
      <a href="spec.html">语言规范</a>
      <a href="changelog.html">更新日志</a>
      <a href="cve.html">安全公告</a>
      <a class="active" href="bench.html">性能基准</a>
      <a class="ext" href="https://github.com/shichencike/Dial" target="_blank" rel="noopener">GitHub</a>
      <a class="ext" href="https://www.llvm.org/" target="_blank" rel="noopener">LLVM</a>
    </nav>
  </div>
</header>
"""

    gen = esc(meta["generated"])
    quick_badge = ('<span style="color:#f7768e">（本地快速冒烟数据，非校准正式值）</span>'
                   if meta.get("quick") else "")
    html_doc = f"""<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Dial 性能基准 · C / Python / Go / Dial 对比</title>
<link rel="stylesheet" href="assets/style.css">
{css_extra}
</head>
<body>
{nav}

<section class="hero">
  <div class="wrap">
    <h1>Dial 性能基准</h1>
    <p class="tag">同一台机器、同一套 <b>{meta['task_count']} 个微基准</b>，
    对比 <b>C / Python / Go / Dial</b> 四种语言。每项在 4 语言算出
    <b>完全相同的校验和</b>才计入对比，保证比的是等价计算。
    表格给出<b>绝对值</b>（单次纯计算耗时，毫秒）与<b>绝对比值</b>
    （以 C = 1.00× 为基准，大于 1 表示比 C 慢相应倍数）。
    页面底部叠加一张<b>六维工程能力雷达图</b> {quick_badge}</p>
    <div class="bkpis">{kpi_html}</div>
    <p class="tag" style="font-size:13px;margin-top:16px">
      采集时间 {gen} · {esc(host['os'])} · {esc(host['cpu'])}</p>
  </div>
</section>

<div class="bsec">

  <h2>1. 测量口径 <small>为什么这套数字可比</small></h2>
  <p class="bd">{esc(meta['method'])}</p>
  <div class="note">
    <b>基准说明：</b>{esc(meta['baseline'])}。所有维度均「越小越好」；
    表格中 <span style="color:var(--accent);font-weight:700">★ 标记</span> 标出该项最优语言。
    「产物体积」对编译型语言为可执行文件字节、对解释型 Python 为源码字节；
    「编译耗时」对 Python 记为 <code>py_compile</code> 生成字节码的耗时 —— 这两列的口径差异已在下表注明。
    <br><span style="color:#f7768e;font-weight:700">† 标记</span>：该项耗时触及
    {floor:.3f}ms 计时下限（或被编译器常量折叠成空操作，如 C 的 <code>-O2</code> 对 b20 字符串拼接），
    此时绝对值是下限而非实测、对应比值会偏大，仅作定性参考，不参与严谨横向比较。
  </div>
  <div class="env2">
    <div>
      <h4 style="color:var(--muted)">运行环境</h4>
      <table class="bt"><tbody>{env_rows}</tbody></table>
    </div>
    <div>
      <h4 style="color:var(--muted)">各语言编译 / 运行档位</h4>
      <table class="bt"><tbody>{flag_rows}</tbody></table>
    </div>
  </div>

  <h2>2. 六大工程维度 <small>绝对值，越小越好</small></h2>
  <p class="bd">把 28 项微基准与工具链特性汇总为 6 个工程维度的<b>绝对数值</b>：
  速度（28 项单次耗时之和）、峰值内存、编译耗时、启动耗时、产物体积、源码行数。
  这是底部雷达图的 6 个轴的<b>原始值</b>；雷达图会按各维最优语言归一化后再叠加。</p>
  <div class="tw">{dim_table}</div>

  <h2>3. 28 维明细 <small>绝对值 与 绝对比值</small></h2>
  <div class="tabs">
    <span class="tab on" id="tab-abs" onclick="showTab('abs')">绝对值（ms）</span>
    <span class="tab" id="tab-ratio" onclick="showTab('ratio')">绝对比值（C = 1.00×）</span>
  </div>
  <p class="bd">切换查看两套数据。<b>绝对值</b>是每种语言在每一项上的实测耗时（毫秒）；
  <b>绝对比值</b>以 C 为 1.00×，数值 = 该语言耗时 ÷ C 耗时。校验和一列是 4 语言共同算出的结果，
  用于证明各语言执行的是等价计算。</p>
  <div class="tw">{abs_table}</div>
  <div class="tw">{ratio_table}</div>

  <h2>4. 六维工程能力雷达图 <small>4 语言叠加，越靠外越优</small></h2>
  <p class="bd">6 个轴的量纲不同，无法直接画在一起，因此先做<b>归一化</b>：
  每个维度取 4 语言中的最优（最小）值为满分 100，其余语言得分 = 最优值 ÷ 自身值 × 100。
  于是「越靠外 = 该维度越接近最优」。速度、内存、编译、启动、体积、源码行数六个方向
  各自独立评分，Dial 的轮廓以加粗描边突出。</p>
  <div class="radar-box">
    {svg}
    {leg}
  </div>
  <div class="tw">{sc_table}</div>
  <div class="note">
    <b>读图提示：</b>得分是「相对本组 4 语言最优者的百分比」，不是绝对性能分；
    例如编译耗时轴上 C 的编译远比 Dial 的 LLVM + LTO 快，Dial 在该轴得分就低，
    但速度轴上 Dial 与 C 接近原生。六边形<b>面积</b>可作综合直觉，但各轴权重相等、
    不代表任何特定场景下的真实取舍，请结合第 2 节的绝对值一起看。
  </div>

  <h2>5. 复现 <small>本地一条命令</small></h2>
  <div class="note" style="border-left-color:var(--accent-2);font-family:ui-monospace,monospace">
    # 编译 dialc 后采集（gcc / go / python3 需可用）<br>
    python3 tools/bench_harness.py --dialc target/release/dialc<br>
    # 由 bench/data.json 生成本页<br>
    python3 tools/build_bench_web.py<br>
    <br>
    # CI 每次 push 也会跑采集器并把最新 bench/data.json 与网页作为 artifact 上传
  </div>

</div>

<footer class="site-footer">
  <div class="wrap">
    <span>Dial 性能基准 · 数据由 <code>tools/bench_harness.py</code> 实测生成，
      页面由 <code>tools/build_bench_web.py</code> 生成</span>
    <span><a href="index.html">首页</a> ·
      <a href="spec.html">语言规范</a> ·
      <a href="https://github.com/shichencike/Dial" target="_blank" rel="noopener">GitHub</a></span>
  </div>
</footer>

</body>
</html>
"""

    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(html_doc, encoding="utf-8")
    print(f"web/bench.html generated: {out} ({len(html_doc)} bytes, "
          f"{len(benches)} benchmarks, {len(axes)} radar axes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
