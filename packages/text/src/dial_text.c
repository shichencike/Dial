/*
 * dial_text.c —— text.dialpkg 的 FFI 包装层。
 *
 * 正则引擎在 dial_regex.c（纯 C、不依赖 Dial stdlib，可独立验证）。本文件只做
 * 三件事：DialString ↔ C 字节串的转换、不透明句柄的生命周期、线程局部错误槽。
 * 协议/算法层自研，不引入任何第三方依赖（README §21.2「不引入无用依赖」）。
 */
#include "dial_text.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static _Thread_local int64_t g_err = DIAL_TEXT_OK;
static _Thread_local char    g_err_msg[256] = {0};

static void tx_set_error(int64_t code, const char *msg) {
    g_err = code;
    if (!msg) msg = "";
    strncpy(g_err_msg, msg, sizeof(g_err_msg) - 1);
    g_err_msg[sizeof(g_err_msg) - 1] = '\0';
}
static void tx_ok(void) { tx_set_error(DIAL_TEXT_OK, ""); }

int64_t dial_text_last_error(void) { return g_err; }

DialString dial_text_last_error_msg(void) {
    return dial_string_new(g_err_msg[0] ? g_err_msg : "无错误");
}

/* ---------------------------------------------------------------- 小工具 */

/* 可增长字节缓冲：replace_all 要拼任意长度的中间结果。 */
typedef struct {
    char   *p;
    int64_t len, cap;
    int     oom;
} TxBuf;

static void tx_buf_init(TxBuf *b) {
    b->p = NULL; b->len = 0; b->cap = 0; b->oom = 0;
}
static void tx_buf_put(TxBuf *b, const char *data, int64_t n) {
    if (b->oom || n <= 0) return;
    if (b->len + n + 1 > b->cap) {
        int64_t nc = b->cap ? b->cap * 2 : 128;
        while (nc < b->len + n + 1) nc *= 2;
        char *np = (char *)realloc(b->p, (size_t)nc);
        if (!np) { b->oom = 1; return; }
        b->p = np; b->cap = nc;
    }
    memcpy(b->p + b->len, data, (size_t)n);
    b->len += n;
    b->p[b->len] = '\0';
}
static DialString tx_buf_finish(TxBuf *b) {
    if (b->oom || !b->p) {
        free(b->p);
        tx_set_error(DIAL_TEXT_ERR_NOMEM, "内存不足：构造结果字符串失败");
        return dial_string_new("");
    }
    DialString s = dial_string_from_bytes(b->p, b->len);
    free(b->p);
    return s;
}

/* 从 DialString 取 (字节指针, 长度)：允许内嵌 NUL，一律按长度处理。 */
static const char *tx_data(DialString s, int64_t *len) {
    if (!s) { *len = 0; return ""; }
    *len = dial_string_length(s);
    return dial_string_cstr(s);
}

/* ---------------------------------------------------------------- 生命周期 */

DialRegex *dial_text_regex_compile(DialString pattern) {
    int64_t plen = 0;
    const char *pat = tx_data(pattern, &plen);
    DialRegex *r = (DialRegex *)calloc(1, sizeof(DialRegex));
    if (!r) {
        tx_set_error(DIAL_TEXT_ERR_NOMEM, "内存不足：无法分配正则句柄");
        return NULL;
    }
    if (!rx_compile(&r->prog, pat, (int)plen)) {
        tx_set_error(DIAL_TEXT_ERR_COMPILE, r->prog.err);
        free(r);
        return NULL;
    }
    r->has_last = 0;
    tx_ok();
    return r;
}

int32_t dial_text_regex_is_valid(DialString pattern) {
    int64_t plen = 0;
    const char *pat = tx_data(pattern, &plen);
    RxProg tmp;
    if (!rx_compile(&tmp, pat, (int)plen)) {
        tx_set_error(DIAL_TEXT_ERR_COMPILE, tmp.err);
        return 0;
    }
    tx_ok();
    return 1;
}

int64_t dial_text_regex_free(DialRegex *r) {
    if (!r) { tx_set_error(DIAL_TEXT_ERR_NULL, "空正则句柄"); return -1; }
    free(r);
    tx_ok();
    return 0;
}

/* 统一跑一次搜索并缓存结果。返回 1 命中 / 0 未命中。 */
static int tx_search(DialRegex *r, DialString text) {
    if (!r) {
        tx_set_error(DIAL_TEXT_ERR_NULL, "空正则句柄");
        return 0;
    }
    int64_t tlen = 0;
    const char *t = tx_data(text, &tlen);
    r->has_last = 0;
    if (!rx_search(&r->prog, t, (int)tlen, 0, &r->last)) {
        tx_ok();                       /* 没匹配不是错误 */
        return 0;
    }
    r->has_last = 1;
    tx_ok();
    return 1;
}

