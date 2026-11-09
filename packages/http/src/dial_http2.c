/*
 * dial_http2.c —— HTTP/2 客户端帧层 / 会话状态机（RFC 7540）。
 * 设计立场与并发模型见 dial_http2.h。要点：
 *   - **单 I/O 线程独占 socket + mbedTLS**：mbedTLS 的 ssl_context 不允许被
 *     多线程并发读写（未开 MBEDTLS_THREADING_C），所有 send/recv 都归这条线程。
 *   - Dial worker 只入队帧 + 在 done_cv 上等自己的 stream；醒来后自行组装
 *     DialHttpResponse（ARC 对象只在 worker 线程构造，reader 线程绝不碰）。
 *   - HPACK 解码上下文是连接级、只被 I/O 线程访问，无需额外锁。
 *   - 平台线程原语分野：POSIX 用 pthread；windows-gnu 无 pthread.h，改用
 *     CRITICAL_SECTION / CONDITION_VARIABLE / CreateThread（与 dial_http.c 的
 *     连接池同款，已用 zig cc 三目标探测确认）。
 */

#include "dial_http2.h"
#include "dial_http_internal.h"
#include "dial_hpack.h"
#include "dial_tls.h"
#include "dial_string.h"
#include "dial_profile.h"

#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
typedef SOCKET h2_sock;
#define H2_INVALID_SOCKET INVALID_SOCKET
#define H2_CLOSE(s) closesocket(s)
#define H2_LASTERR() WSAGetLastError()
#define H2_WOULDBLOCK(e) ((e) == WSAEWOULDBLOCK || (e) == WSAETIMEDOUT)
#else
#include <unistd.h>
#include <errno.h>
#include <netdb.h>
#include <sys/socket.h>
#include <sys/time.h>   /* struct timeval：musl 不会经其他头间接引入，必须显式包含 */
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
typedef int h2_sock;
#define H2_INVALID_SOCKET (-1)
#define H2_CLOSE(s) close(s)
#define H2_LASTERR() errno
#define H2_WOULDBLOCK(e) ((e) == EAGAIN || (e) == EWOULDBLOCK || (e) == EINTR)
#endif

/* ------------------------------------------------------------------ */
/* 平台抽象：锁 / 条件变量 / 线程                                       */
/* ------------------------------------------------------------------ */

#ifdef _WIN32
typedef CRITICAL_SECTION h2_mtx;
typedef CONDITION_VARIABLE h2_cond;
typedef HANDLE h2_thread;
#define H2_MTX_INIT(m)    InitializeCriticalSection(m)
#define H2_MTX_DESTROY(m) DeleteCriticalSection(m)
#define H2_LOCK(m)        EnterCriticalSection(m)
#define H2_UNLOCK(m)      LeaveCriticalSection(m)
#define H2_COND_INIT(c)   InitializeConditionVariable(c)
#define H2_COND_DESTROY(c) ((void)(c))
#define H2_COND_WAIT(c, m) SleepConditionVariableCS(c, m, INFINITE)
#define H2_COND_BCAST(c)  WakeAllConditionVariable(c)
#define H2_THREAD_RET     DWORD WINAPI
#define H2_THREAD_ARG     LPVOID
#define H2_THREAD_JOIN(t) do { WaitForSingleObject((t), INFINITE); CloseHandle(t); } while (0)
#else
typedef pthread_mutex_t h2_mtx;
typedef pthread_cond_t  h2_cond;
typedef pthread_t       h2_thread;
#define H2_MTX_INIT(m)    pthread_mutex_init(m, NULL)
#define H2_MTX_DESTROY(m) pthread_mutex_destroy(m)
#define H2_LOCK(m)        pthread_mutex_lock(m)
#define H2_UNLOCK(m)      pthread_mutex_unlock(m)
#define H2_COND_INIT(c)   pthread_cond_init(c, NULL)
#define H2_COND_DESTROY(c) pthread_cond_destroy(c)
#define H2_COND_WAIT(c, m) pthread_cond_wait(c, m)
#define H2_COND_BCAST(c)  pthread_cond_broadcast(c)
#define H2_THREAD_RET     void *
#define H2_THREAD_ARG     void *
#define H2_THREAD_JOIN(t) pthread_join(t, NULL)
#endif

/* 短 SO_RCVTIMEO：I/O 线程靠它周期性回到循环顶部重查「待发队列」。
 * 单 I/O 线程 + worker 入队模型下，读不能无限阻塞，否则新入队的请求发不出去。
 * 20ms 轮询是刻意选择的简单稳健解（避免 select/pipe 唤醒的跨平台复杂度）。 */
#define H2_RECV_POLL_MS 20

/* ------------------------------------------------------------------ */
/* 帧常量（RFC 7540 §4、§6）                                           */
/* ------------------------------------------------------------------ */
enum {
    H2_FT_DATA = 0x0, H2_FT_HEADERS = 0x1, H2_FT_RST_STREAM = 0x3,
    H2_FT_SETTINGS = 0x4, H2_FT_PING = 0x6, H2_FT_GOAWAY = 0x7,
    H2_FT_WINDOW_UPDATE = 0x8, H2_FT_CONTINUATION = 0x9
};
#define H2_FLAG_END_STREAM  0x1
#define H2_FLAG_ACK         0x1
#define H2_FLAG_END_HEADERS 0x4
#define H2_FLAG_PADDED      0x8
#define H2_FLAG_PRIORITY    0x20

#define H2_DEFAULT_MAX_FRAME 16384
#define H2_DEFAULT_WINDOW    65535
#define H2_MAX_STREAMS       64

