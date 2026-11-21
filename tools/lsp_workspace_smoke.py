#!/usr/bin/env python3
"""dialc lsp 跨工作区全量模块图冒烟测试（v1.21）。

验证 initialize 带 rootUri 后：
  1. 工作区内未打开的 .dial 文件也被索引（workspace/symbol 可见其顶层符号）；
  2. references 对跨文件调用的全局符号，返回**两个文件**的位置；
  3. rename 对全局符号产出跨文件的 documentChanges（每文件一条 TextDocumentEdit）；
  4. 局部符号（Var/Param）仍限本文件，不跨文件误改。

复用 tools/lsp_full_smoke.py 的 LspClient（协议分帧 + 超时看门狗 + 污染检测）。
退出码 0 = 全部通过。
"""
import argparse
import json
import os
import pathlib
import sys
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from lsp_full_smoke import LspClient  # noqa: E402

FAILED = []


def ok(msg):
    print(f"  ✅ {msg}")


def bad(msg):
    FAILED.append(msg)
    print(f"  ❌ {msg}")


def uri_of(path: pathlib.Path) -> str:
    return "file://" + str(path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dialc", default="target/release/dialc")
    args = ap.parse_args()
    dialc = str(pathlib.Path(args.dialc).resolve())

    with tempfile.TemporaryDirectory() as td:
        root = pathlib.Path(td)
        util = root / "util.dial"
        mainf = root / "main.dial"
        util.write_text(
            "fn shared_helper(): int { return 42; }\n"
            "fn util_only(): int { return 7; }\n",
            encoding="utf-8",
        )
        mainf.write_text(
            'import "./util.dial";\n'
            "fn main(): int {\n"
            "    let v: int = shared_helper();\n"
            "    return v;\n"
            "}\n",
            encoding="utf-8",
        )

        c = LspClient(dialc)
        try:
            # 1) initialize 带 rootUri → 触发工作区全量扫描。
            init = c.request("initialize", {
                "processId": None,
                "rootUri": uri_of(root),
                "capabilities": {
                    "workspace": {"workspaceEdit": {"documentChanges": True}}
                },
                "workspaceFolders": [{"uri": uri_of(root), "name": "root"}],
            })
            if not isinstance(init, dict) or "capabilities" not in init:
                bad("initialize 未返回 capabilities")
            else:
                ok("initialize 带 rootUri/workspaceFolders 成功")

            c.notify("initialized", {})

            # 2) workspace/symbol：未 didOpen 的 util.dial 顶层符号应被索引到。
            syms = c.request("workspace/symbol", {"query": "util_only"}) or []
            hit_util = any(
                s.get("name") == "util_only"
                and s.get("location", {}).get("uri") == uri_of(util)
                for s in syms
            )
            if hit_util:
                ok("workspace/symbol 找到未打开文件的顶层符号（全量索引生效）")
            else:
                bad(f"workspace/symbol 未找到 util_only（syms={syms}）")

            # 3) didOpen main.dial，在 shared_helper() 调用处查 references。
            main_uri = uri_of(mainf)
            main_text = mainf.read_text(encoding="utf-8")
            c.notify("textDocument/didOpen", {
                "textDocument": {
                    "uri": main_uri, "languageId": "dial", "version": 1,
                    "text": main_text,
                }
            })
            # shared_helper 调用在第 3 行（0 基 2），"let v: int = " 之后。
            line = 2
            char = main_text.splitlines()[line].index("shared_helper") + 2
            refs = c.request("textDocument/references", {
                "textDocument": {"uri": main_uri},
                "position": {"line": line, "character": char},
                "context": {"includeDeclaration": True},
            }) or []
            uris = {r.get("uri") for r in refs}
            if uri_of(util) in uris and main_uri in uris:
                ok(f"references 跨 2 文件命中（{len(uris)} 个文件：定义 + 调用）")
            else:
                bad(f"references 未跨文件（uris={uris}, refs={refs}）")

            # 4) rename：documentChanges 应含两个文件。
            ren = c.request("textDocument/rename", {
                "textDocument": {"uri": main_uri},
                "position": {"line": line, "character": char},
                "newName": "renamed_helper",
            }) or {}
            changes = ren.get("documentChanges") or []
            change_uris = {d.get("textDocument", {}).get("uri") for d in changes}
            if uri_of(util) in change_uris and main_uri in change_uris:
                ok(f"rename documentChanges 跨 2 文件（{len(changes)} 条 TextDocumentEdit）")
            else:
                bad(f"rename 未跨文件（change_uris={change_uris}, ren={ren}）")

            # 5) 局部符号（v）rename 只改本文件一条。
            vchar = main_text.splitlines()[line].index("let v") + 4
            ren2 = c.request("textDocument/rename", {
                "textDocument": {"uri": main_uri},
                "position": {"line": line, "character": vchar},
                "newName": "value",
            }) or {}
            changes2 = ren2.get("documentChanges") or []
            uris2 = {d.get("textDocument", {}).get("uri") for d in changes2}
            if uris2 == {main_uri}:
                ok("局部符号 rename 仅限本文件（不跨文件误改）")
            else:
                bad(f"局部符号 rename 跨文件了（uris2={uris2}）")

            # stdout 纯净性
            if c.pollution:
                bad(f"stdout 协议污染：{c.pollution[:2]}")
            else:
                ok("stdout 协议流纯净")
        finally:
            c.close()

    if FAILED:
        print(f"\n[workspace smoke] ❌ {len(FAILED)} 项失败")
        for f in FAILED:
            print("  -", f)
        sys.exit(1)
    print("\n[workspace smoke] ✅ 全部通过")


if __name__ == "__main__":
    main()
