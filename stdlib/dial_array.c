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

/* 释放数组 buffer：若是最后一个持有者，先释放它持有的各元素引用，再回收 buffer 本身。
 * 这是「消耗调用者交来的那一份引用」的统一落点（与 dial_map_put 释放旧 buffer 同款）。 */
static void release_buffer(void *a) {
    if (!a) return;
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
        // 容量够：原地复用同一块 buffer。
        // **绝不 retain**——本函数与 dial_map_put 同款契约：「消耗调用者交来的那一份
        // 引用，返回替换用的新指针」。调用者（codegen）把返回的指针写回槽位后，
        // 槽位旧指针的那一份引用已经被本函数消耗掉了，调用者**不得**再 release。
        // 过去这里多做了 retain，于是每 push 一次引用计数就 +1，COW 守卫恒真。
        return a;
    }
    int64_t ncap = cap < 4 ? 4 : cap * 2;
    while (ncap < need) ncap *= 2;
    void *p = dial_rc_alloc(a_size(ncap));
    a_meta(p)[0] = a_meta(a)[0];   // 继承元素类型
    a_meta(p)[1] = len;
    a_meta(p)[2] = ncap;
    memcpy(a_elems(p), a_elems(a), (size_t)len * 8);
    retain_elems(p);               // 新 buffer 取得各元素所有权
    release_buffer(a);             // 旧 buffer：最后一个持有者时释放其元素并回收
    return p;
}

DialArray dial_array_slice(DialArray a, int64_t from, int64_t to) {
    int64_t len = a_meta(a)[1];
    /* Fail-Fast：越界切片绝不静默读相邻内存，当场 ud2 */
    if (from < 0 || to < from || to > len) {
        dial_assert_fail("dial_array_slice: 切片区间越界");
    }
    int64_t kind = a_meta(a)[0];
    DialArray out = dial_array_new(kind);
    /* dial_array_push 内部会按 kind 对新元素 retain 一份，故新切片独占各元素的所有权，
       与原数组互不影响（原数组释放时不会带走切片里的句柄）。 */
    for (int64_t i = from; i < to; i++) {
        out = dial_array_push(out, a_elems(a)[i]);
    }
    return out;
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
     *   - 共享（refcount>1）：克隆出新 buffer（自带引用计数 1），并**消耗掉调用者
     *     交来的那一份引用**（release 旧 buffer）——与 dial_map_put 完全同款；
     *   - 独占（refcount==1）：原地写入，**不 retain 不 release**（所有权原样移交，
     *     引用计数必须保持不变，否则每写一次就 +1）。
     *
     * 旧实现两处都错：共享时没 release 旧 buffer（漏一份引用）、独占时多做了 retain
     * （又凭空 +1）。于是每写一次下标赋值引用计数就涨 1，COW 守卫 `refcount > 1`
     * 从此**恒真**——每一次 a[i] = v 都深拷贝整个数组，N 次写就是 O(N^2) 内存，
     * 百万级循环直接 OOM Killed。 */
    if (((int64_t *)a)[-1] > 1) {
        void *n = dial_rc_clone(a, a_size(a_meta(a)[2]));
        retain_elems(n);
        release_buffer(a);         // 消耗调用者交来的那一份引用
        a = n;
    }
    /* 覆盖槽位：**先 retain 新元素、再 release 旧元素**。
     * 顺序反了会在「旧元素 == 新元素」（如 a[0] = a[0]）时先把计数打到 0（free），
     * 紧接着又对已释放内存 retain —— use-after-free。 */
    int64_t old = a_elems(a)[i];
    elem_retain(kind, v);
    elem_release(kind, old);
    a_elems(a)[i] = v;
    return a;
}

/* 嵌套下标赋值专用的变体：**不消耗** a 的引用。
 *
 * 与 dial_array_set 的唯一区别在所有权：
 *   - dial_array_set 消耗调用者交来的那一份引用（单层 a[i] = v：a 来自变量槽，
 *     槽位旧的那一份引用确实该由它消耗掉）；
 *   - 本函数用于嵌套路径（m[i][j] = v）：内层数组是从**外层数组的元素槽借来**的
 *     指针，它的那一份引用属于外层数组，在这里被消耗掉就会与外层的元素替换
 *     重复释放同一份引用。
 * 故本函数不释放 a，而是交出「一份新拥有的引用」：共享时克隆（不释放被借的 a），
 * 独占时原地写并额外 retain 一份。调用方（codegen）用完后自行 release 这一份。 */
DialArray dial_array_set_borrowed(DialArray a, int64_t i, int64_t v) {
    DIAL_CHECK_BOUNDS(i, a_meta(a)[1]);
    int64_t kind = a_meta(a)[0];
    DialArray res = a;
    if (((int64_t *)a)[-1] > 1) {
        res = dial_rc_clone(a, a_size(a_meta(a)[2]));
        retain_elems(res);
        /* 不 release a：它是被借来的，引用属于外层数组的元素槽。 */
    } else {
        dial_rc_retain(a);         // 独占：原地写，但交出「一份新拥有」的引用
    }
    int64_t old = a_elems(res)[i];
    elem_retain(kind, v);
    elem_release(kind, old);
    a_elems(res)[i] = v;
    return res;
}

