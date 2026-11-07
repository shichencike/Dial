#ifndef DIAL_HTTP_INTERNAL_H
#define DIAL_HTTP_INTERNAL_H

/*
 * dial_http_internal.h —— http.dialpkg **包内**共享接口（不进 http.diali 契约）。
 *
 * 存在的唯一理由：HTTP/2 层（dial_http2.c）要构造一个能被现有
 * dial_http_resp_* 访问器消费的响应对象，而 struct DialHttpResponse 的完整
 * 定义是 dial_http.c 的文件内私有类型。与其把内部布局摊开给第二个 .c，
 * 不如由 dial_http.c 提供几个薄工厂——布局知识仍只此一处。
 *
 * 这些函数只被同一个静态库里的 dial_http2.c 调用，Dial 侧不可见、不进 .diali。
 */
#include <stdint.h>
#include "dial_http.h"

/* 新建一个指定状态码的空响应（失败返回 NULL）。所有权同 dial_http_resp_free。 */
DialHttpResponse *dial_http_resp_new(int64_t status);

/* 追加一条响应头（内部 strdup 名/值）。返回 1 成功、0 失败。 */
int dial_http_resp_add(DialHttpResponse *r, const char *name, const char *value);

/* 追加响应体字节。返回 1 成功、0 失败。 */
int dial_http_resp_set_body(DialHttpResponse *r, const char *data, int64_t len);

/* 复用 dial_http.c 的线程局部错误槽，让 HTTP/2 失败也能被
 * dial_http_last_error() / dial_http_last_error_msg() 读出（口径统一）。 */
void dial_http_internal_set_error(int64_t code, const char *msg);

/* 读取 dial_http_set_ca_path() 设下的自定义 CA 路径（无则 NULL）。
 * HTTP/2 over TLS 与 HTTP/1.1 over TLS 共用同一份受信 CA 配置。 */
const char *dial_http_internal_ca_path(void);

#endif /* DIAL_HTTP_INTERNAL_H */
