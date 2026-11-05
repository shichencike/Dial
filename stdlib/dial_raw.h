#ifndef DIAL_RAW_H
#define DIAL_RAW_H

#include <stdint.h>

/*
 * 裸指针（v1.02）——**不受 ARC 管理**的原始内存。
 *
 * 契约（README §19.3「显式指针回收」）：
 *   - 谁申请、谁释放：raw_alloc 拿到的内存必须由 raw_free / drop 归还；
 *   - 编译器**不会**为裸指针做任何 retain / release，作用域结束时也不会自动释放；
 *   - 裸指针与 ARC 容器之间**没有隐式转换**，取 ARC 对象的地址必须显式 ptr_cast；
 *   - 分配失败当场 Fail-Fast（绝不返回 NULL 让上层带病运行）。
 */

/* 申请 n 字节裸内存；失败当场终止（不返回 NULL）。 */
void *dial_raw_alloc(int64_t n);

/* 归还裸指针；NULL 安全（空指针直接返回）。 */
void dial_raw_free(void *p);

#endif /* DIAL_RAW_H */
