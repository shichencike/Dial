/*
 * dial_regex.c —— Thompson NFA 编译器 + Pike VM 执行器。
 *
 * 编译：递归下降把正则构造成「虚拟机指令」，每条跳转都显式写目标下标
 *   （见 dial_regex.h 的 RxInst::next），因此分支指令可以在子片段之后发射。
 * 执行：Pike VM 按优先级维护并行线程；遇到 MATCH 截断更低优先级线程，
 *   于是自然得到「左分支优先 + 贪婪最长」，且没有回溯引擎的指数爆炸。
 * 搜索：从每个候选起点重跑一次 VM，第一个命中的起点即为最左匹配。
 *   这保证 ^ 锚语义（起点 > 0 时 BOL 直接失败），代价是最坏 O(n^2·m)——
 *   对配置 / 日志行这类短文本足够，边界已写进 text.diali。
 */
#include "dial_regex.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---------------------------------------------------------------- 位集合 */

static void bits_clear(uint8_t *b) { memset(b, 0, 32); }
static void bits_all(uint8_t *b)   { memset(b, 0xff, 32); }
static void bits_set(uint8_t *b, int c) {
    if (c >= 0 && c < 256) b[c >> 3] |= (uint8_t)(1u << (c & 7));
}
static void bits_clear_one(uint8_t *b, int c) {
    if (c >= 0 && c < 256) b[c >> 3] &= (uint8_t)~(1u << (c & 7));
}
static int bits_has(const uint8_t *b, int c) {
    return c >= 0 && c < 256 && (b[c >> 3] & (1u << (c & 7))) != 0;
}
static void bits_or(uint8_t *dst, const uint8_t *src) {
    for (int i = 0; i < 32; i++) dst[i] |= src[i];
}

static int is_word(int c) {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
           (c >= '0' && c <= '9') || c == '_';
}

/* 预定义类：k ∈ {d,D,w,W,s,S}；add=1 表示并入已有集合，否则覆盖。 */
static void bits_predef(uint8_t *b, int k, int add) {
    uint8_t t[32];
    bits_clear(t);
    switch (k) {
        case 'd':
            for (int c = '0'; c <= '9'; c++) bits_set(t, c);
            break;
        case 'D':
            bits_all(t);
            for (int c = '0'; c <= '9'; c++) bits_clear_one(t, c);
            break;
        case 'w':
            for (int c = 0; c < 256; c++) if (is_word(c)) bits_set(t, c);
            break;
        case 'W':
            bits_all(t);
            for (int c = 0; c < 256; c++) if (is_word(c)) bits_clear_one(t, c);
            break;
        case 's':
            bits_set(t, ' ');  bits_set(t, '\t'); bits_set(t, '\n');
            bits_set(t, '\r'); bits_set(t, '\f'); bits_set(t, '\v');
            break;
        case 'S':
            bits_all(t);
            bits_clear_one(t, ' ');  bits_clear_one(t, '\t'); bits_clear_one(t, '\n');
            bits_clear_one(t, '\r'); bits_clear_one(t, '\f'); bits_clear_one(t, '\v');
            break;
        default:
            break;
    }
    if (add) bits_or(b, t);
    else memcpy(b, t, 32);
}

static int escape_byte(int e) {
    switch (e) {
        case 'n': return '\n';
        case 't': return '\t';
        case 'r': return '\r';
        case 'f': return '\f';
        case 'v': return '\v';
        case 'a': return '\a';
        case 'b': return '\b';   /* 字符类内 \b 是退格；类外是词边界 */
        case '0': return 0;
        default:  return e;      /* 其余元字符转义为其本身 */
    }
}

/* ---------------------------------------------------------------- 编译器 */

/* out 列表编码：idx*3 + field（field 0=x / 1=y / 2=next） */
typedef struct {
    int v[RX_MAX_INST * 2];
    int n;
} OutList;

