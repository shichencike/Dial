/*
 * dial_http —— HTTP/1.1 客户端实现（http.dialpkg 的机器码部分）。
 *
 * 协议层全部自研：URL 解析、请求行与 Header 拼装、响应报文解析、
 * chunked 解码、Socket 读写。压缩解压走 zlib（inflate），TLS 不在本文件
 * （由 mbedtls 提供，本轮只编入静态库、暂不向 Dial 暴露）。
 *
 * 跨平台：Windows 用 winsock2，其余用 POSIX socket。句柄释放是显式的
 * （dial_http_close / dial_http_resp_free），不透明句柄不进 ARC。
 */

#include "dial_http.h"

#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <ctype.h>

#include "dial_string.h"
#include "dial_bytes.h"
#include "zlib.h"

#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
typedef SOCKET dh_socket;
typedef int dh_socklen;
#define DH_INVALID_SOCKET INVALID_SOCKET
#define DH_CLOSE(s) closesocket(s)
#define DH_LASTERR() WSAGetLastError()
#define DH_IS_WOULDBLOCK(e) ((e) == WSAEWOULDBLOCK)
#else
#include <unistd.h>
#include <errno.h>
#include <netdb.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
typedef int dh_socket;
typedef socklen_t dh_socklen;
#define DH_INVALID_SOCKET (-1)
#define DH_CLOSE(s) close(s)
#define DH_LASTERR() errno
#define DH_IS_WOULDBLOCK(e) ((e) == EWOULDBLOCK || (e) == EAGAIN)
#endif

/* ------------------------------------------------------------------ */
/* 错误槽（线程局部）                                                  */
/* ------------------------------------------------------------------ */

static _Thread_local int64_t g_err = DIAL_HTTP_OK;
static _Thread_local char g_err_msg[256] = {0};

/* 自己的 strdup：Windows 的严格 C 模式未必暴露 POSIX 的 strdup。 */
static char *dh_strdup(const char *s) {
    size_t n = strlen(s) + 1;
    char *p = (char *)malloc(n);
    if (p) memcpy(p, s, n);
    return p;
}

static void dh_set_error(int64_t code, const char *msg) {
    g_err = code;
    if (!msg) msg = "";
    strncpy(g_err_msg, msg, sizeof(g_err_msg) - 1);
    g_err_msg[sizeof(g_err_msg) - 1] = '\0';
}

int64_t dial_http_last_error(void) { return g_err; }

DialString dial_http_last_error_msg(void) {
    return dial_string_new(g_err_msg[0] ? g_err_msg : "无错误");
}

/* ------------------------------------------------------------------ */
/* 动态字节缓冲                                                        */
/* ------------------------------------------------------------------ */

typedef struct {
    char  *data;
    int64_t len;
    int64_t cap;
} dh_buf;

static void dh_buf_free(dh_buf *b) {
    if (!b) return;
    free(b->data);
    b->data = NULL;
    b->len = 0;
    b->cap = 0;
}

static int dh_buf_reserve(dh_buf *b, int64_t need) {
    if (b->cap - b->len >= need) return 1;
    int64_t cap = b->cap ? b->cap : 256;
    while (cap - b->len < need) {
        if (cap > (int64_t)1 << 40) return 0; /* 防溢出 */
        cap *= 2;
    }
    char *p = (char *)realloc(b->data, (size_t)cap);
    if (!p) return 0;
    b->data = p;
    b->cap = cap;
    return 1;
}

static int dh_buf_append(dh_buf *b, const char *p, int64_t n) {
    if (n < 0) return 0;
    if (!dh_buf_reserve(b, n)) return 0;
    memcpy(b->data + b->len, p, (size_t)n);
    b->len += n;
    return 1;
}

static int dh_buf_puts(dh_buf *b, const char *s) {
    return dh_buf_append(b, s, (int64_t)strlen(s));
}

/* ------------------------------------------------------------------ */
/* 连接                                                                */
/* ------------------------------------------------------------------ */

typedef struct {
    char  *name;
    char  *value;
} dh_header;

struct DialHttpConn {
    dh_socket fd;
    int64_t timeout_sec;
    dh_buf  in;      /* 读缓冲 */
    int64_t in_pos;  /* 读缓冲已消费位置 */
};

struct DialHttpResponse {
    int64_t status;
    dh_buf  body;
    dh_header *headers;
    int64_t nheaders;
    int64_t cap_headers;
};

/* Windows 需要一次性 WSAStartup。用原子标志保证只做一次。 */
#ifdef _WIN32
static volatile LONG g_wsa_inited = 0;
static void dh_net_init(void) {
    if (InterlockedCompareExchange(&g_wsa_inited, 1, 0) == 0) {
        WSADATA wsa;
        WSAStartup(MAKEWORD(2, 2), &wsa);
    }
}
#else
static void dh_net_init(void) { }
#endif

