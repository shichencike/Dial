#!/usr/bin/env bash
#
# 构建 http.dialpkg 的三平台静态库。
#
# 产物：packages/http/lib/<triple>/libhttp.a
#   - dial_http.o   自研 HTTP/1.1 层（协议解析 / Header 拼装 / Socket 读写）
#   - z_*.o         zlib 核心（gzip / deflate 解压）
#   - mt_*.o        mbedTLS 全量（TLS 本轮暂未向 Dial 暴露，先把静态库备好）
#
# 全部用 zig cc 交叉编译，保证三个平台的机器码出自同一套工具链。
# 依赖源码在 third_party/（已 vendor，构建完全离线可复现）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKG="$ROOT/packages/http"
TP="$ROOT/third_party"
OUT="$PKG/lib"

TARGETS=(x86_64-linux-gnu aarch64-linux-musl x86_64-windows-gnu)

# zlib 只取内存解压/压缩用到的核心文件；gz*.c 是「读 .gz 文件」的 API，
# 依赖 POSIX lseek，与 HTTP 无关（且编不过 Windows / musl 目标）。
ZLIB_SRCS=(adler32.c crc32.c deflate.c infback.c inffast.c inflate.c inftrees.c trees.c uncompr.c zutil.c compress.c)

CFLAGS_COMMON=(-O2 -fPIC -Wall -Wextra)

for t in "${TARGETS[@]}"; do
    echo "==> 构建 $t"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT

    # 1) 自研 HTTP 层（需要 stdlib 的 dial_string.h / dial_bytes.h）
    zig cc -target "$t" -c "$PKG/src/dial_http.c" \
        -I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime" -I "$TP/zlib" \
        "${CFLAGS_COMMON[@]}" -o "$work/dial_http.o"

    # 2) zlib 核心
    for f in "${ZLIB_SRCS[@]}"; do
        zig cc -target "$t" -c "$TP/zlib/$f" -I "$TP/zlib" \
            "${CFLAGS_COMMON[@]}" -o "$work/z_${f%.c}.o"
    done

    # 3) mbedTLS 全量
    for f in "$TP"/mbedtls/library/*.c; do
        b="$(basename "$f" .c)"
        zig cc -target "$t" -c "$f" \
            -I "$TP/mbedtls/include" \
            -I "$TP/mbedtls/3rdparty/everest/include" \
            -I "$TP/mbedtls/3rdparty/p256-m" \
            "${CFLAGS_COMMON[@]}" -o "$work/mt_$b.o"
    done

    mkdir -p "$OUT/$t"
    rm -f "$OUT/$t/libhttp.a"
    zig ar rcs "$OUT/$t/libhttp.a" "$work"/*.o
    rm -rf "$work"
    trap - EXIT
    echo "    -> $OUT/$t/libhttp.a ($(du -h "$OUT/$t/libhttp.a" | cut -f1))"
done

echo "✅ http 包三平台静态库构建完成"
echo "   下一步：dialc pack packages/http -o http.dialpkg"