typedef struct {
    int start;
    OutList outs;
} Frag;

typedef struct {
    RxProg     *prog;
    const char *pat;
    int         len, pos;
    int         ngroup;
    int         failed;
} Parser;

static void set_err(Parser *p, const char *msg) {
    if (!p->failed) {
        p->failed = 1;
        snprintf(p->prog->err, sizeof(p->prog->err), "%s", msg);
    }
}

static int emit(Parser *p, RxOp op) {
    if (p->prog->ninst >= RX_MAX_INST) {
        set_err(p, "正则过长：指令数超出上限");
        return -1;
    }
    int i = p->prog->ninst++;
    RxInst *in = &p->prog->inst[i];
    memset(in, 0, sizeof(*in));
    in->op = op;
    in->x = in->y = in->next = -1;
    return i;
}

static void out_add(OutList *o, int idx, int field) {
    if (idx >= 0 && o->n < (int)(sizeof(o->v) / sizeof(o->v[0])))
        o->v[o->n++] = idx * 3 + field;
}

static void out_join(OutList *a, const OutList *b) {
    for (int i = 0; i < b->n; i++) out_add(a, b->v[i] / 3, b->v[i] % 3);
}

static void patch(Parser *p, const OutList *o, int target) {
    for (int i = 0; i < o->n; i++) {
        int idx = o->v[i] / 3, f = o->v[i] % 3;
        if (idx < 0 || idx >= p->prog->ninst) continue;
        if (f == 0)      p->prog->inst[idx].x = target;
        else if (f == 1) p->prog->inst[idx].y = target;
        else             p->prog->inst[idx].next = target;
    }
}

static Frag frag_of(int start, int idx, int field) {
    Frag f;
    f.start = start;
    f.outs.n = 0;
    out_add(&f.outs, idx, field);
    return f;
}

static Frag parse_alt(Parser *p);

static int peek(Parser *p) {
    return p->pos < p->len ? (unsigned char)p->pat[p->pos] : -1;
}
static int peek2(Parser *p, int off) {
    return p->pos + off < p->len ? (unsigned char)p->pat[p->pos + off] : -1;
}

/* [abc] [a-z] [^...] [\d\s...] */
static Frag parse_class(Parser *p) {
    uint8_t bits[32];
    bits_clear(bits);
    int neg = 0;
    if (peek(p) == '^') { neg = 1; p->pos++; }
    int first = 1;
    while (!p->failed) {
        int c = peek(p);
        if (c < 0) { set_err(p, "字符类缺少右括号 ']'"); break; }
        if (c == ']' && !first) { p->pos++; break; }
        first = 0;
        int lo;
        if (c == '\\') {
            p->pos++;
            int e = peek(p);
            if (e < 0) { set_err(p, "字符类末尾是孤立的 '\\'"); break; }
            p->pos++;
            if (e == 'd' || e == 'D' || e == 'w' || e == 'W' || e == 's' || e == 'S') {
                bits_predef(bits, e, 1);
                continue;
            }
            lo = escape_byte(e);
        } else {
            lo = c;
            p->pos++;
        }
        /* 范围 a-z（'-' 后不是 ']' 且没到末尾才算范围） */
        if (peek(p) == '-' && peek2(p, 1) != ']' && peek2(p, 1) >= 0) {
            p->pos++;
            int hi;
            if (peek(p) == '\\') {
                p->pos++;
                int e = peek(p);
                if (e < 0) { set_err(p, "字符范围末尾是孤立的 '\\'"); break; }
                p->pos++;
                hi = escape_byte(e);
            } else {
                hi = peek(p);
                p->pos++;
            }
            if (hi < lo) { set_err(p, "字符范围逆序（如 z-a）"); break; }
            for (int k = lo; k <= hi && k < 256; k++) bits_set(bits, k);
        } else {
            bits_set(bits, lo);
        }
    }
    if (neg) {
        for (int i = 0; i < 32; i++) bits[i] = (uint8_t)~bits[i];
    }
    int i = emit(p, RX_CLASS);
    if (i >= 0) memcpy(p->prog->inst[i].bits, bits, 32);
    return frag_of(i, i, 2);
}