/* 大小写不敏感比较（头名查找用；不依赖 strcasecmp/_stricmp 的平台差异） */
static int dh_eq_ci(const char *a, const char *b) {
    while (*a && *b) {
        int ca = tolower((unsigned char)*a);
        int cb = tolower((unsigned char)*b);
        if (ca != cb) return 0;
        a++; b++;
    }
    return *a == '\0' && *b == '\0';
}

/* 把 "Name: value" 头行拆成两段（就地切分，返回的指针都指向同一块内存） */
static void dh_split_header(char *line, char **name, char **value) {
    char *p = line;
    while (*p == ' ' || *p == '\t') p++;
    char *colon = strchr(p, ':');
    if (!colon) {
        *name = p;
        *value = p + strlen(p);
        return;
    }
    *colon = '\0';
    char *v = colon + 1;
    while (*v == ' ' || *v == '\t') v++;
    /* 去掉行尾空白 */
    size_t vl = strlen(v);
    while (vl > 0 && (v[vl - 1] == ' ' || v[vl - 1] == '\t')) v[--vl] = '\0';
    *name = p;
    *value = v;
}

/* ------------------------------------------------------------------ */
/* URL 解析                                                            */
/* ------------------------------------------------------------------ */

typedef struct {
    char   *host;
    int     port;
    char   *path;    /* 含 query，形如 "/a/b?x=1" */
    char   *hostport;/* Host 头用的值：host[:port]（省略默认端口） */
} dh_url;

static void dh_url_free(dh_url *u) {
    free(u->host);
    free(u->path);
    free(u->hostport);
    memset(u, 0, sizeof(*u));
}

/*
 * 解析 URL：
 *   [scheme://]host[:port][/path][?query]
 * 省略 scheme 按 http 处理；省略端口按 80；省略 path 按 "/"。
 * 只认 http/https——https 返回 DIAL_HTTP_ERR_UNSUPPORTED（本轮只做明文）。
 */
static int dh_url_parse(const char *url, dh_url *out) {
    memset(out, 0, sizeof(*out));
    if (!url) {
        dh_set_error(DIAL_HTTP_ERR_URL, "URL 为空");
        return 0;
    }
    const char *p = url;
    /* 跳过前导空白 */
    while (*p == ' ' || *p == '\t') p++;
    if (*p == '\0') {
        dh_set_error(DIAL_HTTP_ERR_URL, "URL 为空");
        return 0;
    }

    int is_https = 0;
    const char *scheme_end = strstr(p, "://");
    if (scheme_end) {
        size_t slen = (size_t)(scheme_end - p);
        if (slen == 4 && strncmp(p, "http", 4) == 0) {
            is_https = 0;
        } else if (slen == 5 && strncmp(p, "https", 5) == 0) {
            is_https = 1;
        } else {
            dh_set_error(DIAL_HTTP_ERR_UNSUPPORTED, "只支持 http:// 与 https://");
            return 0;
        }
        p = scheme_end + 3;
    }
    if (is_https) {
        dh_set_error(DIAL_HTTP_ERR_UNSUPPORTED,
                     "本轮只提供明文 HTTP/1.1：https:// 需要 TLS（由 mbedtls 提供，尚未接线）");
        return 0;
    }

    /* host[:port] 到 '/' 或 '?' 或结尾 */
    const char *hstart = p;
    const char *hend = hstart;
    while (*hend && *hend != '/' && *hend != '?' && *hend != '#') hend++;
    size_t hlen = (size_t)(hend - hstart);
    if (hlen == 0) {
        dh_set_error(DIAL_HTTP_ERR_URL, "URL 缺少主机名");
        return 0;
    }

    /* 拆出端口（IPv6 字面量 [::1]:80 暂不支持，明文 HTTP 场景足够） */
    const char *colon = (const char *)memchr(hstart, ':', hlen);
    size_t portlen = 0;
    if (colon) {
        portlen = hlen - (size_t)(colon - hstart) - 1;
    }
    out->host = (char *)malloc(hlen + 1);
    if (!out->host) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); return 0; }
    if (colon) {
        memcpy(out->host, hstart, (size_t)(colon - hstart));
        out->host[colon - hstart] = '\0';
        char portbuf[16];
        if (portlen == 0 || portlen >= sizeof(portbuf)) {
            dh_set_error(DIAL_HTTP_ERR_URL, "URL 端口非法");
            dh_url_free(out);
            return 0;
        }
        memcpy(portbuf, colon + 1, portlen);
        portbuf[portlen] = '\0';
        out->port = atoi(portbuf);
        if (out->port <= 0 || out->port > 65535) {
            dh_set_error(DIAL_HTTP_ERR_URL, "URL 端口非法");
            dh_url_free(out);
            return 0;
        }
    } else {
        memcpy(out->host, hstart, hlen);
        out->host[hlen] = '\0';
        out->port = 80;
    }

    /* path（含 query）；缺省 "/" */
    if (*hend == '\0' || *hend == '#') {
        out->path = (char *)malloc(2);
        if (!out->path) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); dh_url_free(out); return 0; }
        strcpy(out->path, "/");
    } else {
        size_t plen = strlen(hend);
        /* 去掉 fragment（HTTP 请求里用不到） */
        const char *hash = strchr(hend, '#');
        if (hash) plen = (size_t)(hash - hend);
        out->path = (char *)malloc(plen + 1);
        if (!out->path) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); dh_url_free(out); return 0; }
        memcpy(out->path, hend, plen);
        out->path[plen] = '\0';
    }

    /* Host 头：默认端口 80 不带端口号（与主流实现一致） */
    size_t need = strlen(out->host) + 8;
    out->hostport = (char *)malloc(need);
    if (!out->hostport) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); dh_url_free(out); return 0; }
    if (out->port == 80) {
        snprintf(out->hostport, need, "%s", out->host);
    } else {
        snprintf(out->hostport, need, "%s:%d", out->host, out->port);
    }
    return 1;
}