static const unsigned char k_preface[] =
    "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";  /* 24 字节，含结尾 NUL 由 sizeof-1 取 */

/* ------------------------------------------------------------------ */
/* 会话 / stream / 待发帧结构                                           */
/* ------------------------------------------------------------------ */

typedef struct {
    int      id;         /* stream id（客户端奇数） */
    int      state;      /* 0 空闲 / 1 等待 / 2 完成 / 3 错误 */
    int64_t  status;     /* :status */
    dh_bytes hdr_block;  /* HEADERS+CONTINUATION 累积（未 END_HEADERS 前） */
    dh_bytes body;
    dh_header_list headers;
    int      done;       /* END_STREAM 收到 */
    int      error;      /* RST_STREAM / 协议错 */
} h2_stream;

typedef struct h2_outframe {
    dh_bytes data;
    struct h2_outframe *next;
} h2_outframe;

struct DialH2Session {
    h2_sock  fd;
    DialTls *tls;             /* 非 NULL = https(h2)；NULL = 明文 h2c */
    int      is_https;
    char     hostport[300];   /* :authority */

    h2_mtx   mtx;
    h2_cond  done_cv;         /* stream 完成 / 会话终止时广播 */
    h2_thread io_thread;
    int      io_started;
    int      io_done;

    h2_stream streams[H2_MAX_STREAMS];
    uint32_t  next_stream_id; /* 1,3,5,... */
    int       shutdown;       /* session_free 请求退出 */
    int       fatal;          /* 连接级不可恢复错误 */

    h2_outframe *out_head, *out_tail;  /* worker 入队、I/O 线程取出的待发帧 */

    dh_bytes inbuf;           /* 只被 I/O 线程访问的读累积缓冲 */
    dh_hpack_ctx hpack;       /* 只被 I/O 线程访问的解码上下文 */
    uint32_t cur_hdr_sid;     /* 正在接收分片头块的 stream（0 = 无） */

    uint32_t max_frame_size;
    int64_t  send_window;     /* 连接级发送窗口 */
    int      settings_recv;   /* 收到过服务端 SETTINGS */
};

/* ---- 诊断（线程局部 / 进程级）---- */
static _Thread_local char g_h2_protocol[16] = {0};
static _Thread_local char g_h2_alpn[32] = {0};
static int64_t g_h2_conn_count = 0;
static int64_t g_h2_stream_count = 0;

/* Windows 一次性 WSAStartup；POSIX 无操作。 */
#ifdef _WIN32
static volatile LONG g_h2_wsa_inited = 0;
static void h2_net_init(void) {
    if (InterlockedCompareExchange(&g_h2_wsa_inited, 1, 0) == 0) {
        WSADATA w; WSAStartup(MAKEWORD(2, 2), &w);
    }
}
#else
static volatile int g_h2_sigpipe_done = 0;
static void h2_net_init(void) {
    if (!g_h2_sigpipe_done) { signal(SIGPIPE, SIG_IGN); g_h2_sigpipe_done = 1; }
}
#endif

static void h2_set_error(int64_t code, const char *msg) {
    dial_http_internal_set_error(code, msg);
}

/* ------------------------------------------------------------------ */
/* URL 解析（自包含：只要 host / port / scheme，path 逐请求给）          */
/* ------------------------------------------------------------------ */
typedef struct {
    char host[256];
    char hostport[300];
    int  port;
    int  is_https;
} h2_url;

static int h2_url_parse(const char *url, h2_url *u) {
    memset(u, 0, sizeof(*u));
    if (!url) { h2_set_error(DIAL_HTTP_ERR_URL, "URL 为空"); return 0; }
    const char *p = url;
    if (strncmp(p, "https://", 8) == 0) { u->is_https = 1; p += 8; }
    else if (strncmp(p, "http://", 7) == 0) { u->is_https = 0; p += 7; }
    else { h2_set_error(DIAL_HTTP_ERR_URL, "HTTP/2 URL 必须以 http:// 或 https:// 开头"); return 0; }

    const char *slash = strchr(p, '/');
    size_t auth_len = slash ? (size_t)(slash - p) : strlen(p);
    if (auth_len == 0 || auth_len >= sizeof(u->hostport)) {
        h2_set_error(DIAL_HTTP_ERR_URL, "URL 主机名缺失或过长"); return 0;
    }
    memcpy(u->hostport, p, auth_len);
    u->hostport[auth_len] = '\0';

    /* 拆 host[:port]（测试用 127.0.0.1:PORT；不含 IPv6 方括号） */
    char *colon = strrchr(u->hostport, ':');
    u->port = u->is_https ? 443 : 80;
    if (colon) {
        *colon = '\0';
        u->port = atoi(colon + 1);
        if (u->port <= 0 || u->port > 65535) {
            h2_set_error(DIAL_HTTP_ERR_URL, "URL 端口非法"); return 0;
        }
    }
    if (strlen(u->hostport) >= sizeof(u->host)) {
        h2_set_error(DIAL_HTTP_ERR_URL, "URL 主机名过长"); return 0;
    }
    strcpy(u->host, u->hostport);
    /* hostport 恢复为含端口的形式用于 :authority */
    if (colon) {
        *colon = ':';
    }
    return 1;
}

