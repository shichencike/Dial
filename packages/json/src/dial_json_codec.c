/*
 * dial_json_codec.c —— 自研 JSON 解析器与紧凑序列化器（纯 C，零依赖）。
 *
 * 严格模式（不做「宽容解析」）：尾随逗号、多余根值、未转义控制字符、
 * 非法 \u 转义、孤立代理项一律报错并给出字节偏移——与项目 Fail-Fast 铁律一致，
 * 绝不静默吞下坏数据。深度上限 64 防栈溢出。
 *
 * 数字分两路存：整数进 int64_t ival、浮点进 double num（见 dial_json.h），
 * 避免 double 在 >2^53 时把整数悄悄改掉。
 */
#include "dial_json.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define JS_MAX_DEPTH 64

/* ---------------------------------------------------------------- 动态缓冲 */

typedef struct {
    char *p;
    int   len, cap;
    int   oom;
} Buf;

static void buf_init(Buf *b) { b->p = NULL; b->len = 0; b->cap = 0; b->oom = 0; }

static void buf_put(Buf *b, const char *data, int n) {
    if (b->oom || n <= 0) return;
    if (b->len + n + 1 > b->cap) {
        int nc = b->cap ? b->cap * 2 : 64;
        while (nc < b->len + n + 1) nc *= 2;
        char *np = (char *)realloc(b->p, (size_t)nc);
        if (!np) { b->oom = 1; return; }
        b->p = np; b->cap = nc;
    }
    memcpy(b->p + b->len, data, (size_t)n);
    b->len += n;
    b->p[b->len] = '\0';
}
static void buf_puts(Buf *b, const char *s) { buf_put(b, s, (int)strlen(s)); }
static void buf_putc(Buf *b, char c)       { buf_put(b, &c, 1); }

static char *buf_finish(Buf *b) {
    if (b->oom || !b->p) { free(b->p); return NULL; }
    return b->p;   /* 所有权交给调用方 */
}

/* ---------------------------------------------------------------- 节点工具 */

static JsonNode *node_new(JsonType t) {
    JsonNode *n = (JsonNode *)calloc(1, sizeof(JsonNode));
    if (n) n->type = t;
    return n;
}

static void arr_push(JsonNode *a, JsonNode *child) {
    if (a->nitems >= a->icap) {
        int nc = a->icap ? a->icap * 2 : 8;
        JsonNode **np = (JsonNode **)realloc(a->items, (size_t)nc * sizeof(JsonNode *));
        if (!np) return;
        a->items = np; a->icap = nc;
    }
    a->items[a->nitems++] = child;
}

static void obj_put(JsonNode *o, char *key, JsonNode *val) {
    /* 重复键：按 JSON 常见实现，后者覆盖前者 */
    for (int i = 0; i < o->nkeys; i++) {
        if (strcmp(o->keys[i], key) == 0) {
            free(key);
            js_free(o->vals[i]);
            o->vals[i] = val;
            return;
        }
    }
    if (o->nkeys >= o->kcap) {
        int nc = o->kcap ? o->kcap * 2 : 8;
        char **nk = (char **)realloc(o->keys, (size_t)nc * sizeof(char *));
        JsonNode **nv = (JsonNode **)realloc(o->vals, (size_t)nc * sizeof(JsonNode *));
        if (!nk || !nv) { free(nk); free(nv); free(key); js_free(val); return; }
        o->keys = nk; o->vals = nv; o->kcap = nc;
    }
    o->keys[o->nkeys] = key;
    o->vals[o->nkeys] = val;
    o->nkeys++;
}

void js_free(JsonNode *n) {
    if (!n) return;
    free(n->str);
    for (int i = 0; i < n->nitems; i++) js_free(n->items[i]);
    free(n->items);
    for (int i = 0; i < n->nkeys; i++) { free(n->keys[i]); js_free(n->vals[i]); }
    free(n->keys);
    free(n->vals);
    free(n);
}