/* ------------------------------------------------------------------ */
/* Socket 读写                                                        */
/* ------------------------------------------------------------------ */

static void dh_set_socket_timeout(dh_socket fd, int64_t sec) {
#ifdef _WIN32
    DWORD tv = (DWORD)(sec > 0 ? sec * 1000 : 0);
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, (const char *)&tv, sizeof(tv));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, (const char *)&tv, sizeof(tv));
#else
    struct timeval tv;
    tv.tv_sec = (time_t)(sec > 0 ? sec : 0);
    tv.tv_usec = 0;
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
#endif
}

/*
 * 单次 recv：**有多少读多少，不等满**。
 *
 * 行读取的补货必须用它，而不是「读满 n 字节」的循环：后者在 keep-alive
 * 服务器上会一直等凑够 n 字节（响应只有 100 字节时就挂到超时）。
 */
static int64_t dh_recv_some(DialHttpConn *c, char *buf, int64_t n) {
    if (n <= 0) return 0;
    int chunk = (int)(n > 65536 ? 65536 : n);
#ifdef _WIN32
    int r = recv(c->fd, buf, chunk, 0);
#else
    ssize_t r = recv(c->fd, buf, (size_t)chunk, 0);
#endif
    if (r == 0) return 0;              /* 对端关闭 */
    if (r < 0) {
        int e = DH_LASTERR();
        if (DH_IS_WOULDBLOCK(e)) {
            dh_set_error(DIAL_HTTP_ERR_IO, "读取超时（对端长时间无响应）");
        } else {
            dh_set_error(DIAL_HTTP_ERR_IO, "读取失败（连接中断）");
        }
        return -1;
    }
    return r;
}

/* 从连接读 n 字节到 buf；返回读到的字节数，0 = 对端关闭，-1 = 出错 */
static int64_t dh_read_raw(DialHttpConn *c, char *buf, int64_t n) {
    int64_t total = 0;
    while (total < n) {
        int64_t r = dh_recv_some(c, buf + total, n - total);
        if (r < 0) return -1;
        if (r == 0) break;              /* 对端关闭 */
        total += r;
    }
    return total;
}

/* 从连接的读缓冲里取出一行（不含 '\n'，去掉末尾 '\r'）。返回 0 成功。 */
static int dh_read_line_raw(DialHttpConn *c, char **out, int64_t *out_len) {
    dh_buf *b = &c->in;
    for (;;) {
        /* 找换行 */
        char *start = b->data + c->in_pos;
        int64_t avail = b->len - c->in_pos;
        char *nl = (char *)memchr(start, '\n', (size_t)avail);
        if (nl) {
            int64_t line_len = (int64_t)(nl - start);
            char *line = (char *)malloc((size_t)line_len + 1);
            if (!line) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); return 0; }
            memcpy(line, start, (size_t)line_len);
            if (line_len > 0 && line[line_len - 1] == '\r') line[line_len - 1] = '\0';
            line[line_len] = '\0';
            *out = line;
            /* 长度必须是**去掉 \r 之后**的长度：调用方靠「长度为 0」判断空行
             * （头部结束）。若把含 \r 的原始长度报出去，"\r\n" 会被当成
             * 「长度 1 的非空行」，头部解析就永远等不到结束。 */
            *out_len = (int64_t)strlen(line);
            c->in_pos += line_len + 1;
            /* 消费完就把缓冲挪回开头，避免无限增长 */
            if (c->in_pos >= b->len) {
                b->len = 0;
                c->in_pos = 0;
            }
            return 1;
        }
        /* 缓冲里没有换行：需要更多数据（单次 recv，有多少读多少） */
        if (!dh_buf_reserve(b, 4096)) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); return 0; }
        char tmp[4096];
        int64_t r = dh_recv_some(c, tmp, (int64_t)sizeof(tmp));
        if (r < 0) return 0;
        if (r == 0) {
            /* 对端关闭：把剩余内容当作最后一行（没有换行符） */
            int64_t left = b->len - c->in_pos;
            if (left <= 0) {
                dh_set_error(DIAL_HTTP_ERR_IO, "连接在读取响应头时被对端关闭");
                return 0;
            }
            char *line = (char *)malloc((size_t)left + 1);
            if (!line) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); return 0; }
            memcpy(line, b->data + c->in_pos, (size_t)left);
            line[left] = '\0';
            *out = line;
            *out_len = (int64_t)strlen(line);
            b->len = 0;
            c->in_pos = 0;
            return 1;
        }
        if (!dh_buf_append(b, tmp, r)) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); return 0; }
    }
}