/* ------------------------------------------------------------------ */
/* 底层连接建立（明文 or TLS h2）                                       */
/* ------------------------------------------------------------------ */
static h2_sock h2_tcp_connect(const h2_url *u) {
    char portbuf[16];
    snprintf(portbuf, sizeof(portbuf), "%d", u->port);
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(u->host, portbuf, &hints, &res) != 0 || !res) {
        if (res) freeaddrinfo(res);
        h2_set_error(DIAL_HTTP_ERR_DNS, "域名解析失败");
        return H2_INVALID_SOCKET;
    }
    h2_sock fd = H2_INVALID_SOCKET;
    for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
        fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
        if (fd == H2_INVALID_SOCKET) continue;
        if (connect(fd, ai->ai_addr, (int)ai->ai_addrlen) == 0) break;
        H2_CLOSE(fd);
        fd = H2_INVALID_SOCKET;
    }
    freeaddrinfo(res);
    if (fd == H2_INVALID_SOCKET) {
        h2_set_error(DIAL_HTTP_ERR_CONNECT, "TCP 连接失败（主机不可达或端口未开放）");
        return H2_INVALID_SOCKET;
    }
    return fd;
}

static void h2_set_recv_poll(h2_sock fd) {
#ifdef _WIN32
    DWORD ms = H2_RECV_POLL_MS;
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, (const char *)&ms, sizeof(ms));
#else
    struct timeval tv;
    tv.tv_sec = 0;
    tv.tv_usec = H2_RECV_POLL_MS * 1000;
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, (const char *)&tv, sizeof(tv));
#endif
}

/* ------------------------------------------------------------------ */
/* 帧收发（只被 I/O 线程调用）                                          */
/* ------------------------------------------------------------------ */

static int64_t h2_write_all(DialH2Session *s, const unsigned char *p, int64_t n) {
    int64_t off = 0;
    while (off < n) {
        int64_t r;
        if (s->tls) {
            r = dial_tls_send(s->tls, p + off, n - off);
        } else {
#ifdef _WIN32
            r = send(s->fd, (const char *)(p + off), (int)(n - off), 0);
#else
            r = send(s->fd, p + off, (size_t)(n - off), 0);
#endif
        }
        if (r <= 0) return 0;
        off += r;
    }
    return 1;
}

/* 读一批：>0 字节数 / 0 EOF / -1 真错误 / -2 超时无数据（可重试）。
 * TLS 侧 dial_tls_recv 已把 WANT_READ 映射为 -2；明文侧把 EAGAIN 映射为 -2。 */
static int64_t h2_read_some(DialH2Session *s, void *buf, int64_t n) {
    if (s->tls) return dial_tls_recv(s->tls, buf, n);
#ifdef _WIN32
    int r = recv(s->fd, (char *)buf, (int)n, 0);
#else
    ssize_t r = recv(s->fd, buf, (size_t)n, 0);
#endif
    if (r == 0) return 0;
    if (r < 0) return H2_WOULDBLOCK(H2_LASTERR()) ? -2 : -1;
    return (int64_t)r;
}

static void h2_frame_header(unsigned char *h, uint32_t len, uint8_t type,
                            uint8_t flags, uint32_t sid) {
    h[0] = (unsigned char)(len >> 16);
    h[1] = (unsigned char)(len >> 8);
    h[2] = (unsigned char)len;
    h[3] = type;
    h[4] = flags;
    h[5] = (unsigned char)((sid >> 24) & 0x7f);
    h[6] = (unsigned char)((sid >> 16) & 0xff);
    h[7] = (unsigned char)((sid >> 8) & 0xff);
    h[8] = (unsigned char)(sid & 0xff);
}

/* 把一帧追加进「待发队列」（worker 与 I/O 线程都可能调用，mtx 必须已持）。 */
static int h2_enqueue(DialH2Session *s, uint8_t type, uint8_t flags,
                      uint32_t sid, const unsigned char *payload, uint32_t len) {
    h2_outframe *f = (h2_outframe *)calloc(1, sizeof(h2_outframe));
    if (!f) return 0;
    unsigned char hdr[9];
    h2_frame_header(hdr, len, type, flags, sid);
    if (!dh_bytes_append(&f->data, hdr, 9) ||
        (len && !dh_bytes_append(&f->data, payload, len))) {
        dh_bytes_free(&f->data); free(f); return 0;
    }
    if (s->out_tail) s->out_tail->next = f;
    else s->out_head = f;
    s->out_tail = f;
    return 1;
}

/* ------------------------------------------------------------------ */
/* 帧分发（I/O 线程，持 mtx 调用）                                      */
/* ------------------------------------------------------------------ */

static h2_stream *h2_find_stream(DialH2Session *s, uint32_t sid) {
    for (int i = 0; i < H2_MAX_STREAMS; i++) {
        if (s->streams[i].id == (int)sid && s->streams[i].state != 0) return &s->streams[i];
    }
    return NULL;
}

static void h2_reset_stream(h2_stream *st) {
    dh_bytes_free(&st->hdr_block);
    dh_bytes_free(&st->body);
    dh_hlist_free(&st->headers);
    memset(st, 0, sizeof(*st));
}

static void h2_fail_all(DialH2Session *s) {
    for (int i = 0; i < H2_MAX_STREAMS; i++) {
        if (s->streams[i].state == 1) s->streams[i].state = 3;  /* 错误 */
    }
}

static int h2_inflight(DialH2Session *s) {
    int n = 0;
    for (int i = 0; i < H2_MAX_STREAMS; i++) if (s->streams[i].state == 1) n++;
    return n;
}

/* 去掉 PADDED / PRIORITY 前导与尾部补齐，返回真正的载荷区间。 */
static void h2_strip_padding(const unsigned char *pl, int64_t plen,
                             uint8_t flags, int is_headers,
                             const unsigned char **out, int64_t *outlen) {
    int64_t start = 0, end = plen;
    if (flags & H2_FLAG_PADDED) {
        if (plen >= 1) { int pad = pl[0]; start += 1; end -= pad; if (end < start) end = start; }
    }
    if (is_headers && (flags & H2_FLAG_PRIORITY)) {
        start += 5;  /* exclusive stream dependency(4) + weight(1) */
        if (start > end) start = end;
    }
    *out = pl + start;
    *outlen = end - start;
}

