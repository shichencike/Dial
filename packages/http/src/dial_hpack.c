/*
 * dial_hpack.c —— HPACK（RFC 7541）编解码，自研实现。详见 dial_hpack.h。
 */
#include "dial_hpack.h"
#include "dial_hpack_tables.h"

#include <stdlib.h>
#include <string.h>
#include <stdio.h>

/* ---------------- 通用字节缓冲 ---------------- */

int dh_bytes_reserve(dh_bytes *b, int64_t need) {
    if (need < 0) return 0;
    if (b->cap - b->len >= need) return 1;
    int64_t cap = b->cap ? b->cap : 128;
    while (cap - b->len < need) {
        if (cap > (int64_t)1 << 40) return 0;
        cap *= 2;
    }
    unsigned char *p = (unsigned char *)realloc(b->p, (size_t)cap);
    if (!p) return 0;
    b->p = p;
    b->cap = cap;
    return 1;
}

int dh_bytes_append(dh_bytes *b, const void *p, int64_t n) {
    if (n < 0) return 0;
    if (n == 0) return 1;
    if (!dh_bytes_reserve(b, n)) return 0;
    memcpy(b->p + b->len, p, (size_t)n);
    b->len += n;
    return 1;
}

int dh_bytes_putc(dh_bytes *b, unsigned char c) {
    if (!dh_bytes_reserve(b, 1)) return 0;
    b->p[b->len++] = c;
    return 1;
}

void dh_bytes_free(dh_bytes *b) {
    if (!b) return;
    free(b->p);
    b->p = NULL;
    b->len = 0;
    b->cap = 0;
}

/* ---------------- 头列表 ---------------- */

void dh_hlist_init(dh_header_list *l) { l->v = NULL; l->n = 0; l->cap = 0; }

void dh_hlist_free(dh_header_list *l) {
    if (!l) return;
    for (int i = 0; i < l->n; i++) {
        free(l->v[i].name);
        free(l->v[i].value);
    }
    free(l->v);
    l->v = NULL; l->n = 0; l->cap = 0;
}

int dh_hlist_add(dh_header_list *l, const char *name, int name_len,
                 const char *value, int value_len) {
    if (name_len < 0 || value_len < 0) return 0;
    if (l->n == l->cap) {
        int cap = l->cap ? l->cap * 2 : 8;
        dh_header *v = (dh_header *)realloc(l->v, (size_t)cap * sizeof(dh_header));
        if (!v) return 0;
        l->v = v; l->cap = cap;
    }
    char *nm = (char *)malloc((size_t)name_len + 1);
    char *vl = (char *)malloc((size_t)value_len + 1);
    if (!nm || !vl) { free(nm); free(vl); return 0; }
    if (name_len) memcpy(nm, name, (size_t)name_len);
    if (value_len) memcpy(vl, value, (size_t)value_len);
    nm[name_len] = '\0';
    vl[value_len] = '\0';
    l->v[l->n].name = nm; l->v[l->n].name_len = name_len;
    l->v[l->n].value = vl; l->v[l->n].value_len = value_len;
    l->n++;
    return 1;
}

/* ---------------- HPACK 上下文 ---------------- */

void dh_hpack_ctx_init(dh_hpack_ctx *ctx, int64_t max_table_size) {
    dh_hlist_init(&ctx->dyn);
    ctx->max_table_size = max_table_size;
    ctx->cur_table_size = 0;
}

void dh_hpack_ctx_free(dh_hpack_ctx *ctx) {
    dh_hlist_free(&ctx->dyn);
}

/* 动态表按「最新在前」存：dyn.v[0] 对应索引 62，dyn.v[k] 对应索引 62+k。
 * 加入新条目时整体后移一位；超上限时从末尾（最旧）淘汰。 */
static void dyn_evict_to(dh_hpack_ctx *ctx, int64_t max_size) {
    while (ctx->dyn.n > 0 && ctx->cur_table_size > max_size) {
        dh_header *old = &ctx->dyn.v[ctx->dyn.n - 1];
        ctx->cur_table_size -= (int64_t)old->name_len + old->value_len + 32;
        free(old->name); free(old->value);
        ctx->dyn.n--;
    }
}

