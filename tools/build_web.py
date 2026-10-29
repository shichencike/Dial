#!/usr/bin/env python3
"""Generate the dialc official documentation website from README.md / CHANGELOG.md.

Outputs a static site into web/:
  web/index.html       landing page
  web/spec.html        full language reference (README)
  web/changelog.html   changelog
  web/assets/style.css shared stylesheet
  web/assets/site.js    minimal nav behavior

The spec/changelog pages are produced with Python-Markdown (extra + toc), so they
stay in sync with the source docs. Run: python3 tools/build_web.py
"""
from __future__ import annotations

import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parent.parent
WEB = ROOT / "web"
ASSETS = WEB / "assets"

import markdown  # noqa: E402

NAV = """
<header class="site-header">
  <div class="wrap">
    <a class="brand" href="index.html"><span class="logo">◆</span> Dial</a>
    <nav class="topnav">
      <a href="index.html">首页</a>
      <a href="spec.html">语言规范</a>
      <a href="changelog.html">更新日志</a>
      <a class="ext" href="https://www.llvm.org/" target="_blank" rel="noopener">LLVM</a>
    </nav>
  </div>
</header>
"""

FOOTER = """
<footer class="site-footer">
  <div class="wrap">
    <span>dialc — 静态强类型、显式标注、无 GC 的编译型语言</span>
    <span class="muted">v0.81 · Rust + inkwell / LLVM 18 · 产物零 LLVM 依赖 · 三重许可 GPL-3.0 OR MIT OR Apache-2.0</span>
  </div>
</footer>
"""

