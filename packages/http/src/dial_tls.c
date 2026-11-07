/*
 * dial_tls.c —— TLS 1.3 客户端会话层（mbedTLS 封装）。
 * 详见 dial_tls.h 的设计立场说明。
 */
#include "dial_tls.h"

#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <stdarg.h>

#include <mbedtls/ssl.h>
#include <mbedtls/x509_crt.h>
#include <mbedtls/entropy.h>
#include <mbedtls/ctr_drbg.h>
#include <mbedtls/error.h>

/* 跨平台 socket 错误宏（与 dial_http.c 同款，避免跨文件依赖） */
#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
#include <winerror.h>
#include <wincrypt.h>
typedef int dh_socklen;
#define DH_LASTERR() WSAGetLastError()
#define DH_IS_WOULDBLOCK(e) ((e) == WSAEWOULDBLOCK)
#else
#include <unistd.h>
#include <errno.h>
#include <sys/socket.h>
#include <netinet/in.h>
#define DH_LASTERR() errno
#define DH_IS_WOULDBLOCK(e) ((e) == EWOULDBLOCK || (e) == EAGAIN)
#endif

struct DialTls {
    mbedtls_ssl_context      ssl;
    mbedtls_ssl_config       conf;
    mbedtls_entropy_context  entropy;
    mbedtls_ctr_drbg_context drbg;
    mbedtls_x509_crt        ca;     /* 受信根 + 自定义 CA 链 */
    int                     fd;    /* 底层 TCP 套接字（BIO 上下文） */
};

/* ---- 自定义 BIO：在调用方的 socket 上收发 ---- */

static int bio_send(void *ctx, const unsigned char *buf, size_t len) {
    int fd = (int)(intptr_t)ctx;
#ifdef _WIN32
    int r = send(fd, (const char *)buf, (int)len, 0);
#else
    ssize_t r = send(fd, buf, len, 0);
#endif
    if (r >= 0) return (int)r;
    int e = DH_LASTERR();
    if (DH_IS_WOULDBLOCK(e)) return MBEDTLS_ERR_SSL_WANT_WRITE;
    return MBEDTLS_ERR_SSL_WANT_WRITE; /* 硬错误：让 mbedTLS 以错误收场 */
}

static int bio_recv(void *ctx, unsigned char *buf, size_t len) {
    int fd = (int)(intptr_t)ctx;
#ifdef _WIN32
    int r = recv(fd, (char *)buf, (int)len, 0);
#else
    ssize_t r = recv(fd, buf, len, 0);
#endif
    if (r > 0) return (int)r;
    if (r == 0) return 0;            /* 对端关闭（EOF） */
    int e = DH_LASTERR();
    if (DH_IS_WOULDBLOCK(e)) return MBEDTLS_ERR_SSL_WANT_READ;
    return MBEDTLS_ERR_SSL_WANT_READ;
}

/* f_recv_timeout 多一个 timeout 形参（毫秒）；我们的 socket 已自带超时，忽略它。 */
static int bio_recv_timeout(void *ctx, unsigned char *buf, size_t len, uint32_t timeout) {
    (void)timeout;
    return bio_recv(ctx, buf, len);
}

/* ---- 系统根证书库 ---- */

/* 各平台实现只在自己对应的分支里被 load_system_ca() 引用。用与调用方一致的
 * 条件编译守卫，避免在 Windows / macOS 目标下产生「未使用的静态函数」告警
 * （-Wall -Wextra 下 zig cc 会报）。 */
#if !defined(_WIN32) && !defined(__APPLE__)
static int load_linux_ca(mbedtls_x509_crt *ca) {
    /* Debian/Ubuntu 等：散列目录 + 合并包 */
    static const char *paths[] = {
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/ssl/cert.pem",
        NULL,
    };
    int ok = 0;
    if (mbedtls_x509_crt_parse_path(ca, "/etc/ssl/certs") == 0) ok = 1;
    for (int i = 0; paths[i]; i++) {
        if (mbedtls_x509_crt_parse_file(ca, paths[i]) == 0) ok = 1;
    }
    return ok;
}
#endif

#ifdef __APPLE__
static int load_macos_ca(mbedtls_x509_crt *ca) {
    static const char *paths[] = {
        "/etc/ssl/cert.pem",
        "/System/Library/Keychain/SystemRootCertificates.pem",
        NULL,
    };
    int ok = 0;
    for (int i = 0; paths[i]; i++) {
        if (mbedtls_x509_crt_parse_file(ca, paths[i]) == 0) ok = 1;
    }
    return ok;
}
#endif