static int dyn_add(dh_hpack_ctx *ctx, const char *name, int nl,
                   const char *value, int vl) {
    int64_t entry = (int64_t)nl + vl + 32;
    if (entry > ctx->max_table_size) {
        /* 单条就超过上限：清空动态表（RFC 7541 §4.4），本条不插入。 */
        dyn_evict_to(ctx, 0);
        return 1;
    }
    if (ctx->dyn.n == ctx->dyn.cap) {
        int cap = ctx->dyn.cap ? ctx->dyn.cap * 2 : 16;
        dh_header *v = (dh_header *)realloc(ctx->dyn.v, (size_t)cap * sizeof(dh_header));
        if (!v) return 0;
        ctx->dyn.v = v; ctx->dyn.cap = cap;
    }
    /* 后移一格，腾出 v[0] */
    memmove(ctx->dyn.v + 1, ctx->dyn.v, (size_t)ctx->dyn.n * sizeof(dh_header));
    char *nm = (char *)malloc((size_t)nl + 1);
    char *vl2 = (char *)malloc((size_t)vl + 1);
    if (!nm || !vl2) { free(nm); free(vl2); return 0; }
    if (nl) memcpy(nm, name, (size_t)nl);
    if (vl) memcpy(vl2, value, (size_t)vl);
    nm[nl] = '\0'; vl2[vl] = '\0';
    ctx->dyn.v[0].name = nm; ctx->dyn.v[0].name_len = nl;
    ctx->dyn.v[0].value = vl2; ctx->dyn.v[0].value_len = vl;
    ctx->dyn.n++;
    ctx->cur_table_size += entry;
    dyn_evict_to(ctx, ctx->max_table_size);
    return 1;
}

/* 按 1-based 索引取表项：1..61 静态，>=62 动态。返回 1 成功，0 越界。 */
static int table_lookup(dh_hpack_ctx *ctx, int64_t idx,
                        const char **name, int *nl, const char **value, int *vl) {
    if (idx >= 1 && idx <= DH_HPACK_STATIC_COUNT) {
        const dh_hpack_static_entry *e = &k_hpack_static[idx];
        *name = e->name; *nl = e->name_len;
        *value = e->value; *vl = e->value_len;
        return 1;
    }
    int64_t k = idx - 62;
    if (idx >= 62 && k >= 0 && k < ctx->dyn.n) {
        dh_header *h = &ctx->dyn.v[k];
        *name = h->name; *nl = h->name_len;
        *value = h->value; *vl = h->value_len;
        return 1;
    }
    return 0;
}

/* ---------------- 整数前缀编码 / 解码（§5.1） ---------------- */

static int encode_int(dh_bytes *out, int prefix_bits, uint8_t tag, int64_t value) {
    int max = (1 << prefix_bits) - 1;
    uint8_t first = (uint8_t)(tag | (value < max ? value : max));
    if (!dh_bytes_putc(out, first)) return 0;
    if (value < max) return 1;
    value -= max;
    while (value >= 128) {
        if (!dh_bytes_putc(out, (unsigned char)((value & 127) | 128))) return 0;
        value >>= 7;
    }
    return dh_bytes_putc(out, (unsigned char)value);
}

/* 从 *p 处读一个整数前缀；first 是已读的首字节。prefix_bits 为前缀位数。 */
static int decode_int(const unsigned char *block, int64_t len, int64_t *p,
                      int prefix_bits, int64_t *out_value, const char **err) {
    if (*p >= len) { *err = "HPACK 整数被截断"; return 0; }
    int max = (1 << prefix_bits) - 1;
    unsigned char first = block[(*p)++];
    int64_t value = first & max;
    if (value < max) { *out_value = value; return 1; }
    int m = 0;
    for (;;) {
        if (*p >= len) { *err = "HPACK 整数续字节被截断"; return 0; }
        unsigned char b = block[(*p)++];
        value += (int64_t)(b & 127) << m;
        if (!(b & 128)) break;
        m += 7;
        if (m > 35) { *err = "HPACK 整数过长"; return 0; }
    }
    *out_value = value;
    return 1;
}

/* ---------------- 字符串编码 / 解码（§5.2） ---------------- */

/* Huffman 编码一段明文到 out（不含长度前缀）。返回 0 成功、-1 内存不足。
 * RFC 7541 Appendix B：k_huff_code[sym] 是右对齐码字，k_huff_len[sym] 是比特数。
 * 末尾不足一字节时用 EOS 码的前缀（全 1）补齐，最多 7 位。 */
int dh_huffman_encode(const char *s, int n, dh_bytes *out) {
    if (n < 0) return -1;
    uint64_t acc = 0;
    int nbits = 0;
    for (int i = 0; i < n; i++) {
        unsigned char sym = (unsigned char)s[i];
        uint32_t code = k_huff_code[sym];
        int len = k_huff_len[sym];
        acc = (acc << len) | code;
        nbits += len;
        while (nbits >= 8) {
            nbits -= 8;
            if (!dh_bytes_putc(out, (unsigned char)(acc >> nbits))) return -1;
        }
        acc = nbits ? (acc & (((uint64_t) 1 << nbits) - 1)) : 0;
    }
    if (nbits > 0) {
        int pad = 8 - nbits;
        unsigned char last = (unsigned char)((acc << pad) | ((1U << pad) - 1U));
        if (!dh_bytes_putc(out, last)) return -1;
    }
    return 0;
}

