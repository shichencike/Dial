#include "dial_map.h"
#include "dial_array.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>
#include <stdlib.h>

/*
 * Map (int64 -> int64) 的 ARC 实现，与 String / Array 共用同一套引用计数契约：
 *
 *   payload: [ int64 cap ][ int64 count ][ entries ... ]
 *                                   ^
 *                                   | Dial 看到的「不透明指针」(opaque pointer)
 *   其前方 [-8] 字节为引用计数（见 dial_rt_mem.h）。
 *
 * 每个 entry 24 字节：{ int64_t key; int64_t val; int64_t state; }
 *   state: 0 = EMPTY, 1 = USED, 2 = DELETED（墓碑）
 *
 * 线性探测开放寻址。**删除只置墓碑，绝不左移紧凑**：把后续条目直接前移会让它
 * 挪出自己的探测链，导致「删除一个键后另一个键再也查不到」。v1.00 的 2 万键
 * 压测暴露了这一经典陷阱（删 1 万个后 size 只剩 13082），故与 dial_kvmap.c
 * 统一改为墓碑 + 负载过高时重建。
 *
 * 内存宪法契合点：
 *   - 绝不追踪式 GC；所有权由 ARC（dial_map_retain / dial_map_release）显式管理。
 *   - put 在「被多者共享」时先 COW 克隆再修改，并把新指针交还编译器写回接收变量；
 *     若无需拷贝（唯一持有）则原地修改，O(1) 零拷贝。
 *   - remove/get/contains/size 只读或就地修改，不更换指针（remove 在共享场景下会
 *     一并影响所有持有者——如需隔离请先 .clone()）。
 */

enum { M_EMPTY = 0, M_USED = 1, M_DELETED = 2 };

typedef struct { int64_t key; int64_t val; int64_t state; } MEntry;

static MEntry *m_entries(void *m) { return (MEntry *)((char *)m + 16); }
static int64_t m_cap(void *m)    { return ((int64_t *)m)[0]; }
static int64_t m_count(void *m)  { return ((int64_t *)m)[1]; }
static void m_set_cap(void *m, int64_t c)   { ((int64_t *)m)[0] = c; }
static void m_set_count(void *m, int64_t c) { ((int64_t *)m)[1] = c; }
static int64_t m_size(int64_t cap) { return 16 + cap * (int64_t)sizeof(MEntry); }

static int64_t m_hash(int64_t k, int64_t cap) {
    uint64_t h = (uint64_t)k;
    if (k < 0) h = (~h) + 1;             /* 绝对值，INT_MIN 也不溢出 */
    return (int64_t)(h % (uint64_t)cap);
}

static int64_t m_find(void *m, int64_t k) {
    int64_t cap = m_cap(m);
    int64_t i = m_hash(k, cap);
    for (int64_t n = 0; n < cap; n++) {
        MEntry *e = &m_entries(m)[i];
        if (e->state == M_EMPTY) return -1;
        if (e->state == M_USED && e->key == k) return i;
        i = (i + 1) % cap;
    }
    return -1;
}

static int64_t m_insert_slot(void *m, int64_t k) {
    int64_t cap = m_cap(m);
    int64_t i = m_hash(k, cap);
    int64_t tomb = -1;
    for (int64_t n = 0; n < cap; n++) {
        MEntry *e = &m_entries(m)[i];
        if (e->state == M_EMPTY) return tomb >= 0 ? tomb : i;
        if (e->state == M_DELETED && tomb < 0) tomb = i;
        i = (i + 1) % cap;
    }
    return tomb;
}

static int64_t m_count_deleted(void *m) {
    int64_t n = 0;
    for (int64_t i = 0; i < m_cap(m); i++)
        if (m_entries(m)[i].state == M_DELETED) n++;
    return n;
}

/* 把 (k,v) 插入到已分配好的 buffer nm/ncap 的探测空位（不查重、不改 cap）。 */
static void m_reinsert(void *nm, int64_t ncap, int64_t k, int64_t v) {
    int64_t i = m_hash(k, ncap);
    while (m_entries(nm)[i].state == M_USED) i = (i + 1) % ncap;
    m_entries(nm)[i].key   = k;
    m_entries(nm)[i].val   = v;
    m_entries(nm)[i].state = M_USED;
    m_set_count(nm, m_count(nm) + 1);
}

/* 重建到 ncap（>cap 即扩容，==cap 即原地清墓碑）。只搬运 USED 条目。
 * 新 buffer 由 dial_rc_alloc 分配（refcount==1），天然唯一持有。 */
