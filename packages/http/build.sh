#!/usr/bin/env bash
#
# 构建 http.dialpkg 的三平台静态库。
#
# 产物：packages/http/lib/<triple>/libhttp.a
#   - dial_http.o            自研 HTTP/1.1 层（协议解析 / Header 拼装 / Socket 读写 / keep-alive 池）
#   - dial_tls.o             mbedTLS 会话封装（TLS 1.3，证书校验 fail-closed）
#   - dial_http2.o           HTTP/2 帧层 + 会话/stream 状态机（TLS h2 + 明文 h2c）
#   - dial_hpack.o           自研 HPACK（静态表 + Huffman 解码 + 动态表）
#   - dial_clienthello_fp.o  ClientHello 报文重排（浏览器一致性画像，包内实现）
#   - z_*.o                  zlib 核心（gzip / deflate 解压）
#   - mt_*.o                 mbedTLS 全量（构建期副本 + 外置 patch）
#
# 全部用 zig cc 交叉编译，保证三个平台的机器码出自同一套工具链。
# 依赖源码在 third_party/（已 vendor，构建完全离线可复现）。
#
# 浏览器一致性画像：mbedTLS 3.6.2 的公开 API 无法表达 Chrome 的 ClientHello
# 编排（扩展顺序 / GREASE / 哑扩展 / 去 SCSV）。按 third_party/README.md 的
# 「不要手改 third_party/」铁律，构建期把 mbedTLS 复制到临时目录、应用
# patches/mbedtls-3.6.2-clienthello.patch，产物出自「上游 + patch」，
# third_party/ 全程零改动。详见 patches/README.md。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKG="$ROOT/packages/http"
TP="$ROOT/third_party"
OUT="$PKG/lib"
PD="$ROOT/patches"
SUMFILE="$PD/mbedtls-3.6.2.SHA256SUMS"
PATCHFILE="$PD/mbedtls-3.6.2-clienthello.patch"

for f in "$SUMFILE" "$PATCHFILE"; do
    [ -f "$f" ] || { echo "缺少 $f（先跑 bash patches/gen_patch.sh --write）" >&2; exit 1; }
done

# 上游基线校验：构建前后各一次。前者保证 patch 上下文没漂移，后者证明
# third_party/ 从未被本脚本或任何手工操作改动。
verify_upstream() {
    ( cd "$TP" && sha256sum -c "$SUMFILE" ) >/dev/null
}
verify_upstream
echo "==> 上游基线完整性校验通过（$(wc -l < "$SUMFILE") 个文件）"

# 目标：**zig cc 的目标名**与**manifest 里的 triple 键**不是同一套命名，
# 必须成对列出——前者给 zig cc，后者要逐字等于编译器 package::host_triple()
# 的取值（x86_64-linux-gnu / aarch64-unknown-linux-musl / x86_64-pc-windows-gnu），
# 否则用户装上包后编译器按 host_triple() 选不到机器码。
# 写成 "zig目标:manifest键" 的成对列表。
TARGETS=(
    "x86_64-linux-gnu:x86_64-linux-gnu"
    "aarch64-linux-musl:aarch64-unknown-linux-musl"
    "x86_64-windows-gnu:x86_64-pc-windows-gnu"
)

# zlib 只取内存解压/压缩用到的核心文件；gz*.c 是「读 .gz 文件」的 API，
# 依赖 POSIX lseek，与 HTTP 无关（且编不过 Windows / musl 目标）。
ZLIB_SRCS=(adler32.c crc32.c deflate.c infback.c inffast.c inflate.c inftrees.c trees.c uncompr.c zutil.c compress.c)

# 可复现性（三平台逐字节一致）三件套：
#   -ffile-prefix-map：规范化 __FILE__ 里的 mktemp 副本路径与仓库根路径；
#   -g0：zig cc 在 COFF（x86_64-pc-windows-gnu）目标下默认生成 CodeView 调试段，
#        会把全局缓存的随机临时 obj 名（zig/tmp/<随机 hex>-xxx.obj）与随内容
#        变化的 section 哈希写进每一个 .o。实测关掉调试信息后这些段消失，
#        而 .pdata / .xdata（Windows x64 异常处理必需）仍完整保留；
#   SOURCE_DATE_EPOCH：固定 COFF 文件头的 TimeDateStamp，双保险。
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}"
CFLAGS_COMMON=(-O2 -fPIC -Wall -Wextra -g0)

