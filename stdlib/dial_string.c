#include "dial_string.h"
#include "dial_array.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>
#include <stdio.h>
#include <stdlib.h>   /* v1.04 第 2 批：strtoll / strtod（严格数值解析） */
#include <errno.h>    /* v1.04 第 2 批：ERANGE（溢出） */

/* payload: [ int64 len ][ char data[] ]；data 指针 = s + 8 */
static char *s_data(void *s) { return (char *)s + 8; }

DialString dial_bool_to_string(int64_t b) {
    return dial_string_new(b ? "true" : "false");
}

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
    if (!s) return NULL;
    /* clone() 必须交出「一份独立副本」：先 retain 让「唯一持有者」判定失效，迫使
     * dial_rc_clone 走进深拷贝分支，克隆完再抵消掉刚才那次 retain。
     * （dial_rc_clone 是纯粹的 COW：唯一持有者时原样返回、不改引用计数。） */
    dial_rc_retain(s);
    DialString n = dial_rc_clone(s, 8 + ((int64_t *)s)[0] + 1);
    dial_rc_release(s);
    return n;
}

void dial_string_retain(DialString s) { dial_rc_retain(s); }
void dial_string_release(DialString s) { dial_rc_release(s); }

/* 查找子串首次出现位置（字节偏移）；未找到返回 -1。 */
int64_t dial_string_index_of(DialString s, DialString sub) {
    if (!s || !sub) return -1;
    int64_t slen = ((int64_t *)s)[0];
    int64_t sublen = ((int64_t *)sub)[0];
    if (sublen == 0) return 0;            /* 空分隔符匹配开头 */
    if (sublen > slen) return -1;
    const char *d = s_data(s);
    const char *needle = (const char *)sub + 8;
    for (int64_t i = 0; i + sublen <= slen; i++) {
        if (memcmp(d + i, needle, (size_t)sublen) == 0) return i;
    }
    return -1;
}

/* 把 from 的所有出现替换为 to，返回新字符串（原串不变）。 */
DialString dial_string_replace(DialString s, DialString from, DialString to) {
    int64_t slen = ((int64_t *)s)[0];
    int64_t flen = ((int64_t *)from)[0];
    int64_t tlen = ((int64_t *)to)[0];
    const char *d = s_data(s);
    const char *fl = (const char *)from + 8;
    const char *tl = (const char *)to + 8;

    if (flen == 0) return dial_string_clone(s);  /* 空 from 无匹配 */

    /* 先数出匹配段数，算出结果长度 */
    int64_t matches = 0;
    for (int64_t i = 0; i + flen <= slen; ) {
        if (memcmp(d + i, fl, (size_t)flen) == 0) { matches++; i += flen; }
        else i++;
    }
    int64_t outlen = slen + matches * (tlen - flen);
    if (outlen < 0) outlen = 0;

    void *p = dial_rc_alloc(8 + outlen + 1);
    ((int64_t *)p)[0] = outlen;
    char *o = s_data(p);
    int64_t w = 0;
    int64_t i = 0;
    for (; i + flen <= slen; ) {
        if (memcmp(d + i, fl, (size_t)flen) == 0) {
            memcpy(o + w, tl, (size_t)tlen); w += tlen; i += flen;
        } else {
            o[w++] = d[i++];
        }
    }
    while (i < slen) o[w++] = d[i++];
    o[outlen] = '\0';
    return p;
}

/* 去除首尾空白（空格 / 制表 / 换行 / 回车），返回新字符串。 */
DialString dial_string_trim(DialString s) {
    int64_t slen = ((int64_t *)s)[0];
    const char *d = s_data(s);
    int64_t start = 0, end = slen;
    while (start < end) {
        char c = d[start];
        if (c == ' ' || c == '\t' || c == '\n' || c == '\r') start++;
        else break;
    }
    while (end > start) {
        char c = d[end - 1];
        if (c == ' ' || c == '\t' || c == '\n' || c == '\r') end--;
        else break;
    }
    return dial_string_substring(s, start, end);
}

