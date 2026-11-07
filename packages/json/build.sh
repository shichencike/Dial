#!/usr/bin/env bash
#
# 构建 json.dialpkg 的三平台静态库。
#
# 产物：packages/json/lib/<triple>/libjson.a
#   - dial_json.o        FFI 包装层（DialString 转换 / 句柄生命周期 / 访问器 / 构造器 / 错误槽）
#   - dial_json_codec.o  自研递归下降解析 + 紧凑序列化 + 深拷贝（纯 C）
#
# 全部用 zig cc 交叉编译，保证三个平台的机器码出自同一套工具链。
# 本包**零第三方依赖**：JSON 协议层自研，运行时只引用标准库已有的
# dial_string_* 符号（由链接器在 stdlib.dialpkg 里解析）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKG="$ROOT/packages/json"
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

    # 两个翻译单元都需要 stdlib 的 dial_string.h（只取头文件，
    # 实现符号在链接期由 stdlib.dialpkg 提供）。
    zig cc -target "$t" -c "$PKG/src/dial_json.c" \
        -I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime" \
        "${CFLAGS_COMMON[@]}" -o "$work/dial_json.o"

    zig cc -target "$t" -c "$PKG/src/dial_json_codec.c" \
        -I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime" \
        "${CFLAGS_COMMON[@]}" -o "$work/dial_json_codec.o"

    mkdir -p "$OUT/$triple"
    rm -f "$OUT/$triple/libjson.a"
    zig ar rcs "$OUT/$triple/libjson.a" "$work"/*.o
    rm -rf "$work"
    trap - EXIT
    echo "    -> $OUT/$triple/libjson.a ($(du -h "$OUT/$triple/libjson.a" | cut -f1))"
done

echo "✅ json 包三平台静态库构建完成"
echo "   下一步：dialc pack packages/json -o json.dialpkg"