static Frag parse_atom(Parser *p) {
    int c = peek(p);
    if (p->failed) return frag_of(-1, -1, 2);
    if (c < 0) { set_err(p, "正则末尾缺少内容"); return frag_of(-1, -1, 2); }
    p->pos++;

    switch (c) {
        case '(': {
            int cap = -1;
            if (peek(p) == '?' && peek2(p, 1) == ':') {
                p->pos += 2;                       /* (?:...) 非捕获 */
            } else {
                if (p->ngroup >= RX_MAX_GROUPS) {
                    set_err(p, "捕获组超过 9 个上限");
                    return frag_of(-1, -1, 2);
                }
                p->ngroup++;
                cap = 2 * p->ngroup;
            }
            Frag inner = parse_alt(p);
            if (p->failed) return inner;
            if (peek(p) != ')') { set_err(p, "分组缺少右括号 ')'"); return inner; }
            p->pos++;
            if (cap < 0) return inner;
            int s1 = emit(p, RX_SAVE);
            int s2 = emit(p, RX_SAVE);
            if (s1 < 0 || s2 < 0) return frag_of(-1, -1, 2);
            p->prog->inst[s1].x = cap;
            p->prog->inst[s2].x = cap + 1;
            p->prog->inst[s1].next = inner.start;
            patch(p, &inner.outs, s2);
            return frag_of(s1, s2, 2);           /* s2.next 由外层 concat/final 补 */
        }
        case '[':
            return parse_class(p);
        case '.': case '^': case '$': {
            RxOp op = (c == '.') ? RX_ANY : (c == '^') ? RX_BOL : RX_EOL;
            int i = emit(p, op);
            return frag_of(i, i, 2);
        }
        case '\\': {
            int e = peek(p);
            if (e < 0) { set_err(p, "正则末尾是孤立的 '\\'"); return frag_of(-1, -1, 2); }
            p->pos++;
            if (e == 'b') {
                int i = emit(p, RX_WORD);
                return frag_of(i, i, 2);
            }
            if (e == 'd' || e == 'D' || e == 'w' || e == 'W' || e == 's' || e == 'S') {
                int i = emit(p, RX_CLASS);
                if (i >= 0) bits_predef(p->prog->inst[i].bits, e, 0);
                return frag_of(i, i, 2);
            }
            int i = emit(p, RX_CHAR);
            if (i >= 0) p->prog->inst[i].lit = (uint8_t)escape_byte(e);
            return frag_of(i, i, 2);
        }
        case '*': case '+': case '?':
            set_err(p, "量词 * + ? 前面没有可重复的内容");
            return frag_of(-1, -1, 2);
        case ')': case '|':
            set_err(p, "正则中出现位置不当的 ')' 或 '|'");
            return frag_of(-1, -1, 2);
        default: {
            int i = emit(p, RX_CHAR);
            if (i >= 0) p->prog->inst[i].lit = (uint8_t)c;
            return frag_of(i, i, 2);
        }
    }
}

static Frag parse_repeat(Parser *p) {
    Frag a = parse_atom(p);
    if (p->failed) return a;
    while (!p->failed && p->pos < p->len) {
        int c = peek(p);
        if (c != '*' && c != '+' && c != '?') break;
        p->pos++;
        int s = emit(p, RX_SPLIT);
        if (s < 0) return a;
        p->prog->inst[s].x = a.start;
        if (c == '*') {
            patch(p, &a.outs, s);
            a = frag_of(s, s, 1);                /* y 待补：跳过循环 */
        } else if (c == '+') {
            patch(p, &a.outs, s);
            a = frag_of(a.start, s, 1);
        } else {                                 /* '?' */
            OutList o = a.outs;
            out_add(&o, s, 1);
            a.start = s;
            a.outs = o;
        }
    }
    return a;
}

