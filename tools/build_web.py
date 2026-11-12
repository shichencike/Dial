#!/usr/bin/env python3
"""Generate the dialc official documentation website from README.md / CHANGELOG.md.

Outputs a static site into web/:
  web/index.html                  landing page
  web/spec.html                   full language reference (README)
  web/changelog.html              version index (one row per release)
  web/changelog/overview.html      CHANGELOG preamble (versioning rules), verbatim
  web/changelog/<version>.html     one page per `## ` section, verbatim text
  web/changelog/index.json         machine-readable index (search + newer/older nav)
  web/assets/style.css             shared stylesheet

Why the changelog is split (v1.04 tooling): one flat HTML had grown past 200 KB.
The evidence-chain style of writing — what changed / why / measured data / where
the boundaries are — is preserved **verbatim**; only the *organisation* changes:
a small index plus one page per version. This fixes three concrete problems:
  1. finding an old version no longer means scrolling through 200 KB;
  2. a mobile / Termux browser only downloads the page it opens;
  3. each page builds its own TOC, so cross-version anchor collisions
     (`..._1` suffixes) disappear.

CHANGELOG.md remains the single source of truth; this script never edits it.
CI runs `git diff --exit-code -- web/`, so generation must be deterministic and
every output file must be committed. Run: python3 tools/build_web.py
"""
from __future__ import annotations

import html
import json
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parent.parent
WEB = ROOT / "web"
ASSETS = WEB / "assets"
CHANGELOG_DIR = WEB / "changelog"


def current_version() -> tuple[str, str]:
    """从 **Cargo.toml** 取版本号——全站唯一真相。

    官网的 footer、KPI、下载链接都从这里取，而不是各自硬编码一份。
    硬编码的版本号必然随发布漂移（v0.96 → v0.97 → v0.98 就漂过三轮：
    footer 停在 v0.96、首页 KPI 停在 v0.96、下载链接还指着早已不存在的
    `dialc-v0.96.zip`）。**版本号只能有一个出处**，否则每次发版都要手工追一遍。

    返回 `(显示版本, 完整版本)`。显示版本遵循 README §23 规则：
    常规发布取主.次两位（如 "v0.98"），补丁发布（第三位非零，如 0.98.1）
    显示完整三位 "v0.98.1"——补丁位必须显式可见。
    """
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^version\s*=\s*"([0-9]+(?:\.[0-9]+)*)"', cargo, re.M)
    if not m:
        raise SystemExit("无法从 Cargo.toml 解析版本号")
    full = m.group(1)
    parts = full.split(".")
    if len(parts) >= 3 and parts[2] != "0":
        display = f"v{full}"                       # 补丁发布：显示完整三位
    else:
        # v1.00 里程碑：Cargo 存 "1.0.0"（semver 不允许前导零），对外显示 "v1.00"。
        major = parts[0]
        minor = parts[1] if len(parts) >= 2 else "0"
        if int(major) >= 1:
            minor = minor.zfill(2)
        display = f"v{major}.{minor}"
    return display, full


VERSION, VERSION_FULL = current_version()


def short_version(full: str) -> str:
    """发布包资产名里的短版本，与 tools/package_release.sh 完全同规则。

    PATCH == 0 时取 MAJOR.MINOR（如 1.10.0 -> 1.10），补丁发布保留完整三段。
    CI 产出的 zip 就是 dialc-v<SHORT>-<slug>.zip，官网下载链接必须与它对齐，
    否则会指向不存在的资产（v1.04 就漂过：官网写 dialc-v1.4.0-*，实际是 dialc-v1.4-*）。
    """
    parts = full.split(".")
    if len(parts) >= 3 and parts[2] != "0":
        return full
    return f"{parts[0]}.{parts[1]}" if len(parts) >= 2 else full


VERSION_SHORT = short_version(VERSION_FULL)

try:
    import markdown  # noqa: E402
except ImportError:  # pragma: no cover
    raise SystemExit(
        "缺少依赖 python-markdown。安装：pip install 'markdown==3.10.1'\n"
        "（版本必须与生成 web/ 时一致，否则 HTML 会有差异、CI 的同步校验会误报。）"
    )

NAV = """
<header class="site-header">
  <div class="wrap">
    <a class="brand" href="index.html"><span class="logo">◆</span> Dial</a>
    <nav class="topnav">
      <a href="index.html">首页</a>
      <a href="spec.html">语言规范</a>
      <a href="changelog.html">更新日志</a>
      <a href="packages.html">包</a>
      <a href="cve.html">安全公告</a>
      <a href="bench.html">性能基准</a>
      <a class="ext" href="https://github.com/shichencike/Dial" target="_blank" rel="noopener">GitHub</a>
      <a class="ext" href="https://www.llvm.org/" target="_blank" rel="noopener">LLVM</a>
    </nav>
  </div>
</header>
"""

FOOTER = """
<footer class="site-footer">
  <div class="wrap">
    <span>dialc — 静态强类型、显式标注、无 GC 的编译型语言</span>
    <span class="muted">{ver} · Rust + inkwell / LLVM 18 · 产物零 LLVM 依赖 · 三重许可 GPL-3.0 OR MIT OR Apache-2.0 · <a class="ext" href="https://github.com/shichencike/Dial" target="_blank" rel="noopener">GitHub 仓库</a></span>
  </div>
</footer>
""".replace("{ver}", VERSION)

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

/* CVE 页面：严重程度徽章 + 状态徽章 */
.badge{display:inline-block;padding:1px 9px;border-radius:999px;font-size:12px;font-weight:600;
  line-height:1.7;border:1px solid transparent;white-space:nowrap}