int64_t dial_array_length(DialArray a) {
    DIAL_ASSERT(a_meta(a)[1] >= 0, "数组内部损坏：len < 0");
    return a_meta(a)[1];
}

DialArray dial_array_clone(DialArray a) {
    if (!a) return NULL;
    /* clone() 必须交出「一份独立副本」：先 retain 让「唯一持有者」判定失效，
     * 迫使 dial_rc_clone 走进深拷贝分支，克隆完再抵消掉刚才那次 retain。
     * （dial_rc_clone 现在是纯粹的 COW：唯一持有者时原样返回、不改引用计数，
     *   若直接用它，clone() 在独占时只会拿到同一个 buffer 的别名。）
     * 过去这里对「别名」也调 retain_elems，等于把同一批元素的引用计数凭空 +1，
     * 数组释放时又只释放一次——元素引用计数净多 1，永久泄漏。 */
    dial_rc_retain(a);
    void *n = dial_rc_clone(a, a_size(a_meta(a)[2]));
    dial_rc_release(a);
    if (n != a) retain_elems(n);   // 克隆体取得各元素所有权
    return n;
}

void dial_array_retain(DialArray a) { dial_rc_retain(a); }

void dial_array_release(DialArray a) {
    /* 未初始化的数组变量槽位是 NULL，scope 退出时仍会被 ARC 释放——dial_rc_release 已对 NULL 短路，
     * 此处先判空，避免后面读 ((int64_t*)a)[-1] 时解引用空指针。 */
    release_buffer(a);
}

DialArray dial_array_remove(DialArray a, int64_t i) {
    int64_t len = a_meta(a)[1];
    if (i < 0 || i >= len) {
        dial_assert_fail("dial_array_remove: 索引越界");
    }
    int64_t kind = a_meta(a)[0];
    int64_t cap = a_meta(a)[2];
    void *p;
    if (((int64_t *)a)[-1] > 1) {
        // 共享：克隆出新 buffer（自带引用 1），消耗调用者交来的那一份旧引用，
        // 然后在新 buffer 上做左移紧凑——与 dial_array_set 的 COW 分支同款契约。
        p = dial_rc_clone(a, a_size(cap));
        a_meta(p)[1] = len - 1;
        memmove(a_elems(p), a_elems(a), (size_t)i * 8);
        memmove(a_elems(p) + i, a_elems(a) + i + 1, (size_t)(len - 1 - i) * 8);
        retain_elems(p);
        release_buffer(a);
    } else {
        // 独占：原地左移紧凑（其余元素引用计数不变），返回同一指针。
        memmove(a_elems(a) + i, a_elems(a) + i + 1, (size_t)(len - 1 - i) * 8);
        a_meta(a)[1] = len - 1;
        p = a;
    }
    (void)kind;
    return p;
}

DialArray dial_array_pop(DialArray a) {
    int64_t len = a_meta(a)[1];
    if (len <= 0) {
        dial_assert_fail("dial_array_pop: 空数组不可 pop");
    }
    return dial_array_remove(a, len - 1);
}

/* 升序排序（v1.01）。被多者共享时先 COW 克隆，再原地排序，返回（可能更新后的）指针。 */
static int cmp_int64(const void *pa, const void *pb) {
    int64_t a = *(const int64_t *)pa, b = *(const int64_t *)pb;
    return (a > b) - (a < b);
}
/* float 元素：把 8 字节槽位按 IEEE754 数值比较（负数排序正确，绝非位模式比较）。 */
static int cmp_float64(const void *pa, const void *pb) {
    double a = *(const double *)pa, b = *(const double *)pb;
    return (a > b) - (a < b);
}

DialArray dial_array_sort(DialArray a) {
    if (!a) return NULL;
    int64_t kind = a_meta(a)[0];
    /* COW：共享时克隆出新 buffer（自带引用计数 1），消耗调用者交来的那一份引用。 */
    if (((int64_t *)a)[-1] > 1) {
        void *n = dial_rc_clone(a, a_size(a_meta(a)[2]));
        retain_elems(n);
        release_buffer(a);
        a = n;
    }
    int64_t len = a_meta(a)[1];
    int64_t *e = a_elems(a);
    /* kind 3 是 float（编译器侧约定的元素类别码），其余标量（int/byte/char/bool）按整数序。
     * 句柄元素（string 等）排序语义未定义，编译器侧已拒绝调用 sort，不会到这。 */
    if (kind == 3) {
        qsort(e, (size_t)len, sizeof(int64_t), cmp_float64);
    } else {
        qsort(e, (size_t)len, sizeof(int64_t), cmp_int64);
    }
    return a;
}
