#include "dial_string.h"
#include "dial_array.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>
#include <stdio.h>

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
