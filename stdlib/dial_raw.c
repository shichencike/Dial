#include "dial_raw.h"
#include "dial_rt_trap.h"
#include <stdlib.h>

/*
 * 裸指针的分配 / 释放（v1.02）。
 *
 * 这里刻意**不**接入 ARC：裸指针是「不受自动管束」的那一类，
 * 它的生命周期完全由开发者显式管理（谁申请、谁释放）。
 *
 * 分配失败按项目的 Fail-Fast 铁律处理：当场终止，绝不返回 NULL 让上层带病运行
 * （与 §20「内存分配失败 → 当场终止」一致）。
 */
void *dial_raw_alloc(int64_t n) {
    if (n < 0) {
        dial_assert_fail("dial_raw_alloc: 字节数为负");
    }
    /* malloc(0) 的返回值可能是 NULL，但它不是「分配失败」——按 1 字节处理。 */
    size_t sz = (size_t)(n == 0 ? 1 : n);
    void *p = malloc(sz);
    if (!p) {
        dial_assert_fail("dial_raw_alloc: 裸内存分配失败");
    }
    return p;
}

void dial_raw_free(void *p) {
    if (!p) return;
    free(p);
}