/* 按分隔符 sep 切分，返回 string[]（kind=1 RC 数组，每元素是 retain 过的子串）。 */
DialArray dial_string_split(DialString s, DialString sep) {
    int64_t slen = ((int64_t *)s)[0];
    int64_t seplen = ((int64_t *)sep)[0];
    const char *d = s_data(s);
    const char *sepdat = (const char *)sep + 8;

    DialArray arr = dial_array_new(1 /* RC */);
    if (seplen == 0) {                      /* 空分隔符：整串作为一个元素 */
        DialString whole = dial_string_substring(s, 0, slen);
        arr = dial_array_push(arr, (int64_t)whole);
        dial_rc_release(whole);
        return arr;
    }
    if (slen == 0) return arr;              /* 空串：返回空数组 */

    int64_t i = 0;
    while (i <= slen) {
        int64_t j = i;
        /* 找下一个分隔符位置 */
        while (j + seplen <= slen && memcmp(d + j, sepdat, (size_t)seplen) != 0) j++;
        DialString piece = dial_string_substring(s, i, j);
        arr = dial_array_push(arr, (int64_t)piece);
        dial_rc_release(piece);             /* push 已 retain，本地引用可释放 */
        if (j + seplen > slen) break;       /* 末尾之后无更多分隔符 */
        i = j + seplen;
    }
    return arr;
}
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

/*
 * 码点 -> UTF-8 字节序列（v0.98.2 起抽成唯一实现）。
 *
 * 整个运行时里只有这一个地方知道「怎么把 Unicode 码点编成 UTF-8」：
 * dial_print_char / dial_write_char / dial_char_to_string 三者共用它。
 * 三处各写一份编码逻辑的话，任何一处改动都会让另外两处悄悄失配——
 * 那种 bug 极难察觉（输出看着像乱码，但只在某个码点区间出现）。
 *
 * 合法范围：0 ~ 0x10FFFF，且**不含 UTF-16 代理区 D800~DFFF**。
 * 代理区不是合法标量值——把它编成 3 字节序列会产生无效 UTF-8（WTF-8），
 * 下游解码时会变成替换字符或直接报错，属于「静默产出乱码」，
 * 比当场崩溃危险得多，因此一律拒绝。
 *
 * 返回写入 buf 的字节数（1~4）；buf 至少要有 5 字节（留 1 字节给 '\0'）。
 * 码点非法时返回 -1，**不做任何输出**——由调用方决定怎么报告错误。
 */
int dial_encode_utf8(int64_t code, char *buf) {
    if (code < 0 || code > 0x10FFFF || (code >= 0xD800 && code <= 0xDFFF)) {
        return -1;
    }
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
    return n;
}

/*
 * char -> 字符串（v0.98.2）：把一个 Unicode 码点编码成 UTF-8 字节序列，
 * 构造长度为 1~4 字节的字符串。支撑 `str(c)` 与 `str(c) + "suffix"` 这类拼接。
 *
 * 码点校验与 dial_print_char / dial_write_char 完全一致（同一个 dial_encode_utf8）：
 * 负数、> 0x10FFFF、UTF-16 代理区（D800~DFFF）都不是合法标量值，
 * 一律 Fail-Fast——绝不能把它们编码成「 WTF-8」（代理区会被编码成 3 字节序列），
 * 那是无效 UTF-8，会在下游变成乱码。
 */
DialString dial_char_to_string(int64_t code) {
    char buf[5];
    int n = dial_encode_utf8(code, buf);
    if (n < 0) {
        fprintf(stderr, "dial: 非法的 char 码点: %lld\n", (long long)code);
        fflush(stdout);
        dial_assert_fail("dial_char_to_string: 非法 Unicode 码点（合法范围 0~0x10FFFF，且不含 UTF-16 代理区 D800~DFFF）");
    }
    return dial_string_from_bytes(buf, (int64_t)n);
}

