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
 *
 * 换行语义：**追加一个 '\n'**（行式输出）。
 * 需要不换行的连续输出请用 write_char —— 两者只差这一个字节。
 */
void dial_print_char(int64_t code) {
    /* Fail-Fast：非法码点绝不静默输出半个坏字符。
     * 编码规则（合法范围 / 代理区）由 dial_encode_utf8 统一定义，本层只负责报错。 */
    char buf[5];
    int n = dial_encode_utf8(code, buf);
    if (n < 0) {
        fprintf(stderr, "dial: 非法的 char 码点: %lld\n", (long long)code);
        fflush(stdout);
        dial_assert_fail("dial_print_char: 非法 Unicode 码点（合法范围 0~0x10FFFF，且不含 UTF-16 代理区 D800~DFFF）");
    }
    printf("%s\n", buf);
}

DialString dial_read_line(void) {
    char buf[4096];
    if (!fgets(buf, sizeof(buf), stdin)) return dial_string_new("");
    size_t n = strlen(buf);
    while (n > 0 && (buf[n - 1] == '\n' || buf[n - 1] == '\r')) buf[--n] = 0;
    return dial_string_new(buf);
}

/*
 * 原样输出字符串，不追加换行（v0.98.2）。
 *
 * 按**字节长度**写，而不是像 puts 那样依赖 NUL 终止——Dial 的 string 允许
 * 内嵌 '\0'（它本质是字节缓冲区，payload 头里带着显式长度），
 * 用 strlen 会在第一个 NUL 处截断，把二进制数据悄悄截短。
 *
 * 与 dial_print_string 的分工：
 *     print("ab")    -> ab\n   （带换行）
 *     print_raw("ab") -> ab    （不带换行，可连续拼行）
 *
 * 写失败被忽略，不抛错（与 dial_print_string 保持同一口径）。
 */
void dial_print_raw(DialString s) {
    /* NULL 字符串：与 dial_print_string 同样的受控 Fail-Fast——
     * 静默当成空串会掩盖「通道 EOF 后仍在用取回的值」这类真 bug。 */
    if (s == NULL) {
        fprintf(stderr, "dial: print_raw 收到了 NULL string\n");
        fflush(stdout);
        dial_assert_fail("dial_print_raw: NULL string（多半是通道 EOF 之后仍在用取回的值）");
    }
    int64_t len = ((int64_t *)s)[0];
    if (len <= 0) return;               /* 空串是合法的 no-op */
    fwrite((const char *)s + 8, 1, (size_t)len, stdout);
}

/*
 * 输出单个字符，不追加换行（v0.98.2）。
 *
 * 码点校验与 dial_print_char 完全一致：非法码点（负数、> 0x10FFFF、
 * UTF-16 代理区 D800~DFFF）一律 Fail-Fast，绝不输出半个坏字符——
 * 半个 UTF-8 序列在下游解码时会变成替换字符或直接报错，
 * 属于「静默产出乱码」，比当场崩溃危险得多。
 */
void dial_write_char(int64_t code) {
    char buf[5];
    int n = dial_encode_utf8(code, buf);
    if (n < 0) {
        fprintf(stderr, "dial: 非法的 char 码点: %lld\n", (long long)code);
        fflush(stdout);
        dial_assert_fail("dial_write_char: 非法 Unicode 码点（合法范围 0~0x10FFFF，且不含 UTF-16 代理区 D800~DFFF）");
    }
    fwrite(buf, 1, (size_t)n, stdout);
}