CSS = """
:root{
  --bg:#0f1117; --panel:#161a23; --panel-2:#1c212c; --fg:#e6e9ef; --muted:#9aa4b2;
  --accent:#4cc2c4; --accent-2:#7aa2f7; --border:#262c38; --code:#0b0d12;
  --ok:#5fd38a; --warn:#e7c56b;
}
*{box-sizing:border-box}
html{scroll-behavior:smooth}
body{margin:0;font:15px/1.65 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,"PingFang SC","Microsoft YaHei",sans-serif;
  background:var(--bg);color:var(--fg)}
a{color:var(--accent);text-decoration:none}
a:hover{text-decoration:underline}
.wrap{max-width:1200px;margin:0 auto;padding:0 20px}

.site-header{position:sticky;top:0;z-index:20;background:rgba(15,17,23,.92);
  backdrop-filter:blur(8px);border-bottom:1px solid var(--border)}
.site-header .wrap{display:flex;align-items:center;justify-content:space-between;height:58px}
.brand{font-weight:700;font-size:19px;color:var(--fg);letter-spacing:.3px}
.brand .logo{color:var(--accent);margin-right:6px}
.topnav a{margin-left:18px;color:var(--muted);font-size:14.5px}
.topnav a:hover,.topnav a.active{color:var(--fg);text-decoration:none}
.topnav a.ext{color:var(--accent-2)}

.layout{display:grid;grid-template-columns:260px 1fr;gap:36px;max-width:1200px;
  margin:0 auto;padding:32px 20px 80px}
.sidebar{position:sticky;top:78px;align-self:start;max-height:calc(100vh - 100px);overflow:auto}
.sidebar .toc{background:var(--panel);border:1px solid var(--border);border-radius:10px;padding:14px 8px}
.sidebar .toc>ul{list-style:none;margin:0;padding:0}
.sidebar .toc li{margin:0}
.sidebar .toc a{display:block;padding:5px 12px;border-radius:6px;color:var(--muted);font-size:13.5px}
.sidebar .toc a:hover{background:var(--panel-2);color:var(--fg);text-decoration:none}
.sidebar .toc ul ul a{padding-left:26px;font-size:13px}
.sidebar .toc .toctitle{display:none}

.content{min-width:0}
.content h1{font-size:30px;margin:0 0 18px;line-height:1.25}
.content h2{font-size:23px;margin:38px 0 12px;padding-top:10px;border-top:1px solid var(--border)}
.content h3{font-size:18px;margin:26px 0 8px;color:#cfe3ff}
.content h4{font-size:15.5px;margin:18px 0 6px;color:var(--muted)}
.content p{margin:10px 0}
.content ul,.content ol{margin:10px 0 10px 4px;padding-left:22px}
.content li{margin:4px 0}
.content code{background:var(--code);border:1px solid var(--border);border-radius:5px;
  padding:1px 6px;font:13px/1.5 "SFMono-Regular",Consolas,monospace;color:#d7e7ff}
.content pre{background:var(--code);border:1px solid var(--border);border-radius:10px;
  padding:14px 16px;overflow:auto}
.content pre code{background:none;border:0;padding:0;color:#dbe4f0}
.content table{border-collapse:collapse;width:100%;margin:14px 0;font-size:14px}
.content th,.content td{border:1px solid var(--border);padding:8px 11px;text-align:left;vertical-align:top}
.content th{background:var(--panel-2);color:#cfe3ff}
.content blockquote{border-left:3px solid var(--accent);margin:14px 0;padding:4px 16px;
  background:var(--panel);border-radius:0 8px 8px 0;color:var(--muted)}
.content hr{border:0;border-top:1px solid var(--border);margin:34px 0}

/* landing */
.hero{padding:74px 0 40px;text-align:center;background:
  radial-gradient(1200px 400px at 50% -120px,rgba(76,194,196,.16),transparent)}
.hero h1{font-size:46px;margin:0 0 14px;letter-spacing:.5px}
.hero .tag{font-size:18px;color:var(--muted);max-width:680px;margin:0 auto 26px}
.hero .cta a{display:inline-block;margin:0 8px;padding:11px 22px;border-radius:9px;font-weight:600}
.btn-primary{background:var(--accent);color:#06201f}
.btn-primary:hover{text-decoration:none;filter:brightness(1.08)}
.btn-ghost{border:1px solid var(--border);color:var(--fg)}
.btn-ghost:hover{text-decoration:none;background:var(--panel)}
.cards{display:grid;grid-template-columns:repeat(3,1fr);gap:18px;margin:48px auto;max-width:1100px;padding:0 20px}
.card{background:var(--panel);border:1px solid var(--border);border-radius:12px;padding:20px}
.card h3{margin:2px 0 8px;color:var(--accent)}
.card p{margin:0;color:var(--muted);font-size:14px}
.quickstart{max-width:780px;margin:10px auto 60px;padding:0 20px}
.quickstart h2{text-align:center;border:0}
.quickstart pre{margin:8px 0 22px}
.kpis{display:flex;gap:26px;justify-content:center;flex-wrap:wrap;margin:8px 0 6px;color:var(--muted);font-size:14px}
.kpis b{color:var(--fg)}

.site-footer{border-top:1px solid var(--border);padding:22px 0;margin-top:40px}
.site-footer .wrap{display:flex;justify-content:space-between;flex-wrap:wrap;gap:8px;color:var(--muted);font-size:13px}
.muted{color:var(--muted)}

@media (max-width:900px){
  .layout{grid-template-columns:1fr}
  .sidebar{position:static;max-height:none}
  .cards{grid-template-columns:1fr}
}
"""


def dial_slugify(value: str, separator: str) -> str:
    """
    保留中文的锚点生成器。

    python-markdown 默认 slugify 会丢弃非 ASCII 字符，"19. 内存宪法（防 GC 铁律）"
    会被压成 "19-gc"，导致所有中文标题的内部链接失效。这里自行处理：
    去掉标点与括号，空白折叠为分隔符，中英文数字一律保留。
    """
    v = value.strip()
    v = re.sub(r"[（）()【】\[\]《》,，。.、：:；;！!？?\"'“”‘’/\\|]+", "", v)
    v = re.sub(r"\s+", separator, v)
    return v.strip(separator).lower()


def md_body(text: str, with_toc: bool) -> str:
    exts = ["extra", "toc", "fenced_code", "sane_lists"]
    md = markdown.Markdown(
        extensions=exts,
        extension_configs={"toc": {"slugify": dial_slugify, "permalink": False}},
    )
    src = ("[TOC]\n\n" + text) if with_toc else text
    return md.convert(src)


