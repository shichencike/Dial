#ifndef DIAL_ARRAY_H
#define DIAL_ARRAY_H

#include <stdint.h>

/*
 * 不透明引用计数动态数组。Dial 侧只持有 i8*。
 * payload 布局：[ int64 kind ][ int64 len ][ int64 cap ][ int64 elems[] ]，
 * 前导 int64 refcount 由 dial_rc_* 管理。
 *
 *   kind: 0 = INT（标量 int）  |  1 = RC（string / int[] / map / error / 不透明指针）
 *         2 = ATOMIC（channel，原子引用计数）
 *
 * v0.98.2 起元素类型不再限于 int：只要是 8 字节指针（句柄）或 int，就能直接存。
 * 数组在头部记着自己元素的类别，push / set / 克隆 / 释放时据此对元素做 retain / release，
 * 从而正确实现「按元素 ARC」（见 dial_array.c）。
 *
 * 注意：push / set 在需要扩容或被多者共享时，会返回“新的”不透明指针，
 * Dial 编译器必须把返回值写回原变量（见 codegen 的 AssignIndex / 方法调用处理）。
 */

typedef void *DialArray;

/* kind：0=INT 1=RC 2=ATOMIC（见 dial_array.c 的 DIAL_ELEM_* 宏） */
DialArray dial_array_new(int64_t kind);
DialArray dial_array_push(DialArray a, int64_t v);   /* 返回（可能更新后的）指针 */
int64_t  dial_array_get(DialArray a, int64_t i);
DialArray dial_array_set(DialArray a, int64_t i, int64_t v); /* 被共享时内部触发 COW，返回新指针 */

/* 嵌套下标赋值专用：**不消耗** a 的引用（a 是从外层数组元素槽借来的），
 * 返回一份「新拥有」的引用，由调用方用完后自行 release。 */
DialArray dial_array_set_borrowed(DialArray a, int64_t i, int64_t v);
int64_t  dial_array_length(DialArray a);

DialArray dial_array_clone(DialArray a);
void dial_array_retain(DialArray a);
void dial_array_release(DialArray a);

#endif /* DIAL_ARRAY_H */
