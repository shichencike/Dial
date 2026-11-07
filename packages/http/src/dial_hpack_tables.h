/* 本文件由 tools/gen_hpack_tables.py 自动生成，请勿手改。
 * 数据源：Python hpack 库内置常量（等价 RFC 7541 Appendix A/B）。 */
#ifndef DIAL_HPACK_TABLES_H
#define DIAL_HPACK_TABLES_H
#include <stdint.h>

typedef struct { const char *name; const char *value; int name_len; int value_len; } dh_hpack_static_entry;

/* HPACK 静态表，索引 1..61。下标 0 留空以便直接用 1-based 索引。 */
static const dh_hpack_static_entry k_hpack_static[62] = {
    { "", "", 0, 0 },  /* 0 保留 */
    { ":authority", "", 10, 0 },  /* 1 */
    { ":method", "GET", 7, 3 },  /* 2 */
    { ":method", "POST", 7, 4 },  /* 3 */
    { ":path", "/", 5, 1 },  /* 4 */
    { ":path", "/index.html", 5, 11 },  /* 5 */
    { ":scheme", "http", 7, 4 },  /* 6 */
    { ":scheme", "https", 7, 5 },  /* 7 */
    { ":status", "200", 7, 3 },  /* 8 */
    { ":status", "204", 7, 3 },  /* 9 */
    { ":status", "206", 7, 3 },  /* 10 */
    { ":status", "304", 7, 3 },  /* 11 */
    { ":status", "400", 7, 3 },  /* 12 */
    { ":status", "404", 7, 3 },  /* 13 */
    { ":status", "500", 7, 3 },  /* 14 */
    { "accept-charset", "", 14, 0 },  /* 15 */
    { "accept-encoding", "gzip, deflate", 15, 13 },  /* 16 */
    { "accept-language", "", 15, 0 },  /* 17 */
    { "accept-ranges", "", 13, 0 },  /* 18 */
    { "accept", "", 6, 0 },  /* 19 */
    { "access-control-allow-origin", "", 27, 0 },  /* 20 */
    { "age", "", 3, 0 },  /* 21 */
    { "allow", "", 5, 0 },  /* 22 */
    { "authorization", "", 13, 0 },  /* 23 */
    { "cache-control", "", 13, 0 },  /* 24 */
    { "content-disposition", "", 19, 0 },  /* 25 */
    { "content-encoding", "", 16, 0 },  /* 26 */
    { "content-language", "", 16, 0 },  /* 27 */
    { "content-length", "", 14, 0 },  /* 28 */
    { "content-location", "", 16, 0 },  /* 29 */
    { "content-range", "", 13, 0 },  /* 30 */
    { "content-type", "", 12, 0 },  /* 31 */
    { "cookie", "", 6, 0 },  /* 32 */
    { "date", "", 4, 0 },  /* 33 */
    { "etag", "", 4, 0 },  /* 34 */
    { "expect", "", 6, 0 },  /* 35 */
    { "expires", "", 7, 0 },  /* 36 */
    { "from", "", 4, 0 },  /* 37 */
    { "host", "", 4, 0 },  /* 38 */
    { "if-match", "", 8, 0 },  /* 39 */
    { "if-modified-since", "", 17, 0 },  /* 40 */
    { "if-none-match", "", 13, 0 },  /* 41 */
    { "if-range", "", 8, 0 },  /* 42 */
    { "if-unmodified-since", "", 19, 0 },  /* 43 */
    { "last-modified", "", 13, 0 },  /* 44 */
    { "link", "", 4, 0 },  /* 45 */
    { "location", "", 8, 0 },  /* 46 */
    { "max-forwards", "", 12, 0 },  /* 47 */
    { "proxy-authenticate", "", 18, 0 },  /* 48 */
    { "proxy-authorization", "", 19, 0 },  /* 49 */
    { "range", "", 5, 0 },  /* 50 */
    { "referer", "", 7, 0 },  /* 51 */
    { "refresh", "", 7, 0 },  /* 52 */
    { "retry-after", "", 11, 0 },  /* 53 */
    { "server", "", 6, 0 },  /* 54 */
    { "set-cookie", "", 10, 0 },  /* 55 */
    { "strict-transport-security", "", 25, 0 },  /* 56 */
    { "transfer-encoding", "", 17, 0 },  /* 57 */
    { "user-agent", "", 10, 0 },  /* 58 */
    { "vary", "", 4, 0 },  /* 59 */
    { "via", "", 3, 0 },  /* 60 */
    { "www-authenticate", "", 16, 0 },  /* 61 */
};
#define DH_HPACK_STATIC_COUNT 61

