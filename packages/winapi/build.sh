#!/usr/bin/env bash
#
# 构建 winapi.dialpkg 的三平台静态库（Windows 真实现 + Linux / musl 桩实现）。
#
# 产物：packages/winapi/lib/<triple>/libwinapi.a
#   - winapi_impl.o   Win32 薄封装（装载 + 调用 + UTF-8→UTF-16 + 错误槽），单源文件
#
# 平台库一律**运行时动态装载**（Win32 LoadLibraryA / GetProcAddress）：
#   - .a 自包含，Dial 侧不需要写 link，链接期也不需要 -luser32 / -lgdi32；
#   - Windows 目标：user32 / gdi32 运行时解析，kernel32 装载器直接链接（总是已加载）；
#   - 非 Windows 目标：编译进桩实现，每个函数返回 -1 / 0 并置 WINAPI_ERR_PLATFORM。
#
# 全部用 zig cc 交叉编译，保证三个平台的机器码出自同一套工具链。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKG="$ROOT/packages/winapi"
OUT="$PKG/lib"

# 目标："zig cc 的目标名"与"manifest 里的 triple 键"不是同一套命名，必须成对列出。
# 写成 "zig目标:manifest键:平台族" 的三元组。
TARGETS=(
    "x86_64-linux-gnu:x86_64-linux-gnu:posix"
    "aarch64-linux-musl:aarch64-unknown-linux-musl:posix"
    "x86_64-windows-gnu:x86_64-pc-windows-gnu:win32"
)

# 可复现性三件套（与 gui / http 包同口径）：
#   -ffile-prefix-map：规范化 __FILE__ 里的仓库绝对路径；
#   -g0：关掉 zig cc 在 COFF 目标下默认生成的 CodeView 调试段（含随机 obj 名）；
#   SOURCE_DATE_EPOCH：固定 COFF 文件头的 TimeDateStamp。
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}"
CFLAGS_COMMON=(-O2 -fPIC -Wall -Wextra -g0)
FP_PREFIX=(-ffile-prefix-map="$ROOT=dialc")

for entry in "${TARGETS[@]}"; do
    IFS=: read -r t triple family <<< "$entry"
    echo "==> 构建 $t（manifest 键 $triple，后端族 $family）"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT

    INCS=(-I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime")
    SRCS=("$PKG/src/winapi_impl.c")

    for f in "${SRCS[@]}"; do
        b="$(basename "$f" .c)"
        zig cc -target "$t" -c "$f" \
            "${INCS[@]}" "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" \
            -o "$work/$b.o"
    done

    mkdir -p "$OUT/$triple"
    rm -f "$OUT/$triple/libwinapi.a"
    zig ar rcs "$OUT/$triple/libwinapi.a" "$work"/*.o
    rm -rf "$work"
    trap - EXIT
    echo "    -> $OUT/$triple/libwinapi.a ($(du -h "$OUT/$triple/libwinapi.a" | cut -f1))"
done

echo "✅ winapi 包三平台静态库构建完成（win32 真实现 / posix 桩实现）"
echo "   下一步：dialc pack packages/winapi -o winapi.dialpkg"