.badge-critical{color:#ff8a8a;background:rgba(255,90,90,.14);border-color:rgba(255,90,90,.4)}
.badge-high{color:#ffb27a;background:rgba(255,150,80,.14);border-color:rgba(255,150,80,.4)}
.badge-medium{color:var(--warn);background:rgba(231,197,107,.14);border-color:rgba(231,197,107,.4)}
.badge-low{color:var(--muted);background:rgba(154,164,178,.12);border-color:rgba(154,164,178,.35)}
.badge-fixed{color:var(--ok);background:rgba(95,211,138,.14);border-color:rgba(95,211,138,.4)}

.cve-summary{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:16px;
  margin:24px 0 8px}
.cve-kpi{background:var(--panel);border:1px solid var(--border);border-radius:12px;padding:16px 18px}
.cve-kpi b{display:block;font-size:26px;color:var(--accent);margin-bottom:4px}
.cve-kpi span{color:var(--muted);font-size:13px}

.cve-card{background:var(--panel);border:1px solid var(--border);border-radius:14px;
  padding:22px 24px;margin:22px 0;scroll-margin-top:80px}
.cve-card h3{margin:0 0 4px;color:var(--accent);font-size:20px;display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.cve-card .meta{color:var(--muted);font-size:13px;margin:2px 0 14px}
.cve-card h4{font-size:15px;margin:16px 0 6px;color:#cfe3ff}
.cve-card .label{display:inline-block;color:var(--accent-2);font-weight:600;font-size:13px;margin-top:10px}

.note{background:var(--panel-2);border:1px solid var(--border);border-left:3px solid var(--accent);
  border-radius:0 8px 8px 0;padding:10px 16px;margin:14px 0;color:var(--muted);font-size:14px}

@media (max-width:900px){
  .layout{grid-template-columns:1fr}
  .sidebar{position:static;max-height:none}
  .cards{grid-template-columns:1fr}
}
"""


def dial_slugify(value: str, separator: str) -> str:
    """
    锚点生成器：**严格复刻 GitHub（github-slugger）的规则**。

    python-markdown 默认的 slugify 会丢弃非 ASCII 字符，"19. 内存宪法（防 GC 铁律）"
    会被压成 "19-gc"，导致所有中文标题的内部链接失效。这里保留中文。

    为什么必须与 GitHub 逐字节一致：README.md 里的手写锚点（例如
    `#145-cow-的触发时机`）是按 GitHub 的规则写的。如果官网生成的锚点与它不同，
    同一份链接就会「在 GitHub 上能跳、在官网上跳不动」。因此两处必须同源。

    GitHub 的规则（依次执行）：
      1. 转小写；
      2. 删除除「字母 / 数字 / 下划线 / 连字符 / **空格**」以外的所有字符
         （因此 `.` `:` `（` `）` `` ` `` `/` 等标点一律消失，但**空格要留着**）；
      3. 每个空格换成一个连字符。
    第 3 步**不合并**连续空格，所以 "a / b" 得到 "a--b"——两个空格对应两个连字符。
    """
    v = value.strip().lower()
    v = re.sub(r"[^\w\-\s]", "", v, flags=re.UNICODE)
    return v.replace(" ", separator)


# 官网里没有「仓库根文件」这类页面：源 markdown 中指向 README.md / CHANGELOG.md /
# ARCHITECTURE.md / LICENSE* / examples/ / editors/ 的相对链接，在 GitHub 上有效，
# 但落到 web/ 目录就成了死链。渲染时统一重写 href（**只改属性，不动正文文字**）：
#   README.md     -> 站内 spec.html（官网就是 README 的渲染版）
#   CHANGELOG.md  -> 站内 changelog.html（版本索引）
#   其余仓库文件/目录 -> GitHub 仓库绝对 URL（官网没有对应页面）
# 分页在 web/changelog/ 下，站内链接要补 "../"；base 由调用方传入。
GITHUB_BLOB = "https://github.com/shichencike/Dial/blob/main/"
GITHUB_TREE = "https://github.com/shichencike/Dial/tree/main/"

REPO_PATH_PREFIXES = (
    "ARCHITECTURE.md",
    "LICENSE-GPLv3", "LICENSE-MIT", "LICENSE-APACHE", "LICENSE",
    "Cargo.toml", "Cargo.lock",
    "examples/", "editors/", "third_party/", "tools/", "tests/",
    "stdlib/", "runtime/", "packages/",
)


def rewrite_repo_links(out: str, base: str) -> str:
    """把源 markdown 的仓库根相对 href 重写成官网可用链接（只动 href 属性）。"""
    def repl(m):
        href = m.group(1)
        if href == "README.md" or href.startswith("README.md#"):
            return f'href="{base}spec.html{href[len("README.md"):]}"'
        if href == "CHANGELOG.md" or href.startswith("CHANGELOG.md#"):
            return f'href="{base}changelog.html{href[len("CHANGELOG.md"):]}"'
        head = href.split("#", 1)[0]
        for p in REPO_PATH_PREFIXES:
            if head == p or head.startswith(p):
                url = GITHUB_TREE if head.endswith("/") else GITHUB_BLOB
                return f'href="{url}{href}"'
        return m.group(0)
    return re.sub(r'href="([^"]+)"', repl, out)


def md_body(text: str, with_toc: bool, strip_web_prefix: bool = False,
            base: str = "") -> str:
    exts = ["extra", "toc", "fenced_code", "sane_lists"]
    md = markdown.Markdown(
        extensions=exts,
        extension_configs={"toc": {"slugify": dial_slugify, "permalink": False}},
    )
    src = ("[TOC]\n\n" + text) if with_toc else text
    out = md.convert(src)
    if strip_web_prefix:
        # README 里的 `](web/xxx.html)` 是相对**仓库根**的路径（GitHub 上有效）；
        # 官网把 spec.html 与这些页面放在同一目录，须去掉 web/ 前缀才点得动。
        # 只改 href，不动任何正文文字；CHANGELOG 无此类链接，分页页不受影响。
        out = out.replace('href="web/', 'href="')
    # 其余指向仓库根 .md / 目录的 href 同理：改成站内页或 GitHub 绝对链接。
    return rewrite_repo_links(out, base)


def render(title: str, body: str, active: str, landing: bool = False,
           base: str = "") -> str:
    """渲染站点页。

    `base` 是站内相对链接前缀：根级页面为 ""，`web/changelog/` 下的分页为
    "../"（样式表与顶部导航都要跟着回退一级）。只重写站内 `.html` 链接，
    NAV 里的 GitHub / LLVM 绝对外链不受影响。
    """
    nav_html = re.sub(
        r'href="(index|spec|changelog|packages|cve|bench)\.html"',
        lambda m: f'href="{base}{m.group(1)}.html"',
        NAV,
    )
    nav_links = {
        "index":     f'href="{base}index.html"',
        "spec":      f'href="{base}spec.html"',
        "changelog": f'href="{base}changelog.html"',
        "packages":  f'href="{base}packages.html"',
        "bench":     f'href="{base}bench.html"',
    }
    if active in nav_links:
        nav_html = nav_html.replace(
            nav_links[active], nav_links[active] + ' class="active"')
    return f"""<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title} · Dial 语言</title>
<link rel="stylesheet" href="{base}assets/style.css">
</head>
<body>
{nav_html}
{"<main>" if landing else '<div class="layout"><aside class="sidebar"></aside><main class="content">' + body + "</main></div>"}
{FOOTER}
</body>
</html>
"""


# ---------------------------------------------------------------------------
# 更新日志分页
#
# 索引页的「一句话主题」不是新写的事实：每条都从 CHANGELOG.md 对应版本段的
# 首段概括而来（v1.00~v1.03 直接取自里程碑段里那张现成的主题表）。
# 详情页正文则是该段 markdown 的原样渲染，绝不改写。未登记的版本会自动
# 派生主题并告警，避免以后新增版本时索引页出现空白行。
# ---------------------------------------------------------------------------

TOPICS = {
    "unreleased-dev": "手搓 GUI 十五层（TextBox 输入层 / 统一键码 / XTest 实机注入 / 可覆写主题色 + Radio·Slider·Progress / 信号槽·控件事件订阅纯值队列 / 菜单·对话框覆盖层复用纯值信号队列 / 第 9 层滚动视图：视口裁剪 + 滚动条几何·命中·绘制 + 偏移推进 wheel·drag·scroll_to、纯 C 零平台符号、零 DISPLAY 依赖 / 第 10 层右键上下文菜单：复用第 7 层 GuiMenu + 弹出锚点 + 命中·悬停·选中·绘制、借用语义不转移菜单所有权、纯 C 零平台符号、零 DISPLAY 依赖 / 第 11 层悬浮提示：文本 + 位置 + 尺寸 + 悬停计时、延迟由调用方推进不读时钟、纯 C 零平台符号、零 DISPLAY 依赖 / 第 12 层状态栏：左·右文本 + 进度条槽位、进度值越界夹紧、纯 C 零平台符号、零 DISPLAY 依赖 / 第 13 层通知气泡：FIFO 消息队列 + 生命周期纯值推进·到期自动消隐、纯 C 零平台符号、零 DISPLAY 依赖 / 第 14 层窗口级覆盖层自动集成：5 类覆盖层挂窗 + win.draw() 固定 z-order 自动叠加 + 借用语义 + 空槽位零像素影响、纯 C 零平台符号、零时钟依赖 / 第 15 层快捷键 / 命令系统：按键组合 → 命令 id 的纯内存表 + 命令启用态、dispatch 只发启用且命中的命令、冲突 Fail-Fast、容量 64 定长、纯 C 零平台符号、零 DISPLAY 依赖 / 第 8 层 Dial 侧 OO 封装 gui_oo.dial：278 个 impl 方法、纯源码随包 src/ 分发（装包即用）、零 extern fn 增量、C 风格与 OO 风格绘制 CRC 逐字节相等）+ int32/float32 一等类型与 FFI 精确 32 位宽度 + B1 impl 方法块（obj.m() 编译期脱糖、不新增关键字、ABI 不变，GUI 句柄 win.draw()）+ 官网 6 个包介绍页 + 包索引页 + http.dialpkg 三处 Fail-Fast/可用性修复 + dialpkg src/ 随包 Dial 源码模块（[src] 段显式声明 + manifest.src 映射 + 三级 import 装包即用 + 逐文件 SHA256 保护 + 向后兼容，见 README §25.11）+ 工具链强化（dialc fmt 间距规范化 / dialc pack --init·--dry-run·--json·--verify，见 README §34.10）（开发中增量，未升版本号 / 未发布）",
    "v1.10": "底层原语补齐（@asm / simd）+ 首个发布标签 v1.1",
    "v1.04": "FFI 成为语言一等公民",
    "v1-0-milestone": "v1.0 里程碑达成的分版记录（v1.00 ~ v1.03）",
    "ci-fix-v1-0-retag": "修复红色 CI 作业并重打 v1.0 标签",
    "install-release-flow": "安装脚本零硬编码，标准库与其他资产同处一个 Release",
    "v1.03": "回调（函数指针）与可变参数",
    "v1.02": "goto / with / 裸指针，补齐不透明句柄契约",
    "v1.01": "按「真实程序」盘点后的缺口补齐",
    "v1.00": "语言特性补齐（五组）+ 泛型容器",
    "v0.99.1": "v0.99 修复发布（补丁位），行为完全一致",
    "v0.99": "数组元素扩展到任意 8 字节句柄，支持多维数组",
    "v0.98.1": "v0.98 修复发布（补丁位），行为完全一致",
    "v0.98": "dialc lsp 打磨到工业级 + 三套编辑器接入",
    "v0.97": "三平台原生构建（Linux / Windows / Termux）",
    "stdlib-v1.0.0": "标准库改造为 .dialpkg，独立发版、与编译器解耦",
    "v0.96": "包 URL 安装 + 一键安装脚本",
    "v0.95": "LSP 首次增强：Hover / 跳转定义 / 补全",
    "v0.94": "dialc lsp 语言服务器骨架",
    "v0.93": "import 与 .dialpkg 包分发落地",
    "v0.92": "char 单字符类型 + 修复宪法级文档矛盾",
    "v0.91": "多路复用 select",
    "v0.9": "并发底座安全加固：判空 / 多消费者安全 / 超时",
    "v0.81": "通道关闭与 EOF",
    "v0.8": "所有权转移 move_to 与多线程并发",
    "v0.7": "可恢复异常 try / catch / throw + 三重许可",
    "v0.61": "文件 IO（file 句柄）+ 版本号规则",
    "v0.6": "Fail-Fast 确立为语言底线",
    "v0.5": "循环控制流 + 代码编写纪律",
    "v0.4": "部分标准库 + 控制流语法糖 + 内存宪法",
    "v0.3": "只改语义与诊断，不动架构契约",
    "v0.2": "本仓库首个规范化基线",
    "unreleased": "早期语言层未实现 / 尚未落地清单",
}

# `## ` 标题里方括号内容不是纯版本号时，给出稳定的英文文件名。
SLUG_OVERRIDES = {
    "v1.0 里程碑": "v1-0-milestone",
    "CI 修复与 v1.0 标签重打": "ci-fix-v1-0-retag",
    "安装与发布流程调整": "install-release-flow",
    "stdlib v1.0.0": "stdlib-v1.0.0",
    "未发布": "unreleased-dev",   # 开发中增量（未升版本号 / 未打标签 / 未发 Release）
}

HEAD_RE = re.compile(r"^##\s+\[([^\]]+)\]\s*—\s*(.*)$")
DATE_RE = re.compile(r"\d{4}-\d{2}-\d{2}")


def parse_head(line: str) -> tuple[str, str, str]:
    """从 `## [vN] — 日期/说明` 取 (版本标签, 日期, 标题后缀原文)。"""
    line = line.strip()
    m = HEAD_RE.match(line)
    if m:
        label, tail = m.group(1).strip(), m.group(2).strip()
    else:
        label, tail = re.sub(r"^##\s+", "", line).strip(), ""
    dm = DATE_RE.search(tail)
    return label, (dm.group(0) if dm else ""), tail


def seg_slug(label: str) -> str:
    if label.startswith("未实现"):
        return "unreleased"
    if label in SLUG_OVERRIDES:
        return SLUG_OVERRIDES[label]
    if re.fullmatch(r"v[0-9][0-9.]*", label):
        return label
    # 兜底：保留原文 slug（可能含中文），并显式告警，提醒补登记。
    print(f"  ⚠ 未登记文件名，已自动派生：{label}")
    return dial_slugify(label, "-")


def auto_topic(segment: str) -> str:
    """未登记主题时的兜底：取该段首个正文句，去掉 markdown 噪音。

    只用于索引元数据，不参与详情页正文（正文永远是原文渲染）。
    """
    buf = ""
    for line in segment.splitlines()[1:]:
        t = line.strip()
        if not t or t.startswith((">", "#", "---", "|", "- ")):
            if buf:
                break
            continue
        t = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", t)   # 链接只留文字
        t = re.sub(r"[*`]", "", t)
        buf += t + " "
        m = re.search(r"[。！？]", buf)
        if m:
            buf = buf[:m.end()]
            break
    buf = " ".join(buf.split())
    return (buf[:70] + "…") if len(buf) > 70 else (buf or "（见详情页）")


def build_changelog(changelog_text: str) -> None:
    """把 CHANGELOG.md 拆成「索引 + 前言 + 每版本一页 + index.json」。"""
    lines = changelog_text.splitlines(keepends=True)
    starts = [i for i, l in enumerate(lines) if l.startswith("## ")]
    if not starts:
        raise SystemExit("CHANGELOG.md 里找不到 `## ` 版本标题，拒绝生成")

    preamble = "".join(lines[:starts[0]])
    segments = ["".join(lines[s:e])
                for s, e in zip(starts, starts[1:] + [len(lines)])]

    # 自检①：切分必须无损——拼回去与原文逐字节相等，才谈得上「一字不改」。
    if preamble + "".join(segments) != changelog_text:
        raise SystemExit("CHANGELOG.md 分段自检失败：拼接结果与原文不一致")

    metas = []
    for seg in segments:
        label, date, note = parse_head(seg.splitlines()[0])
        slug = seg_slug(label)
        topic = TOPICS.get(slug) or auto_topic(seg)
        if slug not in TOPICS:
            print(f"  ⚠ 未登记主题，已自动派生：{slug} -> {topic}")
        display_label = "未实现 / 尚未落地" if slug == "unreleased" else label
        metas.append({
            "id": slug,
            "label": display_label,
            "date": date,
            "note": note,
            "topic": topic,
            "file": f"{slug}.html",
        })

    # newer / older：数组按 CHANGELOG 顺序（新→旧），前一项更新、后一项更早。
    for i, m in enumerate(metas):
        m["newer"] = metas[i - 1]["id"] if i > 0 else None
        m["older"] = metas[i + 1]["id"] if i + 1 < len(metas) else None

    CHANGELOG_DIR.mkdir(exist_ok=True)
    # 清掉上一次生成的产物，避免 slug 变动后留下陈旧页（只删本生成器产物）。
    for stale in list(CHANGELOG_DIR.glob("*.html")) + [CHANGELOG_DIR / "index.json"]:
        if stale.exists():
            stale.unlink()

    # 1) 前言（版本号规则等），原文渲染，不带 TOC。
    (CHANGELOG_DIR / "overview.html").write_text(
        render("更新日志 · 版本规则", md_body(preamble, False, base="../"),
               "changelog", base="../"), encoding="utf-8")

    # 2) 每个版本一页：正文 = 该段 markdown 的原样渲染 + 前后导航条。
    for i, m in enumerate(metas):
        older = metas[i + 1] if m["older"] else None
        newer = metas[i - 1] if m["newer"] else None
        back = ('<p class="note"><a href="../changelog.html">← 版本索引</a>'
                ' · <a href="overview.html">版本规则</a></p>')
        nav = ['<p class="note">']
        if older:
            nav.append(f'← 更早：<a href="{older["file"]}">{html.escape(older["label"])}</a>')
        if older and newer:
            nav.append(" · ")
        if newer:
            nav.append(f'更新：<a href="{newer["file"]}">{html.escape(newer["label"])}</a> →')
        if older or newer:
            nav.append(' · <a href="../changelog.html">版本索引</a>')
        nav.append("</p>")
        page = back + md_body(segments[i], True, base="../") + "".join(nav)
        (CHANGELOG_DIR / m["file"]).write_text(
            render(f"更新日志 {m['label']}", page, "changelog", base="../"),
            encoding="utf-8")

    # 3) 索引页：只放版本表格，保持轻量。
    intro = (
        "<p>本页是<strong>版本索引</strong>。Dial 的更新日志按「证据链」记录："
        "每个版本不只写改了什么，还写为什么改、实测数据与边界在哪。"
        "为便于检索与移动端加载，完整记录按版本拆成独立页，"
        "<strong>各页正文与 CHANGELOG.md 对应条目逐字一致</strong>。"
        "点击版本号查看详情；<a href=\"changelog/overview.html\">版本规则与前言</a>单列一页。</p>"
    )
    rows = []
    for m in metas:
        stamp = m["date"] or (m["note"][:48] + "…" if len(m["note"]) > 48
                              else m["note"]) or "—"
        rows.append(
            f'<tr><td><a href="changelog/{m["file"]}">{html.escape(m["label"])}</a></td>'
            f'<td>{html.escape(stamp)}</td>'
            f'<td>{html.escape(m["topic"])}</td></tr>'
        )
    table = ("<table><thead><tr><th>版本</th><th>日期 / 说明</th><th>主题</th>"
             "</tr></thead><tbody>" + "".join(rows) + "</tbody></table>")
    (WEB / "changelog.html").write_text(
        render("更新日志 · 版本索引", intro + table, "changelog"),
        encoding="utf-8")

    # 4) 机器可读索引：给搜索与「上一版 / 下一版」导航用。
    payload = {
        "source": "CHANGELOG.md",
        "index": "../changelog.html",
        "count": len(metas),
        "versions": metas,
    }
    (CHANGELOG_DIR / "index.json").write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8")

    # 自检②：页数必须与段数 + 前言页严格对应。
    produced = list(CHANGELOG_DIR.glob("*.html"))
    if len(produced) != len(metas) + 1:
        raise SystemExit(
            f"更新日志分页自检失败：生成 {len(produced)} 页，"
            f"预期 {len(metas) + 1} 页")
    print(f"  changelog: {len(metas)} 个版本 + 1 个前言页 + index.json")


# ---------------------------------------------------------------------------
# 包介绍页
#
# 6 个包：stdlib（随编译器发布、自动链入）+ 5 个独立 .dialpkg（gui / http /
# text / json / crypto）。机器可读事实（name / version / abi / extern fn 计数）
# **直接从包源文件提取**，文案只写叙述与示例，绝不硬编码版本号——包一升级，
# 重生成官网即自动同步，避免漂移。每页正文即 PACKAGE_BLURBS 的 markdown 渲染，
# 顶部一张事实表随源自动刷新。
# ---------------------------------------------------------------------------

PACKAGES_DIR = ROOT / "packages"
STDLIB_DIR = ROOT / "stdlib"

# slug -> 叙述 markdown（事实表由 build_packages 自动拼到顶部，这里不写版本号）。
PACKAGE_BLURBS = {
    "stdlib": """\
## 标准库 stdlib

Dial 的**标准库**，随编译器发布、编译你的程序时**自动链入**，无需 `install`、
也无需在源码里 `import`（编译器前端直接认得 `Db` / `string` / `int[]` 等内置类型）。
它把「字符串 / 动态数组 / 哈希表 / IO / 内存管理 / 并发」这些一切容器与运行时
能力用 C 实现，Dial 侧只负责把调用翻译出去——这正是「C 承担一切容器」的设计。

- **类型系统基石**：`string`（UTF-8 不可变字节序列，ARC 拥有）、`int[]` /
  `float[]` / `byte[]` / `char[]`（动态数组，put 触发扩容或写时复制后由编译器把
  新指针写回接收变量）、`map<K,V>`（ARC + COW 哈希表）。
- **不透明指针契约**：`string` 与数组在 IR 中只是 `ptr`，用户代码永远无法触碰其
  内部布局，一切经由 C 导出函数——库作者随意重构、调用方零改动。
- **并发底座**：`thread` / `channel<T>`（原子 ARC 共享，`move_to` 转移所有权取代锁）、
  `spawn` / `select` / `close` / `done` / `try_recv` / `recv_timeout`。
- **Fail-Fast 内存宪法**：绝不引入追踪式 GC；确定性内存靠 ARC + 编译期逃逸分析 /
  移动语义，裸指针坚持「谁申请谁释放」。

> 标准库是 `MIT` / `Apache-2.0` 许可（与编译器本体的 GPL 三选一隔离），因此被
> 静态链进你程序里的运行时不受 GPL 条款约束——工具可以是 GPL，被链进你程序的
> 运行时绝不能是。

详见 [语言规范](../spec.html)（README 全文即规范）。""",

    "gui": """\
## GUI 地基包 gui

不用 Qt / GTK / SDL，**纯 C 手搓**的 GUI 包，前七层 + 第 9/10/11/12/13/14/15 层滚动视图·右键上下文菜单·悬浮提示·状态栏·通知气泡·窗口级覆盖层自动集成·快捷键 / 命令系统是 C 契约、
**共 279 个 `extern fn`**：地基（窗 + 事件 + 绘图 + 字体）/ 控件框架层（Widget 树 + 布局 +
命中 + 主动分发）/ 成品控件外观层（Label / Button / CheckBox + 主题表驱动四态）/
输入层（TextBox + 焦点链 + 统一功能键码 + XTest 实窗注入）/ 主题层与控件扩展
（14 个可覆写颜色角色 + Radio / Slider / ProgressBar）/ 信号槽（6 类可订阅纯值
信号 + 每树环形队列）/ **菜单·对话框覆盖层**（顶栏菜单 + 弹出菜单 + 模态对话框，
复用纯值信号队列）/ **滚动视图层**（视口裁剪 + 滚动条几何 / 命中 / 绘制 + 偏移推进
`wheel` / `drag` / `scroll_to`，越界自动夹紧，零 DISPLAY 依赖，见 §38.15）/
**右键上下文菜单层**（复用第 7 层 `GuiMenu` + 弹出锚点 + 命中 / 悬停 / 选中 / 绘制，
借用语义不转移菜单所有权，零 DISPLAY 依赖，见 §38.16）/
**悬浮提示层**（文本 + 位置 + 尺寸 + 悬停计时，延迟由调用方推进、零时钟依赖，
零 DISPLAY 依赖，见 §38.17）/
**状态栏层**（左 / 右文本 + 进度条槽位，进度值越界夹紧，零 DISPLAY 依赖，见 §38.18）/
**通知气泡层**（FIFO 消息队列 + 生命周期纯值推进、到期自动消隐，零 DISPLAY 依赖，见 §38.19）/
**窗口级覆盖层自动集成**（5 类覆盖层挂到窗口槽位，`win.draw()` 按固定 z-order 自动叠加：
滚动条 → 上下文菜单 → 悬浮提示 → 状态栏 → 通知气泡；窗口借用对象、只持指针不释放，
空槽位零像素影响、既有 CRC 逐字节不破，见 §38.20）/
**快捷键 / 命令系统**（`dial_gui_hotkey_*`：按键组合 `(key, mods)` → 命令 id 的纯内存表 +
命令启用态；`bind` 冲突 Fail-Fast / `rebind` 覆盖 / `unbind` 压实 / `dispatch` 只发启用
且命中的命令（语义归应用、C 侧零反向回调）；容量 64、命令 id `[0, 256)`；空表零像素影响、
重绘幂等，纯 C 零平台符号、零时钟、零 DISPLAY 依赖，见 §38.21）。
**第 8 层是
Dial 侧 OO 封装层**（`gui_oo.dial`，278 个 `impl` 方法把上述 C 风格函数包成 `win.draw()`
这样的惯用写法，纯 Dial 源码、随包 `src/` 分发（装包即用）、**零 `extern fn` 增量、ABI 不变**，见 §38.14）。

```dial
import "gui";

fn main(): int {
    let app: GuiApp = dial_gui_app_new();
    let win: GuiWindow = dial_gui_window_new(app, "Hello GUI", 400, 300);
    let root: GuiWidget = dial_gui_widget_tree(400, 300);
    let btn: GuiWidget = dial_gui_widget_add(root);
    dial_gui_widget_set_rect(btn, 10, 10, 80, 24);
    dial_gui_widget_set_kind(btn, 2);            // BUTTON
    dial_gui_widget_set_text(btn, "Click me");
    dial_gui_widget_subscribe(btn, 1);           // CLICKED
    dial_gui_window_set_root(win, root);
    dial_gui_window_draw(win);
    dial_gui_window_flush(win);
    dial_gui_window_close(win);
    dial_gui_app_free(app);
    return 0;
}
```

**第 8 层：同一套能力也能写成方法**。装好 `gui.dialpkg` 后，包内
`src/gui_oo.dial` 会随包分发；写一行裸名 import 即用，**无需复制任何文件到项目目录**
（模块内已传递 `import "gui";`，前端会汇总去重）：

```dial
import "gui_oo";

fn main(): int {
    let app: GuiApp = dial_gui_app_new();
    let win: GuiWindow = app.window_new("Hello GUI", 400, 300);
    let root: GuiWidget = app.widget_tree(400, 300);
    let btn: GuiWidget = root.add();
    btn.set_rect(10, 10, 80, 24);
    btn.set_kind(2);
    btn.set_text("Click me");
    btn.subscribe(1);
    win.set_root(root);
    win.draw();
    win.flush();
    win.close();
    app.free();
    return 0;
}
```

`impl` 在 parser 阶段就降级为普通函数，`obj.m()` 在语义与代码生成两处就地改写成
`Expr::Call` 复用整条调用链——同一绘制序列以两种风格执行，`painter_crc32()`
**逐字节相等**，产物与 ABI 不漂移。B1 现役边界：不支持关联函数，故 app 自身仍用
`dial_gui_app_new()`；工厂以 `app.window_new(...)` 等形式提供（self 仅作命名空间）。

- **三平台真实现**：`x86_64-linux-gnu` / `aarch64-unknown-linux-musl` /
  `x86_64-pc-windows-gnu` 均由 `zig cc` 交叉编译出真实 `.a`，零链接期 GUI 依赖
  （X11 / Win32 一律 `dlopen` / `LoadLibraryA`），最终链接**不需要** `-lX11` / `-lgdi32`。
- **绝不 C → Dial 反向回调**：事件 / 信号一律「C 写纯值块，Dial 主动
  `pending → data → pop` 消费」——符合内存宪法。
- **ABI 不变**：只追加 `extern fn`，旧签名一字不动；覆盖层不进控件树，控件几何 /
  绘制 CRC 逐字节不漂移。

详见 [§38 手搓 GUI 地基包](../spec.html#38-手搓-gui-地基包guidialpkg)、
[§38.14 Dial 侧 OO 封装层](../spec.html#3814-dial-侧-oo-封装层第-8-层) 与
[§38.15 滚动视图层](../spec.html#3815-滚动视图层第-9-层)、
[§38.16 右键上下文菜单层](../spec.html#3816-右键上下文菜单层第-10-层)、
[§38.17 悬浮提示层](../spec.html#3817-悬浮提示层第-11-层)、
[§38.18 状态栏层](../spec.html#3818-状态栏层第-12-层)、
[§38.19 通知气泡层](../spec.html#3819-通知气泡层第-13-层)、
[§38.20 窗口级覆盖层自动集成](../spec.html#3820-窗口级覆盖层自动集成第-14-层)、
[§38.21 快捷键 / 命令系统](../spec.html#3821-快捷键--命令系统第-15-层)。""",

    "http": """\
## HTTP 客户端包 http

自研 **HTTP/1.1 + TLS 1.3 + keep-alive + HTTP/2** 客户端，**零第三方 HTTP 栈**
（自写报文解析 / Header 拼装 / Socket 读写），TLS 走 vendor 的 mbedTLS。
覆盖面积从普通 GET/POST 到真实并发多路复用的 h2。

- **高层 API**：`http_get` / `http_post` 一行到位（响应头 / 404 / gzip 解压 /
  chunked 全处理）；`http_request` 暴露超时 / 自定义头 / 代理画像。
- **低层流式**：`http_stream_*` 边读边处理大响应，不把整响应塞进内存。
- **HTTP/2**：`h2c` 明文 + `h2` ALPN 双路径；自研 HPACK（含 Huffman）、单连接
  **真·并发多路复用**（reader 线程 + `spawn` worker），超长额外头不截断。
- **TLS 1.3**：证书校验 fail-closed（空 CA 目录也拒绝）、keep-alive 连接池复用。
- **Fail-Fast**：chunked 畸形长度行当场失败、合法 `chunk-ext` 不误伤；错误路径
  明确返回错误码，绝不静默损坏。

```dial
import "http";

fn main(): int {
    let r: HttpResp = http_get("https://example.com/");
    if (not is_null(r)) {
        print(r.status_code);
        print(string_from_cstr(r.body));
    }
    return 0;
}
```

详见 [W 组端到端](../changelog/unreleased-dev.html) 与 [§W](../spec.html) 相关章节。""",

    "text": """\
## 正则包 text

自研 **Thompson NFA** 正则引擎（非回溯、线性时间），零第三方依赖。

- **语法子集**：字符类 `[a-z]` / 锚点 `^` `$` / 分组 `(...)` / 选择 `a|b` /
  量词 `*` `+` `?` / 转义。
- **捕获组**：`match` 返回各分组子串；`replace` 支持 `$1` `$2` … `$n` 反向引用替换。
- **错误路径**：非法 Pattern 编译期 / 运行期明确报错，绝不静默接受。

```dial
import "text";

fn main(): int {
    let re: Regex = regex_compile("[0-9]+");
    let m: RegexMatch = regex_match(re, "order 42 shipped");
    if (m.matched) {
        print(string_from_cstr(m.group));   // "42"
    }
    return 0;
}
```

三平台零警告构建，纯 C 实现。""",

    "json": """\
## JSON 包 json

自研**递归下降解析 + 紧凑序列化**，零第三方依赖，独立拥有句柄。

- **严格模式**：重复键 / 尾随逗号 / 非法转义一律拒绝（Fail-Fast）。
- **大整数精度**：数字以字符串保留，不丢精度；`as_int` / `as_float` 按需转换。
- **`\\u` 与代理对**：正确解码 `\\uXXXX` 与 UTF-16 代理对（emoji 不丢）。
- **独立拥有 + 深拷贝**：`json_parse` 返回的文档句柄独立拥有其内存；`json_copy`
  做深拷贝构造，互不串扰。
- **错误码**：解析失败返回明确错误码与位置，便于定位。

```dial
import "json";

fn main(): int {
    let doc: JsonDoc = json_parse("{\"name\":\"dial\",\"n\":42}");
    let v: JsonValue = json_get(doc, "name");
    print(string_from_cstr(json_as_string(v)));   // "dial"
    return 0;
}
```

三平台零警告构建，纯 C 实现。""",

    "crypto": """\
## 密码学包 crypto

复用 vendor 的 **mbedTLS** 实现摘要 / Base64 / HMAC，零自研密码学原语
（密码学正确性是底线，绝不手搓）。

- **五种摘要**：MD5 / SHA-1 / SHA-256 / SHA-512 / SHA3-256，均用标准测试向量核对。
- **Base64**：`base64_encode` / `base64_decode`，含内嵌 NUL 的二进制数据安全往返。
- **HMAC-SHA256**：`hmac_sha256`，密钥 + 消息得 MAC。
- **双入口**：`string` 与 `bytes` 两种输入，含非 UTF-8 二进制数据（内嵌 NUL）也安全。
- **严格错误路径**：坏输入明确报错，绝不静默吐错值。

```dial
import "crypto";

fn main(): int {
    let h: string = sha256_hex("hello");
    print(h);   // 2cf24dba... 标准向量
    return 0;
}
```

三平台零警告构建（mbedTLS 随包 vendor 进仓库）。""",
}

# (slug, 显示名, dial.toml 路径或 None, .diali 路径, version 覆盖, abi 覆盖)
_PKG_SPECS = [
    ("stdlib", "标准库 stdlib", None, STDLIB_DIR / "stdlib.diali", "1.0.0", "6 (STDLIB_ABI)"),
    ("gui", "GUI 地基包 gui", PACKAGES_DIR / "gui" / "dial.toml",
     PACKAGES_DIR / "gui" / "gui.diali", None, None),
    ("http", "HTTP 客户端包 http", PACKAGES_DIR / "http" / "dial.toml",
     PACKAGES_DIR / "http" / "http.diali", None, None),
    ("text", "正则包 text", PACKAGES_DIR / "text" / "dial.toml",
     PACKAGES_DIR / "text" / "text.diali", None, None),
    ("json", "JSON 包 json", PACKAGES_DIR / "json" / "dial.toml",
     PACKAGES_DIR / "json" / "json.diali", None, None),
    ("crypto", "密码学包 crypto", PACKAGES_DIR / "crypto" / "dial.toml",
     PACKAGES_DIR / "crypto" / "crypto.diali", None, None),
]


def _read_dial_toml(path: pathlib.Path) -> tuple[str, str, str]:
    text = path.read_text(encoding="utf-8")
    name = re.search(r'^name\s*=\s*"([^"]+)"', text, re.M)
    ver = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
    abi = re.search(r'^abi\s*=\s*(\d+)', text, re.M)
    return (name.group(1) if name else "", ver.group(1) if ver else "",
            abi.group(1) if abi else "")


def _count_extern(diali: pathlib.Path) -> int:
    return sum(1 for l in diali.read_text(encoding="utf-8").splitlines()
               if l.startswith("extern fn"))


def build_packages() -> None:
    PKG_DIR = WEB / "packages"
    PKG_DIR.mkdir(exist_ok=True)
    for stale in PKG_DIR.glob("*.html"):
        stale.unlink()

    rows = []
    for slug, display, toml, diali, vover, aover in _PKG_SPECS:
        if toml and toml.exists():
            _n, ver, abi = _read_dial_toml(toml)
        else:
            _n, ver, abi = "", (vover or ""), (aover or "")
        name = display.split(" ", 1)[0] if not _n else _n
        count = _count_extern(diali)
        blurb = PACKAGE_BLURBS.get(slug, "（简介待补）")
        facts = (
            "<table><thead><tr><th>字段</th><th>值</th></tr></thead><tbody>"
            f"<tr><td>包名</td><td><code>{html.escape(name)}</code></td></tr>"
            f"<tr><td>版本</td><td>{html.escape(ver)}</td></tr>"
            f"<tr><td>ABI</td><td>{html.escape(abi)}</td></tr>"
            f"<tr><td>extern fn 数量</td><td>{count}</td></tr>"
            f"<tr><td>来源</td><td><code>{html.escape(str(diali.relative_to(ROOT)))}</code></td></tr>"
            "</tbody></table>"
        )
        (PKG_DIR / f"{slug}.html").write_text(
            render(f"包 · {display}", facts + md_body(blurb, True, base="../"),
                   "packages", base="../"),
            encoding="utf-8")
        rows.append(
            f'<tr><td><a href="packages/{slug}.html">{html.escape(display)}</a></td>'
            f'<td><code>{html.escape(name)}</code></td>'
            f'<td>{html.escape(ver)}</td>'
            f'<td>{html.escape(abi)}</td>'
            f'<td>{count}</td></tr>')

    intro = (
        "<p>Dial 的官方包生态。标准库 <code>stdlib</code> 随编译器发布、编译时自动链入；"
        "其余均为独立 <code>.dialpkg</code>，用 <code>import \"包名\";</code> 引入、"
        "<code>dialc install</code> 安装（按 Target Triple 索引本平台静态库，并对契约与"
        "机器码做 SHA256 强制校验，残缺 / 被篡改绝不进入链接器）。下表事实直接从各包源文件"
        "提取，包升级后重生成官网即自动同步。</p>"
    )
    table = ("<table><thead><tr><th>包</th><th>name</th><th>版本</th><th>ABI</th>"
             f"<th>extern fn</th></tr></thead><tbody>" + "".join(rows) + "</tbody></table>")
    (WEB / "packages.html").write_text(
        render("包 · 索引", intro + table, "packages"), encoding="utf-8")
    print(f"  packages: {len(_PKG_SPECS)} 个包介绍页 + 1 个索引页")


def main() -> None:
    WEB.mkdir(exist_ok=True)
    ASSETS.mkdir(exist_ok=True)
    (ASSETS / "style.css").write_text(CSS, encoding="utf-8")

    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")

    (WEB / "spec.html").write_text(
        render("语言规范", md_body(readme, True, strip_web_prefix=True), "spec"),
        encoding="utf-8")
    build_changelog(changelog)
    build_packages()

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
        ("判空 · 多消费者安全 · 超时（v0.9）", "is_null(x) 让用户能分辨「没有值」与「真实值」——这是 Dial 此前完全缺失的能力；recv_timeout(ch, ms) 用 pthread_cond_timedwait 实现限时接收，绝不无限期阻塞。更重要的是修掉了一个真实死锁：通道通知此前用 signal 只唤醒一个等待者，多消费者下会让其余消费者带着失效条件睡死（对照实验：signal 版本 25 秒挂死，broadcast 版本 4.25 秒跑完 100 万元素）。"),
        ("多路复用 select（v0.91）", "select { case recv(ch) as v { ... } default { ... } } —— Dial 风格：接收永远是函数调用（没有 Go 的 <- 运算符），绑定用 as 关键字，select 是语句不是表达式。等待是事件驱动的（全局事件纪元 + 条件变量广播），实测 50 万次收发 wall 0.19s；多通道同时就绪时随机选取以防饿死。全体 EOF 时 select 直接结束、不执行任何分支。"),
        ("char 类型（v0.92）", "char 是「一个 Unicode 码点」：值类型、无 ARC、无空指针，与 int 同为 i64 但类型独立。字面量写作 'a' / '\\n' / '中'。可比较（按码点序）、可经 char_to_int / int_to_char 零成本互转；但绝不参与 + - * /，也不与 int 混用——'a' + 1 的意图无法判定，源码必须把意图写清楚。print(c) 输出字符本身（按 UTF-8 编码），非法码点当场 Fail-Fast 终止。"),
        ("import 与 .dialpkg 包分发（v0.93）", "import \"包名\"; 即可引入预编译好的机器码——不写路径、不写平台、不写版本。dialc install 把 .dialpkg 解压到 ~/.dial/packages/（项目目录零侵入），按 Target Triple 索引出本平台的 .a / .lib 自动链入，并对 .diali 签名与那份机器码做 SHA256 强制校验：残缺或被篡改的二进制绝不进入链接器。包用 type Db; 声明不透明句柄——你永远拿不到内部布局，这正是「库作者随意重构、调用方零改动」的根基。"),
        ("包安装三条硬规则（v0.94 收尾）", "dialc install 的边界钉死：① 同名包 <b>ABI 相同则覆盖、不同则报错要求先卸载</b>，拒绝「同名不同 ABI 共存」重新打开版本冲突的口子；② targets 的库文件 <b>只允许静态库（.a / .lib），拒绝一切 .so / .dll 动态库</b>——Dial 产物的依赖契约是只链 libc，动态库会破坏它；③ 卸载无需专用命令，直接 rm -rf ~/.dial/packages/&lt;名&gt;/ 即可（缓存是纯派生数据，随时可重装）。三条均被测试 J 组锁死。"),
        ("dialc lsp 语言服务器（v0.94 → v0.95）", "编辑器里的红线与 dialc build 的报错是同一条错误、同一个位置、同一句文案——因为语言服务器直接调用编译器的词法 / 语法 / 语义，不另写第二套解析器（两套实现必然慢慢长歪）。保存即报诊断，语义高亮用的也是编译器自己的词法器。附带免 npm 安装的 VS Code 扩展：LSP 客户端是手写的 200 行，拷进扩展目录即可用。v0.95 在骨架之上补上了跨文件符号表，把悬停、跳转定义、基础补全一并做齐。"),
        ("LSP 增强：Hover / 跳转定义 / 补全（v0.95）", "一座独立于类型检查、可容错的符号表（src/index.rs）：半句没写完的代码也能索引，Hover 与补全照样有反应。作用域链「由内向外」查找，所以 if 块里 let x: int 遮蔽外层 let x: string 在悬停 / 定义 / 补全三处都查的是内层那个；顺着 import 自动扫描 ~/.dial/packages/ 把包的不透明类型与 extern fn 装进内存符号表，Hover 包函数时直接渲染 .diali 的 /// 文档并标注「外部函数」，跳转定义对包符号直接送进 .diali 的声明行。均由测试组 K 端到端锁死。"),
        ("包 URL 安装 + 一键安装（v0.96）", "包可以直接从 URL 装：dialc install https://.../pkg.dialpkg —— 不用先手动下载。只允许 https（本机除外），明文 http 能被中间人改写，而 SHA256 挡不住「换了内容还带着正确哈希」的伪造，所以宁可装不上也不降级。--sha256 可钉死整包哈希，比对发生在解压之前；而无论给不给，包里每个文件都要按 manifest 登记的哈希先验一遍——校验先于解压，一个字节都还没落到磁盘。另有 install-dial.sh / .ps1 一键脚本：初始化环境（编译器 + stdlib + runtime + PATH）→ 自动重新扫描并装上目录里的所有 .dialpkg——包就在那里，脚本看得见，用户不必记住包名。"),
        ("三重开源许可（含许可隔离）", "GPL-3.0 OR MIT OR Apache-2.0 —— 三者任选其一遵守。三份协议原文与说明文档随源码及发布包完整分发。<b>许可按组件划分</b>：编译器前端（dialc 本体）为三重许可；但被静态链进你程序的标准库 libdial_std.a <b>强制锁定为 MIT 或 Apache-2.0，绝不允许以 GPL 条款链接进闭源程序</b>——工具可以是 GPL，被链进你程序里的运行时绝不能是。"),
        ("三平台原生构建（v0.97）", "Linux x86_64 · Windows x64-MinGW · Termux aarch64-musl 全静态——三者均为 CI 在各平台宿主上<b>直接编译</b>：编译期目标即运行期平台，没有交叉编译的 ABI 陷阱。Windows 走官方 MSVC 构建的 LLVM 18 全量包，但只取纯 C ABI 的 C 接口，跨 MSVC / MinGW 无名字改写冲突；Termux 侧 musl 全静态，用户程序零外部依赖。"),
        ("LSP 工业化 + 三编辑器接入（v0.98）", "dialc lsp 分四轮打磨到工业级：<b>基础稳健</b>（能力协商、补全增强）·<b>导航全家桶</b>（文档大纲 / 工作区符号 / 引用查找 / 重命名 / 签名帮助）·<b>编写辅助</b>（保守格式化 / 参数 inlay 提示）·<b>高级能力</b>（代码动作 / 折叠 / 选择范围 / 文档链接）。诊断带稳定错误码、缺包会明确报出并给「一键安装这个包」的 quickfix。编辑器侧交付 VS Code（手写 LSP 客户端，零 npm）、Neovim（内置客户端，零依赖）与 Emacs（eglot 原生连，不开发插件）三套接入。"),
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
      <a class="btn-ghost" href="packages.html">浏览官方包</a>
      <a class="btn-ghost" href="changelog.html">查看更新日志</a>
      <a class="btn-ghost" href="bench.html">性能基准（4 语言 × 28 维）</a>
    </div>
    <div class="kpis">
      <span><b>{VERSION}</b> 当前版本</span>
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
  <p class="muted" style="text-align:center;margin-bottom:12px">
    ⚠️ 三个平台各有各的运行时依赖（详见 <a href="spec.html">语言规范</a> 与 Releases 说明）：
    <b>Linux x86_64</b>（动态 glibc，需系统装 libLLVM-18）·
    <b>Windows x64-MinGW</b>（随包分发 LLVM-C.dll）·
    <b>Termux aarch64</b>（musl 全静态，无外部依赖）。
    三者均为<b>原生构建</b>——CI 在各平台宿主上直接编译，编译期目标即运行期平台，没有交叉编译的 ABI 陷阱。
  </p>
  <pre><code># 获取源码
git clone https://github.com/shichencike/Dial.git
cd Dial                                # 仓库根即为编译器本体（Cargo.toml 与 src/ 都在根）

# 构建编译器（需要本机 LLVM 18 开发库）
export LLVM_SYS_181_PREFIX=/usr/lib/llvm-18
cargo build --release                  # 产物：target/release/dialc

# 让编译器本体能找到 libLLVM（仅影响编译器，不影响你编出来的程序）
export LD_LIBRARY_PATH=/usr/lib/llvm-18/lib:$LD_LIBRARY_PATH
./target/release/dialc --version

# 编译并运行一个 Dial 程序
./target/release/dialc run examples/hello.dial

# 端到端自测（正向 + 负向 + 产物契约 + LSP + 包安装等 A–Z+AA+AB 二十八组）
./tests/run_tests.sh</code></pre>
  <h2>最小示例</h2>
  <pre><code>fn main(): int {{
    let name: string = "world";
    print("hello, ".concat(name));   // 字符串没有 + 运算符：拼接必须显式调用 concat
    return 0;
}}</code></pre>
  <p class="muted" style="text-align:center">
    下载发布包：<a class="ext" href="https://github.com/shichencike/Dial/releases/latest/download/dialc-v{VERSION_SHORT}-linux-x86_64.zip">dialc-v{VERSION_SHORT}-linux-x86_64.zip</a>
    （Windows / Termux 见 <a class="ext" href="https://github.com/shichencike/Dial/releases">Releases</a>；或自行 clone 仓库后构建）<br>
    发布包由 <a class="ext" href="https://github.com/shichencike/Dial/actions/workflows/ci.yml" target="_blank" rel="noopener">GitHub Actions</a>
    自动构建：装 LLVM 18 → 编译 → 跑全部 A–Z+AA+AB 二十八组端到端测试 → 打包上传 Release。
  </p>
</section>
{FOOTER}
</body>
</html>
"""


if __name__ == "__main__":
    main()
