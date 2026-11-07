#include "dial_kvmap.h"
#include "dial_array.h"
#include "dial_string.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>
#include <stdlib.h>

/*
 * map<K,V> 的 ARC 实现（v1.00）。与 int→int 的 dial_map.c 同构但独立：
 *
 *   payload: [int64 key_kind][int64 val_kind][int64 cap][int64 count][entries...]
 *   前 8 字节为引用计数（dial_rt_mem.h 契约）。
 *   每个 entry 24 字节：{ int64 key_bits; int64 val_bits; int64 state; }
 *   state: 0 = EMPTY, 1 = USED, 2 = DELETED（墓碑）
 *
 * 为什么用墓碑而不是「左移紧凑」：开放寻址哈希表删除时把后续条目直接前移，
 * 会把「理想哈希位置在前移目标之后」的条目挪到探测路径之前，令其**永远查不到**。
 * 2 万键压测里删除 1 万个后 size 只剩 13082（而非 10000），正是这个经典陷阱。
 * 墓碑让探测链保持连续；墓碑累积到负载阈值时原地重建（清墓碑），不影响正确性。
 *
 * 所有权不变量（逐条对应 README 内存宪法）：
 *   - 每个 USED 条目里的 STRING 键/值都由**本表**持有一份引用；
 *   - clone / 重建得到新 buffer 时，新 buffer 必须逐条目 retain 一份，
 *     旧 buffer 在最后一个持有者时逐条目 release；
 *   - put 消耗调用者交来的 key/val 那一份引用（C 侧 retain，编译器对
 *     自带所有权的字面量再 release 一份抵消），覆盖时释放旧条目的引用；
 *   - remove 释放被删条目的键引用，值引用随返回值原样移交（V 为标量，无 ARC）。
 *
 * 绝不追踪式 GC；每次释放都是编译期可确定的、对称的 retain/release。
 */

enum { KV_INT = 0, KV_FLOAT = 1, KV_STRING = 2, KV_BOOL = 3, KV_CHAR = 4, KV_BYTE = 5,
       /* v1.04 B6：结构体值——值类型装不进 8 字节槽，故装箱为 RC 对象后存其指针位模式，
        * 与 KV_STRING 同走 retain/release（数组/表释放时逐条目释放装箱对象）。 */
       KV_STRUCT = 6 };
enum { KV_EMPTY = 0, KV_USED = 1, KV_DELETED = 2 };

typedef struct { int64_t key; int64_t val; int64_t state; } KEntry;

static int64_t *k_meta(void *m) { return (int64_t *)m; }
static KEntry  *k_ents(void *m) { return (KEntry *)((char *)m + 32); }
static int64_t  k_kind(void *m) { return k_meta(m)[0]; }
static int64_t  v_kind(void *m) { return k_meta(m)[1]; }
static int64_t  k_cap(void *m)  { return k_meta(m)[2]; }
static int64_t  k_count(void *m){ return k_meta(m)[3]; }
static void     k_set_count(void *m, int64_t c) { k_meta(m)[3] = c; }
static int64_t  k_size(int64_t cap) { return 32 + cap * (int64_t)sizeof(KEntry); }

static int64_t f64_bits(double d) { union { double f; int64_t i; } u; u.f = d; return u.i; }
static double  bits_f64(int64_t i) { union { double f; int64_t i; } u; u.i = i; return u.f; }

static void kv_retain_kind(int64_t kind, int64_t bits) {
    if (kind == KV_STRING || kind == KV_STRUCT) dial_rc_retain((void *)(intptr_t)bits);
}
static void kv_release_kind(int64_t kind, int64_t bits) {
    if (kind == KV_STRING || kind == KV_STRUCT) dial_rc_release((void *)(intptr_t)bits);
}

