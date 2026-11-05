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
/* v1.00：切片 [from, to)，返回同 kind 的新数组；句柄元素逐个 retain。 */
DialArray dial_array_slice(DialArray a, int64_t from, int64_t to);
/* 移除下标 i 的元素 / 弹出末尾元素：返回「可能更新后的」数组指针（共享时 COW 克隆后移位）。
 * 被删元素的所有权移交给调用方（调用方应先 get 取出并 retain），本函数不返回它、也不释放它。
 * 越界 / 空数组 pop 当场 Fail-Fast（dial_assert_fail）。 */
DialArray dial_array_remove(DialArray a, int64_t i);
DialArray dial_array_pop(DialArray a);

/* 升序排序（v1.01）：被多者共享时先 COW 克隆再原地排序，返回（可能更新后的）指针。
 * 仅对标量元素有意义：int/float/byte/char/bool 按数值升序；float 按 IEEE754 数值序
 * （而非位模式，负数排序正确）。句柄元素（string 等）排序语义未定义，调用前已被编译器拒绝。 */
DialArray dial_array_sort(DialArray a);

/* v1.03：接受回调的高阶数组操作（map / filter / sort_by）统一入口。
 *
 * op: 0=map（用回调返回值构造新数组）1=filter（回调返回非 0 才保留）2=sort_by（原地排序）。
 * out_kind: map 结果数组的元素类别码（filter / sort_by 忽略）。
 * fn_raw: Dial 函数指针（纯地址）；state: 用户显式传入的 rawptr<void> 状态。
 * thunk: 由编译器按**精确回调签名**生成的适配函数，C 侧只透传位模式与两个指针，
 *        完全不知道回调的参数/返回类型 —— 没有隐藏类型标签、没有装箱、没有运行期分派。
 *
 * 仅支持标量元素（int/float/bool/byte/char）：新数组元素不需要 ARC 所有权结算；
 * 句柄元素由编译器在语义层直接拒绝。 */
DialArray dial_array_hof(DialArray a, int64_t op, int64_t out_kind,
                         void *fn_raw, void *state,
                         int64_t (*thunk)(int64_t, int64_t, void *, void *));

DialArray dial_array_clone(DialArray a);
void dial_array_retain(DialArray a);
void dial_array_release(DialArray a);

#endif /* DIAL_ARRAY_H */