static void *m_rebuild(void *m, int64_t ncap) {
    void *nm = dial_rc_alloc(m_size(ncap));
    m_set_cap(nm, ncap);
    m_set_count(nm, 0);
    for (int64_t i = 0; i < ncap; i++) m_entries(nm)[i].state = M_EMPTY;
    for (int64_t i = 0; i < m_cap(m); i++) {
        MEntry *e = &m_entries(m)[i];
        if (e->state == M_USED) m_reinsert(nm, ncap, e->key, e->val);
    }
    dial_rc_release(m);      /* 独占则回收，共享则只减引用（另一持有者仍在） */
    return nm;
}

DialMap dial_map_new(void) {
    int64_t cap = 8;
    void *m = dial_rc_alloc(m_size(cap));
    m_set_cap(m, cap);
    m_set_count(m, 0);
    for (int64_t i = 0; i < cap; i++) m_entries(m)[i].state = M_EMPTY;
    return m;
}

DialMap dial_map_put(DialMap mh, int64_t k, int64_t v) {
    void *m = (void *)mh;
    int64_t cap = m_cap(m);

    /* 1) 负载（含墓碑）过高：真实条目也高则扩容，否则原地重建清墓碑。 */
    int64_t used = m_count(m);
    int64_t deleted = m_count_deleted(m);
    if (used + deleted >= cap * 3 / 4) {
        int64_t ncap = (used >= cap * 3 / 4) ? cap * 2 : cap;
        m = m_rebuild(m, ncap);
        cap = m_cap(m);
    }

    /* 2) 仍被多者共享 -> COW 克隆，避免污染其他持有者（与 array.set 一致）。 */
    if (((int64_t *)m)[-1] > 1) {
        void *nm = dial_rc_clone(m, m_size(cap));
        dial_rc_release(m);
        m = nm;
    }

    /* 3) 写入：命中则更新值，未命中则复用墓碑 / 追加条目。 */
    int64_t hit = m_find(m, k);
    if (hit >= 0) {
        m_entries(m)[hit].val = v;
        return m;
    }
    int64_t slot = m_insert_slot(m, k);
    DIAL_ASSERT(slot >= 0, "map 内部损坏：哈希表已满且无墓碑可复用");
    if (m_entries(m)[slot].state != M_USED) m_set_count(m, m_count(m) + 1);
    m_entries(m)[slot].key   = k;
    m_entries(m)[slot].val   = v;
    m_entries(m)[slot].state = M_USED;
    return m;
}

int64_t dial_map_get(DialMap mh, int64_t k) {
    int64_t i = m_find((void *)mh, k);
    return i < 0 ? -1 : m_entries((void *)mh)[i].val;
}

/* v1.04 C6：命中返回真实值，未命中返回调用方显式给出的默认值。
 * 与 contains+get 的两次查找不同，这里只做一次探测，且语义上把「没找到」
 * 和「找到了 -1」彻底分开。默认值由调用方在源码里写死，不引入任何隐式哨兵。 */
int64_t dial_map_get_or(DialMap mh, int64_t k, int64_t def) {
    int64_t i = m_find((void *)mh, k);
    return i < 0 ? def : m_entries((void *)mh)[i].val;
}

int64_t dial_map_contains(DialMap mh, int64_t k) {
    return m_find((void *)mh, k) >= 0 ? 1 : 0;
}

int64_t dial_map_remove(DialMap mh, int64_t k) {
    void *m = (void *)mh;
    int64_t i = m_find(m, k);
    if (i < 0) return -1;
    int64_t val = m_entries(m)[i].val;
    /* 只置墓碑，绝不左移 —— 左移会让哈希位置靠后的条目从探测链上消失。 */
    m_entries(m)[i].state = M_DELETED;
    m_set_count(m, m_count(m) - 1);
    return val;
}

int64_t dial_map_size(DialMap mh) {
    DIAL_ASSERT(m_count((void *)mh) >= 0 && m_count((void *)mh) <= m_cap((void *)mh),
                "map 内部损坏：count 超出 [0, cap]");
    return m_count((void *)mh);
}

/* 快照当前 map 的所有 key 为 int 数组（kind=INT）。遍历用，迭代期间 map 不被修改则语义确定。 */
void *dial_map_keys(DialMap mh) {
    void *m = (void *)mh;
    int64_t cap = m_cap(m);
    DialArray keys = dial_array_new(0); /* kind=INT */
    for (int64_t i = 0; i < cap; i++) {
        MEntry *e = &m_entries(m)[i];
        if (e->state == M_USED) keys = dial_array_push(keys, e->key);
    }
    return keys;
}

void dial_map_retain(DialMap mh)  { dial_rc_retain((void *)mh); }
void dial_map_release(DialMap mh) { dial_rc_release((void *)mh); }
