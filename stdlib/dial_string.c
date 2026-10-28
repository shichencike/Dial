#include "dial_string.h"
#include "dial_rt_mem.h"
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

int64_t dial_string_length(DialString s) { return ((int64_t *)s)[0]; }
int64_t dial_string_get(DialString s, int64_t i) { return (int64_t)(unsigned char)s_data(s)[i]; }

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
    fwrite(s_data(s), 1, (size_t)((int64_t *)s)[0], stdout);
    putchar('\n');
}
