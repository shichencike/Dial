/*
 * crypto.dialpkg —— FFI 包装层（v1.04 增补）。
 *
 * 本文件**不实现任何密码学原语**：摘要走 mbedTLS 的 md 抽象层
 * （mbedtls_md / mbedtls_md_hmac），Base64 走 mbedtls_base64_encode/decode。
 * 这里只做三件事：
 *   1. DialString/bytes 句柄 ↔ 裸字节缓冲 + 长度；
 *   2. 线程局部错误码与人话诊断（Fail-Fast，坏输入不静默吞下）；
 *   3. 摘要字节 → 小写 hex 文本 / 原始 bytes 句柄。
 *
 * 之所以复用 md.c 而不是自己拼 HMAC 的 ipad/opad：密码学绝不手搓，
 * mbedtls_md_hmac 是 vendor 代码里经过审查的实现，包括常量时间比较等细节。
 */

#include "dial_crypto.h"
#include "dial_bytes.h"

#include "mbedtls/md.h"
#include "mbedtls/base64.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* SHA-512 是最长的输出（64 字节）；hex 缓冲留 2 倍 + 结尾 NUL。 */
#define DIAL_CRYPTO_MAX_DIGEST 64

/* ------------------------------------------------------------------------- */
/* 线程局部错误槽                                                             */
/* ------------------------------------------------------------------------- */

static _Thread_local int64_t g_err = DIAL_CRYPTO_OK;
static _Thread_local char    g_err_msg[256] = {0};

static void cok(void) {
    g_err = DIAL_CRYPTO_OK;
    g_err_msg[0] = '\0';
}

static void cerr(int64_t code, const char *msg) {
    g_err = code;
    snprintf(g_err_msg, sizeof(g_err_msg), "%s", msg);
}

int64_t dial_crypto_last_error(void) { return g_err; }

DialString dial_crypto_last_error_msg(void) { return dial_string_new(g_err_msg); }

/* ------------------------------------------------------------------------- */
/* 句柄 → 裸字节                                                              */
/* ------------------------------------------------------------------------- */

/*
 * string 与 bytes 在 C 侧同为 DialString，布局一致：
 *   dial_string_cstr()    = (char*)h + 8，data 区且以 NUL 收尾；
 *   dial_string_length() = ((int64_t*)h)[0]，即字节数。
 * 因此对两者统一使用，且**长度优先于 NUL**——bytes 允许内嵌 \0。
 */
static const unsigned char *cdata(DialString h, size_t *len) {
    if (!h) {
        *len = 0;
        return (const unsigned char *)"";
    }
    int64_t n = dial_string_length(h);
    if (n < 0) n = 0;
    *len = (size_t)n;
    return (const unsigned char *)dial_string_cstr(h);
}

/* string 类函数失败时返回空串（而非裸 NULL，避免 Dial 侧字符串比较踩空句柄）。 */
static DialString empty_str(void) { return dial_string_from_bytes("", 0); }

/*
 * bytes 类函数失败时返回**长度 0 的空缓冲**，而不是 NULL 句柄。
 * 原因：Dial 的 is_null 准入名单不含 Type::Bytes（见 semantic.rs
 * is_nullable_type），语言层无法对 bytes 判空；且 NULL 句柄进入 ARC
 * retain/release 也是隐患。失败与否由 dial_crypto_last_error() 明确区分。
 */
static DialString empty_bytes(void) { return dial_bytes_new("", 0); }

/* ------------------------------------------------------------------------- */
/* 摘要                                                                       */
/* ------------------------------------------------------------------------- */

typedef struct {
    const char        *name;
    mbedtls_md_type_t  type;
} AlgoEntry;

static const AlgoEntry kAlgos[] = {
    { "md5",      MBEDTLS_MD_MD5      },
    { "sha1",     MBEDTLS_MD_SHA1     },
    { "sha256",   MBEDTLS_MD_SHA256   },
    { "sha512",   MBEDTLS_MD_SHA512   },
    { "sha3_256", MBEDTLS_MD_SHA3_256 },
    { "sha3-256", MBEDTLS_MD_SHA3_256 },  /* 常见别名，一并接受 */
};

static const mbedtls_md_info_t *info_of(const char *name) {
    for (size_t i = 0; i < sizeof(kAlgos) / sizeof(kAlgos[0]); i++) {
        if (strcmp(name, kAlgos[i].name) == 0) {
            return mbedtls_md_info_from_type(kAlgos[i].type);
        }
    }
    return NULL;
}