def render(title: str, body: str, active: str, landing: bool = False) -> str:
    nav_links = {
        "index": 'href="index.html"',
        "spec": 'href="spec.html"',
        "changelog": 'href="changelog.html"',
    }
    nav_html = NAV
    if active in nav_links:
        nav_html = nav_html.replace(nav_links[active], nav_links[active] + ' class="active"')
    content_class = "content" if not landing else "landing"
    return f"""<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title} · Dial 语言</title>
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
{nav_html}
{"<main>" if landing else '<div class="layout"><aside class="sidebar"></aside><main class="content">' + body + "</main></div>"}
{FOOTER}
</body>
</html>
"""


def main() -> None:
    WEB.mkdir(exist_ok=True)
    ASSETS.mkdir(exist_ok=True)
    (ASSETS / "style.css").write_text(CSS, encoding="utf-8")

    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")

    (WEB / "spec.html").write_text(
        render("语言规范", md_body(readme, True), "spec"), encoding="utf-8")
    (WEB / "changelog.html").write_text(
        render("更新日志", md_body(changelog, True), "changelog"), encoding="utf-8")

    (WEB / "index.html").write_text(render_index(), encoding="utf-8")

    print("web/ generated:", sorted(p.name for p in WEB.rglob("*") if p.is_file()))


