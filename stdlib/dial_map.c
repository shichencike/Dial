#include "dial_map.h"
#include "dial_rt_mem.h"
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
 * 每个 entry 24 字节：{ int64_t key; int64_t val; int64_t used; }
 * （用 int64_t used 而非 int8_t，是为了保持 8 字节对齐，方便整块 memcpy / 扩容复制）
 *
 * 线性探测（linear probing）开放寻址；删除采用「左移紧凑」，维护簇连续性。
 *
 * 内存宪法契合点：
 *   - 绝不追踪式 GC；所有权由 ARC（dial_map_retain / dial_map_release）显式管理。
 *   - put 在「被多者共享」时先 COW 克隆再修改（与 array.set 一致），并把新指针
 *     交还编译器写回接收变量；若无需拷贝（唯一持有）则原地修改，O(1) 零拷贝。
 *   - remove/get/contains/size 只读或就地修改，不更换指针（remove 在共享场景下会
 *     一并影响所有持有者——如需隔离请先 .clone()）。
 */

typedef struct { int64_t key; int64_t val; int64_t used; } MEntry;

static MEntry *m_entries(void *m) { return (MEntry *)((char *)m + 16); }
static int64_t m_cap(void *m)    { return ((int64_t *)m)[0]; }
static int64_t m_count(void *m)  { return ((int64_t *)m)[1]; }
static void m_set_cap(void *m, int64_t c)   { ((int64_t *)m)[0] = c; }
static void m_set_count(void *m, int64_t c) { ((int64_t *)m)[1] = c; }

static int64_t m_hash(int64_t k, int64_t cap) {
    int64_t h = k < 0 ? -k : k;
    return h % cap;
}

/* 把 (k,v) 插入到已分配好的 buffer nm/ncap 的探测空位（不查重、不改 cap） */
static void m_reinsert(void *nm, int64_t ncap, int64_t k, int64_t v) {
    int64_t i = m_hash(k, ncap);
    while (m_entries(nm)[i].used) i = (i + 1) % ncap;
    m_entries(nm)[i].key   = k;
    m_entries(nm)[i].val   = v;
    m_entries(nm)[i].used  = 1;
    m_set_count(nm, m_count(nm) + 1);
}

DialMap dial_map_new(void) {
    int64_t cap = 8;
    void *m = dial_rc_alloc(16 + cap * 24);
    m_set_cap(m, cap);
    m_set_count(m, 0);
    for (int64_t i = 0; i < cap; i++) m_entries(m)[i].used = 0;
    return m;
}

DialMap dial_map_put(DialMap mh, int64_t k, int64_t v) {
    void *m = (void *)mh;
    int64_t cap   = m_cap(m);
    int64_t count = m_count(m);

    /* 1) 负载过高 -> 扩容并重新哈希到更大的 buffer。
     *    新 buffer 由 dial_rc_alloc 分配（refcount==1），天然唯一持有，无需后续 COW。 */
    if (count >= cap * 3 / 4) {
        int64_t ncap = cap < 8 ? 8 : cap * 2;
        void *nm = dial_rc_alloc(16 + ncap * 24);
        m_set_cap(nm, ncap);
        m_set_count(nm, 0);
        for (int64_t i = 0; i < ncap; i++) m_entries(nm)[i].used = 0;
        for (int64_t i = 0; i < cap; i++) {
            MEntry *e = &m_entries(m)[i];
            if (e->used) m_reinsert(nm, ncap, e->key, e->val);
        }
        dial_rc_release(m);   /* 释放旧 buffer */
        m   = nm;
        cap = ncap;
    }

    /* 2) 仍被多者共享 -> COW 克隆，避免污染其他持有者（与 array.set 一致） */
    if (((int64_t *)m)[-1] > 1) {
        void *nm = dial_rc_clone(m, 16 + cap * 24);
        dial_rc_release(m);
        m = nm;
    }

    /* 3) 写入：命中则更新值，未命中则追加 */
    int64_t i = m_hash(k, cap);
    while (m_entries(m)[i].used) {
        if (m_entries(m)[i].key == k) { m_entries(m)[i].val = v; return m; }
        i = (i + 1) % cap;
    }
    m_entries(m)[i].key  = k;
    m_entries(m)[i].val  = v;
    m_entries(m)[i].used = 1;
    m_set_count(m, m_count(m) + 1);
    return m;
}

int64_t dial_map_get(DialMap mh, int64_t k) {
    void *m = (void *)mh;
    int64_t cap = m_cap(m);
    int64_t i = m_hash(k, cap);
    while (m_entries(m)[i].used) {
        if (m_entries(m)[i].key == k) return m_entries(m)[i].val;
        i = (i + 1) % cap;
    }
    return -1;   /* 未命中 */
}

int64_t dial_map_contains(DialMap mh, int64_t k) {
    void *m = (void *)mh;
    int64_t cap = m_cap(m);
    int64_t i = m_hash(k, cap);
    while (m_entries(m)[i].used) {
        if (m_entries(m)[i].key == k) return 1;
        i = (i + 1) % cap;
    }
    return 0;
}

int64_t dial_map_remove(DialMap mh, int64_t k) {
    void *m = (void *)mh;
    int64_t cap = m_cap(m);
    int64_t i = m_hash(k, cap);

    /* 定位目标槽 */
    int64_t target = -1;
    while (m_entries(m)[i].used) {
        if (m_entries(m)[i].key == k) { target = i; break; }
        i = (i + 1) % cap;
    }
    if (target < 0) return -1;   /* 未命中 */

    int64_t val = m_entries(m)[target].val;

    /* 左移紧凑：把 target 之后同簇的条目前移填洞，直到遇到空槽 */
    int64_t j = target;
    while (1) {
        int64_t nxt = (j + 1) % cap;
        if (!m_entries(m)[nxt].used) break;
        m_entries(m)[j] = m_entries(m)[nxt];
        j = nxt;
    }
    m_entries(m)[j].used = 0;
    m_set_count(m, m_count(m) - 1);
    return val;
}

int64_t dial_map_size(DialMap mh) { return m_count((void *)mh); }

void dial_map_retain(DialMap mh)  { dial_rc_retain((void *)mh); }
void dial_map_release(DialMap mh) { dial_rc_release((void *)mh); }
