#ifndef DIAL_HTTP2_H
#define DIAL_HTTP2_H

#include <stdint.h>
#include "dial_http.h"   /* DialString / DialHttpResponse / 错误码 */

/*
 * dial_http2 —— HTTP/2 客户端（http.dialpkg 的机器码部分），RFC 7540。
 *
 * ## 传输层
 *   - https://  → TLS 1.3 + ALPN 协商 h2（复用 dial_tls.c 的 mbedTLS 封装）。
 *   - http://   → 明文 h2c prior-knowledge（RFC 7540 §3.4，直接发 preface）。
 *   两条路径共用同一套帧层 / HPACK / stream 状态机。
 *
 * ## 并发模型（真·多路复用）
 *   一个 DialH2Session = 一条 TCP/TLS 连接，会话创建时启动**一个专用 I/O 线程**。
 *   该线程独占 socket / mbedTLS 上下文（mbedTLS 的 ssl_context 不能被多线程并发
 *   读写，除非编译期开 MBEDTLS_THREADING_C——我们没有），负责：
 *     - 从「待发帧队列」取出各 worker 提交的 HEADERS/DATA 帧，原子写出；
 *     - 读帧，按 stream_id 分发到对应 stream 的响应缓冲；
 *     - 帧处理完 cond_broadcast 唤醒等待该 stream 的 worker。
 *   Dial 侧每个 `spawn` worker 调 dial_http2_request：分配 stream id、把请求
 *   编码入队、在自己的 stream 上 cond_wait；醒来后**在 worker 自己线程里**组装
 *   DialHttpResponse（dial_string_new / dial_bytes_new 只在 worker 线程调用，
 *   reader 线程绝不构造任何 Dial ARC 对象）。
 *
 *   Http2Session 是**不透明裸指针、不进 ARC**：spawn 传它按值共享、不标 Moved、
 *   不 retain/release（见 codegen.rs 的 spawn argcell 与 release_value）。因此
 *   多个 worker 可安全持有同一会话句柄，由 dial_http2_session_free 显式释放。
 *
 * ## 错误
 *   复用 dial_http.c 的线程局部错误槽（dial_http_internal_set_error），
 *   Dial 侧用 dial_http_last_error()/msg() 读，口径与 HTTP/1.1 一致。
 */

typedef struct DialH2Session DialH2Session;

/* 建立 HTTP/2 会话（TLS h2 或明文 h2c）。失败返回 NULL，错误见 last_error。
 * url 形如 https://host[:port]/ 或 http://host[:port]/；path/query 忽略
 * （具体路径逐请求由 dial_http2_request 的 path 参数给出）。 */
DialH2Session *dial_http2_connect(DialString url);

/* 在会话上发一个请求并阻塞到该 stream 的响应收齐。多线程可并发调用。
 *   method/path/headers/body 都是 DialString；headers 为每行 "Name: value"
 *   的额外请求头（可空串）。失败/被 GOAWAY 返回 NULL（错误见 last_error）。
 * 返回的 HttpResponse 由现有 dial_http_resp_* 访问器读取，用完 dial_http_resp_free。 */
DialHttpResponse *dial_http2_request(DialH2Session *s, DialString method,
                                     DialString path, DialString headers,
                                     DialString body);

/* 关闭会话：唤醒并 join I/O 线程、关 TLS/socket、释放全部资源。返回 0 成功。 */
int64_t dial_http2_session_free(DialH2Session *s);

/* ---- 诊断（供测试断言）---- */
/* 最近一次 connect 协商出的协议："h2" / "h2c" / "none"（线程局部）。 */
DialString dial_http2_last_protocol(void);
/* TLS 协商出的 ALPN："h2" / "none"（线程局部）。 */
DialString dial_http2_alpn(void);
/* 进程级累计：成功建立的会话数与累计发出的 stream 数（证明单连接多路分流）。 */
int64_t dial_http2_conn_count(void);
int64_t dial_http2_stream_count(void);

#endif /* DIAL_HTTP2_H */
