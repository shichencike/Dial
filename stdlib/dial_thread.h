#ifndef DIAL_THREAD_H
#define DIAL_THREAD_H

#include <stdint.h>

/*
 * 线程句柄：不透明指针，**不受 ARC 管理**（与 file 同级）。
 * 生命周期由用户显式终结：join（等待）或 detach（放任），二者选一且仅一次。
 *
 * IR 层看到的是 i8*，类型系统里是内置的 thread 类型。
 */
typedef void *DialThread;

/*
 * 线程入口的统一签名：只接受一个 argcell 指针。
 *
 * 绝不使用「类型化多参函数指针」——Dial 函数的实参类型千变万化（ptr / i64 / f64 / i1），
 * C 侧无法为每一种组合声明函数指针。真正的分发由编译器为每个被 spawn 的函数
 * 生成一个 LLVM 私有 thunk（void f(ptr argcell)）完成：thunk 从 argcell 里
 * 按 8 字节槽宽取出各实参，再调用真正的 Dial 函数。
 */
typedef void (*DialThreadFn)(void *argcell);

/* 分配 n 个 8 字节槽的实参单元（calloc 清零）。由子线程退出前释放。 */
void      *dial_thread_argcell(int64_t n);

/* 创建并**立即**启动线程。所有实参必须在调用本函数之前写入 argcell —— 零竞争窗口。 */
DialThread dial_thread_new(DialThreadFn fn, void *argcell);

/* 等待线程结束并回收句柄。调用后句柄失效（IR 侧会把槽置 null，二次 join 当场 Fail-Fast）。 */
void       dial_thread_join(DialThread t);

/* 分离线程（不再等待）并回收句柄。调用后同样不可再 join。 */
void       dial_thread_detach(DialThread t);

#endif /* DIAL_THREAD_H */
