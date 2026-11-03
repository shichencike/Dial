#include "dial_array.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>
#include <stdlib.h>

/*
 * payload 布局：[ int64 kind ][ int64 len ][ int64 cap ][ int64 elems... ]
 *   kind: 0 = INT    —— 标量 int，元素无所有权，push/set 不 retain/release
 *         1 = RC     —— 句柄（string / int[] / map / error / 不透明指针），按元素走 dial_rc_*
 *         2 = ATOMIC —— channel 句柄（跨线程共享），按元素走 dial_channel_*（原子引用计数）
 * 前导 int64 refcount 由 dial_rc_* 管理（见 dial_rt_mem.h）。
 *
 * v0.98.2 起数组不再是「只能装 int」的盒子：只要元素是 8 字节指针（或 int），
 * 就能直接存——因为所有不透明句柄（string / int[] / map / error / 不透明指针）共享同一份
 * dial_rc_* 引用计数，channel 则走原子引用计数。每个数组在头部记着自己的元素类别，
 * push/set/克隆/释放时据此对元素做对应的 retain/release，从而正确实现「按元素 ARC」。
 */
#define DIAL_ELEM_INT    0
#define DIAL_ELEM_RC     1
#define DIAL_ELEM_ATOMIC 2

static int64_t *a_meta(void *a) { return (int64_t *)a; }        /* [0]=kind [1]=len [2]=cap */
static int64_t *a_elems(void *a) { return (int64_t *)((char *)a + 24); }
static size_t  a_size(int64_t cap) { return (size_t)(24 + cap * 8); }

/* channel 元素走原子引用计数（与 string/map 的非原子 rc 分开，绝不能混用）。 */
extern void dial_channel_retain(void *ch);
extern void dial_channel_release(void *ch);

/* 元素析构：RC 句柄走 dial_rc_release，channel 走 dial_channel_release（原子）。 */
static void elem_retain(int64_t kind, int64_t v) {
    if (!v) return;
    if (kind == DIAL_ELEM_RC)          dial_rc_retain((void *)v);
    else if (kind == DIAL_ELEM_ATOMIC) dial_channel_retain((void *)v);
}
static void elem_release(int64_t kind, int64_t v) {
    if (!v) return;
    if (kind == DIAL_ELEM_RC)          dial_rc_release((void *)v);
    else if (kind == DIAL_ELEM_ATOMIC) dial_channel_release((void *)v);
}

/* 数组取得 len 个元素的所有权：逐一 retain（用于 COW 克隆 / 深拷贝之后）。 */
static void retain_elems(void *a) {
    int64_t kind = a_meta(a)[0];
    if (kind == DIAL_ELEM_INT) return;
    int64_t len = a_meta(a)[1];
    int64_t *e = a_elems(a);
    for (int64_t i = 0; i < len; i++) elem_retain(kind, e[i]);
}

DialArray dial_array_new(int64_t kind) {
    void *p = dial_rc_alloc(a_size(0));
    a_meta(p)[0] = kind;
    a_meta(p)[1] = 0;
    a_meta(p)[2] = 0;
    return p;
}

static DialArray grow(DialArray a, int64_t need) {
    int64_t len = a_meta(a)[1];
    int64_t cap = a_meta(a)[2];
    if (cap >= need) {
        // 同指针返回：+1 使其成为「移交所有权」的新值，与赋值语句的「释放旧值」配对，
        // 避免自赋值 `a = a.push(...)` 的 use-after-free（CVE-2024-DIAL-003 配套修正）。
        dial_rc_retain(a);
        return a;
    }
    int64_t ncap = cap < 4 ? 4 : cap * 2;
    while (ncap < need) ncap *= 2;
    void *p = dial_rc_alloc(a_size(ncap));
    a_meta(p)[0] = a_meta(a)[0];   // 继承元素类型
    a_meta(p)[1] = len;
    a_meta(p)[2] = ncap;
    memcpy(a_elems(p), a_elems(a), (size_t)len * 8);
    retain_elems(p);               // 新 buffer 取得各元素所有权；旧 buffer 由调用方释放
    return p;
}

DialArray dial_array_push(DialArray a, int64_t v) {
    a = grow(a, a_meta(a)[1] + 1);
    int64_t len = a_meta(a)[1];
    a_elems(a)[len] = v;
    a_meta(a)[1] = len + 1;
    elem_retain(a_meta(a)[0], v);  // 新元素被本数组持有
    return a;
}

int64_t dial_array_get(DialArray a, int64_t i) {
    /* Fail-Fast：越界绝不静默读相邻内存，当场 ud2 */
    DIAL_CHECK_BOUNDS(i, a_meta(a)[1]);
    return a_elems(a)[i];
}

DialArray dial_array_set(DialArray a, int64_t i, int64_t v) {
    /* Fail-Fast：越界写是 C 里最阴险的 Bug（悄悄改坏别人的数据），当场终止 */
    DIAL_CHECK_BOUNDS(i, a_meta(a)[1]);
    int64_t kind = a_meta(a)[0];
    /* COW 兜底：若 buffer 被多者共享，原地修改前先克隆，避免污染其他持有者。
     *   - 共享（refcount>1）：克隆出新 buffer（自带 +1），retain 其元素，旧 buffer 由赋值语句释放；
     *   - 独占（refcount==1）：原地修改，但返回前 dial_rc_retain 把「同指针」变成
     *     「移交所有权的新值」，与赋值语句的「释放旧值」配对——否则自赋值
     *     `a = a.set(...)` 会先释放 a 再把悬垂指针写回（use-after-free）。
     * （CVE-2024-DIAL-003 修复后的配套修正） */
    if (((int64_t *)a)[-1] > 1) {
        void *n = dial_rc_clone(a, a_size(a_meta(a)[2]));
        retain_elems(n);
        a = n;
    } else {
        dial_rc_retain(a);
    }
    /* 覆盖槽位：先释放旧元素，再写入新元素并 retain */
    int64_t old = a_elems(a)[i];
    elem_release(kind, old);
    a_elems(a)[i] = v;
    elem_retain(kind, v);
    return a;
}

int64_t dial_array_length(DialArray a) {
    DIAL_ASSERT(a_meta(a)[1] >= 0, "数组内部损坏：len < 0");
    return a_meta(a)[1];
}

DialArray dial_array_clone(DialArray a) {
    void *n = dial_rc_clone(a, a_size(a_meta(a)[2]));
    retain_elems(n);   // 克隆体取得各元素所有权
    return n;
}

void dial_array_retain(DialArray a) { dial_rc_retain(a); }

void dial_array_release(DialArray a) {
    /* 未初始化的数组变量槽位是 NULL，scope 退出时仍会被 ARC 释放——dial_rc_release 已对 NULL 短路，
     * 此处先判空，避免后面读 ((int64_t*)a)[-1] 时解引用空指针。 */
    if (!a) return;
    /* 仅当本数组是最后一个持有者（refcount 即将归零）时才释放元素，
     * 避免共享数组的某一个持有者提前把元素释放，导致其余持有者悬垂（use-after-free）。 */
    if (((int64_t *)a)[-1] == 1) {
        int64_t kind = a_meta(a)[0];
        if (kind != DIAL_ELEM_INT) {
            int64_t len = a_meta(a)[1];
            int64_t *e = a_elems(a);
            for (int64_t i = 0; i < len; i++) elem_release(kind, e[i]);
        }
    }
    dial_rc_release(a);
}