JsonNode *js_clone(const JsonNode *n) {
    if (!n) return NULL;
    JsonNode *c = node_new(n->type);
    if (!c) return NULL;
    c->bval = n->bval; c->num = n->num; c->ival = n->ival; c->is_int = n->is_int;
    if (n->str) {
        size_t l = strlen(n->str) + 1;
        c->str = (char *)malloc(l);
        if (c->str) memcpy(c->str, n->str, l);
    }
    for (int i = 0; i < n->nitems; i++) arr_push(c, js_clone(n->items[i]));
    for (int i = 0; i < n->nkeys; i++) {
        size_t l = strlen(n->keys[i]) + 1;
        char *k = (char *)malloc(l);
        if (k) { memcpy(k, n->keys[i], l); obj_put(c, k, js_clone(n->vals[i])); }
    }
    return c;
}

/* ---------------------------------------------------------------- 解析器 */

typedef struct {
    const char *s;
    int         len, pos;
    char       *err;
    int         errlen;
    int        *errpos;
    int         failed;
} JP;

static void jerr(JP *p, const char *msg) {
    if (p->failed) return;
    p->failed = 1;
    snprintf(p->err, (size_t)p->errlen, "%s", msg);
    if (p->errpos) *p->errpos = p->pos;
}
static void skip_ws(JP *p) {
    while (p->pos < p->len) {
        char c = p->s[p->pos];
        if (c == ' ' || c == '\t' || c == '\n' || c == '\r') p->pos++;
        else break;
    }
}
static int hexval(int c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}
static int read_hex4(JP *p, uint32_t *out) {
    if (p->pos + 4 > p->len) { jerr(p, "\\u 转义不完整（不足 4 位十六进制）"); return 0; }
    uint32_t v = 0;
    for (int i = 0; i < 4; i++) {
        int h = hexval((unsigned char)p->s[p->pos + i]);
        if (h < 0) { jerr(p, "\\u 转义含非法十六进制字符"); return 0; }
        v = v * 16 + (uint32_t)h;
    }
    p->pos += 4;
    *out = v;
    return 1;
}
static void utf8_put(Buf *b, uint32_t cp) {
    if (cp < 0x80) {
        buf_putc(b, (char)cp);
    } else if (cp < 0x800) {
        buf_putc(b, (char)(0xC0 | (cp >> 6)));
        buf_putc(b, (char)(0x80 | (cp & 0x3F)));
    } else if (cp < 0x10000) {
        buf_putc(b, (char)(0xE0 | (cp >> 12)));
        buf_putc(b, (char)(0x80 | ((cp >> 6) & 0x3F)));
        buf_putc(b, (char)(0x80 | (cp & 0x3F)));
    } else {
        buf_putc(b, (char)(0xF0 | (cp >> 18)));
        buf_putc(b, (char)(0x80 | ((cp >> 12) & 0x3F)));
        buf_putc(b, (char)(0x80 | ((cp >> 6) & 0x3F)));
        buf_putc(b, (char)(0x80 | (cp & 0x3F)));
    }
}

