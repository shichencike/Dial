#include "dial_bytes.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>
#include <stdio.h>

/*
 * payload 布局与 string 完全一致：[ int64 len ][ char data[] ]（data 以 '\0' 结尾）。
 * 之所以能复用同一套布局，是因为 bytes 与 string 在**内存表示**上确实没有差别——
 * 差别在**类型系统**层面（编译器认得它们是两种类型，要求显式转换）。
 *
 * 注意：本文件**刻意不调用 dial_string_* 系列**，一律直接用 dial_rc_alloc
 * 自己造对象。原因：如果 bytes 借用 string 的构造/释放函数，
 * 「bytes 与 string 是不同类型」这条契约就只剩编译器单方面在守——
 * 将来有人图省事写一句 dial_string_release(bytes) 就绕过去了。
 * 两套函数各自封闭，边界才是硬的。
 */

/* data 指针 = payload 起始处（跳过 8 字节 len 头） */
static char *b_data(void *b) { return (char *)b + 8; }

DialString dial_bytes_new(const char *buf, int64_t len) {
    if (len < 0) len = 0;
    void *p = dial_rc_alloc(8 + len + 1);
    ((int64_t *)p)[0] = len;
    char *d = b_data(p);
    if (buf && len > 0) memcpy(d, buf, (size_t)len);
    d[len] = '\0';
    return p;
}

DialString dial_bytes_zero(int64_t len) {
    if (len < 0) len = 0;
    void *p = dial_rc_alloc(8 + len + 1);
    ((int64_t *)p)[0] = len;
    char *d = b_data(p);
    if (len > 0) memset(d, 0, (size_t)len);
    d[len] = '\0';
    return p;
}

int64_t dial_bytes_length(DialString b) {
    DIAL_ASSERT(((int64_t *)b)[0] >= 0, "bytes 内部损坏：len < 0");
    return ((int64_t *)b)[0];
}

int64_t dial_bytes_get(DialString b, int64_t i) {
    /* Fail-Fast：越界读直接终止，绝不返回 '\0' 之后的脏字节 */
    DIAL_CHECK_BOUNDS(i, ((int64_t *)b)[0]);
    return (int64_t)(unsigned char)b_data(b)[i];
}

void dial_bytes_retain(DialString b) { dial_rc_retain(b); }
void dial_bytes_release(DialString b) { dial_rc_release(b); }

DialString dial_bytes_clone(DialString b) {
    int64_t len = ((int64_t *)b)[0];
    return dial_rc_clone(b, 8 + len + 1);
}

/*
 * string -> bytes：把文本的**字节**搬进二进制缓冲区。
 *
 * 注意这是**按字节**搬运，不做任何编码转换：UTF-8 文本的字节序列原样保留。
 * 因此它是可逆的（配 bytes_to_string 正好还原），也与「bytes 承载任意字节」一致。
 */
DialString dial_bytes_from_string(DialString s) {
    if (!s) return dial_bytes_zero(0);
    int64_t len = ((int64_t *)s)[0];
    return dial_bytes_new(b_data(s), len);
}

/*
 * int -> byte：带**范围校验**的窄化转换。
 *
 * 越界（负数或 > 255）时当场 Fail-Fast，绝不静默截断——
 * C 里 `(uint8_t)300` 会变成 44，那是被伪装成合法值的**数据损坏**，
 * 往往要到几百万行之后才被发现。宁可当场崩溃。
 */
int64_t dial_int_to_byte(int64_t v) {
    if (v < 0 || v > 255) {
        fprintf(stderr, "dial: int 转 byte 越界: %lld（合法范围 0~255）\n", (long long)v);
        fflush(stdout);
        dial_assert_fail("dial_int_to_byte: 越界（合法范围 0~255）");
    }
    return v;
}

/*
 * byte -> string：把单个字节的**数值**形式写成十进制文本（如 65 -> "65"）。
 * 这不是「把字节当字符」——后者要显式 int_to_char 转换。
 */
DialString dial_byte_to_string(int64_t b) {
    char buf[8];
    int n = snprintf(buf, sizeof(buf), "%lld", (long long)b);
    if (n < 0) n = 0;
    if ((int64_t)n > (int64_t)sizeof(buf) - 1) n = (int)sizeof(buf) - 1;
    return dial_string_from_bytes(buf, (int64_t)n);
}

/*
 * bytes -> string：把二进制缓冲区的字节**原样**当作 UTF-8 文本。
 *
 * 重要：bytes 可能是**非法 UTF-8**（比如从网络收来的原始数据）。
 * 此时产生的是「含非法序列的字符串」——它仍是一段字节，
 * 但任何按 UTF-8 解码的下游（终端、编辑器）都可能显示为乱码或拒绝解码。
 *
 * Dial 选择**不静默替换**非法字节（那会破坏数据），
 * 也不抛错（用户可能确实想原样搬运）；而是把「这段文本可能不是合法 UTF-8」
 * 的责任明确交给调用方——需要校验时请自行判断。
 * 若确实要求严格合法，请先自行校验字节序列。
 */
DialString dial_bytes_to_string(DialString b) {
    if (!b) return dial_string_new("");
    int64_t len = ((int64_t *)b)[0];
    return dial_string_from_bytes(b_data(b), len);
}