/* Huffman 码表：code[sym] 右对齐码字，len[sym] 比特数。sym 256 = EOS（仅编码时补齐用）。 */
static const uint32_t k_huff_code[257] = {
    0x00001ff8U, 0x007fffd8U, 0x0fffffe2U, 0x0fffffe3U, 0x0fffffe4U, 0x0fffffe5U,
    0x0fffffe6U, 0x0fffffe7U, 0x0fffffe8U, 0x00ffffeaU, 0x3ffffffcU, 0x0fffffe9U,
    0x0fffffeaU, 0x3ffffffdU, 0x0fffffebU, 0x0fffffecU, 0x0fffffedU, 0x0fffffeeU,
    0x0fffffefU, 0x0ffffff0U, 0x0ffffff1U, 0x0ffffff2U, 0x3ffffffeU, 0x0ffffff3U,
    0x0ffffff4U, 0x0ffffff5U, 0x0ffffff6U, 0x0ffffff7U, 0x0ffffff8U, 0x0ffffff9U,
    0x0ffffffaU, 0x0ffffffbU, 0x00000014U, 0x000003f8U, 0x000003f9U, 0x00000ffaU,
    0x00001ff9U, 0x00000015U, 0x000000f8U, 0x000007faU, 0x000003faU, 0x000003fbU,
    0x000000f9U, 0x000007fbU, 0x000000faU, 0x00000016U, 0x00000017U, 0x00000018U,
    0x00000000U, 0x00000001U, 0x00000002U, 0x00000019U, 0x0000001aU, 0x0000001bU,
    0x0000001cU, 0x0000001dU, 0x0000001eU, 0x0000001fU, 0x0000005cU, 0x000000fbU,
    0x00007ffcU, 0x00000020U, 0x00000ffbU, 0x000003fcU, 0x00001ffaU, 0x00000021U,
    0x0000005dU, 0x0000005eU, 0x0000005fU, 0x00000060U, 0x00000061U, 0x00000062U,
    0x00000063U, 0x00000064U, 0x00000065U, 0x00000066U, 0x00000067U, 0x00000068U,
    0x00000069U, 0x0000006aU, 0x0000006bU, 0x0000006cU, 0x0000006dU, 0x0000006eU,
    0x0000006fU, 0x00000070U, 0x00000071U, 0x00000072U, 0x000000fcU, 0x00000073U,
    0x000000fdU, 0x00001ffbU, 0x0007fff0U, 0x00001ffcU, 0x00003ffcU, 0x00000022U,
    0x00007ffdU, 0x00000003U, 0x00000023U, 0x00000004U, 0x00000024U, 0x00000005U,
    0x00000025U, 0x00000026U, 0x00000027U, 0x00000006U, 0x00000074U, 0x00000075U,
    0x00000028U, 0x00000029U, 0x0000002aU, 0x00000007U, 0x0000002bU, 0x00000076U,
    0x0000002cU, 0x00000008U, 0x00000009U, 0x0000002dU, 0x00000077U, 0x00000078U,
    0x00000079U, 0x0000007aU, 0x0000007bU, 0x00007ffeU, 0x000007fcU, 0x00003ffdU,
    0x00001ffdU, 0x0ffffffcU, 0x000fffe6U, 0x003fffd2U, 0x000fffe7U, 0x000fffe8U,
    0x003fffd3U, 0x003fffd4U, 0x003fffd5U, 0x007fffd9U, 0x003fffd6U, 0x007fffdaU,
    0x007fffdbU, 0x007fffdcU, 0x007fffddU, 0x007fffdeU, 0x00ffffebU, 0x007fffdfU,
    0x00ffffecU, 0x00ffffedU, 0x003fffd7U, 0x007fffe0U, 0x00ffffeeU, 0x007fffe1U,
    0x007fffe2U, 0x007fffe3U, 0x007fffe4U, 0x001fffdcU, 0x003fffd8U, 0x007fffe5U,
    0x003fffd9U, 0x007fffe6U, 0x007fffe7U, 0x00ffffefU, 0x003fffdaU, 0x001fffddU,
    0x000fffe9U, 0x003fffdbU, 0x003fffdcU, 0x007fffe8U, 0x007fffe9U, 0x001fffdeU,
    0x007fffeaU, 0x003fffddU, 0x003fffdeU, 0x00fffff0U, 0x001fffdfU, 0x003fffdfU,
    0x007fffebU, 0x007fffecU, 0x001fffe0U, 0x001fffe1U, 0x003fffe0U, 0x001fffe2U,
    0x007fffedU, 0x003fffe1U, 0x007fffeeU, 0x007fffefU, 0x000fffeaU, 0x003fffe2U,
    0x003fffe3U, 0x003fffe4U, 0x007ffff0U, 0x003fffe5U, 0x003fffe6U, 0x007ffff1U,
    0x03ffffe0U, 0x03ffffe1U, 0x000fffebU, 0x0007fff1U, 0x003fffe7U, 0x007ffff2U,
    0x003fffe8U, 0x01ffffecU, 0x03ffffe2U, 0x03ffffe3U, 0x03ffffe4U, 0x07ffffdeU,
    0x07ffffdfU, 0x03ffffe5U, 0x00fffff1U, 0x01ffffedU, 0x0007fff2U, 0x001fffe3U,
    0x03ffffe6U, 0x07ffffe0U, 0x07ffffe1U, 0x03ffffe7U, 0x07ffffe2U, 0x00fffff2U,
    0x001fffe4U, 0x001fffe5U, 0x03ffffe8U, 0x03ffffe9U, 0x0ffffffdU, 0x07ffffe3U,
    0x07ffffe4U, 0x07ffffe5U, 0x000fffecU, 0x00fffff3U, 0x000fffedU, 0x001fffe6U,
    0x003fffe9U, 0x001fffe7U, 0x001fffe8U, 0x007ffff3U, 0x003fffeaU, 0x003fffebU,
    0x01ffffeeU, 0x01ffffefU, 0x00fffff4U, 0x00fffff5U, 0x03ffffeaU, 0x007ffff4U,
    0x03ffffebU, 0x07ffffe6U, 0x03ffffecU, 0x03ffffedU, 0x07ffffe7U, 0x07ffffe8U,
    0x07ffffe9U, 0x07ffffeaU, 0x07ffffebU, 0x0ffffffeU, 0x07ffffecU, 0x07ffffedU,
    0x07ffffeeU, 0x07ffffefU, 0x07fffff0U, 0x03ffffeeU, 0x3fffffffU,
};
static const uint8_t k_huff_len[257] = {
    13, 23, 28, 28, 28, 28, 28, 28, 28, 24, 30, 28,
    28, 30, 28, 28, 28, 28, 28, 28, 28, 28, 30, 28,
    28, 28, 28, 28, 28, 28, 28, 28, 6, 10, 10, 12,
    13, 6, 8, 11, 10, 10, 8, 11, 8, 6, 6, 6,
    5, 5, 5, 6, 6, 6, 6, 6, 6, 6, 7, 8,
    15, 6, 12, 10, 13, 6, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 8, 7, 8, 13, 19, 13, 14, 6,
    15, 5, 6, 5, 6, 5, 6, 6, 6, 5, 7, 7,
    6, 6, 6, 5, 6, 7, 6, 5, 5, 6, 7, 7,
    7, 7, 7, 15, 11, 14, 13, 28, 20, 22, 20, 20,
    22, 22, 22, 23, 22, 23, 23, 23, 23, 23, 24, 23,
    24, 24, 22, 23, 24, 23, 23, 23, 23, 21, 22, 23,
    22, 23, 23, 24, 22, 21, 20, 22, 22, 23, 23, 21,
    23, 22, 22, 24, 21, 22, 23, 23, 21, 21, 22, 21,
    23, 22, 23, 23, 20, 22, 22, 22, 23, 22, 22, 23,
    26, 26, 20, 19, 22, 23, 22, 25, 26, 26, 26, 27,
    27, 26, 24, 25, 19, 21, 26, 27, 27, 26, 27, 24,
    21, 21, 26, 26, 28, 27, 27, 27, 20, 24, 20, 21,
    22, 21, 21, 23, 22, 22, 25, 25, 24, 24, 26, 23,
    26, 27, 26, 26, 27, 27, 27, 27, 27, 28, 27, 27,
    27, 27, 27, 26, 30,
};

#endif /* DIAL_HPACK_TABLES_H */