/* 解析 JSON 字符串，返回 malloc 的 UTF-8 文本（已去掉引号）。 */
static char *parse_string(JP *p) {
    if (p->pos >= p->len || p->s[p->pos] != '"') { jerr(p, "期望字符串（双引号）"); return NULL; }
    p->pos++;
    Buf b; buf_init(&b);
    while (p->pos < p->len) {
        unsigned char c = (unsigned char)p->s[p->pos++];
        if (c == '"') {
            /* 空串时 b.p 还是 NULL：必须返回可 free 的堆串，绝不能返回字符串
               字面量（后面 js_free 会 free 它）。也不能重复调用 buf_finish。 */
            if (b.oom) { free(b.p); jerr(p, "内存不足"); return NULL; }
            char *out = buf_finish(&b);
            if (!out) {
                out = (char *)malloc(1);
                if (out) out[0] = '\0';
                else { jerr(p, "内存不足"); return NULL; }
            }
            return out;
        }
        if (c == '\\') {
            if (p->pos >= p->len) { jerr(p, "字符串末尾是孤立的 '\\'"); free(b.p); return NULL; }
            int e = (unsigned char)p->s[p->pos++];
            switch (e) {
                case '"':  buf_putc(&b, '"');  break;
                case '\\': buf_putc(&b, '\\'); break;
                case '/':  buf_putc(&b, '/');  break;
                case 'b':  buf_putc(&b, '\b'); break;
                case 'f':  buf_putc(&b, '\f'); break;
                case 'n':  buf_putc(&b, '\n'); break;
                case 'r':  buf_putc(&b, '\r'); break;
                case 't':  buf_putc(&b, '\t'); break;
                case 'u': {
                    uint32_t cp;
                    if (!read_hex4(p, &cp)) { free(b.p); return NULL; }
                    if (cp >= 0xD800 && cp <= 0xDBFF) {
                        /* 高位代理：必须紧跟 \u 低位代理 */
                        if (p->pos + 2 > p->len || p->s[p->pos] != '\\' || p->s[p->pos + 1] != 'u') {
                            jerr(p, "高位代理项后缺少低位代理项"); free(b.p); return NULL;
                        }
                        p->pos += 2;
                        uint32_t lo;
                        if (!read_hex4(p, &lo)) { free(b.p); return NULL; }
                        if (lo < 0xDC00 || lo > 0xDFFF) {
                            jerr(p, "代理项对的低位不合法"); free(b.p); return NULL;
                        }
                        cp = 0x10000u + ((cp - 0xD800u) << 10) + (lo - 0xDC00u);
                    } else if (cp >= 0xDC00 && cp <= 0xDFFF) {
                        jerr(p, "出现孤立的低位代理项"); free(b.p); return NULL;
                    }
                    if (cp == 0) {
                        /* 内嵌 NUL 会让节点的 char* 被静默截断——Fail-Fast 明确拒绝，
                           绝不产生「看起来解析成功、内容少了一截」的坏数据。 */
                        jerr(p, "不支持 \\u0000（内嵌 NUL 无法用 C 字符串表示）");
                        free(b.p); return NULL;
                    }
                    utf8_put(&b, cp);
                    break;
                }
                default:
                    jerr(p, "字符串中含无法识别的转义序列"); free(b.p); return NULL;
            }
            continue;
        }
        if (c < 0x20) { jerr(p, "字符串中出现未转义的控制字符"); free(b.p); return NULL; }
        buf_putc(&b, (char)c);       /* UTF-8 多字节逐字节透传 */
    }
    jerr(p, "字符串未闭合（缺少右双引号）");
    free(b.p);
    return NULL;
}

static JsonNode *parse_value(JP *p, int depth);

static JsonNode *parse_number(JP *p) {
    int start = p->pos;
    if (p->pos < p->len && p->s[p->pos] == '-') p->pos++;
    if (p->pos >= p->len) { jerr(p, "数字不完整"); return NULL; }
    if (p->s[p->pos] == '0') {
        p->pos++;
    } else if (p->s[p->pos] >= '1' && p->s[p->pos] <= '9') {
        while (p->pos < p->len && p->s[p->pos] >= '0' && p->s[p->pos] <= '9') p->pos++;
    } else {
        jerr(p, "数字格式非法"); return NULL;
    }
    int is_int = 1;
    if (p->pos < p->len && p->s[p->pos] == '.') {
        is_int = 0; p->pos++;
        if (p->pos >= p->len || p->s[p->pos] < '0' || p->s[p->pos] > '9') {
            jerr(p, "小数点后缺少数字"); return NULL;
        }
        while (p->pos < p->len && p->s[p->pos] >= '0' && p->s[p->pos] <= '9') p->pos++;
    }
    if (p->pos < p->len && (p->s[p->pos] == 'e' || p->s[p->pos] == 'E')) {
        is_int = 0; p->pos++;
        if (p->pos < p->len && (p->s[p->pos] == '+' || p->s[p->pos] == '-')) p->pos++;
        if (p->pos >= p->len || p->s[p->pos] < '0' || p->s[p->pos] > '9') {
            jerr(p, "指数部分缺少数字"); return NULL;
        }
        while (p->pos < p->len && p->s[p->pos] >= '0' && p->s[p->pos] <= '9') p->pos++;
    }
    int n = p->pos - start;
    char *tmp = (char *)malloc((size_t)n + 1);
    if (!tmp) { jerr(p, "内存不足"); return NULL; }
    memcpy(tmp, p->s + start, (size_t)n);
    tmp[n] = '\0';

    JsonNode *node = node_new(JS_NUM);
    if (!node) { free(tmp); jerr(p, "内存不足"); return NULL; }
    if (is_int) {
        /* 整数走 strtoll，保住 >2^53 的精度；溢出则退化为 double */
        char *end = NULL;
        long long v = strtoll(tmp, &end, 10);
        if (end && *end == '\0') {
            node->ival = (int64_t)v;
            node->num = (double)v;
            node->is_int = 1;
        } else {
            node->num = strtod(tmp, NULL);
            node->is_int = 0;
        }
    } else {
        node->num = strtod(tmp, NULL);
        node->is_int = 0;
    }
    free(tmp);
    return node;
}

