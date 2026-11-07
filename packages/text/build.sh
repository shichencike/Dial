#!/usr/bin/env bash
#
# 构建 text.dialpkg 的三平台静态库。
#
# 产物：packages/text/lib/<triple>/libtext.a
#   - dial_text.o    FFI 包装层（DialString 转换 / 句柄生命周期 / 错误槽 / 替换）
#   - dial_regex.o   自研 Thompson NFA 编译器 + Pike VM 执行器（纯 C）
#
# 全部用 zig cc 交叉编译，保证三个平台的机器码出自同一套工具链。
# 本包**零第三方依赖**：正则引擎自研，运行时只引用标准库已有的
# dial_string_* / dial_bytes_* 符号（由链接器在 stdlib.dialpkg 里解析）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKG="$ROOT/packages/text"
OUT="$PKG/lib"

# 目标：**zig cc 的目标名**与**manifest 里的 triple 键**不是同一套命名，
# 必须成对列出——前者给 zig cc，后者要逐字等于编译器 package::host_triple()
# 的取值，否则用户装上包后编译器按 host_triple() 选不到机器码。
TARGETS=(
    "x86_64-linux-gnu:x86_64-linux-gnu"
    "aarch64-linux-musl:aarch64-unknown-linux-musl"
    "x86_64-windows-gnu:x86_64-pc-windows-gnu"
)

CFLAGS_COMMON=(-O2 -fPIC -Wall -Wextra)

for pair in "${TARGETS[@]}"; do
    t="${pair%%:*}"
    triple="${pair##*:}"
    echo "==> 构建 $t（manifest 键 $triple）"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT

    # FFI 包装层需要 stdlib 的 dial_string.h / dial_bytes.h（只取头文件，
    # 实现符号在链接期由 stdlib.dialpkg 提供）。
    zig cc -target "$t" -c "$PKG/src/dial_text.c" \
        -I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime" \
        "${CFLAGS_COMMON[@]}" -o "$work/dial_text.o"

    # 正则引擎是纯 C，不需要任何 Dial 头。
    zig cc -target "$t" -c "$PKG/src/dial_regex.c" \
        -I "$PKG/src" \
        "${CFLAGS_COMMON[@]}" -o "$work/dial_regex.o"

    mkdir -p "$OUT/$triple"
    rm -f "$OUT/$triple/libtext.a"
    zig ar rcs "$OUT/$triple/libtext.a" "$work"/*.o
    rm -rf "$work"
    trap - EXIT
    echo "    -> $OUT/$triple/libtext.a ($(du -h "$OUT/$triple/libtext.a" | cut -f1))"
done

echo "✅ text 包三平台静态库构建完成"
echo "   下一步：dialc pack packages/text -o text.dialpkg"