for pair in "${TARGETS[@]}"; do
    t="${pair%%:*}"          # zig cc 目标名
    triple="${pair##*:}"     # manifest 里的 triple 键
    echo "==> 构建 $t（manifest 键 $triple）"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT

    # 0) mbedTLS 上游副本 + 外置 patch。绝不修改 third_party/ 本身。
    cp -a "$TP/mbedtls" "$work/mbedtls"
    MT="$work/mbedtls"

    # 可复现性：mbedTLS 开了 MBEDTLS_DEBUG_C，ssl_debug_helpers.h / common.h 的
    # __FILE__ 会把**源文件绝对路径**编进 .o；用 mktemp 副本后这个路径每次不同，
    # 两次构建的 .a 就不逐字节一致了。-ffile-prefix-map 把副本路径与仓库根路径
    # 都规范化，产物只含相对路径，与构建发生在哪台机器、哪个临时目录无关。
    FP_PREFIX=(-ffile-prefix-map="$MT=mbedtls" -ffile-prefix-map="$ROOT=dialc")
    patch -p1 -d "$MT" --dry-run < "$PATCHFILE" >/dev/null
    patch -p1 -d "$MT"          < "$PATCHFILE" >/dev/null
    echo "    [fp] 已应用 ClientHello 钩子 patch（仅副本，third_party/ 未改动）"

    # 1) 自研 HTTP 层 + TLS 封装（需要 stdlib 的 dial_string.h / dial_bytes.h，
    #    以及 mbedTLS 头；头文件统一指向已打补丁的副本，保证产物同源）。
    zig cc -target "$t" -c "$PKG/src/dial_http.c" \
        -I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime" -I "$TP/zlib" \
        -I "$MT/include" -I "$MT/3rdparty/everest/include" \
        -I "$MT/3rdparty/p256-m" \
        "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" -o "$work/dial_http.o"
    zig cc -target "$t" -c "$PKG/src/dial_tls.c" \
        -I "$PKG/src" -I "$MT/include" -I "$MT/3rdparty/everest/include" \
        -I "$MT/3rdparty/p256-m" \
        "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" -o "$work/dial_tls.o"

    # HTTP/2 层：帧/会话（dial_http2.o）+ 自研 HPACK（dial_hpack.o）。
    # dial_http2.c 需要 stdlib 的 dial_string.h 与 dial_tls.h；平台线程原语
    # （POSIX pthread / Win32 CRITICAL_SECTION+CONDITION_VARIABLE）由目标工具链
    # 自带，静态归档无需额外链接参数。
    zig cc -target "$t" -c "$PKG/src/dial_http2.c" \
        -I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime" \
        -I "$MT/include" -I "$MT/3rdparty/everest/include" \
        -I "$MT/3rdparty/p256-m" \
        "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" -o "$work/dial_http2.o"
    zig cc -target "$t" -c "$PKG/src/dial_hpack.c" \
        -I "$PKG/src" \
        "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" -o "$work/dial_hpack.o"

    # ClientHello 报文重排模块：包内 C，只做字节编排，不碰密码学。
    # ssl_client.c（副本）通过 -I "$PKG/src" 找到它的头文件并调用钩子。
    zig cc -target "$t" -c "$PKG/src/dial_clienthello_fp.c" \
        -I "$PKG/src" \
        "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" -o "$work/dial_clienthello_fp.o"

    # 2) zlib 核心（未打补丁，仍从 third_party/ 直接编译）
    for f in "${ZLIB_SRCS[@]}"; do
        zig cc -target "$t" -c "$TP/zlib/$f" -I "$TP/zlib" \
            "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" -o "$work/z_${f%.c}.o"
    done

    # 3) mbedTLS 全量 —— 编译**副本**里的 .c，头文件也指向副本。
    #    只有 ssl_client.c 被 patch 过，其余与上游逐字节相同。
    for f in "$MT"/library/*.c; do
        b="$(basename "$f" .c)"
        zig cc -target "$t" -c "$f" \
            -I "$MT/include" \
            -I "$MT/3rdparty/everest/include" \
            -I "$MT/3rdparty/p256-m" \
            -I "$PKG/src" \
            "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" -o "$work/mt_$b.o"
    done

    mkdir -p "$OUT/$triple"
    rm -f "$OUT/$triple/libhttp.a"
    zig ar rcs "$OUT/$triple/libhttp.a" "$work"/*.o
    rm -rf "$work"
    trap - EXIT
    echo "    -> $OUT/$triple/libhttp.a ($(du -h "$OUT/$triple/libhttp.a" | cut -f1))"
done

verify_upstream
echo "==> 构建后复查：third_party/ 仍与上游基线一致（零改动）"
echo "✅ http 包三平台静态库构建完成"
echo "   下一步：dialc pack packages/http -o http.dialpkg"