static Frag parse_concat(Parser *p) {
    Frag acc = frag_of(-1, -1, 2);
    acc.outs.n = 0;
    int first = 1;
    while (!p->failed && p->pos < p->len) {
        int c = peek(p);
        if (c == '|' || c == ')') break;
        Frag b = parse_repeat(p);
        if (p->failed) break;
        if (first) { acc = b; first = 0; }
        else {
            patch(p, &acc.outs, b.start);
            acc.outs = b.outs;                  /* start 保持首个片段 */
        }
    }
    if (first) {
        /* 空分支（如 `a|` / `()`）：用一条 JMP 作为空片段 */
        int i = emit(p, RX_JMP);
        acc = frag_of(i, i, 0);
    }
    return acc;
}

static Frag parse_alt(Parser *p) {
    Frag a = parse_concat(p);
    while (!p->failed && peek(p) == '|') {
        p->pos++;
        Frag b = parse_concat(p);
        if (p->failed) break;
        int s = emit(p, RX_SPLIT);
        if (s < 0) break;
        p->prog->inst[s].x = a.start;
        p->prog->inst[s].y = b.start;
        OutList o = a.outs;
        out_join(&o, &b.outs);
        a.start = s;
        a.outs = o;
    }
    return a;
}

int rx_compile(RxProg *prog, const char *pattern, int patlen) {
    memset(prog, 0, sizeof(*prog));
    if (!pattern || patlen < 0) {
        snprintf(prog->err, sizeof(prog->err), "正则为空");
        return 0;
    }
    Parser p;
    memset(&p, 0, sizeof(p));
    p.prog = prog;
    p.pat = pattern;
    p.len = patlen;

    int s0 = emit(&p, RX_SAVE);                 /* cap[0] = 匹配起点 */
    int body = emit(&p, RX_JMP);
    prog->inst[s0].x = 0;
    prog->inst[s0].next = body;

    Frag f = parse_alt(&p);
    if (!p.failed && p.pos < p.len) set_err(&p, "正则尾部有多余字符（如孤立的 ')'）");
    if (p.failed) return 0;

    prog->inst[body].x = f.start;
    int s1 = emit(&p, RX_SAVE);                 /* cap[1] = 匹配终点 */
    int mt = emit(&p, RX_MATCH);
    prog->inst[s1].x = 1;
    prog->inst[s1].next = mt;
    patch(&p, &f.outs, s1);

    prog->ngroups = p.ngroup;
    prog->start_pc = s0;
    return 1;
}

/* ---------------------------------------------------------------- 执行器 */

typedef struct {
    int pc;
    int cap[RX_CAP_SLOTS];
} Thread;

typedef struct {
    Thread       t[RX_MAX_THREADS];
    int          n;
    unsigned char seen[RX_MAX_INST];
} TList;

static int word_boundary(const char *text, int len, int pos) {
    int l = pos > 0 && is_word((unsigned char)text[pos - 1]);
    int r = pos < len && is_word((unsigned char)text[pos]);
    return l != r;
}

/* 闭包展开：SPLIT/JMP/SAVE/零宽断言递归下推，消费型/MATCH 落到当前层。
   seen 按 pc 去重：按优先级顺序调用，先到的（高优先级）保留。 */
