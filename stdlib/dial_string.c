#include "dial_string.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>
#include <stdio.h>

/* payload: [ int64 len ][ char data[] ]；data 指针 = s + 8 */
static char *s_data(void *s) { return (char *)s + 8; }

DialString dial_string_new(const char *lit) {
    int64_t len = (int64_t)strlen(lit);
    void *p = dial_rc_alloc(8 + len + 1);
    ((int64_t *)p)[0] = len;
    char *d = s_data(p);
    memcpy(d, lit, (size_t)len + 1);
    return p;
}

const char *dial_string_cstr(DialString s) { return (const char *)s + 8; }

DialString dial_string_from_bytes(const char *buf, int64_t len) {
    if (len < 0) len = 0;
    void *p = dial_rc_alloc(8 + len + 1);
    ((int64_t *)p)[0] = len;
    char *d = (char *)p + 8;
    if (buf && len > 0) memcpy(d, buf, (size_t)len);
    d[len] = '\0';
    return p;
}

int64_t dial_string_length(DialString s) {
    DIAL_ASSERT(((int64_t *)s)[0] >= 0, "字符串内部损坏：len < 0");
    return ((int64_t *)s)[0];
}

int64_t dial_string_get(DialString s, int64_t i) {
    /* Fail-Fast：越界读直接终止，绝不返回 '\0' 之后的脏字节 */
    DIAL_CHECK_BOUNDS(i, ((int64_t *)s)[0]);
    return (int64_t)(unsigned char)s_data(s)[i];
}

DialString dial_string_concat(DialString a, DialString b) {
    int64_t la = ((int64_t *)a)[0];
    int64_t lb = ((int64_t *)b)[0];
    void *p = dial_rc_alloc(8 + la + lb + 1);
    ((int64_t *)p)[0] = la + lb;
    char *d = s_data(p);
    memcpy(d, s_data(a), (size_t)la);
    memcpy(d + la, s_data(b), (size_t)lb);
    d[la + lb] = '\0';
    return p;
}

int64_t dial_string_eq(DialString a, DialString b) {
    int64_t la = ((int64_t *)a)[0];
    int64_t lb = ((int64_t *)b)[0];
    if (la != lb) return 0;
    return memcmp(s_data(a), s_data(b), (size_t)la) == 0 ? 1 : 0;
}

DialString dial_string_substring(DialString s, int64_t from, int64_t to) {
    int64_t slen = ((int64_t *)s)[0];
    /* Fail-Fast：半开区间 [from, to) 必须完全落在 [0, slen] 内 */
    if (DIAL_SAFETY >= DIAL_SAFETY_SAFE && (from < 0 || to < from || to > slen)) {
        dial_bounds_fail(from < 0 || to > slen ? (from < 0 ? from : to) : to, slen);
    }
    int64_t len = to - from;
    void *p = dial_rc_alloc(8 + len + 1);
    ((int64_t *)p)[0] = len;
    char *d = s_data(p);
    memcpy(d, s_data(s) + from, (size_t)len);
    d[len] = '\0';
    return p;
}

DialString dial_string_clone(DialString s) {
    return dial_rc_clone(s, 8 + ((int64_t *)s)[0] + 1);
}

void dial_string_retain(DialString s) { dial_rc_retain(s); }
void dial_string_release(DialString s) { dial_rc_release(s); }

void dial_print_string(DialString s) {
    /* v0.81：recv / try_recv 在 EOF 时返回 NULL。若用户没用 done(ch) 守住就直接使用，
     * 这里必须当场受控 Fail-Fast（打印原因 + ud2），而不是解引用 NULL 变成一次
     * 无解释的段错误，更不能让它伪装成空串流进业务逻辑（§20 铁律）。 */
    DIAL_HEAP_ASSERT(s != NULL, "print 了一个 NULL string（多半是通道 EOF 之后仍在用取回的值）");
    fwrite(s_data(s), 1, (size_t)((int64_t *)s)[0], stdout);
    putchar('\n');
}

/* 类型 -> 字符串（CVE-2024-DIAL-006 新增）：支撑 `print("x=" + str(n))` 这类拼接。 */
DialString dial_int_to_string(int64_t v) {
    char buf[32];
    int n = snprintf(buf, sizeof(buf), "%lld", (long long)v);
    if (n < 0) n = 0;
    if ((int64_t)n > (int64_t)sizeof(buf) - 1) n = (int)sizeof(buf) - 1;
    return dial_string_from_bytes(buf, (int64_t)n);
}

DialString dial_float_to_string(double v) {
    char buf[64];
    int n = snprintf(buf, sizeof(buf), "%.6g", v);
    if (n < 0) n = 0;
    if ((int64_t)n > (int64_t)sizeof(buf) - 1) n = (int)sizeof(buf) - 1;
    return dial_string_from_bytes(buf, (int64_t)n);
}