static void kv_release_entries(void *m) {
    int64_t kk = k_kind(m), vk = v_kind(m), cap = k_cap(m);
    KEntry *e = k_ents(m);
    for (int64_t i = 0; i < cap; i++) {
        if (e[i].state != KV_USED) continue;
        kv_release_kind(kk, e[i].key);
        kv_release_kind(vk, e[i].val);
    }
}

/* 最后一个持有者离开时：逐条目释放，再回收 buffer。 */
static void kv_release_buffer(void *m) {
    if (!m) return;
    if (((int64_t *)m)[-1] > 1) { dial_rc_release(m); return; }
    kv_release_entries(m);
    dial_rc_release(m);
}

static int64_t kv_hash(int64_t kind, int64_t bits, int64_t cap) {
    if (kind == KV_STRING) {
        const char *s = dial_string_cstr((void *)(intptr_t)bits);
        int64_t len = dial_string_length((void *)(intptr_t)bits);
        uint64_t h = 1469598103934665603ULL;   /* FNV-1a */
        for (int64_t i = 0; i < len; i++) {
            h ^= (unsigned char)s[i];
            h *= 1099511628211ULL;
        }
        return (int64_t)(h % (uint64_t)cap);
    }
    uint64_t h = (uint64_t)bits;
    if (kind == KV_FLOAT) {
        h ^= h >> 33;
        h *= 0xff51afd7ed558ccdULL;
        h ^= h >> 33;
    } else if (bits < 0) {
        h = (~h) + 1;                          /* 绝对值，INT_MIN 也不溢出 */
    }
    return (int64_t)(h % (uint64_t)cap);
}

static int kv_eq(int64_t kind, int64_t a, int64_t b) {
    if (kind == KV_STRING)
        return dial_string_eq((void *)(intptr_t)a, (void *)(intptr_t)b) != 0;
    if (kind == KV_FLOAT) return bits_f64(a) == bits_f64(b);
    return a == b;
}

static int64_t kv_default(int64_t vk) {
    switch (vk) {
        case KV_FLOAT:  return f64_bits(-1.0);
        case KV_BOOL:   return 0;
        case KV_STRING: return (int64_t)(intptr_t)dial_string_new("");
        /* v1.04 B6：结构体值未命中返回 0（空指针）——由编译器还原为零值结构体，
         * 绝不解引用空指针（那会让「未命中」变成段错误而非可预期的值）。 */
        case KV_STRUCT: return 0;
        default:        return -1;   /* int / char / byte 未命中返回 -1 */
    }
}

/* 查找键：遇 EMPTY 停止（墓碑不终止探测），返回槽位或 -1。 */
static int64_t kv_find(void *m, int64_t key) {
    int64_t cap = k_cap(m);
    int64_t i = kv_hash(k_kind(m), key, cap);
    for (int64_t n = 0; n < cap; n++) {
        KEntry *e = &k_ents(m)[i];
        if (e->state == KV_EMPTY) return -1;
        if (e->state == KV_USED && kv_eq(k_kind(m), e->key, key)) return i;
        i = (i + 1) % cap;
    }
    return -1;
}

/* 为新键找可写槽：优先复用第一个墓碑；遇 EMPTY 立即返回该槽（或已记下的墓碑）。 */
static int64_t kv_insert_slot(void *m, int64_t key) {
    int64_t cap = k_cap(m);
    int64_t i = kv_hash(k_kind(m), key, cap);
    int64_t tomb = -1;
    for (int64_t n = 0; n < cap; n++) {
        KEntry *e = &k_ents(m)[i];
        if (e->state == KV_EMPTY) return tomb >= 0 ? tomb : i;
        if (e->state == KV_DELETED && tomb < 0) tomb = i;
        i = (i + 1) % cap;
    }
    return tomb;
}

/* 重建到 ncap（ncap > cap 即扩容，ncap == cap 即原地清墓碑）。
 * 只搬运 USED 条目；新 buffer 取得各条目的所有权，旧 buffer 交给 kv_release_buffer。 */
