#ifndef DIAL_HTTP_H
#define DIAL_HTTP_H

#include <stdint.h>

/*
 * dial_http —— HTTP/1.1 客户端（http.dialpkg 的机器码部分）。
 *
 * ## 设计立场
 *
 * 1. **协议层自研**：HTTP/1.1 的报文解析、Header 拼装、Socket 读写全部自己实现。
 *    线级细节（请求行格式、头序、User-Agent、连接复用策略）必须留在自己手里，
 *    否则「想改一点握手行为」就得去改第三方库。
 * 2. **密码学绝不手搓**：TLS 不在本文件里——它由 mbedtls 提供（本轮只编入静态库、
 *    暂不向 Dial 暴露 TLS API），本文件只做明文 HTTP/1.1。
 * 3. **压缩用成熟实现**：gzip / deflate 解压走 zlib（inflate），不自己写 DEFLATE。
 * 4. **错误走返回码 + 最近错误槽**，不抛 Dial 异常——包不应假定宿主程序装了
 *    异常处理栈；调用方用 `dial_http_last_error()` / `dial_http_last_error_msg()`
 *    拿到原因。错误槽是**线程局部**的，多线程各看各的。
 *
 * ## 内存模型
 *
 * - 本文件自己的结构体（连接、响应）用 `malloc` 分配，由显式的
 *   `dial_http_close()` / `dial_http_resp_free()` 释放——**不透明句柄不进 ARC**
 *   （与 stdlib 的文件句柄同一口径：句柄的释放必须显式可见）。
 * - 返回给 Dial 的字符串 / bytes 用 `dial_string_new` / `dial_bytes_new` 构造，
 *   走 Dial 自己的引用计数堆对象，由 Dial 侧 ARC 负责。
 */

typedef void *DialString;

/* 不透明句柄 */
typedef struct DialHttpConn DialHttpConn;
typedef struct DialHttpResponse DialHttpResponse;

/* ---- 错误码 ---- */
enum {
    DIAL_HTTP_OK = 0,
    DIAL_HTTP_ERR_URL = -1,          /* URL 解析失败（空 host / 不支持的 scheme） */
    DIAL_HTTP_ERR_DNS = -2,          /* 域名解析失败 */
    DIAL_HTTP_ERR_CONNECT = -3,      /* TCP 连接失败 */
    DIAL_HTTP_ERR_IO = -4,           /* 读写失败 / 连接中断 */
    DIAL_HTTP_ERR_PROTOCOL = -5,     /* 响应不是合法 HTTP/1.x */
    DIAL_HTTP_ERR_NOMEM = -6,        /* 内存不足 */
    DIAL_HTTP_ERR_UNSUPPORTED = -7,  /* 例如明文客户端遇到 https:// */
    DIAL_HTTP_ERR_STATE = -8         /* 句柄状态不对（如对已关闭的连接读写） */
};

/* ---- 最近一次错误（线程局部）---- */
int64_t    dial_http_last_error(void);
DialString dial_http_last_error_msg(void);

/* ---- 底层：流式（连接级）---- */

/*
 * 解析 URL 并建立 TCP 连接。
 * URL 形如 `http://host[:port]/path[?query]`；省略 scheme 时按 `http://` 处理，
 * 省略端口时按 80。`https://` 明确返回 NULL 并把错误置为
 * DIAL_HTTP_ERR_UNSUPPORTED（本轮只做明文；TLS 走 mbedtls，后续再接线）。
 * 失败返回 NULL，错误原因见 `dial_http_last_error_msg()`。
 */
DialHttpConn *dial_http_connect(DialString url);

/* 往连接里写任意字节（发请求行 / 头 / 体都用它）。返回写入字节数，失败返回 -1。 */
int64_t dial_http_write(DialHttpConn *c, DialString data);

/* 按行写：写完自动补 CRLF（HTTP 的行结束符）。
 * Dial 字符串字面量不支持 `\r` 转义，所以行结束符由包负责。 */
int64_t dial_http_write_line(DialHttpConn *c, DialString line);

/* 读一行（到 '\n' 为止，**不含**换行符；末尾的 '\r' 也去掉）。失败返回 NULL。 */
DialString dial_http_read_line(DialHttpConn *c);

/* 读到连接关闭为止，返回全部字节。失败返回 NULL。 */
DialString dial_http_read_all(DialHttpConn *c);

/* 关闭连接并释放句柄。返回 0 成功。 */
int64_t dial_http_close(DialHttpConn *c);

/* 设置收发超时（秒）。<= 0 表示不限时。必须在 connect 之前调用才生效
 * （对已建立的连接会立即套用）。 */
int64_t dial_http_set_timeout(DialHttpConn *c, int64_t seconds);

/* ---- 高层：一次请求一个响应 ---- */

/*
 * 发一个 GET 请求并读完响应。
 * `headers` 是额外请求头，格式为每行一个 "Name: value"（**不含**末尾换行）；
 * 传 NULL / 空串表示不加额外头。
 * 失败返回 NULL（原因见 last_error）；成功返回响应句柄。
 */
DialHttpResponse *dial_http_get(DialString url, DialString headers);

/*
 * 发一个 POST 请求并读完响应。
 * `body` 可为 NULL（视作空体）；`content_type` 可为 NULL（则不发 Content-Type）。
 */
DialHttpResponse *dial_http_post(DialString url, DialString headers,
                                  DialString body, DialString content_type);

/* 响应状态码（如 200 / 404 / 500）。句柄无效时返回 -1。 */
int64_t dial_http_resp_status(DialHttpResponse *r);

/* 响应体（bytes；已按 Content-Encoding 解压过）。句柄无效时返回 NULL。 */
DialString dial_http_resp_body(DialHttpResponse *r);

/* 响应体字节长度。 */
int64_t dial_http_resp_body_len(DialHttpResponse *r);

/* 按名字取响应头（大小写不敏感）。缺失返回空串（不是 NULL）。 */
DialString dial_http_resp_header(DialHttpResponse *r, DialString name);

/* 响应头数量 / 按序号取头名 / 取头值（便于遍历）。 */
int64_t   dial_http_resp_header_count(DialHttpResponse *r);
DialString dial_http_resp_header_name(DialHttpResponse *r, int64_t i);
DialString dial_http_resp_header_value(DialHttpResponse *r, int64_t i);

/* 释放响应句柄。返回 0 成功。 */
int64_t dial_http_resp_free(DialHttpResponse *r);

/* ---- 客户端画像（兼容性配置，非规避引擎）---- */

/* 切换客户端画像：目前支持 "default"（默认）与 "chrome"。
 * 画像只控制请求头排序与 User-Agent 这类兼容参数，用于与只认浏览器客户端的
 * API 互通；不以“绕过服务器限制 / 欺骗识别”为目标。 */
void dial_http_set_profile(DialString name);

/* 设置自定义受信 CA（文件或目录，PEM）。传空串恢复为系统根证书库。
 * 用于内网 / 私有 CA 场景；设置后证书校验依旧 fail-closed（不允许自签名冒用）。 */
void dial_http_set_ca_path(DialString path);

/* ---- 连接池统计（供测试/诊断）---- */
void dial_http_pool_stats(int *hits, int *total);
int  dial_http_pool_hits(void);
int  dial_http_pool_total(void);
void dial_http_pool_clear(void);

/* ---- 最近一次请求的 TLS 协商信息（线程局部）---- */
DialString  dial_http_last_tls_version(void);
uint32_t    dial_http_last_tls_verify(void);
DialString  dial_http_last_tls_alpn(void);

#endif /* DIAL_HTTP_H */
