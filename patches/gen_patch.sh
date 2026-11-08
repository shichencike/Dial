#!/usr/bin/env bash
#
# 生成 / 校验 mbedTLS 3.6.2 的「ClientHello 重排钩子」外置 patch。
#
# 背景：third_party/README.md 明令「不要手工修改 third_party/ 里的任何文件」。
# 但浏览器一致性画像需要改 mbedTLS 的 ClientHello 编排（扩展顺序 / GREASE /
# 哑扩展），公开 conf API 做不到。折中方案是：
#   - 上游源码原样保留在 third_party/（一个字节都不改）；
#   - 差异以 patch 文件存在 patches/，可审计、可重放；
#   - packages/http/build.sh 在构建期把 mbedTLS 复制到临时目录再打 patch，
#     产物出自「上游 + patch」，third_party/ 始终干净。
#
# 本脚本是 patch 的**唯一生成口径**：改了 upstream 或想改钩子，改这里的
# Python 替换逻辑再跑 --write；CI 跑 --check 确认 patch 未漂移。
#
# 用法：
#   bash patches/gen_patch.sh --write    # 重新生成 patch + SHA256SUMS
#   bash patches/gen_patch.sh --check    # 校验 upstream 完整性 + patch 可重放（CI）
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TP="$ROOT/third_party"
OUT="$ROOT/patches"
PATCH="$OUT/mbedtls-3.6.2-clienthello.patch"
SUMS="$OUT/mbedtls-3.6.2.SHA256SUMS"
UPSTREAM="$TP/mbedtls/library/ssl_client.c"
MODE="${1:---write}"

case "$MODE" in
    --write|--check) ;;
    *) echo "用法: $0 [--write|--check]" >&2; exit 2 ;;
esac

# ---- 1) 上游基线完整性：全量 311 个文件的 SHA256 ----
# 用 find + sort 保证跨机器顺序一致；路径相对 third_party/，sha256sum -c 时
# 在 third_party/ 下执行即可定位。
gen_sums() {
    ( cd "$TP" && find mbedtls -type f | LC_ALL=C sort | xargs sha256sum ) > "$SUMS"
}

if [ "$MODE" = "--check" ]; then
    ( cd "$TP" && sha256sum -c "$SUMS" ) >/dev/null
    echo "upstream_sha256 = pass"
else
    gen_sums
fi

# ---- 2) 在临时副本上构造「已打 patch」的 ssl_client.c ----
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/a/library" "$work/b/library"
cp "$UPSTREAM" "$work/a/library/ssl_client.c"
cp "$UPSTREAM" "$work/b/library/ssl_client.c"

python3 - "$work/b/library/ssl_client.c" <<'PY'
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as f:
    s = f.read()

# (a) 唯一新增的 include：包内钩子接口，编译期由 build.sh 加 -I packages/http/src。
inc_old = '#include "ssl_debug_helpers.h"\n'
inc_new = (
    '#include "ssl_debug_helpers.h"\n'
    '\n'
    '/* Dial browser-fingerprint reshape hook (external patch, see patches/README.md).\n'
    ' * This is the ONLY upstream header touched; the hook call itself lives at the\n'
    ' * end of ssl_write_client_hello_body(). */\n'
    '#include "dial_clienthello_fp.h"\n'
)
if s.count(inc_old) != 1:
    raise SystemExit("gen_patch: include 锚点不唯一/缺失，upstream 可能已变")
s = s.replace(inc_old, inc_new, 1)

# (b) 钩子：body 写完、回写 out_len 之后、return 之前。上层用 *out_len 做
#     add_hs_hdr_to_checksum / update_checksum / finish_handshake_msg，故改长度
#     后 transcript 自动一致，密钥生成、HKDF、sent_extensions mask 全不动。
hook_old = (
    '    *out_len = (size_t) (p - buf);\n'
    '    return 0;\n'
    '}\n'
    '\n'
    'MBEDTLS_CHECK_RETURN_CRITICAL\n'
    'static int ssl_generate_random(mbedtls_ssl_context *ssl)\n'
)
hook_new = (
    '    *out_len = (size_t) (p - buf);\n'
    '\n'
    '    /* Dial ClientHello reshape hook. Runs after the body is complete and\n'
    '     * before the caller computes the handshake checksum / writes the\n'
    '     * handshake header. Fail-safe: on any error dial_fp_reshape_clienthello()\n'
    '     * leaves the buffer untouched and mbedTLS native ClientHello goes out. */\n'
    '    {\n'
    '        size_t fp_len = 0;\n'
    '        if (dial_fp_reshape_clienthello(buf, *out_len,\n'
    '                                        (size_t) (end - buf),\n'
    '                                        &fp_len) == 0) {\n'
    '            *out_len = fp_len;\n'
    '        }\n'
    '    }\n'
    '\n'
    '    return 0;\n'
    '}\n'
    '\n'
    'MBEDTLS_CHECK_RETURN_CRITICAL\n'
    'static int ssl_generate_random(mbedtls_ssl_context *ssl)\n'
)
if s.count(hook_old) != 1:
    raise SystemExit("gen_patch: 钩子锚点不唯一/缺失，upstream 可能已变")
s = s.replace(hook_old, hook_new, 1)

with open(path, "w", encoding="utf-8") as f:
    f.write(s)
print("patched_ssl_client_c = generated")
PY

# ---- 3) 生成 unified diff（路径带 a/ b/，配合 patch -p1）----
set +e
( cd "$work" && diff -u \
    --label a/library/ssl_client.c \
    --label b/library/ssl_client.c \
    a/library/ssl_client.c b/library/ssl_client.c ) > "$work/patch"
diff_rc=$?
set -e
if [ "$diff_rc" -gt 1 ]; then
    echo "gen_patch: diff 失败 (rc=$diff_rc)" >&2
    exit 1
fi

if [ "$MODE" = "--check" ]; then
    if ! cmp -s "$work/patch" "$PATCH"; then
        echo "patch_replay = FAIL（patches/*.patch 与当前 upstream + 钩子定义不一致）" >&2
        diff -u "$PATCH" "$work/patch" | head -60 >&2 || true
        exit 1
    fi
    echo "patch_replay = pass"
    # 打一遍 dry-run，确认上下文仍可应用
    cp -a "$TP/mbedtls" "$work/mbedtls"
    patch -p1 -d "$work/mbedtls" --dry-run < "$PATCH" >/dev/null
    echo "patch_dry_run = pass"
else
    cp "$work/patch" "$PATCH"
    echo "写出 $PATCH"
    echo "写出 $SUMS（$(wc -l < "$SUMS") 个文件）"
fi