def render_index() -> str:
    cards = [
        ("静态强类型，无推断", "每个变量、参数、返回值都必须显式标注类型。不存在 auto / 类型推导，杜绝隐式行为。"),
        ("不透明指针契约", "string 与 int[] 在 IR 中只是 ptr，用户代码永远无法触碰其内存布局，一切经由 C 导出函数。"),
        ("C 承担一切容器", "字符串、动态数组、哈希表、IO、内存管理全部由 C 实现，Dial 只负责把调用翻译出去。"),
        ("零隐式转换", "int 与 float 之间无隐式提升或窄化，必须显式调用 int_to_float / float_to_int。"),
        ("短路求值", "and / or 与 C 的 && / || 一致短路，保证判空等代码不会因盲目求值而崩溃。"),
        ("产物零 LLVM 依赖", "用户程序只依赖 C Runtime（libc），绝不链接 libLLVM。"),
        ("无 GC 底线（内存宪法）", "永不引入追踪式 GC。确定性内存管理靠 ARC 引用计数 + 编译期逃逸分析/移动语义，未来裸指针坚持「谁申请谁释放」。"),
        ("哈希表 map（ARC + COW）", "v0.4 新增 map 类型（int → int），与 string / int[] 共用同一套引用计数；put 触发扩容或写时复制后由编译器把新指针写回接收变量。"),
        ("for / else if / break / continue", "v0.4–v0.5 新增：for (i in start..end) 半开区间循环、else if 语法糖（编译期展开为嵌套 if）、break 与 continue（for 的 continue 先自增再判条件，不会死循环）。"),
        ("代码编写纪律", "拒绝过度抽象——宁可 WET 绝不 DRY 过头；不引入无用依赖；允许安全与性能的冗余；能用一个循环和一个 if 解决的绝不套设计模式。"),
        ("Fail-Fast 铁律（v0.6）", "宁可当场段错误，绝不产生未定义行为或静默错误数据。越界读写、负下标、整数除零一律当场终止：Debug 发射 int3、ReleaseSafe 发射 ud2，由操作系统接管 Core Dump。"),
        ("三档安全等级", "默认 Debug 崩溃保护全开；--release 保留防 UB 检查但陷阱更精简；--fast 关闭全部检查换取极限性能，等级同时作用于 LLVM IR 与 C Runtime。⚠️ --fast 下的越界将退化为 C 的未定义行为——除非你正在写底层核心算法且确信下标绝不越界，否则永远不要使用它。"),
        ("文件 IO（v0.61，v0.7 改抛 Error）", "新增 file 不透明句柄，仅封装 C 标准库 fopen / fread / fwrite / fclose；open / read / write / close 是最基础的 C 风格 API。无全局变量、无 Result 封装；v0.7 起失败即抛出 Error（业务失败与系统崩溃分离），用后即焚务必 close。"),
        ("可恢复异常 try / catch / throw（v0.7）", "throw Error(\"msg\") 抛出；try { } catch e { } 捕获（catch 后无圆括号）；catch { } 省略变量名忽略具体错误。严禁抛出非 Error 类型。跳转前自动释放 try 作用域内所有活跃 ARC 局部，绝不泄漏；无异常的代码运行时开销为零（已用 nm 实测）。"),
        ("所有权转移与多线程并发（v0.8）", "move_to(x, ch) 是编译期 intrinsic：只发三条 IR 指令（取指针 → 入队 → 原槽清零），O(1) 且零 retain / release。移交后变量进入 Moved 状态，再读 / 再写 / 再转移一律编译期拒绝。spawn(fn, args...) 在 pthread_create 之前把实参写进 argcell，零竞争窗口；channel<T> 用原子 ARC 共享，send / recv 只传所有权不传引用 —— 用转移取代锁。"),
        ("通道关闭与 EOF（v0.81）", "补上并发模型唯一的功能缺口：没有 EOF 时，生产者一退出消费者就永远卡在 recv 上 —— 程序不是跑错而是死锁。现在 close(ch) 广播关闭，done(ch) 给出「已关闭且已空」的单调判据，于是 while (not done(ch)) { r = recv(ch); } 必然终止；try_recv 提供非阻塞取值，len(ch) 用于判断有没有取到。关闭后再 send 会废掉 EOF 语义，故当场 Fail-Fast。"),
        ("三重开源许可", "GPL-3.0 OR MIT OR Apache-2.0 —— 三者任选其一遵守。三份协议原文与说明文档随源码及发布包完整分发；本许可只覆盖编译器源码，你用 Dial 写出的程序只链接 libc，不受其约束。"),
    ]
    card_html = "".join(
        f'<div class="card"><h3>{c[0]}</h3><p>{c[1]}</p></div>' for c in cards)
    return f"""<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Dial 语言 · 静态强类型、无 GC 的编译型语言</title>
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
{NAV}
<section class="hero">
  <div class="wrap">
    <h1>Dial</h1>
    <p class="tag">一门静态强类型、显式标注、无 GC 的编译型语言。编译器前端把 Dial 源码翻译为 LLVM IR，
    所有容器与运行时能力由 C 实现——比 C 更严格，且彼此融洽严谨。</p>
    <div class="cta">
      <a class="btn-primary" href="spec.html">阅读语言规范</a>
      <a class="btn-ghost" href="changelog.html">查看更新日志</a>
    </div>
    <div class="kpis">
      <span><b>v0.81</b> 当前版本</span>
      <span><b>Rust + inkwell</b> / LLVM 18</span>
      <span><b>零</b> LLVM 运行时依赖</span>
      <span><b>短路</b> and / or</span>
      <span><b>Fail-Fast</b> 越界即 ud2 / int3</span>
      <span><b>无 GC</b> 内存宪法</span>
    </div>
  </div>
</section>
<div class="cards">
  {card_html}
</div>
<section class="quickstart">
  <h2>快速开始</h2>
  <pre><code># 构建编译器（需要本机 LLVM 18）
export LLVM_SYS_181_PREFIX=/usr/lib/llvm-18
cargo build --release          # 产物：target/release/dialc

# 编译并运行一个 Dial 程序
./target/release/dialc run examples/hello.dial

# 端到端自测（正向 + 负向 + 产物契约）
./tests/run_tests.sh</code></pre>
  <h2>最小示例</h2>
  <pre><code>fn main(): int {{
    let name: string = "world";
    print("hello, " + name);   // 字符串按内容比较，拼接须显式 concat
    return 0;
}}</code></pre>
  <p class="muted" style="text-align:center">
    下载源码包：<a href="../dialc-v0.81.zip">dialc-v0.81.zip</a>（或自行 clone 仓库后构建）
  </p>
</section>
{FOOTER}
</body>
</html>
"""


if __name__ == "__main__":
    main()