/*
 * 读满 n 字节到 buf。
 *
 * **必须先取连接读缓冲里已有的字节**：行读取可能已经把整份响应吞进缓冲
 * （比如服务器发完就关闭时，一次 recv 就把响应全读了进来）。若直接去
 * socket 读，缓冲里的 body 会被漏掉，而 socket 那边又已经关闭 →
 * 误报「连接提前关闭」。
 */
static int dh_read_exact(DialHttpConn *c, char *buf, int64_t n) {
    int64_t filled = 0;
    int64_t avail = c->in.len - c->in_pos;
    if (avail > 0) {
        int64_t take = avail < n ? avail : n;
        memcpy(buf, c->in.data + c->in_pos, (size_t)take);
        c->in_pos += take;
        filled += take;
        if (c->in_pos >= c->in.len) { c->in.len = 0; c->in_pos = 0; }
    }
    if (filled < n) {
        int64_t got = dh_read_raw(c, buf + filled, n - filled);
        if (got < 0) return 0;
        filled += got;
    }
    if (filled != n) {
        dh_set_error(DIAL_HTTP_ERR_IO, "读取响应体时连接提前关闭");
        return 0;
    }
    return 1;
}

/* ------------------------------------------------------------------ */
/* 连接建立                                                            */
/* ------------------------------------------------------------------ */

/*
 * FFI 约定（重要）：Dial 的 `string` 跨到 C 时传的是**载荷指针**
 * （布局 `[int64 len][bytes...]`），不是字节首地址——标准库因此才用
 * `dial_string_cstr()` 取文本（载荷 + 8）。凡是「从 Dial 收到的字符串」，
 * 都必须先 `dial_string_cstr()` 转成 C 字符串再当 `const char *` 用。
 */
static DialHttpConn *dh_connect_raw(const char *url) {
    dh_set_error(DIAL_HTTP_OK, "");
    dh_net_init();

    dh_url u;
    if (!dh_url_parse(url, &u)) return NULL;

    char portbuf[16];
    snprintf(portbuf, sizeof(portbuf), "%d", u.port);

    struct addrinfo hints;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;          /* IPv4 / IPv6 都试 */
    hints.ai_socktype = SOCK_STREAM;

    struct addrinfo *res = NULL;
    int gai = getaddrinfo(u.host, portbuf, &hints, &res);
    if (gai != 0 || !res) {
        char msg[256];
        snprintf(msg, sizeof(msg), "域名解析失败：%s", gai_strerror(gai));
        dh_set_error(DIAL_HTTP_ERR_DNS, msg);
        dh_url_free(&u);
        return NULL;
    }

    dh_socket fd = DH_INVALID_SOCKET;
    for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
        fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
        if (fd == DH_INVALID_SOCKET) continue;
        if (connect(fd, ai->ai_addr, (dh_socklen)ai->ai_addrlen) == 0) break;
        DH_CLOSE(fd);
        fd = DH_INVALID_SOCKET;
    }
    freeaddrinfo(res);

    if (fd == DH_INVALID_SOCKET) {
        dh_set_error(DIAL_HTTP_ERR_CONNECT, "TCP 连接失败（主机不可达或端口未开放）");
        dh_url_free(&u);
        return NULL;
    }

    DialHttpConn *c = (DialHttpConn *)calloc(1, sizeof(DialHttpConn));
    if (!c) {
        DH_CLOSE(fd);
        dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
        dh_url_free(&u);
        return NULL;
    }
    c->fd = fd;
    c->timeout_sec = 30;   /* 默认 30 秒收发超时：宁可超时失败，也不无限期挂死 */
    dh_set_socket_timeout(fd, c->timeout_sec);
    dh_url_free(&u);
    return c;
}