#ifdef _WIN32
/* Windows：枚举系统 ROOT 存储，逐张 DER 解析进 mbedTLS 链。 */
static int load_windows_ca(mbedtls_x509_crt *ca) {
    HCERTSTORE store = CertOpenStore(CERT_STORE_PROV_SYSTEM_A,
                                     X509_ASN_ENCODING,
                                     0,
                                     CERT_STORE_OPEN_EXISTING_FLAG |
                                     CERT_SYSTEM_STORE_LOCAL_MACHINE,
                                     "ROOT");
    if (!store) return 0;
    int ok = 0;
    PCCERT_CONTEXT cert = NULL;
    while ((cert = CertEnumCertificatesInStore(store, cert)) != NULL) {
        if (mbedtls_x509_crt_parse_der(ca, cert->pbCertEncoded,
                                       cert->cbCertEncoded) == 0) {
            ok = 1;
        }
    }
    if (cert) CertFreeCertificateContext(cert);
    CertCloseStore(store, 0);
    return ok;
}
#endif

static int load_system_ca(mbedtls_x509_crt *ca) {
#ifdef _WIN32
    return load_windows_ca(ca);
#elif defined(__APPLE__)
    return load_macos_ca(ca);
#else
    return load_linux_ca(ca);
#endif
}

static void set_err(char *buf, size_t len, const char *fmt, ...) {
    if (!buf || len == 0) return;
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, len, fmt, ap);
    va_end(ap);
    buf[len - 1] = '\0';
}

DialTls *dial_tls_connect(int fd, const char *host, const char *const alpn[],
                          const char *ca_path, uint32_t *verify_result,
                          char *errbuf, size_t errlen) {
    if (verify_result) *verify_result = (uint32_t)-1;
    if (errbuf && errlen) errbuf[0] = '\0';
    if (fd < 0 || !host) {
        set_err(errbuf, errlen, "dial_tls_connect 参数非法");
        return NULL;
    }

    DialTls *t = (DialTls *)calloc(1, sizeof(*t));
    if (!t) { set_err(errbuf, errlen, "内存不足"); return NULL; }
    t->fd = fd;

    mbedtls_ssl_init(&t->ssl);
    mbedtls_ssl_config_init(&t->conf);
    mbedtls_entropy_init(&t->entropy);
    mbedtls_ctr_drbg_init(&t->drbg);
    mbedtls_x509_crt_init(&t->ca);

    /* RNG：熵源 + CTR-DRBG（握手需要随机数） */
    if (mbedtls_ctr_drbg_seed(&t->drbg, mbedtls_entropy_func, &t->entropy,
                              (const unsigned char *)"dial-http-client", 17) != 0) {
        set_err(errbuf, errlen, "TLS 随机数发生器初始化失败");
        dial_tls_free(t);
        return NULL;
    }

    /* 客户端默认配置：允许 TLS1.2 与 TLS1.3（优先 1.3，但保留 1.2 兼容老服务器） */
    if (mbedtls_ssl_config_defaults(&t->conf, MBEDTLS_SSL_IS_CLIENT,
                                     MBEDTLS_SSL_TRANSPORT_STREAM,
                                     MBEDTLS_SSL_PRESET_DEFAULT) != 0) {
        set_err(errbuf, errlen, "TLS 配置初始化失败");
        dial_tls_free(t);
        return NULL;
    }
    mbedtls_ssl_conf_rng(&t->conf, mbedtls_ctr_drbg_random, &t->drbg);
    mbedtls_ssl_conf_min_tls_version(&t->conf, MBEDTLS_SSL_VERSION_TLS1_2);
    mbedtls_ssl_conf_max_tls_version(&t->conf, MBEDTLS_SSL_VERSION_TLS1_3);

    /* 受信根证书：自定义 CA 优先，否则系统库 */
    int ca_ok = 0;
    if (ca_path && *ca_path) {
        if (mbedtls_x509_crt_parse_file(&t->ca, ca_path) == 0) ca_ok = 1;
        else if (mbedtls_x509_crt_parse_path(&t->ca, ca_path) == 0) ca_ok = 1;
        if (!ca_ok) {
            set_err(errbuf, errlen, "无法读取自定义 CA：%s", ca_path);
            dial_tls_free(t);
            return NULL;
        }
    } else {
        ca_ok = load_system_ca(&t->ca);
        if (!ca_ok) {
            set_err(errbuf, errlen,
                    "无法加载系统根证书库（Linux 需 /etc/ssl/certs，"
                    "macOS 需 /etc/ssl/cert.pem）");
            dial_tls_free(t);
            return NULL;
        }
    }
    mbedtls_ssl_conf_ca_chain(&t->conf, &t->ca, NULL);
    /* 证书校验 fail-closed：对端必须给出可链到受信根的合法证书 */
    mbedtls_ssl_conf_authmode(&t->conf, MBEDTLS_SSL_VERIFY_REQUIRED);

    /* SNI 严格等于主机名 */
    if (mbedtls_ssl_set_hostname(&t->ssl, host) != 0) {
        set_err(errbuf, errlen, "设置 SNI 失败：%s", host);
        dial_tls_free(t);
        return NULL;
    }
    /* ALPN（可为空） */
    if (alpn) {
        if (mbedtls_ssl_conf_alpn_protocols(&t->conf, (const char **)alpn) != 0) {
            set_err(errbuf, errlen, "设置 ALPN 失败");
            dial_tls_free(t);
            return NULL;
        }
    }

    if (mbedtls_ssl_setup(&t->ssl, &t->conf) != 0) {
        set_err(errbuf, errlen, "TLS 会话初始化失败");
        dial_tls_free(t);
        return NULL;
    }
    mbedtls_ssl_set_bio(&t->ssl, (void *)(intptr_t)fd, bio_send, bio_recv, bio_recv_timeout);

    int hr = mbedtls_ssl_handshake(&t->ssl);
    if (hr != 0) {
        char hex[128];
        mbedtls_strerror(hr, hex, sizeof(hex));
        set_err(errbuf, errlen, "TLS 握手失败：%s", hex);
        dial_tls_free(t);
        return NULL;
    }

    /* 握手“成功”后仍要查证书校验结果——VERIFY_REQUIRED 下 mbedTLS 一般不会
     * 走进这里，但显式再查一次，绝不依赖隐式行为（§20 宁崩溃不静默损坏）。 */
    uint32_t flags = mbedtls_ssl_get_verify_result(&t->ssl);
    if (flags != 0) {
        char info[512];
        info[0] = '\0';
        mbedtls_x509_crt_verify_info(info, sizeof(info), "", flags);
        set_err(errbuf, errlen, "TLS 证书校验失败：%s", info);
        if (verify_result) *verify_result = flags;
        dial_tls_free(t);
        return NULL;
    }
    if (verify_result) *verify_result = 0;
    return t;
}