/* 完成一个头块：HPACK 解码 -> 头列表 + :status。 */
static void h2_finish_headers(DialH2Session *s, h2_stream *st) {
    dh_header_list decoded;
    dh_hlist_init(&decoded);
    char err[256] = {0};
    int r = dh_hpack_decode(&s->hpack, st->hdr_block.p, st->hdr_block.len,
                            &decoded, err, sizeof(err));
    if (r < 0) {
        st->error = 1;
        h2_set_error(DIAL_HTTP_ERR_PROTOCOL, err[0] ? err : "HPACK 头块解码失败");
        dh_hlist_free(&decoded);
        return;
    }
    /* 转交解码结果 */
    dh_hlist_free(&st->headers);
    st->headers = decoded;
    for (int i = 0; i < st->headers.n; i++) {
        if (strcmp(st->headers.v[i].name, ":status") == 0) {
            st->status = (int64_t)strtol(st->headers.v[i].value, NULL, 10);
        }
    }
    dh_bytes_free(&st->hdr_block);
}

static void h2_dispatch(DialH2Session *s, uint8_t type, uint8_t flags,
                        uint32_t sid, const unsigned char *pl, int64_t plen) {
    switch (type) {
        case H2_FT_DATA: {
            h2_stream *st = h2_find_stream(s, sid);
            if (!st) return;
            const unsigned char *body; int64_t blen;
            h2_strip_padding(pl, plen, flags, 0, &body, &blen);
            if (blen > 0) dh_bytes_append(&st->body, body, blen);
            /* 基础流控：收到 DATA 就回 WINDOW_UPDATE（连接级 + 流级），
             * 让服务端窗口不被耗尽。大流背压（body > 初始窗口）不在本实现范围。 */
            if (blen > 0) {
                unsigned char wu[4];
                uint32_t inc = (uint32_t)blen;
                wu[0] = (unsigned char)(inc >> 24); wu[1] = (unsigned char)(inc >> 16);
                wu[2] = (unsigned char)(inc >> 8);  wu[3] = (unsigned char)inc;
                h2_enqueue(s, H2_FT_WINDOW_UPDATE, 0, 0, wu, 4);
                h2_enqueue(s, H2_FT_WINDOW_UPDATE, 0, sid, wu, 4);
            }
            if (flags & H2_FLAG_END_STREAM) {
                st->done = 1;
                if (st->state == 1) st->state = 2;
                H2_COND_BCAST(&s->done_cv);
            }
            break;
        }
        case H2_FT_HEADERS: {
            h2_stream *st = h2_find_stream(s, sid);
            if (!st) return;
            const unsigned char *hb; int64_t hblen;
            h2_strip_padding(pl, plen, flags, 1, &hb, &hblen);
            if (hblen > 0) dh_bytes_append(&st->hdr_block, hb, hblen);
            if (flags & H2_FLAG_END_HEADERS) {
                h2_finish_headers(s, st);
                s->cur_hdr_sid = 0;
            } else {
                s->cur_hdr_sid = sid;
            }
            if (flags & H2_FLAG_END_STREAM) {
                st->done = 1;
                if (st->state == 1) st->state = 2;
                H2_COND_BCAST(&s->done_cv);
            }
            break;
        }
        case H2_FT_CONTINUATION: {
            uint32_t target = s->cur_hdr_sid ? s->cur_hdr_sid : sid;
            h2_stream *st = h2_find_stream(s, target);
            if (!st) return;
            if (plen > 0) dh_bytes_append(&st->hdr_block, pl, plen);
            if (flags & H2_FLAG_END_HEADERS) {
                h2_finish_headers(s, st);
                s->cur_hdr_sid = 0;
            }
            if (flags & H2_FLAG_END_STREAM) {
                st->done = 1;
                if (st->state == 1) st->state = 2;
                H2_COND_BCAST(&s->done_cv);
            }
            break;
        }
        case H2_FT_SETTINGS: {
            if (flags & H2_FLAG_ACK) break;
            s->settings_recv = 1;
            /* 逐条解析 id(2) + value(4) */
            for (int64_t off = 0; off + 6 <= plen; off += 6) {
                uint16_t id = (uint16_t)((pl[off] << 8) | pl[off + 1]);
                uint32_t val = ((uint32_t)pl[off + 2] << 24) | ((uint32_t)pl[off + 3] << 16) |
                               ((uint32_t)pl[off + 4] << 8) | (uint32_t)pl[off + 5];
                /* 服务端 SETTINGS_HEADER_TABLE_SIZE(id=1) 约束的是**本端编码**侧
                 * 动态表，而本端请求编码一律 without indexing、不使用动态表，
                 * 故不覆盖 s->hpack.max_table_size（那是解码上限，只由本端自己
                 * 在 connect 时按声明值设定）。INITIAL_WINDOW_SIZE/MAX_FRAME_SIZE
                 * 才影响本端发送行为。 */
                if (id == 0x4) s->send_window = (int64_t)val;       /* INITIAL_WINDOW_SIZE */
                else if (id == 0x5 && val >= 16384 && val <= 16777215) s->max_frame_size = val;
            }
            h2_enqueue(s, H2_FT_SETTINGS, H2_FLAG_ACK, 0, NULL, 0);
            break;
        }
        case H2_FT_PING: {
            if (!(flags & H2_FLAG_ACK)) {
                const unsigned char *data = plen >= 8 ? pl : NULL;
                h2_enqueue(s, H2_FT_PING, H2_FLAG_ACK, 0, data, data ? 8 : 0);
            }
            break;
        }
        case H2_FT_WINDOW_UPDATE: {
            if (plen >= 4) {
                uint32_t inc = ((uint32_t)pl[0] << 24) | ((uint32_t)pl[1] << 16) |
                               ((uint32_t)pl[2] << 8) | (uint32_t)pl[3];
                s->send_window += (int64_t)(inc & 0x7fffffff);
            }
            break;
        }
        case H2_FT_RST_STREAM: {
            h2_stream *st = h2_find_stream(s, sid);
            if (st) {
                st->error = 1; st->done = 1;
                if (st->state == 1) st->state = 3;
                H2_COND_BCAST(&s->done_cv);
            }
            break;
        }
        case H2_FT_GOAWAY: {
            s->fatal = 1;
            h2_set_error(DIAL_HTTP_ERR_PROTOCOL, "服务端发送 GOAWAY，连接终止");
            h2_fail_all(s);
            H2_COND_BCAST(&s->done_cv);
            break;
        }
        default:
            break;  /* 未知帧类型按 RFC 忽略 */
    }
}