/* ==========================================================================
 * v1.04 第 2 批：C1 字符串查询 / 规范化、C4 UTF-8 码点视图、C2/C3 解析格式化
 *
 * 宪法要点：
 *  - to_upper/to_lower 只碰 ASCII，UTF-8 多字节序列逐字节 >= 0x80 不会被改写；
 *  - 码点解码遇到非法序列（overlong / 代理区 / 截断 / >U+10FFFF）当场 Fail-Fast，
 *    绝不静默跳过坏字节或产出 U+FFFD（§20：静默乱码比崩溃危险）；
 *  - length()/get()/substring() 字节语义原样不动，Unicode 全走新增 char_* 方法。
 * ========================================================================== */

DialString dial_string_to_upper(DialString s) {
    int64_t len = ((int64_t *)s)[0];
    void *p = dial_rc_alloc(8 + len + 1);
    ((int64_t *)p)[0] = len;
    const unsigned char *src = (const unsigned char *)s_data(s);
    char *dst = s_data(p);
    for (int64_t i = 0; i < len; i++) {
        unsigned char c = src[i];
        dst[i] = (c >= 'a' && c <= 'z') ? (char)(c - 32) : (char)c;
    }
    dst[len] = '\0';
    return p;
}

DialString dial_string_to_lower(DialString s) {
    int64_t len = ((int64_t *)s)[0];
    void *p = dial_rc_alloc(8 + len + 1);
    ((int64_t *)p)[0] = len;
    const unsigned char *src = (const unsigned char *)s_data(s);
    char *dst = s_data(p);
    for (int64_t i = 0; i < len; i++) {
        unsigned char c = src[i];
        dst[i] = (c >= 'A' && c <= 'Z') ? (char)(c + 32) : (char)c;
    }
    dst[len] = '\0';
    return p;
}

int64_t dial_string_contains(DialString s, DialString sub) {
    return dial_string_index_of(s, sub) >= 0 ? 1 : 0;
}

int64_t dial_string_starts_with(DialString s, DialString prefix) {
    int64_t sl = ((int64_t *)s)[0], pl = ((int64_t *)prefix)[0];
    if (pl > sl) return 0;
    return memcmp(s_data(s), s_data(prefix), (size_t)pl) == 0 ? 1 : 0;
}

int64_t dial_string_ends_with(DialString s, DialString suffix) {
    int64_t sl = ((int64_t *)s)[0], ul = ((int64_t *)suffix)[0];
    if (ul > sl) return 0;
    return memcmp(s_data(s) + (sl - ul), s_data(suffix), (size_t)ul) == 0 ? 1 : 0;
}

/*
 * 严格 UTF-8 解码：从 d[off] 解出一个码点写入 *cp，返回消耗字节数（1~4）。
 * 返回 0 表示已到串尾；返回 -1 表示**非法序列**（调用方必须 Fail-Fast）。
 *
 * 与 dial_encode_utf8 对称：那里是「码点 -> 字节」的唯一实现，这里是「字节 -> 码点」
 * 的唯一实现。四处 char_* 方法共用它，避免各自写一遍解码规则后悄悄失配。
 * 拒绝：0x80~0xC1 作首字节、0xF5~0xFF、overlong、UTF-16 代理区、>U+10FFFF、
 *       续字节不是 10xxxxxx、序列被长度截断。
 */