static void *kv_rebuild(void *m, int64_t ncap) {
    void *nm = dial_rc_alloc(k_size(ncap));
    k_meta(nm)[0] = k_kind(m);
    k_meta(nm)[1] = v_kind(m);
    k_meta(nm)[2] = ncap;
    k_meta(nm)[3] = 0;
    for (int64_t i = 0; i < ncap; i++) k_ents(nm)[i].state = KV_EMPTY;
    for (int64_t i = 0; i < k_cap(m); i++) {
        KEntry *e = &k_ents(m)[i];
        if (e->state != KV_USED) continue;
        int64_t j = kv_hash(k_kind(nm), e->key, ncap);
        while (k_ents(nm)[j].state == KV_USED) j = (j + 1) % ncap;
        k_ents(nm)[j].key = e->key;
        k_ents(nm)[j].val = e->val;
        k_ents(nm)[j].state = KV_USED;
        k_set_count(nm, k_count(nm) + 1);
        kv_retain_kind(k_kind(nm), e->key);
        kv_retain_kind(v_kind(nm), e->val);
    }
    kv_release_buffer(m);
    return nm;
}

static int64_t kv_count_deleted(void *m) {
    int64_t n = 0;
    for (int64_t i = 0; i < k_cap(m); i++)
        if (k_ents(m)[i].state == KV_DELETED) n++;
    return n;
}

DialKvMap dial_kvmap_new(int64_t key_kind, int64_t val_kind) {
    /* 7 种 kind（0..6）对键与值都合法；string 值由编译期 check_type_wellformed
     * 拦下（v1.00 不开放），C 侧保持对称以免把 bool/char/byte 值误判为非法。
     * v1.04 B6：新增 KV_STRUCT(6)——结构体值装箱为 RC 对象后存指针位模式。 */
    DIAL_ASSERT(key_kind >= KV_INT && key_kind <= KV_STRUCT &&
                val_kind >= KV_INT && val_kind <= KV_STRUCT,
                "dial_kvmap_new: 非法的键/值 kind");
    int64_t cap = 8;
    void *m = dial_rc_alloc(k_size(cap));
    k_meta(m)[0] = key_kind;
    k_meta(m)[1] = val_kind;
    k_meta(m)[2] = cap;
    k_meta(m)[3] = 0;
    for (int64_t i = 0; i < cap; i++) k_ents(m)[i].state = KV_EMPTY;
    return m;
}

DialKvMap dial_kvmap_put(DialKvMap mh, int64_t key, int64_t val) {
    void *m = (void *)mh;
    int64_t cap = k_cap(m);

    /* 1) 负载（含墓碑）过高：真实条目也高则扩容，否则原地重建清墓碑。 */
    int64_t used = k_count(m);
    int64_t deleted = kv_count_deleted(m);
    if (used + deleted >= cap * 3 / 4) {
        int64_t ncap = (used >= cap * 3 / 4) ? cap * 2 : cap;
        m = kv_rebuild(m, ncap);
        cap = k_cap(m);
    }

    /* 2) 仍被多者共享 -> COW 克隆，避免污染其他持有者。 */
    if (((int64_t *)m)[-1] > 1) {
        void *nm = dial_rc_clone(m, k_size(cap));
        for (int64_t i = 0; i < cap; i++) {
            KEntry *e = &k_ents(nm)[i];
            if (e->state == KV_USED) {
                kv_retain_kind(k_kind(nm), e->key);
                kv_retain_kind(v_kind(nm), e->val);
            }
        }
        dial_rc_release(m);   /* 只减引用：另一持有者仍持有旧 buffer 的条目 */
        m = nm;
    }

    /* 3) 取得调用者交来的 key/val 引用。 */
    kv_retain_kind(k_kind(m), key);
    kv_retain_kind(v_kind(m), val);

    int64_t hit = kv_find(m, key);
    if (hit >= 0) {
        /* 命中更新：释放旧条目引用，整体替换为新值。 */
        kv_release_kind(k_kind(m), k_ents(m)[hit].key);
        kv_release_kind(v_kind(m), k_ents(m)[hit].val);
        k_ents(m)[hit].key = key;
        k_ents(m)[hit].val = val;
        k_ents(m)[hit].state = KV_USED;
        return m;
    }

    int64_t slot = kv_insert_slot(m, key);
    DIAL_ASSERT(slot >= 0, "map<K,V> 内部损坏：哈希表已满且无墓碑可复用");
    if (k_ents(m)[slot].state != KV_USED) k_set_count(m, k_count(m) + 1);
    k_ents(m)[slot].key = key;
    k_ents(m)[slot].val = val;
    k_ents(m)[slot].state = KV_USED;
    return m;
}