/* 编码一个 HPACK 字符串：H=1（Huffman），长度用 7 位整数前缀。 */
static int encode_string(dh_bytes *out, const char *s, int n) {
    dh_bytes tmp = {0};
    if (dh_huffman_encode(s, n, &tmp) != 0) { dh_bytes_free(&tmp); return 0; }
    int ok = encode_int(out, 7, 0x80, tmp.len);
    if (ok) ok = dh_bytes_append(out, tmp.p, tmp.len);
    dh_bytes_free(&tmp);
    return ok;
}

/* Huffman 解码一段比特流到 out。线性查表（257 项）逐位匹配，简单且对头部尺寸足够快。 */
int dh_huffman_decode(const unsigned char *in, int64_t len, dh_bytes *out) {
    uint32_t acc = 0;
    int nbits = 0;
    for (int64_t i = 0; i < len; i++) {
        unsigned char byte = in[i];
        for (int b = 7; b >= 0; b--) {
            acc = (acc << 1) | ((byte >> b) & 1);
            nbits++;
            if (nbits > 30) return -1;  /* 无符号 30 位以上必是 EOS/非法 */
            int sym = -1;
            for (int s = 0; s < 256; s++) {  /* 排除 EOS(256) */
                if (k_huff_len[s] == nbits && k_huff_code[s] == acc) { sym = s; break; }
            }
            if (sym >= 0) {
                /* EOS 码（全 1）不应出现在数据里；但短前缀全 1 是合法补齐，见末尾处理。
                 * 只有当匹配到的是真符号（非全 1 短码）才输出。 */
                if (!dh_bytes_putc(out, (unsigned char)sym)) return -1;
                acc = 0; nbits = 0;
            }
        }
    }
    /* 补齐：剩余比特必须是 EOS 码的前缀（全 1），且少于 8 位。 */
    if (nbits > 7) return -1;
    if (nbits > 0) {
        uint32_t allones = (nbits == 32) ? 0xffffffffU : ((1U << nbits) - 1);
        if (acc != allones) return -1;
    }
    (void)len;
    return 0;
}

/* 读一个 HPACK 字符串（首字节含 H 位与 7 位长度前缀）。明文追加到 tmp。 */
static int decode_string(const unsigned char *block, int64_t len, int64_t *p,
                         dh_bytes *tmp, const char **err) {
    if (*p >= len) { *err = "HPACK 字符串首字节被截断"; return 0; }
    int huffman = (block[*p] & 0x80) != 0;
    int64_t slen = 0;
    if (!decode_int(block, len, p, 7, &slen, err)) return 0;
    if (slen < 0 || *p + slen > len) { *err = "HPACK 字符串长度越界"; return 0; }
    const unsigned char *src = block + *p;
    if (huffman) {
        if (dh_huffman_decode(src, slen, tmp) != 0) {
            *err = "HPACK Huffman 解码失败（非法码字或补齐位）";
            return 0;
        }
    } else {
        if (!dh_bytes_append(tmp, src, slen)) { *err = "内存不足"; return 0; }
    }
    *p += slen;
    return 1;
}

/* ---------------- 编码：请求头块 ---------------- */

/* 在静态表里找 (name, value) 完全匹配项，返回 1..61；0 = 未命中。 */
static int static_find_exact(const char *name, int nl,
                             const char *value, int vl) {
    for (int i = 1; i <= DH_HPACK_STATIC_COUNT; i++) {
        const dh_hpack_static_entry *e = &k_hpack_static[i];
        if (e->name_len == nl && e->value_len == vl &&
            (nl == 0 || memcmp(e->name, name, (size_t) nl) == 0) &&
            (vl == 0 || memcmp(e->value, value, (size_t) vl) == 0)) return i;
    }
    return 0;
}

/* 在静态表里找 name 匹配项（value 不必匹配），返回 1..61；0 = 未命中。 */
static int static_find_name(const char *name, int nl) {
    for (int i = 1; i <= DH_HPACK_STATIC_COUNT; i++) {
        const dh_hpack_static_entry *e = &k_hpack_static[i];
        if (e->name_len == nl &&
            (nl == 0 || memcmp(e->name, name, (size_t) nl) == 0)) return i;
    }
    return 0;
}

