#ifndef DIAL_HPACK_H
#define DIAL_HPACK_H

#include <stdint.h>
#include <stddef.h>

/*
 * dial_hpack —— HTTP/2 的 HPACK（RFC 7541）头压缩，自研实现。
 *
 * 立场：协议层自研，不引第三方 HPACK 库。表数据（静态表 61 项 + Huffman 码表）
 * 由 tools/gen_hpack_tables.py 从 RFC 常量导出到 dial_hpack_tables.h，避免手敲出错。
 *
 * 客户端编码：静态表索引 + Huffman（浏览器一致性画像）。
 *   (name,value) 完全命中静态表就发 indexed（0x80）；只 name 命中就发
 *   literal-with-indexed-name（4 位索引）+ Huffman value；都不命中才发
 *   literal-new-name（0x00）+ 两个 Huffman 串。一律 without indexing，
 *   客户端不维护发送侧动态表，跨请求无状态。
 *   编码正确性由 tests/pkg/fingerprint/check_hpack.c 的往返自校验保证，
 *   互操作性由 W 组 h2 测试（Python h2/hpack 服务端）保证。
 *
 * 响应解码：完整实现——indexed（0x80）/ 字面量增量索引（0x40）/ 不带索引（0x00）/
 *   never indexed（0x10）/ 动态表大小更新（0x20）、Huffman 解码、动态表的增删与
 *   按索引回查、整数前缀编码（§5.1）。服务端通常用 Huffman + 静态表，必须能读懂。
 *
 * 内存：所有缓冲用 malloc/realloc 自己管，句柄由本模块显式 init/free，不进 ARC。
 */

typedef struct { char *name; int name_len; char *value; int value_len; } dh_header;

typedef struct {
    dh_header *v;
    int n;
    int cap;
} dh_header_list;

/* ---- 通用动态字节缓冲（帧层与 HPACK 共用） ---- */
typedef struct { unsigned char *p; int64_t len; int64_t cap; } dh_bytes;

int  dh_bytes_reserve(dh_bytes *b, int64_t need);
int  dh_bytes_append(dh_bytes *b, const void *p, int64_t n);
int  dh_bytes_putc(dh_bytes *b, unsigned char c);
void dh_bytes_free(dh_bytes *b);

void dh_hlist_init(dh_header_list *l);
void dh_hlist_free(dh_header_list *l);
/* 追加一条头（拷贝 name/value，各自按 len 取，允许内嵌 NUL 之外的任意字节）。 */
int  dh_hlist_add(dh_header_list *l, const char *name, int name_len,
                  const char *value, int value_len);

/*
 * HPACK 上下文：持有动态表（每次连接一个，跨多个 HEADERS 块保序）。
 * max_table_size 来自对端 SETTINGS_HEADER_TABLE_SIZE（默认 4096）。
 */
typedef struct {
    dh_header_list dyn;      /* 动态表：dyn.v[0] 是最新条目，对应索引 62 */
    int64_t max_table_size;  /* 对端声明的上限 */
    int64_t cur_table_size;  /* 当前动态表字节数（每条 = name+value+32） */
} dh_hpack_ctx;

void dh_hpack_ctx_init(dh_hpack_ctx *ctx, int64_t max_table_size);
void dh_hpack_ctx_free(dh_hpack_ctx *ctx);

/*
 * 编码：把 req 里的每条头编成静态表索引 / indexed-name / new-name 三种
 * literal-without-indexing 之一，字符串一律 Huffman，追加到 out。
 * 返回 0 成功；-1 内存不足。out 由调用方 init/free，可连续追加多个头块。
 */
int dh_hpack_encode(dh_bytes *out, const dh_header_list *req);

/*
 * 解码：把一段完整的 header block fragment（HEADERS + 全部 CONTINUATION 拼好后）
 * 解成 out，并按需更新 ctx 的动态表。
 * 返回 >=0 表示解出的头条数；-1 参数/内存错误；-2 协议错误（非法索引、坏 Huffman、
 *   越界整数等），errbuf 写入中文原因。
 */
int dh_hpack_decode(dh_hpack_ctx *ctx, const unsigned char *block, int64_t len,
                    dh_header_list *out, char *errbuf, size_t errlen);

/* Huffman 编码（单段字符串）：s 明文、n 字节数，out 追加码流（不含长度前缀）。
 * 末尾不足一字节用 EOS 前缀（全 1）补齐。返回 0 成功；-1 内存不足 / 参数非法。 */
int dh_huffman_encode(const char *s, int n, dh_bytes *out);

/* Huffman 解码（单段字符串）：in 是 Huffman 比特流，out 追加明文字节。
 * 返回 0 成功；-1 非法（EOS 出现在数据里 / 补齐位不是全 1 / 长度越界）。 */
int dh_huffman_decode(const unsigned char *in, int64_t len, dh_bytes *out);

#endif /* DIAL_HPACK_H */
