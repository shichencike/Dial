/*
 * dial_text.h —— text.dialpkg 对外 C 接口（对应 text.diali 的唯一契约）。
 *
 * FFI 约定（与 http 包一致，写包时最容易踩的两个坑）：
 *   1. Dial 的 `string` / `bytes` 跨到 C 是 **DialString 句柄**（布局
 *      `[int64 len][bytes...]`），文本必须用 dial_string_cstr() 取、长度用
 *      dial_string_length() 取，绝不能把句柄当 const char*。
 *   2. 返回 `string` 必须返回 dial_string_* 构造的**句柄**，不能返回裸
 *      const char*，否则 Dial 会把指针当载荷、读出越界垃圾。
 *
 * 宽度：`.diali` 的 int → LLVM i64；bool → C int(i32)；句柄 / string / bytes → ptr。
 */
#ifndef DIAL_TEXT_H
#define DIAL_TEXT_H

#include <stdint.h>
#include "dial_string.h"
#include "dial_regex.h"

#define DIAL_TEXT_OK          0
#define DIAL_TEXT_ERR_COMPILE (-1)
#define DIAL_TEXT_ERR_NULL    (-2)
#define DIAL_TEXT_ERR_RANGE   (-3)
#define DIAL_TEXT_ERR_NOMEM   (-4)

/* 不透明句柄：一个已编译的正则 + 最近一次匹配结果缓存（per-handle 状态）。
   错误槽是**线程局部**，句柄本身不建议跨线程并发调用 find/group。 */
typedef struct DialRegex {
    RxProg  prog;
    RxMatch last;
    int     has_last;
} DialRegex;

DialRegex *dial_text_regex_compile(DialString pattern);
int64_t    dial_text_regex_free(DialRegex *r);
int32_t    dial_text_regex_is_match(DialRegex *r, DialString text);
int64_t    dial_text_regex_find(DialRegex *r, DialString text);
int64_t    dial_text_regex_match_end(DialRegex *r);
int64_t    dial_text_regex_group_count(DialRegex *r);
DialString dial_text_regex_group(DialRegex *r, DialString text, int64_t index);
DialString dial_text_regex_replace_all(DialRegex *r, DialString text, DialString replacement);
int32_t    dial_text_regex_is_valid(DialString pattern);

int64_t    dial_text_last_error(void);
DialString dial_text_last_error_msg(void);

#endif /* DIAL_TEXT_H */