/* 从 inbuf 解析出所有完整帧并分发。持 mtx 调用。 */
static void h2_parse_frames(DialH2Session *s) {
    dh_bytes *b = &s->inbuf;
    int64_t pos = 0;
    while (b->len - pos >= 9) {
        const unsigned char *h = b->p + pos;
        uint32_t len = ((uint32_t)h[0] << 16) | ((uint32_t)h[1] << 8) | h[2];
        uint8_t type = h[3], flags = h[4];
        uint32_t sid = (((uint32_t)h[5] & 0x7f) << 24) | ((uint32_t)h[6] << 16) |
                       ((uint32_t)h[7] << 8) | h[8];
        if (len > 0x00ffffff || b->len - pos - 9 < (int64_t)len) break;  /* 帧未收齐 */
        const unsigned char *pl = b->p + pos + 9;
        h2_dispatch(s, type, flags, sid, pl, (int64_t)len);
        pos += 9 + (int64_t)len;
        if (s->fatal) break;
    }
    if (pos > 0) {
        memmove(b->p, b->p + pos, (size_t)(b->len - pos));
        b->len -= pos;
    }
}

/* ------------------------------------------------------------------ */
/* I/O 线程主循环                                                       */
/* ------------------------------------------------------------------ */
static H2_THREAD_RET h2_io_main(H2_THREAD_ARG arg) {
    DialH2Session *s = (DialH2Session *)arg;
    unsigned char rbuf[16384];
    for (;;) {
        /* 1) 取出待发帧 + 快照状态 */
        H2_LOCK(&s->mtx);
        h2_outframe *list = s->out_head;
        s->out_head = s->out_tail = NULL;
        int inflight = h2_inflight(s);
        int fatal = s->fatal;
        int shutdown = s->shutdown;
        H2_UNLOCK(&s->mtx);

        if (fatal || (shutdown && inflight == 0)) break;

        /* 2) 写出所有待发帧（唯一写者） */
        int write_failed = 0;
        for (h2_outframe *f = list; f; ) {
            if (!h2_write_all(s, f->data.p, f->data.len)) { write_failed = 1; }
            h2_outframe *nx = f->next;
            dh_bytes_free(&f->data);
            free(f);
            f = nx;
        }
        if (write_failed) {
            H2_LOCK(&s->mtx);
            s->fatal = 1;
            h2_set_error(DIAL_HTTP_ERR_IO, "HTTP/2 发送帧失败（连接中断）");
            h2_fail_all(s);
            H2_COND_BCAST(&s->done_cv);
            H2_UNLOCK(&s->mtx);
            break;
        }

        /* 3) 读一批（短超时，超时则回循环顶部重查队列） */
        int64_t r = h2_read_some(s, rbuf, sizeof(rbuf));
        if (r > 0) {
            H2_LOCK(&s->mtx);
            dh_bytes_append(&s->inbuf, rbuf, r);
            h2_parse_frames(s);
            H2_UNLOCK(&s->mtx);
        } else if (r == 0 || r == -1) {
            H2_LOCK(&s->mtx);
            if (!s->fatal) {
                s->fatal = 1;
                h2_set_error(DIAL_HTTP_ERR_IO, "HTTP/2 连接被对端关闭或读取失败");
                h2_fail_all(s);
                H2_COND_BCAST(&s->done_cv);
            }
            H2_UNLOCK(&s->mtx);
            break;
        }
        /* r == -2：超时无数据，继续 */
    }
    H2_LOCK(&s->mtx);
    s->io_done = 1;
    s->fatal = 1;
    h2_fail_all(s);
    H2_COND_BCAST(&s->done_cv);
    H2_UNLOCK(&s->mtx);
#ifdef _WIN32
    return 0;
#else
    return NULL;
#endif
}

/* ------------------------------------------------------------------ */
/* 对外 API                                                             */
/* ------------------------------------------------------------------ */

