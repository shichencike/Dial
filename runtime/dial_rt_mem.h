#ifndef DIAL_RT_MEM_H
#define DIAL_RT_MEM_H

#include <stdint.h>
#include <stdlib.h>

/*
 * 静态 immortal 对象的 refcount 哨兵（v1.10，map<string> 值支持）。
 * 合法 refcount 恒 > 0；INT64_MIN 落在合法区间之外，专用于进程级共享的
 * 不可变对象（当前仅 map<K,string> 未命中的空串）。dial_rc_retain /
 * dial_rc_release / dial_rc_clone 三处对该哨兵一律 no-op，使这类对象
 * "永不释放、永不泄漏、可被安全借用"，命中/未命中在两端 codegen 看来
 * 统一为借用语义，无需任何表达式级临时对象结算。
 * 注意：no-op 判断必须在 DIAL_HEAP_ASSERT(rc > 0) 之前（哨兵为负数）。
 */
#define DIAL_RC_IMMORTAL INT64_MIN

/*
 * dialc 引用计数堆对象布局（C Runtime 内部契约）：
 *
 *     [ int64 refcount ][ payload ... ]
 *                          ^
 *                          |  Dial 语言看到的“不透明指针”(opaque pointer)
 *
 * 任何由 Dial 堆对象（String / Array）对外暴露的指针都指向 payload 起始处，
 * 其前方 8 字节是 refcount。这样 Dial 编译器只需要在 IR 里传 i8*，
 * 永远无法直接访问底层内存布局（写时复制 / 引用计数完全由 Runtime 控制）。
 */

/* 分配 payload_size 字节的引用计数对象，refcount 初始化为 1，返回 payload 指针 */
void *dial_rc_alloc(int64_t payload_size);

/* 引用 +1 */
void dial_rc_retain(void *p);

/* 引用 -1；归零时释放整个对象（含前导 refcount 头） */
void dial_rc_release(void *p);

/*
 * 写时复制（COW）感知的克隆：
 *   - 若对象当前唯一持有（refcount == 1），把引用计数 +1 后返回自身——
 *     它仍是零拷贝（不拷 payload），但“原对象”与“克隆”现在是两个独立持有者，
 *     所以**必须 +1**：否则 COW 守卫 `refcount > 1` 失效，一方就地修改会污染另一方，
 *     且任意一方的 release 都会提前把内存归还（double-free / use-after-free）。
 *     （这正是 CVE-2024-DIAL-003 的根因：旧实现漏了这步 +1。）
 *   - 若被共享（refcount > 1），深拷贝 payload_size 字节得到新对象（refcount 1）。
 * 这是“COW 兜底”的底层实现——只在静态分析失效（共享 buffer 被原地修改）时触发真实拷贝。
 */
void *dial_rc_clone(void *p, int64_t payload_size);

/* v1.04 B6：把值类型结构体装箱为引用计数对象（[int64 refcount][payload]），
 * 用于放进容器。仅拷贝标量位模式；含句柄字段的结构体须由调用处额外 retain。 */
void *dial_struct_box(const void *src, int64_t size);

/* ---------------------------------------------------------------------------
 * 字段级 ARC 装箱（v1.21）：含句柄字段（string / bytes / array / map / set /
 * kvmap / error / channel）的结构体进容器时使用。
 *
 * 普通 dial_struct_box 的 box 只管自己的 rc，容器释放它时仅 free 内存，
 * 不知道 payload 里的句柄字段也需要 release——那会泄漏（或原 struct 先退出
 * 时字段被提前释放导致悬垂）。为此扩展 box 头，携带一张「字段描述符表」：
 *
 *     普通 box:   [ int64 rc ][ payload ... ]
 *                               ^ p
 *     ARC  box:   [ desc* ][ int64 rc ][ payload ... ]
 *                                       ^ p
 *
 * 对外句柄仍指向 payload 起点；refcount 恒在 p[-1]（紧邻 payload），因此容器
 * COW / retain 走的通用 dial_rc_retain(p) 无需改动即正确。desc 指针在 p[-2]，
 * 只有「释放」需要专用入口 dial_struct_box_arc_release：rc 归零时先按 desc
 * 逐字段 release，再 free(p-16)。（注意 desc 绝不能放 p[-1]——通用 retain
 * 会把 p[-1] 当 rc +1，从而把 desc 指针加 1。）
 *
 * desc 表是编译器按 struct 类型生成的静态只读数据，布局：
 *     [ int64 nfields ][ (int64 kind, int64 offset) × nfields ]
 * kind 与 ArcKind 一致：1=String 2=Bytes 3=Array 4=Error 5=Generic(rc)
 *                       6=Channel(atomic)；offset 是字段相对 payload 起点的字节偏移。
 * ------------------------------------------------------------------------- */
typedef struct {
    int64_t kind;
    int64_t offset;
} DialArcFieldDesc;

void *dial_struct_box_arc(const void *src, int64_t size, const void *desc);
void dial_struct_box_arc_release(void *p);

#endif /* DIAL_RT_MEM_H */
