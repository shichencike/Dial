/*
 * emit_http.c —— 浏览器一致性画像的「候选 HTTP 行为发射器」（期 2）。
 *
 * 用途：让 http.dialpkg 的 HTTP/1.1 与 HTTP/2 层**真实地**各发一次请求，
 * 供 record_http.py 录制，再交给 compare_fp.py --behavior 与 Chrome 基准的
 * http_behavior 段逐字段比对。
 *
 *   --h1 <port>  dial_http_get()        → record_http.py --h1 录头序 / 头值
 *   --h2 <port>  dial_http2_connect()+request() → record_http.py --h2 录帧序
 *
 * 两种模式都先 dial_http_set_profile("chrome")：浏览器一致性画像是显式切换的，
 * default 画像保持保守行为（既有回归零影响）。
 *
 * 录制端只读到需要的部分就关闭连接、不回完整响应（H2 尤其如此），因此请求
 * 成败都不影响录制——报文已经在线上了。本程序无论请求成功与否都退出 0。
 *
 * 为什么带 stdlib stub：libhttp.a 只含包内 + zlib + mbedTLS 的 .o，不含 Dial
 * 运行时的 dial_string.o / dial_bytes.o。本 harness 不经过 dialc 编译器，直接
 * 用 zig cc 链接静态库，故在此提供四个最小实现（内存布局与 stdlib 完全一致：
 * [int64 len][data...]，cstr = s+8）。它们只服务于「把参数传进包里」，不做
 * ARC 释放——进程短命，无副作用。
 *
 * 编译：
 *   zig cc -target x86_64-linux-gnu tests/pkg/fingerprint/emit_http.c \
 *       -I packages/http/src -I stdlib -I runtime \
 *       packages/http/lib/x86_64-linux-gnu/libhttp.a -o /tmp/emit_http
 * 用法：/tmp/emit_http --h1|--h2 <port>
 */
#include "dial_http.h"
#include "dial_http2.h"
#include "dial_string.h"
#include "dial_bytes.h"

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- stdlib 最小 stub（仅为本 harness 链接 libhttp.a） ---- */
DialString dial_string_new(const char *lit) {
    size_t n = strlen(lit);
    unsigned char *p = (unsigned char *) malloc(8 + n + 1);
    if (!p) return NULL;
    int64_t len = (int64_t) n;
    memcpy(p, &len, 8);
    memcpy(p + 8, lit, n + 1);
    return p;
}

const char *dial_string_cstr(DialString s) {
    return s ? (const char *) ((unsigned char *) s + 8) : "";
}

int64_t dial_string_length(DialString s) {
    return s ? *(const int64_t *) s : 0;
}

DialString dial_bytes_new(const char *buf, int64_t len) {
    if (len < 0) len = 0;
    unsigned char *p = (unsigned char *) malloc(8 + (size_t) len + 1);
    if (!p) return NULL;
    memcpy(p, &len, 8);
    if (buf && len > 0) memcpy(p + 8, buf, (size_t) len);
    p[8 + (size_t) len] = '\0';
    return p;
}

int main(int argc, char **argv) {
    /* 录制端可能提前 close，后续写会触发 SIGPIPE。 */
    signal(SIGPIPE, SIG_IGN);

    if (argc < 3) {
        fprintf(stderr, "用法: %s --h1|--h2 <port>\n", argv[0]);
        return 2;
    }
    int port = atoi(argv[2]);
    if (port <= 0 || port > 65535) {
        fprintf(stderr, "端口非法: %s\n", argv[2]);
        return 2;
    }
    char url[128];
    snprintf(url, sizeof(url), "http://127.0.0.1:%d/", port);

    /* argv[3] 可选画像名（默认 chrome），供 default/chrome 对照诊断用。 */
    const char *profile = (argc > 3 && argv[3][0]) ? argv[3] : "chrome";
    DialString prof = dial_string_new(profile);
    dial_http_set_profile(prof);
    fprintf(stderr, "profile = %s\n", profile);

    if (strcmp(argv[1], "--h1") == 0) {
        DialString u = dial_string_new(url);
        DialString empty = dial_string_new("");
        DialHttpResponse *r = dial_http_get(u, empty);
        printf("h1_status = %lld\n",
               (long long) (r ? dial_http_resp_status(r) : -1));
        if (r) dial_http_resp_free(r);
        return 0;
    }

    if (strcmp(argv[1], "--h2") == 0) {
        DialString u = dial_string_new(url);
        DialH2Session *s = dial_http2_connect(u);
        if (!s) {
            fprintf(stderr, "h2 connect 失败（不影响录制：连接前导已在线上）\n");
            return 0;
        }
        DialString m = dial_string_new("GET");
        DialString p = dial_string_new("/");
        DialString empty = dial_string_new("");
        DialHttpResponse *r = dial_http2_request(s, m, p, empty, empty);
        printf("h2_status = %lld\n",
               (long long) (r ? dial_http_resp_status(r) : -1));
        if (!r) {
            DialString em = dial_http_last_error_msg();
            fprintf(stderr, "h2_error = %s\n", dial_string_cstr(em));
        }
        if (r) dial_http_resp_free(r);
        dial_http2_session_free(s);
        return 0;
    }

    fprintf(stderr, "未知模式 %s（应为 --h1 / --h2）\n", argv[1]);
    return 2;
}