DialH2Session *dial_http2_connect(DialString url) {
    h2_net_init();
    h2_set_error(DIAL_HTTP_OK, "");

    h2_url u;
    if (!h2_url_parse(url ? dial_string_cstr(url) : NULL, &u)) return NULL;

    h2_sock fd = h2_tcp_connect(&u);
    if (fd == H2_INVALID_SOCKET) return NULL;

    DialTls *tls = NULL;
    const char *alpn_neg = NULL;
    if (u.is_https) {
        /* 短超时必须在握手前设：mbedTLS 的 BIO 靠它把超时表达成 WANT_READ。 */
        h2_set_recv_poll(fd);
        /* 双 offer 与 Chrome 基准一致（首项 h2）；下面仍 fail-closed 要求
         * 协商结果必须是 h2，绝不因为多 offer 了 http/1.1 就静默降级。 */
        const char *alpn[] = { "h2", "http/1.1", NULL };
        uint32_t verify = (uint32_t)-1;
        char errbuf[256];
        tls = dial_tls_connect((int)fd, u.host, alpn,
                               dial_http_internal_ca_path(),
                               &verify, errbuf, sizeof(errbuf));
        if (!tls) {
            H2_CLOSE(fd);
            h2_set_error(DIAL_HTTP_ERR_UNSUPPORTED, errbuf[0] ? errbuf : "TLS 握手失败");
            return NULL;
        }
        alpn_neg = dial_tls_alpn(tls);
        if (!alpn_neg || strcmp(alpn_neg, "h2") != 0) {
            /* ALPN 没协商出 h2：绝不假装能说 HTTP/2（fail-closed）。 */
            dial_tls_free(tls);
            H2_CLOSE(fd);
            h2_set_error(DIAL_HTTP_ERR_UNSUPPORTED,
                         "服务端未通过 ALPN 协商出 h2（拒绝降级为 http/1.1）");
            return NULL;
        }
    } else {
        h2_set_recv_poll(fd);
    }

    DialH2Session *s = (DialH2Session *)calloc(1, sizeof(DialH2Session));
    if (!s) {
        if (tls) dial_tls_free(tls);
        H2_CLOSE(fd);
        h2_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
        return NULL;
    }
    s->fd = fd;
    s->tls = tls;
    s->is_https = u.is_https;
    strcpy(s->hostport, u.hostport);
    s->next_stream_id = 1;
    s->max_frame_size = H2_DEFAULT_MAX_FRAME;
    s->send_window = H2_DEFAULT_WINDOW;

    /* 客户端 HPACK 解码表上限 = **本端自己在 SETTINGS_HEADER_TABLE_SIZE 声明的值**。
     * RFC 7541 §4.2：该值由接收方声明，编码方据此发 dynamic table size update。
     * chrome 画像声明 65536，服务端就会在响应 HPACK 里发 65536 的 update；
     * 若这里仍写死 4096，解码会误判「动态表大小超过上限」导致整条 stream 失败
     * （期 2 实测踩到：default 画像 4096 正常、chrome 画像 65536 全挂）。
     * 取 profile 里 id==1 的 settings 值，default 无 settings 时退回 4096。 */
    const dh_profile *prof = dh_profile_current();
    int64_t hpack_max = 4096;
    for (int i = 0; i < prof->nsettings; i++) {
        if (prof->settings_id[i] == 0x1) {
            hpack_max = (int64_t) prof->settings_val[i];
            break;
        }
    }
    dh_hpack_ctx_init(&s->hpack, hpack_max);
    H2_MTX_INIT(&s->mtx);
    H2_COND_INIT(&s->done_cv);

    /* client connection preface + SETTINGS + 连接级 WINDOW_UPDATE。
     * 全部在握手阶段同步发出（此时还没有 I/O 线程，独占 socket 无竞争），
     * 保证客户端连接前导的帧序严格是 preface → SETTINGS → WINDOW_UPDATE →
     * HEADERS，与 Chrome 基准一致。
     * default 画像 nsettings=0 / increment=0，退回原有的「preface + 空 SETTINGS」，
     * 既有回归零影响。 */
    unsigned char settings[9 + 6 * 6];   /* 帧头 9 + 最多 6 组 × 6 字节 */
    int settings_len = 0;
    for (int i = 0; i < prof->nsettings && i < 6; i++) {
        uint16_t id  = prof->settings_id[i];
        uint32_t val = prof->settings_val[i];
        unsigned char *q = settings + 9 + settings_len;
        q[0] = (unsigned char)(id >> 8);
        q[1] = (unsigned char)(id & 0xff);
        q[2] = (unsigned char)(val >> 24);
        q[3] = (unsigned char)(val >> 16);
        q[4] = (unsigned char)(val >> 8);
        q[5] = (unsigned char)val;
        settings_len += 6;
    }
    h2_frame_header(settings, (uint32_t)settings_len, H2_FT_SETTINGS, 0, 0);

    unsigned char wu[13];   /* 帧头 9 + 4 字节 increment（连接级 stream 0） */
    size_t wu_len = 0;
    if (prof->window_update_increment) {
        uint32_t inc = prof->window_update_increment;
        wu[9]  = (unsigned char)(inc >> 24);
        wu[10] = (unsigned char)(inc >> 16);
        wu[11] = (unsigned char)(inc >> 8);
        wu[12] = (unsigned char)inc;
        h2_frame_header(wu, 4, H2_FT_WINDOW_UPDATE, 0, 0);
        wu_len = 13;
    }
    if (!h2_write_all(s, k_preface, sizeof(k_preface) - 1) ||
        !h2_write_all(s, settings, (size_t)(9 + settings_len)) ||
        (wu_len && !h2_write_all(s, wu, wu_len))) {
        h2_set_error(DIAL_HTTP_ERR_IO, "发送 HTTP/2 preface/SETTINGS 失败");
        if (tls) dial_tls_free(tls);
        H2_CLOSE(fd);
        dh_hpack_ctx_free(&s->hpack);
        H2_COND_DESTROY(&s->done_cv);
        H2_MTX_DESTROY(&s->mtx);
        free(s);
        return NULL;
    }

    /* 诊断信息（线程局部） */
    strncpy(g_h2_protocol, u.is_https ? "h2" : "h2c", sizeof(g_h2_protocol) - 1);
    g_h2_protocol[sizeof(g_h2_protocol) - 1] = '\0';
    strncpy(g_h2_alpn, alpn_neg ? alpn_neg : "none", sizeof(g_h2_alpn) - 1);
    g_h2_alpn[sizeof(g_h2_alpn) - 1] = '\0';
    g_h2_conn_count++;

    /* 启动 I/O 线程 */
#ifdef _WIN32
    s->io_thread = CreateThread(NULL, 0, (LPTHREAD_START_ROUTINE)h2_io_main, s, 0, NULL);
    s->io_started = (s->io_thread != NULL);
#else
    s->io_started = (pthread_create(&s->io_thread, NULL, h2_io_main, s) == 0);
#endif
    if (!s->io_started) {
        h2_set_error(DIAL_HTTP_ERR_IO, "无法启动 HTTP/2 I/O 线程");
        if (tls) dial_tls_free(tls);
        H2_CLOSE(fd);
        dh_hpack_ctx_free(&s->hpack);
        H2_COND_DESTROY(&s->done_cv);
        H2_MTX_DESTROY(&s->mtx);
        free(s);
        return NULL;
    }
    return s;
}

