#ifndef DIAL_CHANNEL_H
#define DIAL_CHANNEL_H

#include <stdint.h>

/*
 * 通道：不透明指针，**跨线程共享**，因此走**原子**引用计数（C11 stdatomic）。
 *
 * 这是 Dial 取代锁机制的核心：send / recv 移交的是**所有权**，不是共享引用——
 * 指针进出队列全程不做 retain / release，队列里永远只有「唯一持有者」。
 * 因此通道本身没有数据竞争的可能，用户代码也无需任何锁。
 *
 * 元素类型在编译期可见（channel<T>），运行时已擦除（队列只存 void*）。
 * IR 层看到的是 i8*，类型系统里是内置的 channel<T> 类型。
 */
typedef void *DialChannel;

/* 创建有界通道（cap 为容量；<1 时按 1 处理）。初始引用计数为 1。 */
DialChannel dial_channel_new(int64_t cap);

/*
 * 移交一个元素的所有权进通道。
 * 不做 retain —— 调用方必须放弃该指针（Dial 层由 move_to 保证：原槽清零）。
 * 队列满时阻塞等待。
 */
void  dial_channel_send(DialChannel ch, void *p);

/*
 * 从通道取出一个元素的所有权（返回指针，不做 retain —— 接收方成为唯一持有者）。
 * 队列空时阻塞等待。
 */
void *dial_channel_recv(DialChannel ch);

/* ARC（原子）：引用计数 +1 / -1（归零时销毁互斥量并释放通道）。 */
void  dial_channel_retain(DialChannel ch);
void  dial_channel_release(DialChannel ch);

#endif /* DIAL_CHANNEL_H */
