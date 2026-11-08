/*
 * check_hpack.c —— HPACK / Huffman 编码侧往返自校验（期 2）。
 *
 * 为什么需要：期 2 把请求侧 HPACK 从「0x00 明文」换成「静态表索引 + Huffman」。
 * 手写 Huffman 编码器最容易错在补齐位与多字节累积，一旦错了，真实 h2 服务端
 * 会直接判 HPACK 协议错。这里用**本模块自己既有的完整解码器**做往返验证：
 *   encode → decode → 逐字节/逐头比对，解码器是独立于本次改动的既有实现。
 *
 * 编译（Linux 本机验证/回归用）：
 *   zig cc -target x86_64-linux-gnu tests/pkg/fingerprint/check_hpack.c \
 *       -I packages/http/src \
 *       packages/http/lib/x86_64-linux-gnu/libhttp.a -o /tmp/check_hpack
 */
#include "dial_hpack.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int g_fail = 0;

static void ok(int cond, const char *name) {
    printf("%s = %s\n", name, cond ? "pass" : "FAIL");
    if (!cond) g_fail = 1;
}

/* Huffman 往返：编码 → 解码 → 与原文逐字节一致。 */
static int huff_roundtrip(const char *s) {
    dh_bytes enc = {0}, dec = {0};
    int n = (int) strlen(s);
    if (dh_huffman_encode(s, n, &enc) != 0) { dh_bytes_free(&enc); return 0; }
    int r = dh_huffman_decode(enc.p, enc.len, &dec);
    int same = (r == 0 && dec.len == n &&
                (n == 0 || memcmp(dec.p, s, (size_t) n) == 0));
    dh_bytes_free(&enc);
    dh_bytes_free(&dec);
    return same;
}

#define ADD(list, n, v) dh_hlist_add(&(list), n, (int) strlen(n), v, (int) strlen(v))