DialHttpConn *dial_http_connect(DialString url) {
    if (!url) {
        dh_set_error(DIAL_HTTP_ERR_URL, "URL 为空");
        return NULL;
    }
    return dh_connect_raw(dial_string_cstr(url));
}

static int64_t dh_write_raw(DialHttpConn *c, const char *p, int64_t len) {
    int64_t total = 0;
    while (total < len) {
        int chunk = (int)((len - total) > 65536 ? 65536 : (len - total));
#ifdef _WIN32
        int r = send(c->fd, p + total, chunk, 0);
#else
        ssize_t r = send(c->fd, p + total, (size_t)chunk, 0);
#endif
        if (r <= 0) {
            int e = DH_LASTERR();
            if (DH_IS_WOULDBLOCK(e)) {
                dh_set_error(DIAL_HTTP_ERR_IO, "发送超时");
            } else {
                dh_set_error(DIAL_HTTP_ERR_IO, "发送失败（连接中断）");
            }
            return -1;
        }
        total += r;
    }
    return total;
}

int64_t dial_http_write(DialHttpConn *c, DialString data) {
    if (!c || !data) {
        dh_set_error(DIAL_HTTP_ERR_STATE, "dial_http_write 参数非法");
        return -1;
    }
    int64_t len = dial_string_length(data);
    const char *p = dial_string_cstr(data);
    return dh_write_raw(c, p, len);
}

/*
 * 按「行」写：写完自动补 CRLF。
 *
 * 为什么需要它：Dial 的字符串字面量只支持 `\n \t \\ \"` 四种转义（README §字面量表），
 * **不支持 `\r`** ——而 HTTP 的报文行结束符恰恰是 CRLF。改语言去加 `\r`
 * 是一处不该由「发一个 HTTP 包」顺带做的语言变更；把 CRLF 收进包里更合适：
 * 调用方按行写，包负责行结束符，既贴合 HTTP 语义，又不动语言。
 */
int64_t dial_http_write_line(DialHttpConn *c, DialString line) {
    if (!c || !line) {
        dh_set_error(DIAL_HTTP_ERR_STATE, "dial_http_write_line 参数非法");
        return -1;
    }
    int64_t len = dial_string_length(line);
    const char *p = dial_string_cstr(line);
    int64_t wrote = dh_write_raw(c, p, len);
    if (wrote < 0) return -1;
    wrote = dh_write_raw(c, "\r\n", 2);
    if (wrote < 0) return -1;
    return len + 2;
}

DialString dial_http_read_line(DialHttpConn *c) {
    if (!c) {
        dh_set_error(DIAL_HTTP_ERR_STATE, "dial_http_read_line 句柄为空");
        return NULL;
    }
    char *line = NULL;
    int64_t len = 0;
    if (!dh_read_line_raw(c, &line, &len)) return NULL;
    DialString s = dial_string_new(line);
    free(line);
    return s;
}

DialString dial_http_read_all(DialHttpConn *c) {
    if (!c) {
        dh_set_error(DIAL_HTTP_ERR_STATE, "dial_http_read_all 句柄为空");
        return NULL;
    }
    dh_buf out;
    memset(&out, 0, sizeof(out));
    /* 连接里可能已有缓冲数据，先并进去 */
    if (c->in.len > c->in_pos) {
        if (!dh_buf_append(&out, c->in.data + c->in_pos, c->in.len - c->in_pos)) {
            dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
            dh_buf_free(&out);
            return NULL;
        }
        c->in.len = 0;
        c->in_pos = 0;
    }
    char tmp[8192];
    for (;;) {
        int64_t r = dh_recv_some(c, tmp, (int64_t)sizeof(tmp));
        if (r < 0) { dh_buf_free(&out); return NULL; }
        if (r == 0) break;
        if (!dh_buf_append(&out, tmp, r)) {
            dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
            dh_buf_free(&out);
            return NULL;
        }
    }
    DialString s = dial_bytes_new(out.data ? out.data : "", out.len);
    dh_buf_free(&out);
    return s;
}

int64_t dial_http_close(DialHttpConn *c) {
    if (!c) return 0;
    if (c->fd != DH_INVALID_SOCKET) {
        DH_CLOSE(c->fd);
        c->fd = DH_INVALID_SOCKET;
    }
    dh_buf_free(&c->in);
    free(c);
    return 0;
}

int64_t dial_http_set_timeout(DialHttpConn *c, int64_t seconds) {
    if (!c) {
        dh_set_error(DIAL_HTTP_ERR_STATE, "dial_http_set_timeout 句柄为空");
        return -1;
    }
    c->timeout_sec = seconds > 0 ? seconds : 0;
    dh_set_socket_timeout(c->fd, c->timeout_sec);
    return 0;
}

/* ------------------------------------------------------------------ */
/* gzip / deflate 解压（zlib inflate）                                */
/* ------------------------------------------------------------------ */

