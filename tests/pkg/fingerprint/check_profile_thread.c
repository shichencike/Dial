/*
 * check_profile_thread.c —— 期 3 验证：画像为**线程局部**，多线程各自设定互不干扰。
 *
 * 两个线程：主线程设 "chrome"，子线程不设定（保持 default）。各自回读
 * dial_http_profile()，断言主线程读到 "chrome"、子线程读到 "default"。
 * 若画像仍是进程级全局，子线程会读到被主线程污染的 "chrome" → 测试失败。
 *
 * 编译：
 *   zig cc -target x86_64-linux-gnu tests/pkg/fingerprint/check_profile_thread.c \
 *       -I packages/http/src -I stdlib -I runtime \
 *       packages/http/lib/x86_64-linux-gnu/libhttp.a -o /tmp/check_profile_thread -lpthread
 * 退出码 0 = 通过；非 0 = 失败。
 */
#include "dial_http.h"
#include "dial_string.h"
#include "dial_bytes.h"

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- stdlib 最小 stub（与 emit_http.c 一致） ---- */
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
int64_t dial_string_length(DialString s) { return s ? *(const int64_t *) s : 0; }
DialString dial_bytes_new(const char *buf, int64_t len) {
    if (len < 0) len = 0;
    unsigned char *p = (unsigned char *) malloc(8 + (size_t) len + 1);
    if (!p) return NULL;
    memcpy(p, &len, 8);
    if (buf && len > 0) memcpy(p + 8, buf, (size_t) len);
    p[8 + (size_t) len] = '\0';
    return p;
}

static const char *g_child_read = "UNSET";

static void *child(void *arg) {
    (void) arg;
    /* 子线程不调用 set_profile，应停留在 default（线程局部，不被主线程污染）。 */
    DialString got = dial_http_profile();
    g_child_read = dial_string_cstr(got);
    return NULL;
}

int main(void) {
    /* 主线程设 chrome。 */
    DialString p = dial_string_new("chrome");
    dial_http_set_profile(p);

    pthread_t t;
    if (pthread_create(&t, NULL, child, NULL) != 0) {
        fprintf(stderr, "pthread_create 失败\n");
        return 2;
    }
    pthread_join(t, NULL);

    DialString main_got = dial_http_profile();
    printf("main_thread_profile = %s\n", dial_string_cstr(main_got));
    printf("child_thread_profile = %s\n", g_child_read);

    int ok = 1;
    if (strcmp(dial_string_cstr(main_got), "chrome") != 0) {
        fprintf(stderr, "FAIL: 主线程应读到 chrome，实际 %s\n", dial_string_cstr(main_got));
        ok = 0;
    }
    if (strcmp(g_child_read, "default") != 0) {
        fprintf(stderr, "FAIL: 子线程应读到 default（画像线程局部），实际 %s\n", g_child_read);
        ok = 0;
    }
    if (ok) {
        printf("profile_thread_local = pass\n");
        return 0;
    }
    return 1;
}