int64_t dial_kvmap_get(DialKvMap mh, int64_t key) {
    void *m = (void *)mh;
    int64_t i = kv_find(m, key);
    if (i < 0) return kv_default(v_kind(m));
    return k_ents(m)[i].val;
}

/* v1.04 C6：命中返回真实值的位模式，未命中返回调用方给的默认值位模式。
 * 与 dial_map_get_or 同款「一次探测」，把「没找到」与「找到了默认值」分开。
 * 只服务标量 V；句柄 V 由语义层拦下，不进入这里（见头文件注释）。 */
int64_t dial_kvmap_get_or(DialKvMap mh, int64_t key, int64_t def_bits) {
    void *m = (void *)mh;
    int64_t i = kv_find(m, key);
    return i < 0 ? def_bits : k_ents(m)[i].val;
}

int64_t dial_kvmap_contains(DialKvMap mh, int64_t key) {
    return kv_find((void *)mh, key) >= 0 ? 1 : 0;
}

int64_t dial_kvmap_remove(DialKvMap mh, int64_t key) {
    void *m = (void *)mh;
    int64_t i = kv_find(m, key);
    if (i < 0) return kv_default(v_kind(m));

    int64_t val = k_ents(m)[i].val;
    /* 被删条目的键引用归零；值引用随返回值移交（V 恒为标量，无 ARC）。
     * 只置墓碑，绝不左移 —— 左移会让哈希位置靠后的条目从探测链上消失。 */
    kv_release_kind(k_kind(m), k_ents(m)[i].key);
    k_ents(m)[i].state = KV_DELETED;
    k_set_count(m, k_count(m) - 1);
    return val;
}

int64_t dial_kvmap_size(DialKvMap mh) {
    void *m = (void *)mh;
    DIAL_ASSERT(k_count(m) >= 0 && k_count(m) <= k_cap(m),
                "map<K,V> 内部损坏：count 超出 [0, cap]");
    return k_count(m);
}

void *dial_kvmap_keys(DialKvMap mh) {
    void *m = (void *)mh;
    int64_t kk = k_kind(m);
    DialArray out = dial_array_new(kk == KV_STRING ? 1 : 0);
    for (int64_t i = 0; i < k_cap(m); i++) {
        KEntry *e = &k_ents(m)[i];
        if (e->state == KV_USED) out = dial_array_push(out, e->key);
    }
    return out;
}

void *dial_kvmap_values(DialKvMap mh) {
    void *m = (void *)mh;
    int64_t vk = v_kind(m);
    DialArray out = dial_array_new((vk == KV_STRING || vk == KV_STRUCT) ? 1 : 0);
    for (int64_t i = 0; i < k_cap(m); i++) {
        KEntry *e = &k_ents(m)[i];
        if (e->state == KV_USED) out = dial_array_push(out, e->val);
    }
    return out;
}

void dial_kvmap_retain(DialKvMap m)  { dial_rc_retain((void *)m); }
void dial_kvmap_release(DialKvMap m) { kv_release_buffer((void *)m); }
