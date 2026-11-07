/*
 * dial_regex —— 自研 Thompson NFA 正则引擎（text.dialpkg 的机器码部分）。
 *
 * 为什么自研：三平台都要能跑，POSIX regex 在 Windows MinGW 上不存在，
 * 而 vendor 一个 PCRE2 会把「不引入无用依赖」（README §21.2）和离线可复现
 * 一起拖下水。Thompson NFA 是 ~500 行纯 C、零依赖、线性时间（无回溯爆炸），
 * 线级细节全在自己手里——与 http 包「协议层自研」是同一条立场。
 *
 * 语法子集（刻意划清边界，见 text.diali / README §37.10）：
 *   - 字面量、. 、字符类 [abc] [a-z] [^...]、\d \D \w \W \s \S、元字符转义
 *   - 量词 * + ?（贪婪）、分支 |、分组 (...) / (?:...)、锚 ^ $ 与 \b
 *   - 捕获组最多 9 个（组 0 为整体匹配）
 * 明确不做：后向引用、lookaround、懒惰量词 *?、{m,n} 区间量词、
 *   Unicode 属性 / 码点类。按**字节**匹配（与 Dial 的 UTF-8 字节串一致），
 *   不做 Unicode 归一化；中文等多字节字面量逐字节比较，行为可预期。
 *
 * 编译产物是一段「正则虚拟机指令」（Russ Cox, Regular Expression Matching:
 * the Virtual Machine），执行走 Pike VM：并行的线程按优先级排列，先到的
 * MATCH 截断低优先级线程，因此天然得到「左分支优先 + 贪婪最长」，且不会
 * 像回溯引擎那样在 (a+)+b 上指数爆炸。
 */
#ifndef DIAL_REGEX_H
#define DIAL_REGEX_H

#include <stdint.h>

#define RX_MAX_GROUPS   9
#define RX_MAX_INST     2048
#define RX_MAX_THREADS  2048
#define RX_CAP_SLOTS    (2 * (RX_MAX_GROUPS + 1))

typedef enum {
    RX_CHAR = 1,   /* 匹配一个固定字节            */
    RX_CLASS,      /* 匹配 bits[] 里的任一字节    */
    RX_ANY,        /* 匹配除 '\n' 外的任一字节    */
    RX_BOL,        /* ^                           */
    RX_EOL,        /* $                           */
    RX_WORD,       /* \b 词边界（零宽）           */
    RX_SPLIT,      /* 优先走 x，其次 y            */
    RX_JMP,        /* 无条件跳 x                  */
    RX_SAVE,       /* cap[x] = 当前位置           */
    RX_MATCH       /* 接受                        */
} RxOp;

typedef struct {
    uint8_t op;
    /* 所有跳转都显式写目标，不依赖 `pc+1`：Thompson 构造里分支指令常在
       子片段**之后**发射，若假定顺序执行就会跳错。 */
    int     x, y;          /* SPLIT：两个分支目标；JMP：x；SAVE：x = 槽号 */
    int     next;          /* CHAR/CLASS/ANY/BOL/EOL/WORD/SAVE 消费成功后的后继 */
    uint8_t lit;          /* RX_CHAR 的字面字节 */
    uint8_t bits[32];     /* RX_CLASS：256 位集合 */
} RxInst;

typedef struct {
    RxInst inst[RX_MAX_INST];
    int    ninst;
    int    ngroups;       /* 捕获组数（不含组 0） */
    int    start_pc;      /* 入口指令下标 */
    char   err[160];      /* 编译失败原因（中文） */
} RxProg;

typedef struct {
    int start, end;                 /* 整体匹配区间 [start, end) */
    int cap[RX_CAP_SLOTS];          /* 2*i / 2*i+1；未参与的槽为 -1 */
} RxMatch;

/* 编译 pattern（不必 NUL 结尾，用 patlen 定长）。成功返回 1，失败返回 0 并写 err。 */
int rx_compile(RxProg *prog, const char *pattern, int patlen);

/* 从 from 起找第一个匹配。命中返回 1 并填 m，否则返回 0。
   O(n·m) 的 Pike VM；每次从候选起点重跑，保证 ^ 锚与左优先语义。 */
int rx_search(const RxProg *prog, const char *text, int textlen, int from, RxMatch *m);

#endif /* DIAL_REGEX_H */