static int digest_raw(mbedtls_md_type_t type,
                      const unsigned char *data, size_t len,
                      unsigned char out[DIAL_CRYPTO_MAX_DIGEST], size_t *outlen) {
    const mbedtls_md_info_t *info = mbedtls_md_info_from_type(type);
    if (!info) return -1;                     /* 配置里没编进该算法 */
    int rc = mbedtls_md(info, data, len, out);
    if (rc != 0) return rc;
    *outlen = (size_t)mbedtls_md_get_size(info);
    return 0;
}

static DialString hex_of(const unsigned char *raw, size_t n) {
    static const char kHex[] = "0123456789abcdef";
    char buf[DIAL_CRYPTO_MAX_DIGEST * 2 + 1];
    if (n > DIAL_CRYPTO_MAX_DIGEST) n = DIAL_CRYPTO_MAX_DIGEST;
    for (size_t i = 0; i < n; i++) {
        buf[2 * i]     = kHex[raw[i] >> 4];
        buf[2 * i + 1] = kHex[raw[i] & 0x0F];
    }
    buf[2 * n] = '\0';
    return dial_string_from_bytes(buf, (int64_t)(2 * n));
}

static DialString digest_hex(mbedtls_md_type_t type, DialString h) {
    if (!h) {
        cerr(DIAL_CRYPTO_ERR_NULL, "摘要输入为空句柄");
        return empty_str();
    }
    size_t len = 0;
    const unsigned char *data = cdata(h, &len);
    unsigned char raw[DIAL_CRYPTO_MAX_DIGEST];
    size_t n = 0;
    int rc = digest_raw(type, data, len, raw, &n);
    if (rc == -1) {
        cerr(DIAL_CRYPTO_ERR_ALGO, "该摘要算法在当前 mbedTLS 配置下不可用");
        return empty_str();
    }
    if (rc != 0) {
        cerr(DIAL_CRYPTO_ERR_MBED, "mbedTLS 摘要计算失败");
        return empty_str();
    }
    cok();
    return hex_of(raw, n);
}

#define DEF_DIGEST(fn, ty)                                                    \
    DialString dial_crypto_##fn(DialString s)         { return digest_hex((ty), s); } \
    DialString dial_crypto_##fn##_bytes(DialString b) { return digest_hex((ty), b); }

DEF_DIGEST(md5,      MBEDTLS_MD_MD5)
DEF_DIGEST(sha1,     MBEDTLS_MD_SHA1)
DEF_DIGEST(sha256,   MBEDTLS_MD_SHA256)
DEF_DIGEST(sha512,   MBEDTLS_MD_SHA512)
DEF_DIGEST(sha3_256, MBEDTLS_MD_SHA3_256)

/* 按算法名返回**原始摘要字节**（不是 hex），供二进制用途。 */
DialString dial_crypto_digest(DialString algo, DialString data) {
    if (!algo || !data) {
        cerr(DIAL_CRYPTO_ERR_NULL, "算法名或数据为空句柄");
        return empty_bytes();
    }
    size_t alen = 0, dlen = 0;
    const char *a = (const char *)cdata(algo, &alen);
    const unsigned char *d = cdata(data, &dlen);

    char name[32];
    if (alen >= sizeof(name)) {
        cerr(DIAL_CRYPTO_ERR_ALGO, "未知摘要算法（名称过长）");
        return empty_bytes();
    }
    memcpy(name, a, alen);
    name[alen] = '\0';

    const mbedtls_md_info_t *info = info_of(name);
    if (!info) {
        cerr(DIAL_CRYPTO_ERR_ALGO,
             "未知摘要算法（仅支持 md5 / sha1 / sha256 / sha512 / sha3_256）");
        return empty_bytes();
    }
    unsigned char raw[DIAL_CRYPTO_MAX_DIGEST];
    if (mbedtls_md(info, d, dlen, raw) != 0) {
        cerr(DIAL_CRYPTO_ERR_MBED, "mbedTLS 摘要计算失败");
        return empty_bytes();
    }
    size_t n = (size_t)mbedtls_md_get_size(info);
    cok();
    return dial_bytes_new((const char *)raw, (int64_t)n);
}

/* ------------------------------------------------------------------------- */
/* Base64（RFC 4648，标准字母表 + = 补齐）                                     */
/* ------------------------------------------------------------------------- */