static int dial_utf8_decode(const unsigned char *d, int64_t len, int64_t off, int64_t *cp) {
    if (off >= len) return 0;
    unsigned char b0 = d[off];
    if (b0 < 0x80) { *cp = b0; return 1; }
    int n;
    int64_t v, min_cp;
    if (b0 >= 0xC2 && b0 <= 0xDF)      { n = 2; v = b0 & 0x1F; min_cp = 0x80; }
    else if (b0 >= 0xE0 && b0 <= 0xEF) { n = 3; v = b0 & 0x0F; min_cp = 0x800; }
    else if (b0 >= 0xF0 && b0 <= 0xF4) { n = 4; v = b0 & 0x07; min_cp = 0x10000; }
    else return -1;
    if (off + n > len) return -1;                 /* 截断 */
    for (int k = 1; k < n; k++) {
        unsigned char bk = d[off + k];
        if ((bk & 0xC0) != 0x80) return -1;      /* 非法续字节 */
        v = (v << 6) | (bk & 0x3F);
    }
    if (v < min_cp) return -1;                    /* overlong */
    if (v > 0x10FFFF) return -1;                  /* 超出 Unicode */
    if (v >= 0xD800 && v <= 0xDFFF) return -1;    /* UTF-16 代理区 */
    *cp = v;
    return n;
}

int64_t dial_string_char_count(DialString s) {
    int64_t slen = ((int64_t *)s)[0];
    const unsigned char *d = (const unsigned char *)s_data(s);
    int64_t off = 0, cnt = 0;
    while (off < slen) {
        int64_t cp;
        int n = dial_utf8_decode(d, slen, off, &cp);
        if (n < 0) dial_assert_fail("char_count: 字符串含非法 UTF-8 序列（Fail-Fast，绝不静默跳过坏字节）");
        off += n; cnt++;
    }
    return cnt;
}

DialArray dial_string_chars(DialString s) {
    int64_t slen = ((int64_t *)s)[0];
    const unsigned char *d = (const unsigned char *)s_data(s);
    DialArray arr = dial_array_new(0 /* INT：码点是标量整数 */);
    int64_t off = 0;
    while (off < slen) {
        int64_t cp;
        int n = dial_utf8_decode(d, slen, off, &cp);
        if (n < 0) dial_assert_fail("chars: 字符串含非法 UTF-8 序列（Fail-Fast，绝不静默跳过坏字节）");
        arr = dial_array_push(arr, cp);
        off += n;
    }
    return arr;
}

int64_t dial_string_char_at(DialString s, int64_t idx) {
    int64_t slen = ((int64_t *)s)[0];
    const unsigned char *d = (const unsigned char *)s_data(s);
    int64_t off = 0, cur = 0;
    while (off < slen) {
        int64_t cp;
        int n = dial_utf8_decode(d, slen, off, &cp);
        if (n < 0) dial_assert_fail("char_at: 字符串含非法 UTF-8 序列（Fail-Fast，绝不静默跳过坏字节）");
        if (cur == idx) return cp;
        off += n; cur++;
    }
    dial_bounds_fail(idx, cur);   /* 越界：当场终止，绝不返回 0 伪装成 U+0000 */
    return 0;
}

DialString dial_string_char_slice(DialString s, int64_t from, int64_t to) {
    int64_t slen = ((int64_t *)s)[0];
    const unsigned char *d = (const unsigned char *)s_data(s);
    if (from < 0 || to < from) {
        dial_assert_fail("char_slice: 半开区间非法（from < 0 或 to < from）");
    }
    /* 一次扫描同时定位 from / to 对应的字节偏移；边界等于码点总数时落在 slen。 */
    int64_t off = 0, idx = 0, byte_from = -1, byte_to = -1;
    while (off < slen) {
        if (idx == from) byte_from = off;
        if (idx == to) byte_to = off;
        int64_t cp;
        int n = dial_utf8_decode(d, slen, off, &cp);
        if (n < 0) dial_assert_fail("char_slice: 字符串含非法 UTF-8 序列（Fail-Fast，绝不静默跳过坏字节）");
        off += n; idx++;
    }
    if (from == idx) byte_from = slen;
    if (to == idx) byte_to = slen;
    if (byte_from < 0 || byte_to < 0) dial_bounds_fail(byte_from < 0 ? from : to, idx);
    return dial_string_substring(s, byte_from, byte_to);
}

