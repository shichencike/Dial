#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>

void *dial_rc_alloc(int64_t payload_size) {
    char *blk = (char *)malloc((size_t)(8 + payload_size));
    /* 分配失败绝不返回 NULL 让上层“带病运行”——那是静默损坏的开端。 */
    if (!blk) dial_assert_fail("内存分配失败（OOM）");
    *(int64_t *)blk = 1;       /* refcount = 1 */
    return blk + 8;            /* 不透明指针指向 payload */
}

void dial_rc_retain(void *p) {
    if (!p) return;
    /* 静态 immortal 对象（如 map<string> 未命中的空串）：refcount 是哨兵，
     * 不是真引用——必须先于 DIAL_HEAP_ASSERT(rc>0)短路，否则哨兵负数会误炸。 */
    if (((int64_t *)p)[-1] == DIAL_RC_IMMORTAL) return;
    /*
     * refcount <= 0 说明这块内存已经被释放过了（use-after-free）。
     * C 会让它“看起来还能用”，Dial 在这里立刻炸。
     */
    DIAL_HEAP_ASSERT(((int64_t *)p)[-1] > 0, "retain 了已释放的对象（use-after-free）");
    ((int64_t *)p)[-1] += 1;
}

void dial_rc_release(void *p) {
    if (!p) return;
    /* 静态 immortal 对象：永不释放（否则会 free 静态存储区 → 崩溃）。 */
    if (((int64_t *)p)[-1] == DIAL_RC_IMMORTAL) return;
    /* 多释放一次（double free / 引用计数下溢）同样当场终止。 */
    DIAL_HEAP_ASSERT(((int64_t *)p)[-1] > 0, "release 了引用计数已归零的对象（double free）");
    if (--((int64_t *)p)[-1] == 0) free((char *)p - 8);
}

// v1.04 B6：把「值类型结构体」装箱为引用计数对象，用于放进容器（数组 / map 值）。
// 布局与 Dial 堆对象一致：[int64 refcount][payload...]，payload 起始处即对外指针。
// 仅拷贝标量位模式；若结构体含句柄字段，调用处须额外 retain 各句柄字段
// （本期仅放开「字段全为标量」的结构体，含句柄字段的结构体在语义层拒绝）。
void *dial_struct_box(const void *src, int64_t size) {
    void *p = dial_rc_alloc(size);
    memcpy(p, src, (size_t)size);
    return p;
}

/* ---- 字段级 ARC 装箱（v1.21）------------------------------------------- */
/* 各句柄字段的 release 由 stdlib 提供（链接期解析）。用通用 dial_rc_release
 * 之外的专用入口是为了尊重 immortal 短路与 channel 的原子 rc。 */
extern void dial_string_release(void *);
extern void dial_bytes_release(void *);
extern void dial_array_release(void *);
extern void dial_map_release(void *);
extern void dial_kvmap_release(void *);
extern void dial_error_release(void *);
extern void dial_channel_release(void *);

/* 描述符 kind（与编译器 ir.rs::ArcKind 的判别值一致）：
 * 1=String 2=Bytes 3=Array 4=Error 5=Generic(map/set/kvmap) 6=Channel */
#define DAF_STRING  1
#define DAF_BYTES   2
#define DAF_ARRAY   3
#define DAF_ERROR   4
#define DAF_GENERIC 5
#define DAF_CHANNEL 6

void *dial_struct_box_arc(const void *src, int64_t size, const void *desc) {
    /* ARC box 头布局：[desc_ptr][rc][payload]，比普通 box 多 8 字节 desc 指针。
     * 关键：refcount 必须紧邻 payload（p[-1]），因为容器 COW / retain 走的是
     * 通用 dial_rc_retain(p)，它读 p[-1]。desc 放 p[-2]，只有专用 release 读它。
     * 若把 desc 放 p[-1]，通用 retain 会把 desc 指针 +1 —— 灾难。 */
    char *blk = (char *)malloc((size_t)(16 + size));
    if (!blk) dial_assert_fail("内存分配失败（OOM）");
    ((int64_t *)blk)[0] = (int64_t)desc;  /* p[-2] = desc */
    ((int64_t *)blk)[1] = 1;              /* p[-1] = rc = 1 */
    memcpy(blk + 16, src, (size_t)size);
    return blk + 16;                       /* payload 指针 */
}

void dial_struct_box_arc_release(void *p) {
    if (!p) return;
    int64_t *rcp = (int64_t *)p - 1;   /* rcp[0] = rc（p[-1]，与通用 retain 同一位置） */
    int64_t *descp = (int64_t *)p - 2; /* descp[0] = desc 指针（p[-2]） */
    /* 静态 immortal 短路（ARC box 正常不会是 immortal，但保持与通用 release 同 idiom）。 */
    if (rcp[0] == DIAL_RC_IMMORTAL) return;
    DIAL_HEAP_ASSERT(rcp[0] > 0, "release 了引用计数已归零的 ARC box（double free）");
    if (--rcp[0] != 0) return;

    const int64_t *desc = (const int64_t *)descp[0];
    if (desc) {
        int64_t n = desc[0];
        const DialArcFieldDesc *f = (const DialArcFieldDesc *)(desc + 1);
        for (int64_t i = 0; i < n; i++) {
            void *h = *(void **)((char *)p + f[i].offset);
            if (!h) continue;
            switch (f[i].kind) {
                case DAF_STRING:  dial_string_release(h);  break;
                case DAF_BYTES:   dial_bytes_release(h);   break;
                case DAF_ARRAY:   dial_array_release(h);   break;
                case DAF_ERROR:   dial_error_release(h);   break;
                case DAF_GENERIC: dial_rc_release(h);      break;
                case DAF_CHANNEL: dial_channel_release(h); break;
                default: break;
            }
        }
    }
    free((char *)p - 16);
}

void *dial_rc_clone(void *p, int64_t payload_size) {
    if (!p) return NULL;
    // 静态 immortal 对象不可变：原样返回自身（配合 dial_string_clone 的
    // retain→clone→release 组合——三步全 no-op，仍返回这唯一的静态串）。
    if (((int64_t *)p)[-1] == DIAL_RC_IMMORTAL) return p;
    // 与 retain/release 对齐：传入已是 freed 的指针（refcount <= 0）时立刻终止，
    // 而非去读 ((int64_t*)p)[-1] 这块已归还给分配器的内存（那是 use-after-free）。
    DIAL_HEAP_ASSERT(((int64_t *)p)[-1] > 0, "clone 了已释放的对象（use-after-free）");
    // 纯粹的 COW 克隆：
    //   - 唯一持有者（refcount==1）：直接返回同一块内存，**不改动引用计数**；
    //   - 多个持有者（refcount>1）：深拷贝出一份独立副本（引用计数为 1）。
    //
    // 这里刻意**不**再 +1：克隆是「借用者的只读视图」，本身不产生新的所有权。
    // 真正需要「一份独立副本」的调用方（各类型的 clone() 方法）在自己的 C 实现里
    // 用「先 retain 再 clone 再 release」强制走进深拷贝分支——那样既拿到独立副本，
    // 又不会像过去那样给原对象凭空 +1（那会让 COW 守卫 refcount>1 恒真）。
    if (((int64_t *)p)[-1] == 1) {
        return p;
    }
    void *n = dial_rc_alloc(payload_size);
    memcpy(n, p, (size_t)payload_size);
    return n;
}
