#ifndef DIAL_ARRAY_H
#define DIAL_ARRAY_H

#include <stdint.h>

/*
 * Array<int> 不透明引用计数动态数组。Dial 侧只持有 i8*。
 * payload 布局：[ int64 len ][ int64 cap ][ int64 elems[] ]（元素暂定为 int64）。
 *
 * 注意：push / set 在需要扩容或被多者共享时，会返回“新的”不透明指针，
 * Dial 编译器必须把返回值写回原变量（见 codegen 的 AssignIndex / 方法调用处理）。
 */

typedef void *DialArray;

DialArray dial_array_new(void);
DialArray dial_array_push(DialArray a, int64_t v);   /* 返回（可能更新后的）指针 */
int64_t  dial_array_get(DialArray a, int64_t i);
DialArray dial_array_set(DialArray a, int64_t i, int64_t v); /* 被共享时内部触发 COW，返回新指针 */
int64_t  dial_array_length(DialArray a);

DialArray dial_array_clone(DialArray a);
void dial_array_retain(DialArray a);
void dial_array_release(DialArray a);

#endif /* DIAL_ARRAY_H */
