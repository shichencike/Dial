#include "dial_io.h"
#include "dial_string.h"
#include "dial_rt_trap.h"
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

void dial_print_int(int64_t v)   { printf("%lld\n", (long long)v); }
void dial_print_float(double v)  { printf("%lf\n", v); }
void dial_print_bool(int64_t b)  { printf("%s\n", b ? "true" : "false"); }

/*
 * 打印一个 char（v0.92）：char 的语义是「一个 Unicode 码点」，
 * 因此输出时必须按 UTF-8 重新编码为 1~4 字节，而不是把码点当数字打印。
 * 这与 print(int) 形成明确分工：
 *     print('a')            -> a
 *     print(char_to_int('a')) -> 97
 */
void dial_print_char(int64_t code) {
    /* Fail-Fast：非法码点绝不静默输出半个坏字符。
     * 负数、超出 Unicode 上界、以及 UTF-16 代理区（D800~DFFF）都不是合法标量值。 */
    if (code < 0 || code > 0x10FFFF || (code >= 0xD800 && code <= 0xDFFF)) {
        fprintf(stderr, "dial: 非法的 char 码点: %lld\n", (long long)code);
        fflush(stdout);
        dial_assert_fail("dial_print_char: 非法 Unicode 码点（合法范围 0~0x10FFFF，且不含 UTF-16 代理区 D800~DFFF）");
    }

    char buf[5];
    int n = 0;
    if (code < 0x80) {
        buf[n++] = (char)code;                                    /* 0xxxxxxx            */
    } else if (code < 0x800) {
        buf[n++] = (char)(0xC0 | (code >> 6));                    /* 110xxxxx 10xxxxxx   */
        buf[n++] = (char)(0x80 | (code & 0x3F));
    } else if (code < 0x10000) {
        buf[n++] = (char)(0xE0 | (code >> 12));                   /* 1110xxxx 10xxxxxx x2 */
        buf[n++] = (char)(0x80 | ((code >> 6) & 0x3F));
        buf[n++] = (char)(0x80 | (code & 0x3F));
    } else {
        buf[n++] = (char)(0xF0 | (code >> 18));                   /* 11110xxx 10xxxxxx x3 */
        buf[n++] = (char)(0x80 | ((code >> 12) & 0x3F));
        buf[n++] = (char)(0x80 | ((code >> 6) & 0x3F));
        buf[n++] = (char)(0x80 | (code & 0x3F));
    }
    buf[n] = '\0';
    printf("%s\n", buf);
}

DialString dial_read_line(void) {
    char buf[4096];
    if (!fgets(buf, sizeof(buf), stdin)) return dial_string_new("");
    size_t n = strlen(buf);
    while (n > 0 && (buf[n - 1] == '\n' || buf[n - 1] == '\r')) buf[--n] = 0;
    return dial_string_new(buf);
}

/* ---- 文件 IO（v0.61）：FILE* 封装，显式 open/close，不受 ARC 管理 ----
 * v0.7 起，D 失败即返回「空哨兵」交由 Dial 层抛出 Error（业务失败与系统崩溃分离）：
 *   - open  失败 -> 返回 NULL；
 *   - read  失败（句柄无效/分配失败）-> 返回 NULL；读 0 字节请求仍返回空串（成功）；
 *   - write 失败 -> 返回 -1。
 * Dial 的 open/read/write 内置据此转换成 throw Error（见 codegen）。 */

DialFile dial_file_open(DialString path, DialString mode) {
    if (!path || !mode) return NULL;
    FILE *fp = fopen(dial_string_cstr(path), dial_string_cstr(mode));
    return fp ? (DialFile)fp : NULL;
}

DialString dial_file_read(DialFile f, int64_t n) {
    if (!f) return NULL;
    if (n <= 0) return dial_string_new("");
    char *buf = (char *)malloc((size_t)n);
    if (!buf) return NULL;
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
