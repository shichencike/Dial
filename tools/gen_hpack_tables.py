#!/usr/bin/env python3
"""生成 packages/http/src/dial_hpack_tables.h —— HPACK 静态表 + Huffman 表。

数据源用 Python `hpack` 库内置的权威常量（等价于 RFC 7541 Appendix A/B），
避免手敲 257 项 Huffman 码字出错。生成物是纯 C 数据，编进静态库、零运行期依赖。

用法（在仓库根目录）：
    python3 tools/gen_hpack_tables.py

依赖：pip install hpack（仅**生成时**需要；构建与运行 http.dialpkg 不需要 Python）。
"""
import inspect
import textwrap
from pathlib import Path

from hpack import huffman_constants as H
from hpack import table as T


def c_literal(b: bytes) -> str:
    """把一段字节转成 C 字符串字面量（ASCII 可打印直出，其余 \\xNN 转义）。"""
    out = ['"']
    for ch in b.decode("latin1"):
        o = ord(ch)
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif 32 <= o < 127:
            out.append(ch)
        else:
            out.append("\\x%02x" % o)
    out.append('"')
    return "".join(out)


def extract_static_table():
    """从 hpack.table 源码里取出类属性 STATIC_TABLE 元组字面量并求值。"""
    src = inspect.getsource(T)
    start = src.find("STATIC_TABLE = (")
    end = src.find("\n    )", start)
    ns = {}
    exec(textwrap.dedent(src[start : end + 6]), ns)
    return ns["STATIC_TABLE"]


def main():
    st = extract_static_table()
    assert len(st) == 61, "静态表应为 61 项（RFC 7541 Appendix A）"
    assert len(H.REQUEST_CODES) == 257 and len(H.REQUEST_CODES_LENGTH) == 257

    L = []
    L.append("/* 本文件由 tools/gen_hpack_tables.py 自动生成，请勿手改。")
    L.append(" * 数据源：Python hpack 库内置常量（等价 RFC 7541 Appendix A/B）。 */")
    L.append("#ifndef DIAL_HPACK_TABLES_H")
    L.append("#define DIAL_HPACK_TABLES_H")
    L.append("#include <stdint.h>")
    L.append("")
    L.append("typedef struct { const char *name; const char *value; int name_len; int value_len; } dh_hpack_static_entry;")
    L.append("")
    L.append("/* HPACK 静态表，索引 1..61。下标 0 留空以便直接用 1-based 索引。 */")
    L.append("static const dh_hpack_static_entry k_hpack_static[62] = {")
    L.append('    { "", "", 0, 0 },  /* 0 保留 */')
    for i, (n, v) in enumerate(st, start=1):
        L.append("    { %s, %s, %d, %d },  /* %d */" % (
            c_literal(n), c_literal(v), len(n), len(v), i))
    L.append("};")
    L.append("#define DH_HPACK_STATIC_COUNT 61")
    L.append("")
    L.append("/* Huffman 码表：code[sym] 右对齐码字，len[sym] 比特数。sym 256 = EOS（仅编码时补齐用）。 */")
    L.append("static const uint32_t k_huff_code[257] = {")
    for i in range(0, 257, 6):
        L.append("    " + ", ".join("0x%08xU" % c for c in H.REQUEST_CODES[i : i + 6]) + ",")
    L.append("};")
    L.append("static const uint8_t k_huff_len[257] = {")
    for i in range(0, 257, 12):
        L.append("    " + ", ".join("%d" % c for c in H.REQUEST_CODES_LENGTH[i : i + 12]) + ",")
    L.append("};")
    L.append("")
    L.append("#endif /* DIAL_HPACK_TABLES_H */")

    out = Path(__file__).resolve().parent.parent / "packages/http/src/dial_hpack_tables.h"
    out.write_text("\n".join(L) + "\n", encoding="utf-8")
    print("wrote", out, "(%d static, 257 huffman entries)" % len(st))


if __name__ == "__main__":
    main()
