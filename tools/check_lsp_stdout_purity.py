#!/usr/bin/env python3
"""检查 `dialc lsp` 的协议流纯净性（静态守卫）。

背景
----
LSP 协议跑在 **stdout** 上：`Content-Length: N\r\n\r\n` + N 字节，一条接一条。
只要 stdout 上出现任何**非协议字节**（哪怕只是一行 `println!` 的日志），
客户端的分帧就会错位——症状不是"报错"，而是**卡死**（客户端在等一条永远对不上的消息）。

真实事故：`resolve_imports()`（CLI 与 LSP 共用）里有两处 `println!`，
于是"打开一个 import 了已安装包的文档"就会让编辑器彻底卡住。

本脚本把纪律固化成**可执行的检查**，而不是靠人记住：

规则一：**库模块零 stdout 打印**
    lsp / index / semantic / parser / lexer / ast 这几个模块是 LSP 运行时直接跑的，
    里面不允许出现 `println!` / `print!`。

规则二：**CLI 文件里被 LSP 调用的函数，函数体内零 stdout 打印**
    package.rs / main.rs 是 CLI 与 LSP 共用的，它们的 CLI 专属函数可以打印，
    但**被 LSP 调用到的那些函数**必须干净（打印要走注入的日志出口）。

用法：
    python3 tools/check_lsp_stdout_purity.py [--src src]

退出码 0 = 干净；非 0 = 有违规（逐条列出）。
"""
import argparse
import os
import re
import sys

# 规则一：这些模块整体不允许 stdout 打印。
PURE_MODULES = [
    "lsp.rs",
    "index.rs",
    "semantic.rs",
    "parser.rs",
    "lexer.rs",
    "ast.rs",
]

# 规则二：CLI 文件中被 LSP 调用到的函数（函数名 -> 文件名）。
# 这些函数体内不允许 stdout 打印。
LSP_CALLED_FUNCTIONS = {
    "package.rs": [
        "find_installed",
        "verify_iface",
        "parse_diali",
        "resolve_type_name",
        "resolve_lib",
        "host_triple",
        "cache_root",
    ],
    "main.rs": [
        "resolve_imports",
    ],
}

# stdout 打印宏。eprintln!/dbg! 走 stderr，不算违规。
PRINT_MACROS = re.compile(r"(?<![A-Za-z0-9_])(println!|print!)")


def strip_comments_and_strings(text: str) -> str:
    """把注释与字符串字面量抹成等长空白，避免注释/字符串里的 print! 被误判。"""
    out = list(text)
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        # 行注释
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            while i < n and text[i] != "\n":
                out[i] = " "
                i += 1
            continue
        # 块注释（可嵌套）
        if c == "/" and i + 1 < n and text[i + 1] == "*":
            depth = 0
            while i < n:
                if text[i] == "/" and i + 1 < n and text[i + 1] == "*":
                    depth += 1
                    out[i] = out[i + 1] = " "
                    i += 2
                    continue
                if text[i] == "*" and i + 1 < n and text[i + 1] == "/":
                    depth -= 1
                    out[i] = out[i + 1] = " "
                    i += 2
                    if depth == 0:
                        break
                    continue
                if text[i] != "\n":
                    out[i] = " "
                i += 1
            continue
        # 字符串字面量（含 raw string）
        if c == '"':
            out[i] = " "
            i += 1
            while i < n:
                if text[i] == "\\":
                    out[i] = " "
                    if i + 1 < n:
                        out[i + 1] = " "
                    i += 2
                    continue
                if text[i] == '"':
                    out[i] = " "
                    i += 1
                    break
                if text[i] != "\n":
                    out[i] = " "
                i += 1
            continue
        i += 1
    return "".join(out)


def line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


def find_function_bodies(text: str, fname: str):
    """返回 [(函数名, 起始行, 函数体文本)]：定位顶层 fn 的花括号范围。"""
    results = []
    pattern = re.compile(
        r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:async[ \t]+)?fn[ \t]+" + re.escape(fname) + r"\b",
        re.MULTILINE,
    )
    for m in pattern.finditer(text):
        # 从 fn 关键字往后找第一个 '{'
        i = m.end()
        while i < len(text) and text[i] != "{":
            if text[i] == ";":
                i = -1
                break
            i += 1
        if i < 0:
            continue
        depth = 0
        start = i
        while i < len(text):
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        results.append((fname, line_of(text, m.start()), text[start : i + 1]))
    return results


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", default="src", help="源码目录（默认 src）")
    args = ap.parse_args()

    problems = []

    # ---- 规则一：库模块整体零 stdout 打印 ----
    for mod in PURE_MODULES:
        path = os.path.join(args.src, mod)
        if not os.path.exists(path):
            continue
        raw = open(path, encoding="utf-8").read()
        clean = strip_comments_and_strings(raw)
        for m in PRINT_MACROS.finditer(clean):
            problems.append(
                f"{path}:{line_of(clean, m.start())}: 库模块出现 {m.group(1)}——"
                f"LSP 的 stdout 是协议流，日志请走 stderr 或注入的日志出口"
            )

    # ---- 规则二：CLI 文件里被 LSP 调用的函数，函数体内零 stdout 打印 ----
    for fname, fns in LSP_CALLED_FUNCTIONS.items():
        path = os.path.join(args.src, fname)
        if not os.path.exists(path):
            continue
        raw = open(path, encoding="utf-8").read()
        clean = strip_comments_and_strings(raw)
        for fn in fns:
            for _, start_line, body in find_function_bodies(clean, fn):
                for m in PRINT_MACROS.finditer(body):
                    problems.append(
                        f"{path}:{start_line}: 函数 {fn}() 内部出现 {m.group(1)}——"
                        f"该函数被 LSP 调用，打印会污染协议流，请改用注入的日志出口"
                    )

    if problems:
        print("✗ LSP 协议流纯净性检查未通过：")
        for p in problems:
            print(f"  {p}")
        sys.exit(1)

    total = len(PURE_MODULES) + sum(len(v) for v in LSP_CALLED_FUNCTIONS.values())
    print(f"✅ LSP 协议流纯净性检查通过（{total} 处规则点，stdout 无裸输出）")
    sys.exit(0)


if __name__ == "__main__":
    main()
