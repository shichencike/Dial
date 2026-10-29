#include "dial_io.h"
#include "dial_string.h"
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

void dial_print_int(int64_t v)   { printf("%lld\n", (long long)v); }
void dial_print_float(double v)  { printf("%lf\n", v); }
void dial_print_bool(int64_t b)  { printf("%s\n", b ? "true" : "false"); }

DialString dial_read_line(void) {
    char buf[4096];
    if (!fgets(buf, sizeof(buf), stdin)) return dial_string_new("");
    size_t n = strlen(buf);
    while (n > 0 && (buf[n - 1] == '\n' || buf[n - 1] == '\r')) buf[--n] = 0;
    return dial_string_new(buf);
}

/* ---- 文件 IO（v0.61）：FILE* 封装，显式 open/close，不受 ARC 管理 ---- */

DialFile dial_file_open(DialString path, DialString mode) {
    if (!path || !mode) return NULL;
    FILE *fp = fopen(dial_string_cstr(path), dial_string_cstr(mode));
    return fp ? (DialFile)fp : NULL;
}

DialString dial_file_read(DialFile f, int64_t n) {
    if (!f || n <= 0) return dial_string_new("");
    char *buf = (char *)malloc((size_t)n);
    if (!buf) return dial_string_new("");
    size_t got = fread(buf, 1, (size_t)n, (FILE *)f);
    DialString s = dial_string_from_bytes(buf, (int64_t)got);
    free(buf);
    return s;
}

int64_t dial_file_write(DialFile f, DialString content) {
    if (!f || !content) return -1;
    int64_t len = ((int64_t *)content)[0];
    size_t wrote = fwrite((const char *)content + 8, 1, (size_t)len, (FILE *)f);
    return (int64_t)wrote;
}

void dial_file_close(DialFile f) {
    if (!f) return;
    fclose((FILE *)f);
}
