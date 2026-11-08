/* dial_clienthello_fp.c —— 浏览器一致性画像：ClientHello 报文重排。
 *
 * 铁律（third_party/README.md「绝不手搓密码学」）：本文件**只做报文编排**——
 * 写 uint16_t / 字节、调整 cipher 与扩展顺序、插 GREASE 哑值和哑扩展占位。
 * 真实 key_share 公钥、SNI、ALPN、psk_modes 一律沿用 mbedTLS 已生成的内容，
 * 只是被搬到基准顺序里。不生成密钥、不碰 HKDF / AEAD / 签名 / RNG / transcript。
 *
 * 基准来源：tests/pkg/fingerprint/chrome_windows_x64.json
 *   （Google Chrome 144.0.7559.132 域名模式实测录制）
 *   JA4 = t13d1516h2_8daaf6152771_d8a2da3f94cd
 *
 * 策略：在 ssl_write_client_hello_body() 末尾、update_checksum() 之前，
 * 把 mbedTLS 已生成的 ClientHello body 整体重建为基准布局：
 *   - cipher 区：按基准 16 项重写（含首 GREASE），去掉 mbedTLS 强制追加的 SCSV；
 *   - 扩展区：按基准 18 项顺序重写（首/尾 GREASE），
 *       * 真实扩展 0000(server_name) / 0010(ALPN) / 0033(key_share) 沿用原 body；
 *       * 000a(groups) / 000d(sig_algs) / 002b(supported_versions) 用基准常量；
 *       * 其余为哑扩展占位（最小合法 body，服务器不认识则忽略）。
 *
 * 失败语义：任何异常返回非 0 且**不修改 buf**，让 mbedTLS 原生 ClientHello 照常
 * 发出（握手不受影响，仅指纹不对齐）。Fail-Safe 而非 Fail-Closed。
 *
 * 已知边界（写进 .diali 与文档，不许含糊）：
 *   - GREASE 用固定常量 0x0a0a（真 Chrome 每次随机，但 JA3/JA4 都忽略 GREASE，
 *     归一化后一致）；不调 RNG，保证产物可复现。
 *   - 哑扩展只对齐「扩展 id 集合」，不实现 ECH(HPKE) / 证书压缩(brotli) /
 *     ALPS 协商 / 真·多组 key_share / PQC 组 11ec 的密钥交换。
 *   - key_share 只保留 mbedTLS 的真实 X25519 一组；supported_groups 仍声明
 *     11ec（JA3 curves 字段要求），服务器通常会选 X25519 而不发 HRR。
 *   - 服务器若对哑扩展作出响应，mbedTLS 接收侧可能拒绝——本地 Python ssl/h2
 *     服务器实测不回；真实环境属已声明边界。
 */

#include "dial_clienthello_fp.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* RFC 8701 GREASE。固定常量而非随机：JA3/JA4 都剥离 GREASE，归一化后与基准一致。 */
#define FP_GREASE 0x0A0A

/* Chrome 144 cipher 序列（16 项；首项 GREASE）。去 GREASE 后 15 项决定 JA4_b。 */
static const uint16_t k_ciphers[16] = {
    FP_GREASE,
    0x1301, 0x1302, 0x1303,
    0xC02B, 0xC02F, 0xC02C, 0xC030,
    0xCCA9, 0xCCA8,
    0xC013, 0xC014,
    0x009C, 0x009D, 0x002F, 0x0035
};

/* 需要重写的三个扩展 body（长度前缀 + 序列，均为基准值）。 */
static const uint8_t k_body_versions[] = {
    0x06,                                   /* versions 字节长度 = 6 */
    0x0A, 0x0A,                             /* GREASE */
    0x03, 0x04,                             /* TLS 1.3 */
    0x03, 0x03                              /* TLS 1.2 */
};
static const uint8_t k_body_groups[] = {
    0x00, 0x0A,                             /* named_group_list 字节长度 = 10 */
    0x0A, 0x0A,                             /* GREASE */
    0x11, 0xEC,                             /* X25519MLKEM768（仅占位，不做 PQC 交换） */
    0x00, 0x1D,                             /* X25519 */
    0x00, 0x17,                             /* secp256r1 */
    0x00, 0x18                              /* secp384r1 */
};
static const uint8_t k_body_sigalgs[] = {
    0x00, 0x10,                             /* 字节长度 = 16 */
    0x04, 0x03, 0x08, 0x04, 0x04, 0x01, 0x05, 0x03,
    0x08, 0x05, 0x05, 0x01, 0x08, 0x06, 0x06, 0x01
};

