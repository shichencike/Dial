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
    if (cap >= need) return a;
    int64_t ncap = cap < 4 ? 4 : cap * 2;
    while (ncap < need) ncap *= 2;
    void *p = dial_rc_alloc(16 + ncap * 8);
    a_meta(p)[0] = len;
    a_meta(p)[1] = ncap;
    memcpy(a_elems(p), a_elems(a), (size_t)len * 8);
    dial_rc_release(a);
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
    /* COW 兜底：若 buffer 被多者共享，原地修改前先克隆，避免污染其他持有者 */
    if (((int64_t *)a)[-1] > 1) {
        void *n = dial_rc_clone(a, 16 + a_meta(a)[1] * 8);
        dial_rc_release(a);
        a = n;
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