/*
 * 标量版本的无换行输出（v0.98.2）。
 *
 * 为什么单独提供，而不是在 Dial 侧先 str(v) 再 print_raw：
 *   str(v) 会**分配一个堆字符串**（带引用计数），而它只是 print_raw 的
 *   一次性中转——用完即弃却仍占着引用计数，属于实打实的内存泄漏。
 *   这里直接用栈上缓冲格式化，零堆分配。
 *
 * 三者与对应的 dial_print_* 保持**完全一致的文本形态**，
 * 唯一区别是末尾不补 '\n'：
 *     print(42)      -> 42\n
 *     print_raw(42)  -> 42
 *     print(1.5)     -> 1.500000\n     （%lf，六位小数）
 *     print_raw(1.5) -> 1.500000
 *     print(true)    -> true\n
 *     print_raw(true)-> true
 */
void dial_print_raw_int(int64_t v) {
    printf("%lld", (long long)v);
}

void dial_print_raw_float(double v) {
    printf("%lf", v);
}

void dial_print_raw_bool(int64_t b) {
    printf("%s", b ? "true" : "false");
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

/* ==========================================================================
 * stderr 输出族（v1.04 C8）：eprint / eprint_raw 的 C 侧落地。
 *
 * 与 stdout 族逐函数对称，唯一差别是 fprintf / fwrite 的目标是 stderr。
 * 刻意保持「一份函数一个名字」的直白形态，不抽公共宏、不引入全局流指针：
 * 这既符合 §21.1「绝对拒绝过度抽象」，也守住宪法 §1「绝不引入可变全局状态」。
 * char 的 UTF-8 编码与非法码点 Fail-Fast 规则同 dial_print_char / dial_write_char。
 * ========================================================================== */
void dial_eprint_int(int64_t v)   { fprintf(stderr, "%lld\n", (long long)v); }
void dial_eprint_float(double v)  { fprintf(stderr, "%lf\n", v); }
void dial_eprint_bool(int64_t b)  { fprintf(stderr, "%s\n", b ? "true" : "false"); }

void dial_eprint_char(int64_t code) {
    char buf[5];
    int n = dial_encode_utf8(code, buf);
    if (n < 0) {
        fprintf(stderr, "dial: 非法的 char 码点: %lld\n", (long long)code);
        dial_assert_fail("dial_eprint_char: 非法 Unicode 码点（合法范围 0~0x10FFFF，且不含 UTF-16 代理区 D800~DFFF）");
    }
    fwrite(buf, 1, (size_t)n, stderr);
    fputc('\n', stderr);
}

void dial_eprint_string(DialString s) {
    if (s == NULL) {
        fprintf(stderr, "dial: eprint 收到了 NULL string\n");
        dial_assert_fail("dial_eprint_string: NULL string（多半是通道 EOF 之后仍在用取回的值）");
    }
    int64_t len = ((int64_t *)s)[0];
    fwrite((const char *)s + 8, 1, (size_t)len, stderr);
    fputc('\n', stderr);
}

void dial_eprint_raw(DialString s) {
    if (s == NULL) {
        fprintf(stderr, "dial: eprint_raw 收到了 NULL string\n");
        dial_assert_fail("dial_eprint_raw: NULL string（多半是通道 EOF 之后仍在用取回的值）");
    }
    int64_t len = ((int64_t *)s)[0];
    if (len <= 0) return;
    fwrite((const char *)s + 8, 1, (size_t)len, stderr);
}

void dial_eprint_write_char(int64_t code) {
    char buf[5];
    int n = dial_encode_utf8(code, buf);
    if (n < 0) {
        fprintf(stderr, "dial: 非法的 char 码点: %lld\n", (long long)code);
        dial_assert_fail("dial_eprint_write_char: 非法 Unicode 码点（合法范围 0~0x10FFFF，且不含 UTF-16 代理区 D800~DFFF）");
    }
    fwrite(buf, 1, (size_t)n, stderr);
}

void dial_eprint_raw_int(int64_t v)   { fprintf(stderr, "%lld", (long long)v); }
void dial_eprint_raw_float(double v)  { fprintf(stderr, "%lf", v); }
void dial_eprint_raw_bool(int64_t b)  { fprintf(stderr, "%s", b ? "true" : "false"); }