static JsonNode *parse_array(JP *p, int depth) {
    p->pos++;                                   /* '[' */
    JsonNode *a = node_new(JS_ARR);
    if (!a) { jerr(p, "内存不足"); return NULL; }
    skip_ws(p);
    if (p->pos < p->len && p->s[p->pos] == ']') { p->pos++; return a; }
    while (!p->failed) {
        skip_ws(p);
        JsonNode *v = parse_value(p, depth + 1);
        if (!v) { js_free(a); return NULL; }
        arr_push(a, v);
        skip_ws(p);
        if (p->pos < p->len && p->s[p->pos] == ',') { p->pos++; continue; }
        if (p->pos < p->len && p->s[p->pos] == ']') { p->pos++; return a; }
        jerr(p, "数组里期望 ',' 或 ']'");
        break;
    }
    js_free(a);
    return NULL;
}

static JsonNode *parse_object(JP *p, int depth) {
    p->pos++;                                   /* '{' */
    JsonNode *o = node_new(JS_OBJ);
    if (!o) { jerr(p, "内存不足"); return NULL; }
    skip_ws(p);
    if (p->pos < p->len && p->s[p->pos] == '}') { p->pos++; return o; }
    while (!p->failed) {
        skip_ws(p);
        char *k = parse_string(p);
        if (!k) { js_free(o); return NULL; }
        skip_ws(p);
        if (p->pos >= p->len || p->s[p->pos] != ':') {
            jerr(p, "对象键后缺少 ':'"); free(k); js_free(o); return NULL;
        }
        p->pos++;
        JsonNode *v = parse_value(p, depth + 1);
        if (!v) { free(k); js_free(o); return NULL; }
        obj_put(o, k, v);
        skip_ws(p);
        if (p->pos < p->len && p->s[p->pos] == ',') { p->pos++; continue; }
        if (p->pos < p->len && p->s[p->pos] == '}') { p->pos++; return o; }
        jerr(p, "对象里期望 ',' 或 '}'");
        break;
    }
    js_free(o);
    return NULL;
}

static JsonNode *parse_value(JP *p, int depth) {
    if (depth > JS_MAX_DEPTH) { jerr(p, "JSON 嵌套深度超过 64 层"); return NULL; }
    skip_ws(p);
    if (p->pos >= p->len) { jerr(p, "JSON 意外的结尾"); return NULL; }
    char c = p->s[p->pos];
    switch (c) {
        case 'n':
            if (p->pos + 4 <= p->len && memcmp(p->s + p->pos, "null", 4) == 0) {
                p->pos += 4; return node_new(JS_NULL);
            }
            jerr(p, "无法识别的字面量（期望 null）"); return NULL;
        case 't':
            if (p->pos + 4 <= p->len && memcmp(p->s + p->pos, "true", 4) == 0) {
                p->pos += 4;
                JsonNode *n = node_new(JS_BOOL); if (n) n->bval = 1; return n;
            }
            jerr(p, "无法识别的字面量（期望 true）"); return NULL;
        case 'f':
            if (p->pos + 5 <= p->len && memcmp(p->s + p->pos, "false", 5) == 0) {
                p->pos += 5; return node_new(JS_BOOL);
            }
            jerr(p, "无法识别的字面量（期望 false）"); return NULL;
        case '"': {
            char *s = parse_string(p);
            if (!s) return NULL;
            JsonNode *n = node_new(JS_STR);
            if (!n) { free(s); jerr(p, "内存不足"); return NULL; }
            n->str = s;
            return n;
        }
        case '[': return parse_array(p, depth);
        case '{': return parse_object(p, depth);
        default:
            if (c == '-' || (c >= '0' && c <= '9')) return parse_number(p);
            jerr(p, "无法解析的值（期望 null/true/false/数字/字符串/数组/对象）");
            return NULL;
    }
}

