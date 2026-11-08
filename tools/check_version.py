#!/usr/bin/env python3
"""校验「当前版本」在各处一致（Cargo.toml / README / CHANGELOG / 官网产物）。

为什么需要这道检查
------------------
v0.97 → v0.98 这一轮发布时，代码、测试、CHANGELOG 都是新的，对外文档却不是：

| 位置 | 声称的版本 | 实际最新 |
| --- | --- | --- |
| README 首行标题 | v0.97 | v0.98 |
| 官网首页 KPI「当前版本」 | v0.96 | v0.98 |
| 官网首页下载链接 | `dialc-v0.96.zip` | 该资产早已不存在（三平台分包后改名） |
| 官网各页 footer | v0.96 | v0.98 |

而 CI 当时是**绿的**——因为没有任何一处检查版本号是否一致。
**没有检查 = 一定会漂**，所以本脚本把「当前版本」钉成**一个出处**（Cargo.toml），
再检查所有对外文档都跟上了。

用法：
    python3 tools/check_version.py

退出码 0 = 一致；非 0 = 有不一致（逐条列出）。
"""
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def cargo_version() -> tuple[str, str]:
    """从 Cargo.toml 取 (显示版本, 完整版本)。

    显示版本遵循 README §23 的版本号规则：
      - 常规发布只取主.次两位，如 "v0.98"；
      - 补丁发布（第三位非零，如 0.98.1）显示完整三位 "v0.98.1"，
        因为补丁位必须显式可见（见 §23「唯一例外」）。
    """
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^version\s*=\s*"([0-9]+(?:\.[0-9]+)*)"', cargo, re.M)
    if not m:
        sys.exit("✗ 无法从 Cargo.toml 解析版本号")
    full = m.group(1)
    parts = full.split(".")
    if len(parts) >= 3 and parts[2] != "0":
        display = "v" + full            # 补丁发布：显示完整三位
    else:
        # v1.00 里程碑：Cargo 的 semver 不允许前导零（"1.00.0" 非法），故存储为
        # "1.0.0"、对外按用户指定的里程碑名显示 "v1.00"（次版本补零到两位）。
        # 主版本仍为 0 时保持原样（v0.98 / v0.99 ...），不补零。
        major = parts[0]
        minor = parts[1] if len(parts) >= 2 else "0"
        if int(major) >= 1:
            minor = minor.zfill(2)
        display = "v" + major + "." + minor
    return display, full


def main() -> None:
    ver, ver_full = cargo_version()
    problems: list[str] = []

    def check(label: str, ok: bool, detail: str = "") -> None:
        if not ok:
            problems.append(f"{label}：{detail}" if detail else label)

    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")

    # 1) README 首行标题
    first = readme.split("\n")[0]
    check("README 首行标题的版本号", ver in first, f"首行是「{first}」，应为 {ver}")

    # 2) CHANGELOG 最新版本标题
    heads = re.findall(r"^## \[(v[0-9.]+)\]", changelog, re.M)
    check("CHANGELOG 最新版本标题", bool(heads) and heads[0] == ver,
          f"最新是「{heads[0] if heads else '无'}」，应为 {ver}")

    # 3) 官网产物（web/ 由 tools/build_web.py 生成，版本取自 Cargo.toml）
    for name in ("index.html", "spec.html", "changelog.html"):
        path = ROOT / "web" / name
        if not path.exists():
            problems.append(f"web/{name} 不存在——请先跑 python3 tools/build_web.py")
            continue
        html = path.read_text(encoding="utf-8")
        m = re.search(r'class="muted">(v[0-9.]+) · Rust', html)
        check(f"web/{name} footer 的版本号", bool(m) and m.group(1) == ver,
              f"是 {m.group(1) if m else '未找到'}，应为 {ver}")

    idx_path = ROOT / "web" / "index.html"
    if idx_path.exists():
        idx = idx_path.read_text(encoding="utf-8")
        m = re.search(r"<span><b>(v[0-9.]+)</b> 当前版本</span>", idx)
        check("web/index.html KPI 的版本号", bool(m) and m.group(1) == ver,
              f"是 {m.group(1) if m else '未找到'}，应为 {ver}")
        # 首页的下载链接应指向当前版本的三平台分包。
        check("web/index.html 下载链接指向当前版本",
              f"dialc-v{ver_full}-linux-x86_64.zip" in idx,
              f"未找到 dialc-v{ver_full}-linux-x86_64.zip")
        # 首页不该再提供单包名 `dialc-v<版本>.zip`——三平台分包后已无此资产。
        # （**只查首页**：changelog 里的旧包名是**历史发布说明**，那一期确实叫这名字。）
        stale = re.findall(r"dialc-v[0-9.]+\.zip", idx)
        check("web/index.html 不引用已废弃的单包名", not stale,
              f"仍有 {sorted(set(stale))}（三平台分包后无此资产）")

    # 4) VS Code 扩展的版本号：扩展是随编译器走的 LSP 客户端，版本必须跟上。
    #    漂了的后果很具体：README 与安装脚本里的 `dial-<版本>.vsix` 推导名
    #    会与实际产物对不上（v1.04 就出现过：扩展还停在 0.98.0，
    #    README 却写 `dial-1.4.0.vsix`——指向一个不存在的文件）。
    pkg_path = ROOT / "editors" / "vscode" / "package.json"
    if pkg_path.exists():
        ext_ver = json.loads(pkg_path.read_text(encoding="utf-8")).get("version", "")
        check("editors/vscode/package.json 的版本号", ext_ver == ver_full,
              f"是 {ext_ver}，应为 {ver_full}")
        want = f"dial-{ext_ver}.vsix"

        # 4a) README 的编辑器支持表那一行必须指向当前版本的 .vsix。
        #     （只查那一行：README 别处出现的 `dial-1.3.0.vsix` 是讲资产命名规则的**举例**，
        #       那是历史事实，不该被版本检查当成漂移。）
        row = [ln for ln in readme.split("\n")
               if "VS Code" in ln and ".vsix" in ln]
        got = sorted({m for ln in row for m in re.findall(r"dial-[0-9.]+\.vsix", ln)})
        check("README 编辑器支持表的 .vsix 文件名", got == [want],
              f"是 {got}，应为 [{want}]")

        # 4b) 扩展自己的 README 里出现的 .vsix 名必须全部是当前版本。
        ext_readme = ROOT / "editors" / "vscode" / "README.md"
        if ext_readme.exists():
            got2 = sorted(set(re.findall(r"dial-[0-9.]+\.vsix",
                                         ext_readme.read_text(encoding="utf-8"))))
            check("editors/vscode/README.md 的 .vsix 文件名", got2 == [want],
                  f"是 {got2}，应为 [{want}]")

    if problems:
        print(f"✗ 版本号一致性检查未通过（当前版本 {ver}）：")
        for p in problems:
            print(f"  {p}")
        sys.exit(1)
    print(f"✅ 版本号一致性检查通过（当前版本 {ver}，完整 {ver_full}）")


if __name__ == "__main__":
    main()