/* 哑扩展 body：最小合法占位，尽量让服务器不作出响应。 */
static const uint8_t k_body_ec_point_formats[] = { 0x01, 0x00 };
static const uint8_t k_body_status_request[]   = { 0x01, 0x00, 0x00, 0x00, 0x00 };
static const uint8_t k_body_compress_cert[]    = { 0x00 };  /* 空算法列表：不请求压缩 */
static const uint8_t k_body_ech[]              = { 0x00 };  /* ECH GREASE 占位，非真实加密 */
static const uint8_t k_body_alps[]             = { 0x00, 0x03, 0x02, 0x68, 0x32 };
static const uint8_t k_body_psk_modes[]        = { 0x01, 0x01 };  /* psk_dhe_ke */
static const uint8_t k_body_renego[]           = { 0x00 };
static const uint8_t k_body_grease_tail[]      = { 0x00 };

typedef struct {
    uint16_t        type;
    const uint8_t  *body;      /* 静态 body；from_orig=1 时为 NULL */
    uint16_t        body_len;
    uint8_t         from_orig; /* 1 = 从 mbedTLS 原 ClientHello 提取同 type 的 body */
} fp_ext;

#define EXT_STATIC(t, arr) { (uint16_t)(t), (arr), (uint16_t) sizeof(arr), 0 }
#define EXT_EMPTY(t)       { (uint16_t)(t), NULL, 0, 0 }
#define EXT_ORIG(t)        { (uint16_t)(t), NULL, 0, 1 }

/* 基准扩展顺序（Chrome 144 域名模式单次采样，18 项，首尾 GREASE）。
 * JA4 排序后哈希与顺序无关；JA3 顺序敏感，故固定对齐这一次采样。 */
static const fp_ext k_exts[] = {
    EXT_EMPTY(FP_GREASE),                       /*  0 GREASE */
    EXT_EMPTY(0x0017),                          /*  1 extended_master_secret */
    EXT_STATIC(0x000B, k_body_ec_point_formats),/*  2 ec_point_formats */
    EXT_STATIC(0x0005, k_body_status_request),  /*  3 status_request */
    EXT_ORIG(0x0033),                           /*  4 key_share（真实 X25519 公钥） */
    EXT_STATIC(0x002B, k_body_versions),        /*  5 supported_versions */
    EXT_STATIC(0x001B, k_body_compress_cert),   /*  6 compress_certificate（哑） */
    EXT_STATIC(0xFE0D, k_body_ech),             /*  7 ECH（哑） */
    EXT_STATIC(0x002D, k_body_psk_modes),       /*  8 psk_key_exchange_modes */
    EXT_STATIC(0x44CD, k_body_alps),            /*  9 ALPS（哑） */
    EXT_EMPTY(0x0012),                          /* 10 signed_certificate_timestamp */
    EXT_STATIC(0x000A, k_body_groups),          /* 11 supported_groups */
    EXT_STATIC(0x000D, k_body_sigalgs),         /* 12 signature_algorithms */
    EXT_STATIC(0xFF01, k_body_renego),          /* 13 renegotiation_info */
    EXT_ORIG(0x0010),                           /* 14 ALPN（真实，双 offer h2/http1.1） */
    EXT_EMPTY(0x0023),                          /* 15 session_ticket */
    EXT_ORIG(0x0000),                           /* 16 server_name（真实 SNI） */
    EXT_STATIC(FP_GREASE, k_body_grease_tail)   /* 17 GREASE（尾，len=1） */
};
#define FP_NEXTS ((sizeof(k_exts)) / (sizeof(k_exts[0])))

/* 在一段 extensions 区里按 type 找 body。 */
static const uint8_t *find_ext(const uint8_t *start, size_t total,
                               uint16_t type, uint16_t *out_len)
{
    size_t q = 0;
    while (q + 4 <= total) {
        uint16_t t = (uint16_t) ((start[q] << 8) | start[q + 1]);
        uint16_t l = (uint16_t) ((start[q + 2] << 8) | start[q + 3]);
        if (q + 4 + (size_t) l > total) {
            return NULL;
        }
        if (t == type) {
            *out_len = l;
            return start + q + 4;
        }
        q += 4 + (size_t) l;
    }
    return NULL;
}

#define FP_NEED(n) do {                                                   \
    if ((size_t) (tmp_end - w) < (size_t) (n)) { free(tmp); return -5; }  \
} while (0)

#define FP_PUT16(v) do {                                                  \
    FP_NEED(2);                                                           \
    *w++ = (uint8_t) ((v) >> 8);                                          \
    *w++ = (uint8_t) ((v) & 0xFF);                                        \
} while (0)

