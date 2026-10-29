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
 *
 * 往**已关闭**的通道发送是编程错误（关闭后绝不会再有人接收，元素将永久滞留
 * 并让消费者侧永远等不到 done）——ReleaseSafe 及以上当场 Fail-Fast。
 */
void  dial_channel_send(DialChannel ch, void *p);

/*
 * 从通道取出一个元素的所有权（返回指针，不做 retain —— 接收方成为唯一持有者）。
 * 队列空时阻塞等待。
 *
 * v0.81 起具备 EOF 语义：若通道**已关闭且已空**，立即返回 NULL，**不再永久阻塞**。
 * 这是「生产者结束 → 消费者收尾」得以成立的唯一前提；在此之前，生产者一旦退出，
 * 消费者就会卡在 recv 里形成死锁。
 *
 * 配合 done(ch) 使用即可永不拿到 NULL：while (!done(ch)) { r = recv(ch); }。
 * 若在 done 之后仍调用 recv，会得到 NULL —— 后续任何操作都会 Fail-Fast 当场暴露，
 * 而不是让它伪装成空值悄悄流进业务逻辑（Fail-Fast 铁律，§20）。
 */
void *dial_channel_recv(DialChannel ch);

/*
 * 非阻塞版本：队列空（或已关闭）时立即返回 NULL，绝不等待。
 * 配合 len(ch) 使用：单消费者下 `len(ch) > 0` 保证这次取值必然拿到元素。
 */
void *dial_channel_try_recv(DialChannel ch);

/*
 * 关闭通道（幂等，重复调用无副作用）。
 * 会广播唤醒所有阻塞在 send / recv 上的线程，让它们得以重新判定并退出。
 * 关闭是不可逆的：一旦关闭就绝不会再有元素进来。
 */
void  dial_channel_close(DialChannel ch);

/* 是否已调用过 close（0 / 1）。 */
int64_t dial_channel_closed(DialChannel ch);

/* 队列中当前的剩余元素数。 */
int64_t dial_channel_len(DialChannel ch);

/*
 * 是否**不会再有元素了**（= 已关闭 且 队列为空）。
 *
 * 这是 EOF 的精确定义，且具有关键性质：**单调**——一旦为真就永远为真
 * （关闭不可逆，且关闭后 send 被禁止，元素只会越取越少）。
 * 单调性保证了 `while (!done(ch)) { r = recv(ch); }` 这个循环必然终止。
 */
int64_t dial_channel_done(DialChannel ch);

/* ARC（原子）：引用计数 +1 / -1（归零时销毁互斥量并释放通道）。 */
void  dial_channel_retain(DialChannel ch);
void  dial_channel_release(DialChannel ch);

#endif /* DIAL_CHANNEL_H */