int64_t dial_tls_send(DialTls *t, const void *buf, int64_t len) {
    if (!t || len < 0) return -1;
    int r = mbedtls_ssl_write(&t->ssl, (const unsigned char *)buf, (size_t)len);
    if (r < 0) return -1;
    return (int64_t)r;
}

int64_t dial_tls_recv(DialTls *t, void *buf, int64_t len) {
    if (!t || len < 0) return -1;
    int r = mbedtls_ssl_read(&t->ssl, (unsigned char *)buf, (size_t)len);
    if (r == 0) return 0;          /* EOF */
    /* HTTP/2 的单 I/O 线程用短 SO_RCVTIMEO 轮询读：底层 BIO 在超时时返回
     * WANT_READ/WANT_WRITE，这不是错误而是「此刻无数据」。返回 -2 让上层区分：
     *   >0 字节数 / 0 EOF / -2 暂无数据(可重试) / -1 真错误(连接中断)。
     * HTTP/1.1 的 dh_recv_some 把所有 <0 一律当 IO 错误，行为不变（同步请求下
     * 读超时本就该失败），故这个细分对既有路径零影响。 */
    if (r == MBEDTLS_ERR_SSL_WANT_READ || r == MBEDTLS_ERR_SSL_WANT_WRITE) return -2;
    if (r < 0) return -1;
    return (int64_t)r;
}

int64_t dial_tls_free(DialTls *t) {
    if (!t) return 0;
    /* 先发 close_notify（优雅关闭），失败也继续释放 */
    mbedtls_ssl_close_notify(&t->ssl);
    mbedtls_ssl_free(&t->ssl);
    mbedtls_ssl_config_free(&t->conf);
    mbedtls_x509_crt_free(&t->ca);
    mbedtls_ctr_drbg_free(&t->drbg);
    mbedtls_entropy_free(&t->entropy);
    free(t);
    return 0;
}

const char *dial_tls_version(DialTls *t) {
    if (!t) return "unknown";
    return mbedtls_ssl_get_version(&t->ssl);
}

const char *dial_tls_alpn(DialTls *t) {
    if (!t) return NULL;
    return mbedtls_ssl_get_alpn_protocol(&t->ssl);
}
