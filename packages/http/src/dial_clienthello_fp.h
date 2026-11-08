/* dial_clienthello_fp.h —— 浏览器一致性画像：ClientHello 报文重排钩子。
 *
 * 设计立场（严格遵守 third_party/README.md 的「绝不手搓密码学」铁律）：
 *   本模块**只做 ClientHello 的报文编排**（写 uint16_t / 字节、调整扩展顺序、
 *   插入 GREASE 哑值与哑扩展占位），不生成密钥、不碰 HKDF / AEAD / 签名 /
 *   RNG / transcript hash。真实 key_share 公钥、SNI、ALPN 等一律沿用 mbedTLS
 *   已生成的内容，只是被搬到基准顺序里。
 *
 * 注入点：mbedTLS ssl_client.c 的 ssl_write_client_hello_body() 末尾、
 * update_checksum() 之前。重排后回写 out_len，上层 checksum / finish_handshake_msg
 * 自动使用新长度，transcript 无需任何改动。
 *
 * 失败语义：任何异常（缓冲不足 / 解析失败 / 非 TLS1.3 握手 / 带 PSK binder /
 * HelloRetryRequest 后的 cookie CH）都返回非 0 且**不修改 buffer**，让
 * mbedTLS 原生 ClientHello 照常发出（握手不受影响，仅指纹不对齐）。
 * Fail-Safe 而非 Fail-Closed，避免画像改造拖垮连接。
 */
#ifndef DIAL_CLIENTHELLO_FP_H
#define DIAL_CLIENTHELLO_FP_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 把 buf 里已生成的 ClientHello body 就地重排为 Chrome 基准布局。
 *   buf      : ClientHello body 起点（不含 record / handshake 头）
 *   len      : 当前 body 长度
 *   buf_cap  : buf 可用容量（end - buf）
 *   out_len  : 成功时写回新长度
 * 返回 0 = 已重排；非 0 = 保持原样（调用方忽略即可）。
 */
int dial_fp_reshape_clienthello(unsigned char *buf, size_t len,
                                size_t buf_cap, size_t *out_len);

#ifdef __cplusplus
}
#endif

#endif /* DIAL_CLIENTHELLO_FP_H */