static void addthread(TList *l, const RxProg *prog, int pc, int pos,
                      int len, const char *text, const int *cap) {
    if (pc < 0 || pc >= prog->ninst) return;
    if (l->seen[pc]) return;
    l->seen[pc] = 1;
    const RxInst *in = &prog->inst[pc];
    switch (in->op) {
        case RX_JMP:
            addthread(l, prog, in->x, pos, len, text, cap);
            break;
        case RX_SPLIT:
            addthread(l, prog, in->x, pos, len, text, cap);
            addthread(l, prog, in->y, pos, len, text, cap);
            break;
        case RX_SAVE: {
            int c2[RX_CAP_SLOTS];
            memcpy(c2, cap, sizeof(c2));
            if (in->x >= 0 && in->x < RX_CAP_SLOTS) c2[in->x] = pos;
            addthread(l, prog, in->next, pos, len, text, c2);
            break;
        }
        case RX_BOL:
            if (pos == 0) addthread(l, prog, in->next, pos, len, text, cap);
            break;
        case RX_EOL:
            if (pos == len) addthread(l, prog, in->next, pos, len, text, cap);
            break;
        case RX_WORD:
            if (word_boundary(text, len, pos))
                addthread(l, prog, in->next, pos, len, text, cap);
            break;
        default:   /* RX_CHAR / RX_CLASS / RX_ANY / RX_MATCH */
            if (l->n < RX_MAX_THREADS) {
                l->t[l->n].pc = pc;
                memcpy(l->t[l->n].cap, cap, sizeof(l->t[l->n].cap));
                l->n++;
            }
            break;
    }
}

/* 固定起点跑一次 VM（要求从 start 起完整走到 MATCH），返回是否命中。 */
static int rx_vm(const RxProg *prog, const char *text, int len, int start,
                 RxMatch *m, TList *buf_a, TList *buf_b) {
    int cap0[RX_CAP_SLOTS];
    for (int i = 0; i < RX_CAP_SLOTS; i++) cap0[i] = -1;

    TList *cl = buf_a, *nl = buf_b;
    memset(cl, 0, sizeof(*cl));
    addthread(cl, prog, prog->start_pc, start, len, text, cap0);

    int matched = 0;
    int mcap[RX_CAP_SLOTS];
    for (int pos = start; pos <= len && cl->n > 0; pos++) {
        memset(nl, 0, sizeof(*nl));
        int ch = pos < len ? (unsigned char)text[pos] : -1;
        for (int i = 0; i < cl->n; i++) {
            const RxInst *in = &prog->inst[cl->t[i].pc];
            if (in->op == RX_MATCH) {
                memcpy(mcap, cl->t[i].cap, sizeof(mcap));
                mcap[1] = pos;
                matched = 1;
                cl->n = i;          /* 截断更低优先级线程（左优先 / 贪婪） */
                break;
            }
            int ok = 0;
            if (ch >= 0) {
                switch (in->op) {
                    case RX_CHAR:  ok = (ch == in->lit); break;
                    case RX_ANY:   ok = (ch != '\n'); break;
                    case RX_CLASS: ok = bits_has(in->bits, ch); break;
                    default: break;
                }
            }
            if (ok)
                addthread(nl, prog, in->next, pos + 1, len, text, cl->t[i].cap);
        }
        /* 指针交换（零拷贝）：nl 里是优先级更高的线程继续尝试更长匹配 */
        TList *tmp = cl;
        cl = nl;
        nl = tmp;
    }
    if (!matched) return 0;
    m->start = mcap[0];
    m->end = mcap[1];
    memcpy(m->cap, mcap, sizeof(mcap));
    return 1;
}

int rx_search(const RxProg *prog, const char *text, int textlen,
              int from, RxMatch *m) {
    if (!prog || prog->ninst == 0 || !text || textlen < 0) return 0;
    if (from < 0) from = 0;
    if (from > textlen) return 0;

    /* TList 约 170 KB，放堆上避免吃满 Dial spawn worker 的栈。 */
    TList *cl = (TList *)calloc(1, sizeof(TList));
    TList *nl = (TList *)calloc(1, sizeof(TList));
    if (!cl || !nl) { free(cl); free(nl); return 0; }

    int found = 0;
    for (int s = from; s <= textlen; s++) {
        if (rx_vm(prog, text, textlen, s, m, cl, nl)) { found = 1; break; }
    }
    free(cl);
    free(nl);
    return found;
}
