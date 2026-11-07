#ifndef DIAL_RT_MEM_H
#define DIAL_RT_MEM_H

#include <stdint.h>
#include <stdlib.h>

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

#endif /* DIAL_RT_MEM_H */