int dial_fp_reshape_clienthello(unsigned char *buf, size_t len,
                                size_t buf_cap, size_t *out_len)
{
    if (buf == NULL || out_len == NULL || len < 40 || buf_cap < len + 256) {
        return -1;
    }

    const uint8_t *p = buf;
    size_t q = 2 + 32;                 /* legacy_version + random */
    if (q + 1 > len) { return -1; }
    uint8_t sid_len = p[q]; q += 1;
    if (q + (size_t) sid_len + 2 > len) { return -1; }
    q += sid_len;
    uint16_t cs_len = (uint16_t) ((p[q] << 8) | p[q + 1]); q += 2;
    if (q + (size_t) cs_len + 1 > len) { return -1; }
    q += cs_len;
    uint8_t comp_len = p[q]; q += 1;
    if (q + (size_t) comp_len + 2 > len) { return -1; }
    const uint8_t *comp = p + q; q += comp_len;
    uint16_t ext_total = (uint16_t) ((p[q] << 8) | p[q + 1]); q += 2;
    if (q + (size_t) ext_total > len) { return -1; }
    const uint8_t *ext_start = p + q;

    /* 只在「首次、标准 TLS 1.3」ClientHello 上重排。以下三种情形一律放行
     * mbedTLS 原生 CH，因为重建会丢掉它写入的关键扩展、直接拖垮握手：
     *   - 0029 pre_shared_key：上层 checksum 按 msg_len - binders_len 截断，
     *     重排会破坏 PSK binder；
     *   - 002c cookie：HelloRetryRequest 之后的第二个 ClientHello 带 cookie
     *     （RFC 8446，服务器靠它确认地址所有权），丢了会被再次 HRR / 拒绝。
     *     真实站点常用 cookie-HRR 做防 DoS，故这条例外是功能正确性必需；
     *   - 无 002b supported_versions：纯 TLS 1.2 连接，不是画像目标。
     * 放行时指纹不对齐，但连接照常完成（Fail-Safe）。 */
    {
        uint16_t dl;
        if (find_ext(ext_start, ext_total, 0x0029, &dl) != NULL) { return -2; }
        if (find_ext(ext_start, ext_total, 0x002C, &dl) != NULL) { return -8; }
        if (find_ext(ext_start, ext_total, 0x002B, &dl) == NULL) { return -3; }
    }

    uint8_t *tmp = (uint8_t *) malloc(buf_cap);
    if (tmp == NULL) { return -4; }
    uint8_t *w = tmp;
    uint8_t *tmp_end = tmp + buf_cap;

    /* 1) 保留 legacy_version + random + legacy_session_id（mbedTLS 真实生成）。 */
    size_t fixed_len = 2 + 32 + 1 + (size_t) sid_len;
    FP_NEED(fixed_len);
    memcpy(w, p, fixed_len);
    w += fixed_len;

    /* 2) cipher_suites：按基准 16 项重写（含首 GREASE，无 SCSV）。 */
    uint16_t new_cs_len = (uint16_t) sizeof(k_ciphers);
    FP_PUT16(new_cs_len);
    FP_NEED(new_cs_len);
    for (size_t i = 0; i < sizeof(k_ciphers) / sizeof(k_ciphers[0]); i++) {
        *w++ = (uint8_t) (k_ciphers[i] >> 8);
        *w++ = (uint8_t) (k_ciphers[i] & 0xFF);
    }

    /* 3) legacy_compression_methods：原样（应为 01 00）。 */
    FP_NEED((size_t) 1 + comp_len);
    *w++ = comp_len;
    if (comp_len > 0) {
        memcpy(w, comp, comp_len);
        w += comp_len;
    }

    /* 4) extensions：先占 2 字节总长，再按基准顺序逐项写。 */
    FP_NEED(2);
    uint8_t *p_ext_len = w;
    w += 2;
    uint8_t *ext_body_start = w;

    for (size_t i = 0; i < FP_NEXTS; i++) {
        const fp_ext *e = &k_exts[i];
        const uint8_t *body = e->body;
        uint16_t blen = e->body_len;

        if (e->from_orig) {
            uint16_t dl = 0;
            body = find_ext(ext_start, ext_total, e->type, &dl);
            if (body == NULL) { free(tmp); return -6; }
            blen = dl;
        }

        FP_PUT16(e->type);
        FP_PUT16(blen);
        if (blen > 0) {
            FP_NEED(blen);
            memcpy(w, body, blen);
            w += blen;
        }
    }

    uint16_t new_ext_total = (uint16_t) (w - ext_body_start);
    p_ext_len[0] = (uint8_t) (new_ext_total >> 8);
    p_ext_len[1] = (uint8_t) (new_ext_total & 0xFF);

    size_t new_len = (size_t) (w - tmp);
    if (new_len > buf_cap) { free(tmp); return -7; }

    memcpy(buf, tmp, new_len);
    free(tmp);
    *out_len = new_len;
    return 0;
}