/*
 * 严格十进制整数语法：[+-]?[0-9]+，无空白、无尾随字符。
 * parse_int 与 is_int 共用这一份判定——「探测」和「解析」若各写一套规则，
 * 迟早会出现 is_int 放行而 parse_int 崩溃的漂移（§21：唯一真相优于两份近似）。
 * data 区以 '\0' 结尾，strtoll 可安全承接溢出检查（ERANGE）。
 */
static int dial_try_parse_int(const char *d, int64_t len, int64_t *out) {
    if (len <= 0) return 0;
    int64_t i = (d[0] == '+' || d[0] == '-') ? 1 : 0;
    if (i >= len) return 0;
    for (int64_t k = i; k < len; k++) {
        if (d[k] < '0' || d[k] > '9') return 0;
    }
    errno = 0;
    char *end = NULL;
    long long v = strtoll(d, &end, 10);
    if (errno == ERANGE || end != d + len) return 0;
    *out = (int64_t)v;
    return 1;
}

/* 严格浮点语法：[+-]?(dig+ | dig* '.' dig*)([eE][+-]?dig+)?，至少一位数字。
 * 显式拒绝 nan / inf / 0x 十六进制 / 前后空白——strtod 会悄悄接受它们，
 * 那是「解析成功却拿到特殊值」的静默损坏。 */
static int dial_try_parse_float(const char *d, int64_t len, double *out) {
    if (len <= 0) return 0;
    int64_t i = (d[0] == '+' || d[0] == '-') ? 1 : 0;
    int64_t digits = 0;
    while (i < len && d[i] >= '0' && d[i] <= '9') { i++; digits++; }
    if (i < len && d[i] == '.') {
        i++;
        while (i < len && d[i] >= '0' && d[i] <= '9') { i++; digits++; }
    }
    if (digits == 0) return 0;
    if (i < len && (d[i] == 'e' || d[i] == 'E')) {
        i++;
        if (i < len && (d[i] == '+' || d[i] == '-')) i++;
        int64_t ed = 0;
        while (i < len && d[i] >= '0' && d[i] <= '9') { i++; ed++; }
        if (ed == 0) return 0;
    }
    if (i != len) return 0;
    errno = 0;
    char *end = NULL;
    double v = strtod(d, &end);
    /* ERANGE 上溢到 inf 属静默损坏，下溢到 0.0 也一并拒绝（更简单、更严格）。 */
    if (errno == ERANGE || end != d + len) return 0;
    *out = v;
    return 1;
}

int64_t dial_parse_int(DialString s) {
    int64_t out = 0;
    if (!dial_try_parse_int(s_data(s), ((int64_t *)s)[0], &out)) {
        dial_assert_fail("parse_int: 非法整数字符串（要求 [+-]?[0-9]+ 全串、无空白、无溢出）；请先用 is_int(s) 探测");
    }
    return out;
}

double dial_parse_float(DialString s) {
    double out = 0.0;
    if (!dial_try_parse_float(s_data(s), ((int64_t *)s)[0], &out)) {
        dial_assert_fail("parse_float: 非法浮点字符串（要求十进制全串、无空白、无 nan/inf、无溢出）；请先用 is_float(s) 探测");
    }
    return out;
}

int64_t dial_is_int(DialString s) {
    int64_t out = 0;
    return dial_try_parse_int(s_data(s), ((int64_t *)s)[0], &out) ? 1 : 0;
}

int64_t dial_is_float(DialString s) {
    double out = 0.0;
    return dial_try_parse_float(s_data(s), ((int64_t *)s)[0], &out) ? 1 : 0;
}

DialString dial_format_float(double v, int64_t digits) {
    if (digits < 0 || digits > 9) {
        dial_assert_fail("format_float: 小数位数必须是 0~9 的 int（拒绝 printf 式任意格式串）");
    }
    char buf[512];
    int n = snprintf(buf, sizeof(buf), "%.*f", (int)digits, v);
    if (n < 0 || (int64_t)n >= (int64_t)sizeof(buf)) {
        dial_assert_fail("format_float: 格式化结果超出缓冲区");
    }
    return dial_string_from_bytes(buf, (int64_t)n);
}