JsonNode *js_parse(const char *text, int len, char *err, int errlen, int *errpos) {
    if (err && errlen > 0) err[0] = '\0';
    if (errpos) *errpos = -1;
    if (!text || len < 0) { snprintf(err, (size_t)errlen, "JSON 输入为空"); return NULL; }
    JP p;
    memset(&p, 0, sizeof(p));
    p.s = text; p.len = len; p.pos = 0;
    p.err = err; p.errlen = errlen; p.errpos = errpos;

    JsonNode *root = parse_value(&p, 0);
    if (p.failed) { js_free(root); return NULL; }
    skip_ws(&p);
    if (p.pos < p.len) {
        jerr(&p, "JSON 根值之后有多余内容");
        js_free(root);
        return NULL;
    }
    return root;
}

/* ---------------------------------------------------------------- 序列化 */

static void esc_string(Buf *b, const char *s) {
    buf_putc(b, '"');
    for (const unsigned char *q = (const unsigned char *)s; *q; q++) {
        unsigned char c = *q;
        switch (c) {
            case '"':  buf_puts(b, "\\\""); break;
            case '\\': buf_puts(b, "\\\\"); break;
            case '\b': buf_puts(b, "\\b");  break;
            case '\f': buf_puts(b, "\\f");  break;
            case '\n': buf_puts(b, "\\n");  break;
            case '\r': buf_puts(b, "\\r");  break;
            case '\t': buf_puts(b, "\\t");  break;
            default:
                if (c < 0x20) {
                    char tmp[8];
                    snprintf(tmp, sizeof(tmp), "\\u%04x", (unsigned)c);
                    buf_puts(b, tmp);
                } else {
                    buf_putc(b, (char)c);     /* UTF-8 原样输出，不做 \u 转义 */
                }
        }
    }
    buf_putc(b, '"');
}

static void stringify_into(Buf *b, const JsonNode *n) {
    if (!n) { buf_puts(b, "null"); return; }
    switch (n->type) {
        case JS_NULL: buf_puts(b, "null"); break;
        case JS_BOOL: buf_puts(b, n->bval ? "true" : "false"); break;
        case JS_NUM: {
            char tmp[64];
            if (n->is_int) snprintf(tmp, sizeof(tmp), "%lld", (long long)n->ival);
            else           snprintf(tmp, sizeof(tmp), "%.17g", n->num);
            buf_puts(b, tmp);
            break;
        }
        case JS_STR: esc_string(b, n->str ? n->str : ""); break;
        case JS_ARR:
            buf_putc(b, '[');
            for (int i = 0; i < n->nitems; i++) {
                if (i) buf_putc(b, ',');
                stringify_into(b, n->items[i]);
            }
            buf_putc(b, ']');
            break;
        case JS_OBJ:
            buf_putc(b, '{');
            for (int i = 0; i < n->nkeys; i++) {
                if (i) buf_putc(b, ',');
                esc_string(b, n->keys[i] ? n->keys[i] : "");
                buf_putc(b, ':');
                stringify_into(b, n->vals[i]);
            }
            buf_putc(b, '}');
            break;
        default: buf_puts(b, "null"); break;
    }
}

char *js_stringify(const JsonNode *n) {
    Buf b; buf_init(&b);
    stringify_into(&b, n);
    return buf_finish(&b);
}