static DialString b64_encode(DialString h) {
    if (!h) {
        cerr(DIAL_CRYPTO_ERR_NULL, "Base64 输入为空句柄");
        return empty_str();
    }
    size_t len = 0;
    const unsigned char *data = cdata(h, &len);
    if (len == 0) {                 /* 空输入 → 空串，不是错误 */
        cok();
        return empty_str();
    }

    /* 探测：dlen=0 时 mbedTLS 在 *olen 里返回所需缓冲（含结尾 NUL）。 */
    size_t need = 0;
    int rc = mbedtls_base64_encode(NULL, 0, &need, data, len);
    if (rc != 0 && rc != MBEDTLS_ERR_BASE64_BUFFER_TOO_SMALL) {
        cerr(DIAL_CRYPTO_ERR_MBED, "mbedTLS Base64 长度探测失败");
        return empty_str();
    }
    if (need == 0) need = 1;
    unsigned char *buf = (unsigned char *)malloc(need);
    if (!buf) {
        cerr(DIAL_CRYPTO_ERR_NOMEM, "Base64 编码缓冲分配失败");
        return empty_str();
    }
    size_t olen = 0;
    rc = mbedtls_base64_encode(buf, need, &olen, data, len);
    if (rc != 0) {
        free(buf);
        cerr(DIAL_CRYPTO_ERR_MBED, "mbedTLS Base64 编码失败");
        return empty_str();
    }
    /* 实际编码的 olen 不含结尾 NUL，正好是纯 Base64 文本长度。 */
    DialString r = dial_string_from_bytes((const char *)buf, (int64_t)olen);
    free(buf);
    cok();
    return r;
}

DialString dial_crypto_base64_encode(DialString s)       { return b64_encode(s); }
DialString dial_crypto_base64_encode_bytes(DialString b) { return b64_encode(b); }

DialString dial_crypto_base64_decode(DialString s) {
    if (!s) {
        cerr(DIAL_CRYPTO_ERR_NULL, "Base64 输入为空句柄");
        return empty_bytes();
    }
    size_t len = 0;
    const unsigned char *data = cdata(s, &len);

    /* 探测阶段 mbedTLS 会先做完整字符校验，非法输入直接在这里被挡下。 */
    size_t need = 0;
    int rc = mbedtls_base64_decode(NULL, 0, &need, data, len);
    if (rc == MBEDTLS_ERR_BASE64_INVALID_CHARACTER) {
        cerr(DIAL_CRYPTO_ERR_DECODE, "Base64 输入含非法字符或多余的 =");
        return empty_bytes();
    }
    if (rc != 0 && rc != MBEDTLS_ERR_BASE64_BUFFER_TOO_SMALL) {
        cerr(DIAL_CRYPTO_ERR_MBED, "mbedTLS Base64 长度探测失败");
        return empty_bytes();
    }
    unsigned char *buf = (unsigned char *)malloc(need ? need : 1);
    if (!buf) {
        cerr(DIAL_CRYPTO_ERR_NOMEM, "Base64 解码缓冲分配失败");
        return empty_bytes();
    }
    size_t olen = 0;
    rc = mbedtls_base64_decode(buf, need, &olen, data, len);
    if (rc != 0) {
        free(buf);
        if (rc == MBEDTLS_ERR_BASE64_INVALID_CHARACTER) {
            cerr(DIAL_CRYPTO_ERR_DECODE, "Base64 输入含非法字符或多余的 =");
        } else {
            cerr(DIAL_CRYPTO_ERR_MBED, "mbedTLS Base64 解码失败");
        }
        return empty_bytes();
    }
    DialString r = dial_bytes_new((const char *)buf, (int64_t)olen);
    free(buf);
    cok();
    return r;
}

/* ------------------------------------------------------------------------- */
/* HMAC-SHA256                                                                */
/* ------------------------------------------------------------------------- */

static DialString hmac_hex(DialString key, DialString msg) {
    if (!key || !msg) {
        cerr(DIAL_CRYPTO_ERR_NULL, "HMAC 密钥或消息为空句柄");
        return empty_str();
    }
    size_t klen = 0, mlen = 0;
    const unsigned char *k = cdata(key, &klen);
    const unsigned char *m = cdata(msg, &mlen);
    const mbedtls_md_info_t *info = mbedtls_md_info_from_type(MBEDTLS_MD_SHA256);
    if (!info) {
        cerr(DIAL_CRYPTO_ERR_ALGO, "SHA-256 在当前 mbedTLS 配置下不可用");
        return empty_str();
    }
    unsigned char raw[DIAL_CRYPTO_MAX_DIGEST];
    if (mbedtls_md_hmac(info, k, klen, m, mlen, raw) != 0) {
        cerr(DIAL_CRYPTO_ERR_MBED, "mbedTLS HMAC 计算失败");
        return empty_str();
    }
    size_t n = (size_t)mbedtls_md_get_size(info);
    cok();
    return hex_of(raw, n);
}

DialString dial_crypto_hmac_sha256(DialString key, DialString msg) {
    return hmac_hex(key, msg);
}

DialString dial_crypto_hmac_sha256_bytes(DialString key, DialString msg) {
    return hmac_hex(key, msg);
}