/*
 * 按 Content-Encoding 解压：gzip 用 15+16，deflate 用 15（zlib 格式），
 * 另加 32 表示「自动识别 zlib / gzip」。
 * 返回 0 成功；输出经 out 交还（调用方负责 free）。
 */
static int dh_inflate(const char *in, int64_t in_len, const char *encoding,
                      dh_buf *out) {
    int windowBits;
    if (!encoding || !*encoding || dh_eq_ci(encoding, "identity")) {
        /* 未压缩：原样搬运 */
        return dh_buf_append(out, in, in_len) ? 1 : 0;
    }
    if (dh_eq_ci(encoding, "gzip") || dh_eq_ci(encoding, "x-gzip")) {
        windowBits = 15 + 16;
    } else if (dh_eq_ci(encoding, "deflate")) {
        windowBits = 15 + 32;   /* 自动识别 zlib 头 / 裸 deflate */
    } else {
        dh_set_error(DIAL_HTTP_ERR_PROTOCOL, "不支持的 Content-Encoding（只支持 gzip / deflate / identity）");
        return 0;
    }

    z_stream strm;
    memset(&strm, 0, sizeof(strm));
    if (inflateInit2(&strm, windowBits) != Z_OK) {
        dh_set_error(DIAL_HTTP_ERR_NOMEM, "初始化解压器失败");
        return 0;
    }
    strm.next_in = (Bytef *)(void *)(uintptr_t)(const void *)in;
    strm.avail_in = (uInt)in_len;

    char chunk[16384];
    int rc = Z_OK;
    while (rc != Z_STREAM_END) {
        strm.next_out = (Bytef *)chunk;
        strm.avail_out = (uInt)sizeof(chunk);
        rc = inflate(&strm, Z_NO_FLUSH);
        if (rc != Z_OK && rc != Z_STREAM_END && rc != Z_BUF_ERROR) {
            inflateEnd(&strm);
            dh_set_error(DIAL_HTTP_ERR_PROTOCOL, "响应体解压失败（gzip / deflate 数据损坏）");
            return 0;
        }
        int64_t produced = (int64_t)sizeof(chunk) - (int64_t)strm.avail_out;
        if (produced > 0 && !dh_buf_append(out, chunk, produced)) {
            inflateEnd(&strm);
            dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
            return 0;
        }
        if (rc == Z_BUF_ERROR && produced == 0) {
            inflateEnd(&strm);
            dh_set_error(DIAL_HTTP_ERR_PROTOCOL, "响应体解压失败（数据不完整）");
            return 0;
        }
    }
    inflateEnd(&strm);
    return 1;
}

/* ------------------------------------------------------------------ */
/* 响应解析                                                            */
/* ------------------------------------------------------------------ */

static int dh_resp_add_header(DialHttpResponse *r, char *name, char *value) {
    if (r->nheaders >= r->cap_headers) {
        int64_t cap = r->cap_headers ? r->cap_headers * 2 : 8;
        void *p = realloc(r->headers, (size_t)cap * sizeof(*r->headers));
        if (!p) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); return 0; }
        r->headers = (dh_header *)p;
        r->cap_headers = cap;
    }
    r->headers[r->nheaders].name = name;
    r->headers[r->nheaders].value = value;
    r->nheaders++;
    return 1;
}

static const char *dh_resp_find_header(DialHttpResponse *r, const char *name) {
    for (int64_t i = 0; i < r->nheaders; i++) {
        if (dh_eq_ci(r->headers[i].name, name)) return r->headers[i].value;
    }
    return NULL;
}

