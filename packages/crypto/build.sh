#!/usr/bin/env bash
#
# 构建 crypto.dialpkg 的三平台静态库。
#
# 产物：packages/crypto/lib/<triple>/libcrypto.a
#   - dial_crypto.o  FFI 包装层（DialString/bytes ↔ 裸字节、错误诊断、hex 转换）
#   - mt_*.o         mbedTLS 全量（摘要 md/sha*/md5/sha3、Base64 及 md.c 的
#                    PSA/ripemd160 依赖闭包由配置宏自动引入，故按全量归档）
#
# 为什么不只挑几个 .c：本仓库 mbedtls_config.h 打开了 MBEDTLS_PSA_CRYPTO_C 与
# MBEDTLS_RIPEMD160_C，md.c 会引用 psa_* 与 ripemd160 符号；只编摘要子集会在
# 链接期缺符号。静态库是**按需抽取**的，全量归档不会让用户可执行文件变大，
# 只是包体积略增——与 http 包同一口径，且三平台零警告已验证。
#
# 全部用 zig cc 交叉编译，保证三个平台的机器码出自同一套工具链。
# 依赖源码在 third_party/（已 vendor，构建完全离线可复现）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKG="$ROOT/packages/crypto"
TP="$ROOT/third_party"
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
    t="${pair%%:*}"          # zig cc 目标名
    triple="${pair##*:}"     # manifest 里的 triple 键
    echo "==> 构建 $t（manifest 键 $triple）"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT

    # 1) FFI 包装层：需要 stdlib 的 dial_string.h / dial_bytes.h（只取头文件，
    #    实现符号在链接期由 stdlib.dialpkg 提供）+ mbedTLS 头。
    zig cc -target "$t" -c "$PKG/src/dial_crypto.c" \
        -I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime" \
        -I "$TP/mbedtls/include" -I "$TP/mbedtls/3rdparty/everest/include" \
        -I "$TP/mbedtls/3rdparty/p256-m" \
        "${CFLAGS_COMMON[@]}" -o "$work/dial_crypto.o"

    # 2) mbedTLS 全量（与 http 包同一命令，配置宏由 build_info.h 自动引入）。
    for f in "$TP"/mbedtls/library/*.c; do
        b="$(basename "$f" .c)"
        zig cc -target "$t" -c "$f" \
            -I "$TP/mbedtls/include" \
            -I "$TP/mbedtls/3rdparty/everest/include" \
            -I "$TP/mbedtls/3rdparty/p256-m" \
            "${CFLAGS_COMMON[@]}" -o "$work/mt_$b.o"
    done

    mkdir -p "$OUT/$triple"
    rm -f "$OUT/$triple/libcrypto.a"
    zig ar rcs "$OUT/$triple/libcrypto.a" "$work"/*.o
    rm -rf "$work"
    trap - EXIT
    echo "    -> $OUT/$triple/libcrypto.a ($(du -h "$OUT/$triple/libcrypto.a" | cut -f1))"
done

echo "✅ crypto 包三平台静态库构建完成"
echo "   下一步：dialc pack packages/crypto -o crypto.dialpkg"