/* 把一个请求编码成 HEADERS(+CONTINUATION) + DATA 帧并入队。持 mtx 调用。 */
static int h2_submit(DialH2Session *s, h2_stream *st, const char *method,
                     const char *path, const char *extra_headers, const char *body) {
    uint32_t mfs = s->max_frame_size;
    if (mfs < 16384) mfs = 16384;

    dh_header_list req;
    dh_hlist_init(&req);
    dh_hlist_add(&req, ":method", 7, method, (int)strlen(method));
    const char *scheme = s->is_https ? "https" : "http";
    dh_hlist_add(&req, ":scheme", 7, scheme, (int)strlen(scheme));
    dh_hlist_add(&req, ":authority", 10, s->hostport, (int)strlen(s->hostport));
    const char *p = (path && *path) ? path : "/";
    dh_hlist_add(&req, ":path", 5, p, (int)strlen(p));

    /* 普通头：与 HTTP/1.1 共用同一张 profile 表（小写头名，HPACK 要求）。
     * h2_name=NULL 的项 H2 不发：Host 由 :authority 取代；Connection 属
     * RFC 7540 §8.1.2.2 明令禁止的 hop-by-hop 头，发了对端会判协议错。 */
    const dh_profile *prof = dh_profile_current();
    for (int i = 0; i < prof->nheaders; i++) {
        const dh_profile_header *h = &prof->headers[i];
        if (!h->h2_name) continue;
        const char *v = h->value ? h->value
                                : dh_profile_dynamic(prof, h->dyn_kind, s->hostport);
        if (!v || !*v) continue;
        dh_hlist_add(&req, h->h2_name, (int)strlen(h->h2_name), v, (int)strlen(v));
    }

    /* 额外头：每行 "Name: value" */
    if (extra_headers && *extra_headers) {
        const char *line = extra_headers;
        while (*line) {
            const char *eol = strchr(line, '\n');
            size_t llen = eol ? (size_t)(eol - line) : strlen(line);
            if (llen > 0) {
                /* §20 Fail-Fast：额外头按实际行长动态分配，绝不再静默截到
                 * 511 字节（很长的 Authorization / Cookie 会被悄悄砍掉，请求
                 * 照样发出却鉴权失败，调用方无从察觉）。分配失败即整条请求
                 * 失败，不发半截头。dh_hlist_add 内部会拷贝 name/value，
                 * 因此本次迭代结束即可释放 buf。 */
                char *buf = (char *)malloc(llen + 1);
                if (!buf) {
                    dh_hlist_free(&req);
                    h2_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
                    return 0;
                }
                memcpy(buf, line, llen);
                buf[llen] = '\0';
                if (buf[llen - 1] == '\r') buf[llen - 1] = '\0';
                char *colon = strchr(buf, ':');
                if (colon) {
                    *colon = '\0';
                    /* RFC 7540 §8.1.2：HTTP/2 头名必须小写，大写会被对端以
                     * ProtocolError 拒绝。用户按 HTTP/1.1 习惯写的
                     * "Authorization: <超长 token>" 在此统一规范化；H1 侧
                     * 大小写不敏感、保持原样，对外契约不变。 */
                    for (char *q = buf; q < colon; q++) {
                        if (*q >= 'A' && *q <= 'Z') *q = (char)(*q - 'A' + 'a');
                    }
                    char *v = colon + 1;
                    while (*v == ' ' || *v == '\t') v++;
                    if (!dh_hlist_add(&req, buf, (int)strlen(buf), v, (int)strlen(v))) {
                        free(buf);
                        dh_hlist_free(&req);
                        h2_set_error(DIAL_HTTP_ERR_NOMEM, "内存不足");
                        return 0;
                    }
                }
                free(buf);
            }
            if (!eol) break;
            line = eol + 1;
        }
    }

    dh_bytes block = {0};
    int ok = (dh_hpack_encode(&block, &req) == 0);
    dh_hlist_free(&req);
    if (!ok) { dh_bytes_free(&block); return 0; }

    int64_t body_len = body ? (int64_t)strlen(body) : 0;
    int has_body = body_len > 0;

    /* HEADERS 帧：若头块大于 max_frame_size，拆成 HEADERS + CONTINUATION。 */
    int64_t off = 0;
    int first = 1;
    do {
        int64_t chunk = (int64_t)block.len - off;
        if (chunk > (int64_t)mfs) chunk = (int64_t)mfs;
        int last_chunk = (off + chunk >= block.len);
        uint8_t flags = 0;
        if (last_chunk) flags |= H2_FLAG_END_HEADERS;
        if (first && last_chunk && !has_body) flags |= H2_FLAG_END_STREAM;
        uint8_t type = first ? H2_FT_HEADERS : H2_FT_CONTINUATION;
        if (!h2_enqueue(s, type, flags, (uint32_t)st->id,
                        block.p + off, (uint32_t)chunk)) { ok = 0; break; }
        off += chunk;
        first = 0;
    } while (off < block.len);
    dh_bytes_free(&block);
    if (!ok) return 0;

    /* DATA 帧：按 max_frame_size 分片，最后一帧 END_STREAM。 */
    if (has_body) {
        int64_t doff = 0;
        while (doff < body_len) {
            int64_t chunk = body_len - doff;
            if (chunk > (int64_t)mfs) chunk = (int64_t)mfs;
            int last = (doff + chunk >= body_len);
            uint8_t flags = last ? H2_FLAG_END_STREAM : 0;
            if (!h2_enqueue(s, H2_FT_DATA, flags, (uint32_t)st->id,
                            (const unsigned char *)body + doff, (uint32_t)chunk)) return 0;
            doff += chunk;
        }
    }
    return 1;
}

