#ifndef DIAL_TLS_H
#define DIAL_TLS_H

#include <stdint.h>
#include <stddef.h>

/*
 * dial_tls —— 用 vendor 的 mbedTLS 3.6.2 封装的 TLS 1.3 客户端会话层。
 *
 * 设计立场（与包整体一致）：
 *   - 密码学**绝不手搓**：握手、密钥交换、证书链校验全部交给 mbedTLS。
 *   - **证书校验 fail-closed**：默认 VERIFY_REQUIRED，握手后若
 *     mbedtls_ssl_get_verify_result() != 0（过期 / 域名不符 / 不受信根 /
 *     自签名），立刻失败并给出中文原因——**绝不做静默降级**（符合 §20 Fail-Fast）。
 *   - 系统根证书库：Linux 读 /etc/ssl/certs；macOS 读 /etc/ssl/cert.pem 与
 *     系统钥匙串；Windows 读系统 ROOT 存储（wincrypt）。允许传入自定义 CA
 *     路径（内网 / 私有 CA 场景）。
 *   - 复用调用方的 TCP 套接字：通过 mbedTLS 的自定义 BIO（f_send/f_recv）
 *     直接在我们已建立的 socket 上收发，不引入 mbedTLS 自己的联网层。
 *   - SNI 严格等于对端主机名；ALPN 由调用方决定（HTTP/1.1 层 offer
 *     `http/1.1`，HTTP/2 层 offer `h2`），协商结果由各层自行校验，
 *     绝不静默降级为不会说的协议。
 *
 * 注意：本层只负责「在已连上的 TCP 之上做 TLS」；TCP 连接本身由 dial_http.c
 * 负责。Dial 侧拿到的还是不透明句柄 + 收发函数，看不到任何 mbedTLS 符号。
 */

typedef struct DialTls DialTls;

/*
 * 在已连上的 TCP 套接字 fd 之上建立 TLS 客户端会话。
 *   host     —— SNI 与证书校验的主机名（必填）。
 *   alpn     —— 以 NULL 结尾的协议名数组（如 {"http/1.1", NULL}），可为 NULL（不协商 ALPN）。
 *   ca_path  —— 自定义 CA 文件或目录（PEM）；为 NULL 时加载系统根证书库。
 *   verify_result —— 输出握手后的证书校验结果位掩码（0 = 通过）。
 *   errbuf/errlen —— 失败时的中文原因（至少为 1 字节，会被 '\0' 终止）。
 * 成功返回 DialTls*；失败返回 NULL（原因写入 errbuf，*verify_result 置为非零）。
 */
DialTls *dial_tls_connect(int fd, const char *host, const char *const alpn[],
                          const char *ca_path, uint32_t *verify_result,
                          char *errbuf, size_t errlen);

/* 经 TLS 收发（语义同 send/recv）。返回字节数，< 0 表示错误。 */
int64_t dial_tls_send(DialTls *t, const void *buf, int64_t len);
int64_t dial_tls_recv(DialTls *t, void *buf, int64_t len);

/* 发送 close_notify 并释放全部 mbedTLS 资源。返回 0 成功。 */
int64_t dial_tls_free(DialTls *t);

/* 协商出的协议版本字符串（如 "TLS1.3" / "TLS1.2"）。 */
const char *dial_tls_version(DialTls *t);

/* 协商出的 ALPN 协议名（如 "http/1.1"）；未协商返回 NULL。 */
const char *dial_tls_alpn(DialTls *t);

#endif /* DIAL_TLS_H */