/* 请求侧编码：与浏览器一致地用静态表索引 + Huffman。
 *   - (name,value) 完全命中静态表 → indexed header field（0x80，7 位索引）；
 *   - 只 name 命中 → literal without indexing / indexed name（0000 + 4 位索引）
 *     + Huffman value；
 *   - 都未命中 → literal without indexing / new name（0x00）+ 两个 Huffman 串。
 * 一律 without indexing：客户端不维护发送侧动态表，跨请求无状态，最简单也最稳。 */
int dh_hpack_encode(dh_bytes *out, const dh_header_list *req) {
    for (int i = 0; i < req->n; i++) {
        const dh_header *h = &req->v[i];
        int idx = static_find_exact(h->name, h->name_len,
                                    h->value, h->value_len);
        if (idx > 0) {
            if (!encode_int(out, 7, 0x80, idx)) return -1;
            continue;
        }
        int nidx = static_find_name(h->name, h->name_len);
        if (nidx > 0) {
            if (!encode_int(out, 4, 0x00, nidx)) return -1;
            if (!encode_string(out, h->value, h->value_len)) return -1;
        } else {
            if (!dh_bytes_putc(out, 0x00)) return -1;
            if (!encode_string(out, h->name, h->name_len)) return -1;
            if (!encode_string(out, h->value, h->value_len)) return -1;
        }
    }
    return 0;
}

/* ---------------- 解码：响应头块 ---------------- */

static void set_err(char *errbuf, size_t errlen, const char *msg) {
    if (!errbuf || errlen == 0) return;
    strncpy(errbuf, msg, errlen - 1);
    errbuf[errlen - 1] = '\0';
}

int dh_hpack_decode(dh_hpack_ctx *ctx, const unsigned char *block, int64_t len,
                    dh_header_list *out, char *errbuf, size_t errlen) {
    if (!ctx || !block || !out || len < 0) { set_err(errbuf, errlen, "HPACK 参数非法"); return -1; }
    int64_t p = 0;
    const char *err = "HPACK 协议错误";
    while (p < len) {
        unsigned char first = block[p];
        if (first & 0x80) {
            /* indexed header field（0x80） */
            int64_t idx = 0;
            if (!decode_int(block, len, &p, 7, &idx, &err)) goto proto_err;
            const char *name, *value; int nl, vl;
            if (idx == 0 || !table_lookup(ctx, idx, &name, &nl, &value, &vl)) {
                err = "HPACK indexed 索引越界或为 0"; goto proto_err;
            }
            if (!dh_hlist_add(out, name, nl, value, vl)) return -1;
        } else if ((first & 0xE0) == 0x20) {
            /* dynamic table size update（001xxxxx） */
            int64_t new_size = 0;
            if (!decode_int(block, len, &p, 5, &new_size, &err)) goto proto_err;
            /* max_table_size 是**本端**在 SETTINGS_HEADER_TABLE_SIZE 里声明的
             * 解码表上限（见 dial_http2.c 的 connect 初始化）；服务端发的 update
             * 不得超过它。chrome 画像声明 65536，服务端据此发 65536，合法。 */
            if (new_size > ctx->max_table_size) {
                err = "HPACK 动态表大小超过本端声明上限"; goto proto_err;
            }
            dyn_evict_to(ctx, new_size);
        } else {
            /* 字面量：0x40 增量索引 / 0x00 不带索引 / 0x10 never indexed */
            int incremental = (first & 0xC0) == 0x40;
            int prefix_bits = incremental ? 6 : 4;
            int64_t name_idx = 0;
            if (!decode_int(block, len, &p, prefix_bits, &name_idx, &err)) goto proto_err;

            dh_bytes name = {0}, value = {0};
            const char *nm; int nl;
            if (name_idx == 0) {
                if (!decode_string(block, len, &p, &name, &err)) { dh_bytes_free(&name); goto cleanup_str; }
                nm = (const char *)name.p; nl = (int)name.len;
            } else {
                const char *vn; int vl0;
                if (!table_lookup(ctx, name_idx, &nm, &nl, &vn, &vl0)) {
                    err = "HPACK 字面量 name 索引越界"; dh_bytes_free(&name); goto cleanup_str;
                }
            }
            if (!decode_string(block, len, &p, &value, &err)) { dh_bytes_free(&name); goto cleanup_str; }

            int ok = dh_hlist_add(out, nm, nl, (const char *)value.p, (int)value.len);
            if (ok && incremental) {
                ok = dyn_add(ctx, nm, nl, (const char *)value.p, (int)value.len);
            }
            dh_bytes_free(&name);
            dh_bytes_free(&value);
            if (!ok) return -1;
            continue;

        cleanup_str:
            dh_bytes_free(&value);
            set_err(errbuf, errlen, err);
            return -2;
        }
    }
    return out->n;

proto_err:
    set_err(errbuf, errlen, err);
    return -2;
}
