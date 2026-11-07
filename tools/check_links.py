#!/usr/bin/env python3
"""内部锚点一致性检查 / 修复工具。

背景：README.md 里的手写锚点（如 `#145-cow-的触发时机`）必须与官网生成器
`tools/build_web.py` 的 `dial_slugify` **逐字节一致**，否则同一份链接会
「在 GitHub 上能跳、在官网上跳不动」。两者现在都按 GitHub（github-slugger）的
规则生成，因此本工具用同一套规则来校验。

用法：
    python3 tools/check_links.py            # 只检查，报告失效锚点
    python3 tools/check_links.py --fix      # 检查并自动修正可修正的失效锚点

修正原理：锚点与标题的「宽松键」（去掉全部非字母数字字符后的串）一一对应，
据此把失效锚点替换成当前正确的锚点；无法唯一对应的会单独列出来人工处理。
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# 与 build_web.dial_slugify 完全一致（GitHub / github-slugger 语义）
def gh_slug(value: str, separator: str = "-") -> str:
    v = value.strip().lower()
    v = re.sub(r"[^\w\-\s]", "", v, flags=re.UNICODE)
    return v.replace(" ", separator)


def loose(value: str) -> str:
    """宽松键：去掉全部非字母数字字符，用于容忍连字符位置的差异。"""
    return re.sub(r"[^\w]", "", value.lower(), flags=re.UNICODE)


HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*$", re.M)

# 行内代码（`...`）与围栏代码块（```...```）里的内容不是链接，必须排除，
# 否则正文里「写出 `](#x)` 这种示例」会被误判成失效链接。
# 替换成等长空白，保证行号与列号不变。
FENCE_RE = re.compile(r"```.*?```", re.S)
INLINE_CODE_RE = re.compile(r"`[^`\n]*`")


def strip_code(text: str) -> str:
    text = FENCE_RE.sub(lambda m: " " * len(m.group(0)), text)
    return INLINE_CODE_RE.sub(lambda m: " " * len(m.group(0)), text)


def headings(text: str):
    """返回 [(标题, 锚点)]，同名标题按出现顺序加 -1 / -2 后缀（与 GitHub 一致）。"""
    seen = {}
    out = []
    for m in HEADING_RE.finditer(text):
        title = m.group(2)
        anchor = gh_slug(title)
        if anchor in seen:
            seen[anchor] += 1
            anchor = f"{anchor}-{seen[anchor]}"
        else:
            seen[anchor] = 0
        out.append((title, anchor))
    return out


def alias_map(hs):
    """宽松键 -> 规范锚点。"""
    amap = {}
    for title, anchor in hs:
        amap.setdefault(loose(anchor), anchor)
        amap.setdefault(loose(title), anchor)
    return amap


def check(path: Path, external_readme=None):
    text = path.read_text(encoding="utf-8")
    hs = headings(text)
    amap = alias_map(hs)
    valid = {a for _, a in hs}

    broken = []
    for m in re.finditer(r"\]\(#([^)]+)\)", text):
        a = m.group(1)
        if a not in valid:
            fixed = amap.get(loose(a))
            broken.append((text[: m.start()].count("\n") + 1, a, fixed))
    return text, hs, broken


def fix(path: Path):
    text, hs, broken = check(path)
    amap = alias_map(hs)
    unresolved = [(l, a) for l, a, f in broken if not f]
    changes = 0

    def repl(m):
        nonlocal changes
        a = m.group(1)
        if a in {x[1] for x in hs}:
            return m.group(0)
        fixed = amap.get(loose(a))
        if fixed:
            changes += 1
            return f"](#{fixed})"
        return m.group(0)

    new_text = re.sub(r"\]\(#([^)]+)\)", repl, text)
    if changes:
        path.write_text(new_text, encoding="utf-8")
    return changes, unresolved


def main() -> int:
    do_fix = "--fix" in sys.argv
    readme = ROOT / "README.md"
    changelog = ROOT / "CHANGELOG.md"

    readme_text = readme.read_text(encoding="utf-8")
    readme_hs = headings(readme_text)
    readme_valid = {a for _, a in readme_hs}
    readme_alias = alias_map(readme_hs)

    rc = 0
    for path in (readme, changelog):
        text = path.read_text(encoding="utf-8")
        hs = headings(text)
        amap = alias_map(hs)
        valid = {a for _, a in hs}
        # CHANGELOG.md 里的 `](#x)` 指的是 **README.md** 的章节（CHANGELOG 自己没有这些标题），
        # 因此跨文件解析到 README，并在修好后改写成 `](README.md#x)`。
        cross = path is changelog

        broken = []
        scan = strip_code(text)
        for m in re.finditer(r"\]\(#([^)]+)\)", scan):
            a = m.group(1)
            if a in valid:
                continue
            if cross:
                fixed = readme_alias.get(loose(a))
                prefix = "README.md#"
            else:
                fixed = amap.get(loose(a))
                prefix = ""
            broken.append((text[: m.start()].count("\n") + 1, a, prefix + fixed if fixed else None))

        # 另一种坏法：链接目标**漏了 `#`**，写成 `](slug)` 而非 `](#slug)`。
        # 这类目标看起来像锚点却不是锚点，点击会跳到相对路径 404。
        # 只在「目标既不是仓库里真实存在的文件、也不像 URL」时才判定为漏写。
        repo_files = {str(p.relative_to(ROOT)) for p in ROOT.rglob("*")
                      if p.is_file() and ".git" not in p.parts and "target" not in p.parts}
        for m in re.finditer(r"\]\(([^)#][^)]*)\)", scan):
            tgt = m.group(1)
            if tgt.startswith(("http", "mailto:", "../", "./", "/")):
                continue
            if tgt in repo_files or tgt.split("/")[-1] in repo_files:
                continue
            if not re.fullmatch(r"[a-z0-9\u00c0-\uffff\-_]+", tgt):
                continue          # 含大写/点号等，更像路径而非锚点
            line = text[: m.start()].count("\n") + 1
            broken.append((line, tgt, "（漏了 #，应为 #" + tgt + "）"))

        print(f"== {path.name}：标题 {len(hs)} 个")
        if not broken:
            print("   ✅ 所有内部锚点均可解析")
            continue
        print(f"   ❌ 失效锚点 {len(broken)} 处")
        for line, a, fixed in broken:
            hint = f"  -> 应为 {fixed}" if fixed else "  -> 无法自动对应，需人工处理"
            print(f"      行 {line}: #{a}{hint}")
        rc = 1

        if do_fix:
            # 按偏移量回写，避免误改行内代码里的「示例链接」。
            # strip_code 保持等长，所以 scan 的偏移与 text 完全一致。
            edits = []
            unresolved = []
            for m in re.finditer(r"\]\(#([^)]+)\)", scan):
                a = m.group(1)
                if a in valid:
                    continue
                fixed = (readme_alias if cross else amap).get(loose(a))
                if not fixed:
                    unresolved.append(a)
                    continue
                # 同文件链接必须带 `#`；跨文件链接用 `README.md#`。
                prefix = "README.md#" if cross else "#"
                new = f"]({prefix}{fixed})"
                edits.append((m.start(), m.end(), new))

            if edits:
                buf = text
                for start, end, new in reversed(edits):
                    buf = buf[:start] + new + buf[end:]
                path.write_text(buf, encoding="utf-8")
            print(f"   🔧 已自动修正 {len(edits)} 处"
                  + (f"，{len(unresolved)} 处需人工处理" if unresolved else ""))
    # ---- 跨文件锚点：](README.md#x) 必须能在 README.md 的标题里解析 ----------
    # CHANGELOG.md 大量引用 README 章节。手写锚点一旦与标题脱节，GitHub 与官网
    # 会同时点不动（两处 slug 同源，见 gh_slug / build_web.dial_slugify）。
    # 本段只检查、不自动改写：跨文件目标应人工确认，避免把链接悄悄指错章节。
    for path in (readme, changelog):
        text = path.read_text(encoding="utf-8")
        scan = strip_code(text)
        broken = []
        for m in re.finditer(r"\]\(README\.md#([^)]+)\)", scan):
            a = m.group(1)
            if a not in readme_valid:
                broken.append((text[: m.start()].count("\n") + 1, a,
                               readme_alias.get(loose(a))))
        label = f"== {path.name}：跨文件 README.md# 锚点"
        if not broken:
            print(f"{label} ✅ 全部可解析")
            continue
        print(f"{label} ❌ 失效 {len(broken)} 处")
        for line, a, fixed in broken:
            hint = f"  -> 应为 {fixed}" if fixed else "  -> 无法自动对应，需人工处理"
            print(f"      行 {line}: #{a}{hint}")
        rc = 1

    # ---- 官网产物：href 重写是否彻底 ----------------------------------------
    # build_web.py 会把指向仓库根 .md / 目录的相对 href 重写为站内页或 GitHub
    # 绝对链接。这里守住两件事：不得有残留；站内 spec.html# 锚点必须真实存在。
    web = ROOT / "web"
    spec_html = web / "spec.html"
    if spec_html.exists():
        spec_ids = set(re.findall(r'id="([^"]+)"',
                                  spec_html.read_text(encoding="utf-8")))
        repo_prefixes = (
            "README.md", "CHANGELOG.md", "ARCHITECTURE.md", "LICENSE",
            "Cargo.toml", "examples/", "editors/", "third_party/",
            "tools/", "tests/", "stdlib/", "runtime/", "packages/",
        )
        site_problems = []
        for p in sorted(list(web.glob("*.html")) + list(web.glob("changelog/*.html"))):
            t = p.read_text(encoding="utf-8")
            for href in re.findall(r'href="([^"]+)"', t):
                if any(href.startswith(pre) for pre in repo_prefixes):
                    site_problems.append((p.relative_to(ROOT),
                                          f"残留仓库根相对链接 {href}"))
                if href.startswith("spec.html#") or href.startswith("../spec.html#"):
                    a = href.split("#", 1)[1]
                    if a not in spec_ids:
                        site_problems.append((p.relative_to(ROOT),
                                              f"spec.html 锚点不存在 #{a}"))
        print("== web/：官网 href 重写检查")
        if site_problems:
            print(f"   ❌ {len(site_problems)} 处问题")
            for p, msg in site_problems[:20]:
                print(f"      {p}: {msg}")
            rc = 1
        else:
            print("   ✅ 无残留仓库根相对链接，站内 spec.html# 锚点全部可解析")
    return rc


if __name__ == "__main__":
    sys.exit(main())
