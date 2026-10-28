#ifndef DIAL_MAP_H
#define DIAL_MAP_H

#include <stdint.h>

/* 极简哈希表：int64 -> int64（开放寻址，v0.2 演示用）。
 * 不透明指针直接是 C 侧 Map*（非引用计数，进程退出由 OS 回收，符合“无 GC”定位）。 */
typedef void *DialMap;

DialMap  dial_map_new(void);
void     dial_map_put(DialMap m, int64_t key, int64_t val);
int64_t  dial_map_get(DialMap m, int64_t key);  /* 未命中返回 -1 */

#endif /* DIAL_MAP_H */