int32_t dial_text_regex_is_match(DialRegex *r, DialString text) {
    return tx_search(r, text) ? 1 : 0;
}

int64_t dial_text_regex_find(DialRegex *r, DialString text) {
    if (!tx_search(r, text)) return -1;
    return (int64_t)r->last.start;
}

int64_t dial_text_regex_match_end(DialRegex *r) {
    if (!r || !r->has_last) {
        tx_set_error(DIAL_TEXT_ERR_NULL, "尚无匹配结果：请先调用 find / is_match");
        return -1;
    }
    tx_ok();
    return (int64_t)r->last.end;
}

int64_t dial_text_regex_group_count(DialRegex *r) {
    if (!r) { tx_set_error(DIAL_TEXT_ERR_NULL, "空正则句柄"); return -1; }
    tx_ok();
    return (int64_t)r->prog.ngroups;
}

DialString dial_text_regex_group(DialRegex *r, DialString text, int64_t index) {
    if (!r || !r->has_last) {
        tx_set_error(DIAL_TEXT_ERR_NULL, "尚无匹配结果：请先调用 find / is_match");
        return dial_string_new("");
    }
    if (index < 0 || index > (int64_t)r->prog.ngroups) {
        tx_set_error(DIAL_TEXT_ERR_RANGE, "捕获组序号越界");
        return dial_string_new("");
    }
    int a, b;
    if (index == 0) {
        a = r->last.start; b = r->last.end;
    } else {
        a = r->last.cap[2 * (int)index];
        b = r->last.cap[2 * (int)index + 1];
    }
    if (a < 0 || b < a) { tx_ok(); return dial_string_new(""); }

    int64_t tlen = 0;
    const char *t = tx_data(text, &tlen);
    if (a > tlen || b > tlen) {
        tx_set_error(DIAL_TEXT_ERR_RANGE, "捕获区间超出文本长度（text 与匹配时不一致？）");
        return dial_string_new("");
    }
    tx_ok();
    return dial_string_from_bytes(t + a, (int64_t)(b - a));
}

/* ---------------------------------------------------------------- 替换 */

/* 展开 replacement 里的 $0..$9 与 $$，追加到 dst。 */
static void tx_expand(const char *text, int64_t textlen, const RxMatch *m,
                      const char *repl, int64_t rlen, TxBuf *dst) {
    int64_t i = 0;
    while (i < rlen) {
        if (repl[i] != '$' || i + 1 >= rlen) {
            tx_buf_put(dst, repl + i, 1);
            i++;
            continue;
        }
        char c = repl[i + 1];
        if (c == '$') {
            tx_buf_put(dst, "$", 1);
            i += 2;
            continue;
        }
        if (c >= '0' && c <= '9') {
            int g = c - '0';
            int a, b;
            if (g == 0) { a = m->start; b = m->end; }
            else {
                a = m->cap[2 * g];
                b = m->cap[2 * g + 1];
            }
            if (a >= 0 && b >= a && b <= textlen)
                tx_buf_put(dst, text + a, (int64_t)(b - a));
            i += 2;
            continue;
        }
        tx_buf_put(dst, repl + i, 1);   /* $ 后不是数字，按字面 '$' 处理 */
        i++;
    }
}

DialString dial_text_regex_replace_all(DialRegex *r, DialString text, DialString replacement) {
    if (!r) {
        tx_set_error(DIAL_TEXT_ERR_NULL, "空正则句柄");
        return dial_string_new("");
    }
    int64_t tlen = 0, rlen = 0;
    const char *t = tx_data(text, &tlen);
    const char *rp = tx_data(replacement, &rlen);

    TxBuf out;
    tx_buf_init(&out);
    int64_t pos = 0;
    RxMatch m;
    int guard = 0;
    while (pos <= tlen && guard++ < 1000000) {
        if (!rx_search(&r->prog, t, (int)tlen, (int)pos, &m)) break;
        if (m.start > pos) tx_buf_put(&out, t + pos, (int64_t)(m.start - pos));
        tx_expand(t, tlen, &m, rp, rlen, &out);
        if (m.end == m.start) {
            /* 零宽匹配：吞掉一个字节，避免原地死循环 */
            if (m.end < tlen) tx_buf_put(&out, t + m.end, 1);
            pos = (int64_t)m.end + 1;
        } else {
            pos = (int64_t)m.end;
        }
        r->last = m;
        r->has_last = 1;
    }
    if (pos < tlen) tx_buf_put(&out, t + pos, tlen - pos);
    return tx_buf_finish(&out);
}
