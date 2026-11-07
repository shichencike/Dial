#ifndef DIAL_CRYPTO_H
#define DIAL_CRYPTO_H

#include <stdint.h>
#include "dial_string.h"

/*
 * crypto.dialpkg 的 C 接口。
 *
 * 注意：Dial 的 `string` 与 `bytes` 在 FFI 边界上**都是 DialString 句柄**——
 * 两者内存布局相同（[refcount/arc 头][int64 len][data...]，data 以 NUL 收尾），
 * 只是语言层类型不同、禁止隐式互转。因此 C 侧签名统一写 DialString，
 * 由 .diali 一侧声明成 string 或 bytes 来给 Dial 程序定类型。
 * 取数据一律用 dial_string_cstr() + dial_string_length()，对两者都成立。
 *
 * 返回 bytes 的函数必须用 dial_bytes_new() 构造句柄，返回 string 的用
 * dial_string_new()/dial_string_from_bytes()——绝不返回裸 const char *。
 */

/* 错误码（线程局部，与 crypto.diali 的注释一致） */
#define DIAL_CRYPTO_OK          0
#define DIAL_CRYPTO_ERR_ALGO    (-1)   /* 未知摘要算法 */
#define DIAL_CRYPTO_ERR_NULL    (-2)   /* 空句柄 */
#define DIAL_CRYPTO_ERR_DECODE  (-3)   /* Base64 输入非法 */
#define DIAL_CRYPTO_ERR_NOMEM   (-4)   /* 内存不足 */
#define DIAL_CRYPTO_ERR_MBED    (-5)   /* mbedTLS 内部错误 */

/* ---- 摘要：string 输入 → 小写 hex ---- */
DialString dial_crypto_md5(DialString s);
DialString dial_crypto_sha1(DialString s);
DialString dial_crypto_sha256(DialString s);
DialString dial_crypto_sha512(DialString s);
DialString dial_crypto_sha3_256(DialString s);

/* ---- 摘要：bytes 输入 → 小写 hex ---- */
DialString dial_crypto_md5_bytes(DialString b);
DialString dial_crypto_sha1_bytes(DialString b);
DialString dial_crypto_sha256_bytes(DialString b);
DialString dial_crypto_sha512_bytes(DialString b);
DialString dial_crypto_sha3_256_bytes(DialString b);

/* ---- 按算法名取原始摘要字节（返回 bytes 句柄） ---- */
DialString dial_crypto_digest(DialString algo, DialString data);

/* ---- Base64 ---- */
DialString dial_crypto_base64_encode(DialString s);
DialString dial_crypto_base64_encode_bytes(DialString b);
DialString dial_crypto_base64_decode(DialString s);

/* ---- HMAC-SHA256 → 小写 hex ---- */
DialString dial_crypto_hmac_sha256(DialString key, DialString msg);
DialString dial_crypto_hmac_sha256_bytes(DialString key, DialString msg);

/* ---- 错误诊断 ---- */
int64_t    dial_crypto_last_error(void);
DialString dial_crypto_last_error_msg(void);

#endif /* DIAL_CRYPTO_H */
