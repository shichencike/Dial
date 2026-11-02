#include "dial_array.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>
#include <stdlib.h>

/* payload: [ int64 len ][ int64 cap ][ int64 elems[] ] */
static int64_t *a_meta(void *a) { return (int64_t *)a; }      /* [0]=len [1]=cap */
static int64_t *a_elems(void *a) { return (int64_t *)((char *)a + 16); }

DialArray dial_array_new(void) {
    void *p = dial_rc_alloc(16);
    a_meta(p)[0] = 0;
    a_meta(p)[1] = 0;
    return p;
}

static DialArray grow(DialArray a, int64_t need) {
    int64_t len = a_meta(a)[0];
    int64_t cap = a_meta(a)[1];
    if (cap >= need) {
        // 同指针返回：+1 使其成为「移交所有权」的新值，与赋值语句的「释放旧值」配对，
        // 避免自赋值 `a = a.push(...)` 的 use-after-free（CVE-2024-DIAL-003 配套修正）。
        dial_rc_retain(a);
        return a;
    }
    int64_t ncap = cap < 4 ? 4 : cap * 2;
    while (ncap < need) ncap *= 2;
    void *p = dial_rc_alloc(16 + ncap * 8);
    a_meta(p)[0] = len;
    a_meta(p)[1] = ncap;
    memcpy(a_elems(p), a_elems(a), (size_t)len * 8);
    // 新 buffer 由 dial_rc_alloc 自带 +1（refcount=1），直接移交所有权；旧 buffer 由赋值语句释放。
    return p;
}

DialArray dial_array_push(DialArray a, int64_t v) {
    a = grow(a, a_meta(a)[0] + 1);
    int64_t len = a_meta(a)[0];
    a_elems(a)[len] = v;
    a_meta(a)[0] = len + 1;
    return a;
}

int64_t dial_array_get(DialArray a, int64_t i) {
    /* Fail-Fast：越界绝不静默读相邻内存，当场 ud2 */
    DIAL_CHECK_BOUNDS(i, a_meta(a)[0]);
    return a_elems(a)[i];
}

DialArray dial_array_set(DialArray a, int64_t i, int64_t v) {
    /* Fail-Fast：越界写是 C 里最阴险的 Bug（悄悄改坏别人的数据），当场终止 */
    DIAL_CHECK_BOUNDS(i, a_meta(a)[0]);
    /* COW 兜底：若 buffer 被多者共享，原地修改前先克隆，避免污染其他持有者。
     * - 共享（refcount>1）：克隆出新 buffer（自带 +1），旧 buffer 由赋值语句释放；
     * - 独占（refcount==1）：原地修改，但返回前 dial_rc_retain 把「同指针」变成
     *   「移交所有权的新值」，与赋值语句的「释放旧值」配对——否则自赋值
     *   `a = a.set(...)` 会先释放 a 再把悬垂指针写回（use-after-free）。
     * （CVE-2024-DIAL-003 修复后的配套修正） */
    if (((int64_t *)a)[-1] > 1) {
        void *n = dial_rc_clone(a, 16 + a_meta(a)[1] * 8);
        a = n;
    } else {
        dial_rc_retain(a);
    }
    DIAL_ASSERT(a_meta(a)[0] <= a_meta(a)[1], "数组内部损坏：len > cap");
    a_elems(a)[i] = v;
    return a;
}

int64_t dial_array_length(DialArray a) {
    DIAL_ASSERT(a_meta(a)[0] >= 0, "数组内部损坏：len < 0");
    return a_meta(a)[0];
}

DialArray dial_array_clone(DialArray a) {
    return dial_rc_clone(a, 16 + a_meta(a)[1] * 8);
}

void dial_array_retain(DialArray a) { dial_rc_retain(a); }
void dial_array_release(DialArray a) { dial_rc_release(a); }
