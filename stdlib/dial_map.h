#ifndef DIAL_MAP_H
#define DIAL_MAP_H

#include <stdint.h>

/* 极简哈希表：int64 -> int64（线性探测 + 紧凑数组，v0.4 演示用）。
 * 不透明指针直接是 C 侧 Map*（refcount 布局与 String / Array 一致：
 * [-8] 处为引用计数，Dial 看到的 payload 起始处存放 [cap][count][entries...]）。
 *
 * 与 String / Array 一样走 ARC：dial_map_retain / dial_map_release 操作其前导
 * 引用计数；put 在“被多者共享”时先 COW 克隆再修改，与 array.set 行为一致。
 * 这完全契合 README §18「内存宪法」：确定性、零 GC 停顿、可控的所有权管理。 */
typedef void *DialMap;

DialMap  dial_map_new(void);
DialMap  dial_map_put(DialMap m, int64_t key, int64_t val); /* 返回（可能更新后的）指针 */
int64_t  dial_map_get(DialMap m, int64_t key);              /* 未命中返回 -1 */
int64_t  dial_map_contains(DialMap m, int64_t key);         /* 命中返回 1，否则 0 */
int64_t  dial_map_remove(DialMap m, int64_t key);           /* 移除并返回其值，未命中返回 -1 */
int64_t  dial_map_size(DialMap m);

void dial_map_retain(DialMap m);
void dial_map_release(DialMap m);

#endif /* DIAL_MAP_H */