/* 读 chunked 编码的响应体 */
static int dh_read_chunked(DialHttpConn *c, dh_buf *out) {
    for (;;) {
        char *line = NULL;
        int64_t len = 0;
        if (!dh_read_line_raw(c, &line, &len)) return 0;
        /* "1a;ext=1" —— 十六进制长度，可能带扩展参数 */
        char *endp = NULL;
        long long chunk_len = strtoll(line, &endp, 16);
        free(line);
        if (chunk_len < 0) {
            dh_set_error(DIAL_HTTP_ERR_PROTOCOL, "chunked 响应体长度非法");
            return 0;
        }
        if (chunk_len == 0) {
            /* 结束块：可能还有 trailer 头，读到空行为止 */
            for (;;) {
                char *t = NULL;
                int64_t tl = 0;
                if (!dh_read_line_raw(c, &t, &tl)) return 0;
                int is_blank = (tl == 0);
                free(t);
                if (is_blank) break;
            }
            return 1;
        }
        char *data = (char *)malloc((size_t)chunk_len);
        if (!data) { dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足"); return 0; }
        if (!dh_read_exact(c, data, chunk_len)) { free(data); return 0; }
        if (!dh_buf_append(out, data, chunk_len)) {
            free(data);
            dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
            return 0;
        }
        free(data);
        /* 每个块后面跟一个 CRLF */
        char *crlf = NULL;
        int64_t cl = 0;
        if (!dh_read_line_raw(c, &crlf, &cl)) return 0;
        free(crlf);
    }
}

/*
 * 读并解析一个 HTTP 响应（状态行 + 头 + 体）。
 * 调用方负责保证连接上正好是一个响应。
 */
static int dh_read_response(DialHttpConn *c, DialHttpResponse *out) {
    memset(out, 0, sizeof(*out));

    char *status_line = NULL;
    int64_t sl = 0;
    if (!dh_read_line_raw(c, &status_line, &sl)) return 0;

    /* "HTTP/1.1 200 OK" —— 只取中间的状态码 */
    if (strncmp(status_line, "HTTP/", 5) != 0) {
        char msg[256];
        snprintf(msg, sizeof(msg), "响应不是合法的 HTTP 状态行：%.60s", status_line);
        free(status_line);
        dh_set_error(DIAL_HTTP_ERR_PROTOCOL, msg);
        return 0;
    }
    const char *sp = strchr(status_line, ' ');
    if (!sp) {
        free(status_line);
        dh_set_error(DIAL_HTTP_ERR_PROTOCOL, "响应状态行缺少状态码");
        return 0;
    }
    while (*sp == ' ') sp++;
    out->status = (int64_t)strtol(sp, NULL, 10);
    free(status_line);

    /* 头部 */
    int chunked = 0;
    int64_t content_length = -1;
    const char *encoding = NULL;
    char *encoding_copy = NULL;

    for (;;) {
        char *line = NULL;
        int64_t ll = 0;
        if (!dh_read_line_raw(c, &line, &ll)) {
            free(encoding_copy);
            return 0;
        }
        if (ll == 0) {           /* 空行 = 头结束 */
            free(line);
            break;
        }
        char *name = NULL, *value = NULL;
        dh_split_header(line, &name, &value);
        /* name/value 都指向 line 内部，需要各自 strdup 后才能长期持有 */
        char *n = dh_strdup(name);
        char *v = dh_strdup(value);
        free(line);
        if (!n || !v) {
            free(n); free(v); free(encoding_copy);
            dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
            return 0;
        }
        if (dh_eq_ci(n, "Transfer-Encoding")) {
            if (strstr(v, "chunked")) chunked = 1;
        } else if (dh_eq_ci(n, "Content-Length")) {
            content_length = (int64_t)strtoll(v, NULL, 10);
        } else if (dh_eq_ci(n, "Content-Encoding")) {
            free(encoding_copy);
            encoding_copy = dh_strdup(v);
            encoding = encoding_copy;
        }
        if (!dh_resp_add_header(out, n, v)) {
            free(n); free(v); free(encoding_copy);
            return 0;
        }
    }

    /* 响应体 */
    dh_buf raw;
    memset(&raw, 0, sizeof(raw));
    int ok = 1;
    if (chunked) {
        ok = dh_read_chunked(c, &raw);
    } else if (content_length >= 0) {
        if (content_length > 0) {
            char *data = (char *)malloc((size_t)content_length);
            if (!data) {
                ok = 0;
                dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
            } else if (!dh_read_exact(c, data, content_length)) {
                free(data);
                ok = 0;
            } else {
                ok = dh_buf_append(&raw, data, content_length) ? 1 : 0;
                if (!ok) dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
                free(data);
            }
        }
    } else {
        /* 没有长度也没有 chunked：读到连接关闭为止 */
        char tmp[8192];
        for (;;) {
            int64_t r = dh_recv_some(c, tmp, (int64_t)sizeof(tmp));
            if (r < 0) { ok = 0; break; }
            if (r == 0) break;
            if (!dh_buf_append(&raw, tmp, r)) {
                ok = 0;
                dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
                break;
            }
        }
    }

    if (ok) {
        /* 按 Content-Encoding 解压 */
        if (!dh_inflate(raw.data ? raw.data : "", raw.len, encoding, &out->body)) {
            dh_buf_free(&raw);
            free(encoding_copy);
            return 0;
        }
    }
    dh_buf_free(&raw);
    free(encoding_copy);
    return ok;
}

/* ------------------------------------------------------------------ */
/* 高层：一次请求                                                      */
/* ------------------------------------------------------------------ */

/*
 * 拼装并发送请求，然后读完响应。
 * method 为 "GET" / "POST"；extra_headers 可为 NULL；body 可为 NULL。
 */
static DialHttpResponse *dh_request(const char *method, const char *url,
                                     const char *extra_headers,
                                     const char *body, const char *content_type,
                                     int64_t timeout_sec) {
    dh_set_error(DIAL_HTTP_OK, "");

    dh_url u;
    if (!dh_url_parse(url, &u)) return NULL;

    DialHttpConn *c = dh_connect_raw(url);
    if (!c) { dh_url_free(&u); return NULL; }
    if (timeout_sec > 0) dial_http_set_timeout(c, timeout_sec);

    dh_buf req;
    memset(&req, 0, sizeof(req));
    dh_buf_puts(&req, method);
    dh_buf_puts(&req, " ");
    dh_buf_puts(&req, u.path);
    dh_buf_puts(&req, " HTTP/1.1\r\n");
    dh_buf_puts(&req, "Host: ");
    dh_buf_puts(&req, u.hostport);
    dh_buf_puts(&req, "\r\n");
    dh_buf_puts(&req, "User-Agent: dial-http/1.0\r\n");
    /* 用 Connection: close 简化取体：读完即止，不做连接复用（keep-alive 留后续） */
    dh_buf_puts(&req, "Connection: close\r\n");
    dh_buf_puts(&req, "Accept: */*\r\n");
    if (content_type && *content_type) {
        dh_buf_puts(&req, "Content-Type: ");
        dh_buf_puts(&req, content_type);
        dh_buf_puts(&req, "\r\n");
    }
    if (extra_headers && *extra_headers) {
        dh_buf_puts(&req, extra_headers);
        if (req.len == 0 || req.data[req.len - 1] != '\n') dh_buf_puts(&req, "\r\n");
    }
    int64_t body_len = body ? (int64_t)strlen(body) : 0;
    if (strcmp(method, "POST") == 0 || body_len > 0) {
        char lenbuf[32];
        snprintf(lenbuf, sizeof(lenbuf), "Content-Length: %lld\r\n", (long long)body_len);
        dh_buf_puts(&req, lenbuf);
    }
    dh_buf_puts(&req, "\r\n");
    if (body_len > 0) dh_buf_append(&req, body, body_len);

    int64_t wrote = dh_write_raw(c, req.data ? req.data : "", req.len);
    dh_buf_free(&req);
    dh_url_free(&u);
    if (wrote < 0) {
        dial_http_close(c);
        return NULL;
    }

    DialHttpResponse *resp = (DialHttpResponse *)calloc(1, sizeof(DialHttpResponse));
    if (!resp) {
        dial_http_close(c);
        dh_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
        return NULL;
    }
    if (!dh_read_response(c, resp)) {
        dial_http_close(c);
        dial_http_resp_free(resp);
        return NULL;
    }
    dial_http_close(c);
    return resp;
}

DialHttpResponse *dial_http_get(DialString url, DialString headers) {
    return dh_request("GET",
                      url ? dial_string_cstr(url) : NULL,
                      headers ? dial_string_cstr(headers) : NULL,
                      NULL, NULL, 0);
}

DialHttpResponse *dial_http_post(DialString url, DialString headers,
                                  DialString body, DialString content_type) {
    return dh_request("POST",
                      url ? dial_string_cstr(url) : NULL,
                      headers ? dial_string_cstr(headers) : NULL,
                      body ? dial_string_cstr(body) : NULL,
                      content_type ? dial_string_cstr(content_type) : NULL,
                      0);
}

/* ------------------------------------------------------------------ */
/* 响应访问器                                                          */
/* ------------------------------------------------------------------ */

int64_t dial_http_resp_status(DialHttpResponse *r) {
    return r ? r->status : -1;
}

DialString dial_http_resp_body(DialHttpResponse *r) {
    if (!r) return NULL;
    return dial_bytes_new(r->body.data ? r->body.data : "", r->body.len);
}

int64_t dial_http_resp_body_len(DialHttpResponse *r) {
    return r ? r->body.len : -1;
}

DialString dial_http_resp_header(DialHttpResponse *r, DialString name) {
    if (!r || !name) return dial_string_new("");
    const char *v = dh_resp_find_header(r, dial_string_cstr(name));
    return dial_string_new(v ? v : "");
}

int64_t dial_http_resp_header_count(DialHttpResponse *r) {
    return r ? r->nheaders : -1;
}

DialString dial_http_resp_header_name(DialHttpResponse *r, int64_t i) {
    if (!r || i < 0 || i >= r->nheaders) return dial_string_new("");
    return dial_string_new(r->headers[i].name);
}

DialString dial_http_resp_header_value(DialHttpResponse *r, int64_t i) {
    if (!r || i < 0 || i >= r->nheaders) return dial_string_new("");
    return dial_string_new(r->headers[i].value);
}

int64_t dial_http_resp_free(DialHttpResponse *r) {
    if (!r) return 0;
    for (int64_t i = 0; i < r->nheaders; i++) {
        free(r->headers[i].name);
        free(r->headers[i].value);
    }
    free(r->headers);
    dh_buf_free(&r->body);
    free(r);
    return 0;
}