int main(void) {
    /* 1) Huffman 逐例往返，覆盖空串、单字符、静态表值、完整 UA / sec-ch-ua /
     *    Accept 长串、含引号与问号的值、URL 风格串、标点全集。 */
    static const char *cases[] = {
        "",
        "a",
        "GET",
        "https",
        "127.0.0.1:12345",
        "?1",
        "\"Windows\"",
        "gzip, deflate",
        "en-US,en;q=0.9",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
        "\"Chromium\";v=\"144\", \"Google Chrome\";v=\"144\", \"Not_A Brand\";v=\"24\"",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,"
        "image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
        "0123456789abcdefghijklmnopqrstuvwxyz-._~:/?#[]@!$&'()*+,;=%",
    };
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        char name[64];
        snprintf(name, sizeof(name), "huff_roundtrip_%02zu", i);
        ok(huff_roundtrip(cases[i]), name);
    }

    /* 2) 全 256 个字节值（含 0x00 与 0xff）一次编码/解码，确认码表覆盖完整。 */
    char all[256];
    for (int i = 0; i < 256; i++) all[i] = (char) (unsigned char) i;
    {
        dh_bytes enc = {0}, dec = {0};
        ok(dh_huffman_encode(all, 256, &enc) == 0, "huff_all_bytes_encode");
        ok(dh_huffman_decode(enc.p, enc.len, &dec) == 0 &&
           dec.len == 256 && memcmp(dec.p, all, 256) == 0,
           "huff_all_bytes_roundtrip");
        dh_bytes_free(&enc);
        dh_bytes_free(&dec);
    }

    /* 3) HPACK 编码 → 解码往返：覆盖 indexed / indexed-name / new-name 三条路径。 */
    dh_header_list req, out;
    dh_hlist_init(&req);
    dh_hlist_init(&out);
    ADD(req, ":method", "GET");                 /* 完全命中静态表 → indexed */
    ADD(req, ":scheme", "https");               /* 完全命中静态表 → indexed */
    ADD(req, ":authority", "127.0.0.1:8443");  /* name 命中 → indexed-name */
    ADD(req, ":path", "/index.html?q=1");      /* name 命中 → indexed-name */
    ADD(req, "user-agent", "Chrome/144");      /* name 命中 → indexed-name */
    ADD(req, "accept-encoding", "gzip, deflate");  /* 完全命中 → indexed */
    ADD(req, "sec-fetch-mode", "navigate");    /* 未命中 → new-name */
    ADD(req, "sec-ch-ua-platform", "\"Windows\""); /* 未命中 → new-name */

    dh_bytes block = {0};
    ok(dh_hpack_encode(&block, &req) == 0, "hpack_encode");

    dh_hpack_ctx ctx;
    dh_hpack_ctx_init(&ctx, 4096);
    char err[256];
    err[0] = '\0';
    int n = dh_hpack_decode(&ctx, block.p, block.len, &out, err, sizeof(err));
    ok(n == req.n, "hpack_decode_count");
    int same = (n == req.n);
    for (int i = 0; same && i < req.n; i++) {
        if (strcmp(req.v[i].name, out.v[i].name) != 0 ||
            strcmp(req.v[i].value, out.v[i].value) != 0) same = 0;
    }
    ok(same, "hpack_roundtrip_values");

    /* 4) 首字节必须是 indexed（0x80+）——录制器据此判定 static_index。 */
    ok(block.len > 0 && (block.p[0] & 0x80) != 0, "hpack_first_is_indexed");

    /* 5) 块里必须存在带 H=1 的字符串长度字节（Huffman 生效的直接证据）。
     *    HPACK 块是若干表示的顺序拼接，逐条跳过表示头与字符串即可定位每个
     *    字符串的首字节；只要出现过 0x80 高位，就说明至少一段用了 Huffman。 */
    int huffman_seen = 0, static_seen = 0;
    int64_t p = 0;
    while (p < block.len) {
        unsigned char first = block.p[p];
        if (first & 0x80) {
            static_seen = 1;
            /* indexed：7 位整数，读掉续字节 */
            int64_t v = first & 0x7f;
            p++;
            if (v == 0x7f) {
                while (p < block.len && (block.p[p] & 0x80)) p++;
                if (p < block.len) p++;
            }
            continue;
        }
        if ((first & 0xE0) == 0x20) {   /* dynamic table size update */
            int64_t v = first & 0x1f;
            p++;
            if (v == 0x1f) {
                while (p < block.len && (block.p[p] & 0x80)) p++;
                if (p < block.len) p++;
            }
            continue;
        }
        int prefix = ((first & 0xC0) == 0x40) ? 6 : 4;
        int max = (1 << prefix) - 1;
        int64_t name_idx = first & max;
        p++;
        if (name_idx == max) {
            int m = 0;
            while (p < block.len) {
                unsigned char b = block.p[p++];
                name_idx += (int64_t) (b & 0x7f) << m;
                if (!(b & 0x80)) break;
                m += 7;
            }
        }
        int nstr = (name_idx == 0) ? 2 : 1;   /* new name → 两个串；否则只 value */
        for (int k = 0; k < nstr && p < block.len; k++) {
            unsigned char sh = block.p[p];
            if (sh & 0x80) huffman_seen = 1;
            int64_t slen = sh & 0x7f;
            p++;
            if (slen == 0x7f) {
                int m = 0;
                while (p < block.len) {
                    unsigned char b = block.p[p++];
                    slen += (int64_t) (b & 0x7f) << m;
                    if (!(b & 0x80)) break;
                    m += 7;
                }
            }
            p += slen;
        }
    }
    ok(huffman_seen, "hpack_huffman_bit_seen");
    ok(static_seen, "hpack_static_index_seen");

    dh_hpack_ctx_free(&ctx);
    dh_bytes_free(&block);
    dh_hlist_free(&req);
    dh_hlist_free(&out);

    printf("hpack_selfcheck = %s\n", g_fail ? "FAIL" : "pass");
    return g_fail ? 1 : 0;
}