DialHttpResponse *dial_http2_request(DialH2Session *s, DialString method,
                                     DialString path, DialString headers,
                                     DialString body) {
    if (!s) { h2_set_error(DIAL_HTTP_ERR_STATE, "HTTP/2 会话句柄为空"); return NULL; }
    const char *m = method ? dial_string_cstr(method) : "GET";
    const char *p = path ? dial_string_cstr(path) : "/";
    const char *h = headers ? dial_string_cstr(headers) : NULL;
    const char *b = body ? dial_string_cstr(body) : NULL;

    H2_LOCK(&s->mtx);
    if (s->fatal || s->shutdown) {
        H2_UNLOCK(&s->mtx);
        h2_set_error(DIAL_HTTP_ERR_STATE, "HTTP/2 会话已关闭或连接出错");
        return NULL;
    }
    int slot = -1;
    for (int i = 0; i < H2_MAX_STREAMS; i++) {
        if (s->streams[i].state == 0) { slot = i; break; }
    }
    if (slot < 0) {
        H2_UNLOCK(&s->mtx);
        h2_set_error(DIAL_HTTP_ERR_STATE, "HTTP/2 并发 stream 数超过上限");
        return NULL;
    }
    h2_stream *st = &s->streams[slot];
    h2_reset_stream(st);
    st->id = (int)s->next_stream_id;
    s->next_stream_id += 2;
    st->state = 1;
    st->status = -1;

    if (!h2_submit(s, st, m, p, h, b)) {
        h2_reset_stream(st);
        H2_UNLOCK(&s->mtx);
        h2_set_error(DIAL_HTTP_ERR_NOMEM, "HTTP/2 请求编码失败");
        return NULL;
    }
    g_h2_stream_count++;

    /* 等自己的 stream 完成；I/O 线程每处理完相关帧会 broadcast。 */
    while (st->state == 1) {
        H2_COND_WAIT(&s->done_cv, &s->mtx);
    }

    DialHttpResponse *resp = NULL;
    if (st->state == 2 && !st->error) {
        resp = dial_http_resp_new(st->status >= 0 ? st->status : 0);
        if (resp) {
            for (int i = 0; i < st->headers.n; i++) {
                if (!dial_http_resp_add(resp, st->headers.v[i].name,
                                        st->headers.v[i].value)) break;
            }
            if (st->body.len > 0) {
                dial_http_resp_set_body(resp, (const char *)st->body.p, st->body.len);
            }
        }
    } else {
        h2_set_error(DIAL_HTTP_ERR_PROTOCOL, "HTTP/2 stream 被重置或会话终止");
    }
    h2_reset_stream(st);  /* 归还槽位 */
    H2_UNLOCK(&s->mtx);
    return resp;
}

int64_t dial_http2_session_free(DialH2Session *s) {
    if (!s) return 0;
    /* 请求 I/O 线程退出（它靠短超时轮询，最多 H2_RECV_POLL_MS 就会看到）。 */
    H2_LOCK(&s->mtx);
    s->shutdown = 1;
    H2_COND_BCAST(&s->done_cv);
    H2_UNLOCK(&s->mtx);
    if (s->io_started) H2_THREAD_JOIN(s->io_thread);

    /* 释放剩余待发帧、读缓冲、HPACK 上下文、各 stream 缓冲 */
    h2_outframe *f = s->out_head;
    while (f) { h2_outframe *nx = f->next; dh_bytes_free(&f->data); free(f); f = nx; }
    dh_bytes_free(&s->inbuf);
    dh_hpack_ctx_free(&s->hpack);
    for (int i = 0; i < H2_MAX_STREAMS; i++) h2_reset_stream(&s->streams[i]);

    if (s->tls) dial_tls_free(s->tls);
    if (s->fd != H2_INVALID_SOCKET) H2_CLOSE(s->fd);
    H2_COND_DESTROY(&s->done_cv);
    H2_MTX_DESTROY(&s->mtx);
    free(s);
    return 0;
}

DialString dial_http2_last_protocol(void) {
    return dial_string_new(g_h2_protocol[0] ? g_h2_protocol : "none");
}
DialString dial_http2_alpn(void) {
    return dial_string_new(g_h2_alpn[0] ? g_h2_alpn : "none");
}
int64_t dial_http2_conn_count(void)   { return g_h2_conn_count; }
int64_t dial_http2_stream_count(void) { return g_h2_stream_count; }
