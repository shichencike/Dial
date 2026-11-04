#ifndef DIAL_KVMAP_H
#define DIAL_KVMAP_H

#include <stdint.h>

/*
 * 泛型哈希表 map<K,V>（v1.00）。与 int→int 的 DialMap **完全独立**：
 * 老 map 一个字节都不改，新类型自带一份 C 运行时，回归风险隔离。
 *
 * 不透明指针指向 payload：
 *   [ int64 key_kind ][ int64 val_kind ][ int64 cap ][ int64 count ][ entries... ]
 * 前 8 字节是引用计数（与 String / Array / Map 同契约）。
 * 每个 entry 24 字节：{ int64 key_bits; int64 val_bits; int64 used; }
 *
 * kind 编码（编译器与 C 侧共享，见 codegen 的 kv_kind()）：
 *   0 = INT   —— int / bool / char / byte，直接存 i64 值
 *   1 = FLOAT —— 存 IEEE754 位模式（bitcast，绝不 fptosi 丢小数）
 *   2 = STRING—— 存 DialString 指针的位模式，按条目 ARC 管理
 *
 * 线性探测 + 删除左移紧凑；扩容与被共享时 COW 克隆（与 dial_map_put 同款）。
 */

typedef void *DialKvMap;

/* kk/vk 为上面的 kind 编码。 */
DialKvMap dial_kvmap_new(int64_t key_kind, int64_t val_kind);

/* 返回（可能更新后的）指针，调用方须写回接收变量（COW 契约）。 */
DialKvMap dial_kvmap_put(DialKvMap m, int64_t key_bits, int64_t val_bits);

/* 返回值的位模式；未命中返回 V 的默认值（-1 / -1.0 / 0）。 */
int64_t dial_kvmap_get(DialKvMap m, int64_t key_bits);
int64_t dial_kvmap_contains(DialKvMap m, int64_t key_bits);
int64_t dial_kvmap_remove(DialKvMap m, int64_t key_bits);
int64_t dial_kvmap_size(DialKvMap m);

/* 迭代 API：返回成员快照数组。键/值为 INT 类时是 int[]，为 STRING 时是 string[]
 * （按元素 ARC：dial_array_push 会自行 retain，快照与原表互不影响）。 */
void *dial_kvmap_keys(DialKvMap m);
void *dial_kvmap_values(DialKvMap m);

void dial_kvmap_retain(DialKvMap m);
void dial_kvmap_release(DialKvMap m);

#endif /* DIAL_KVMAP_H */
