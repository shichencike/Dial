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
 *
 * ------------------------------------------------------------------
 *  线程安全契约（v0.9 明确）
 * ------------------------------------------------------------------
 *  1) 环形队列的每一次读写都在同一个互斥量下完成，因此**任意多个**线程
 *     并发 send / recv 都是安全的：每个元素只会被弹出一次，
 *     也就只会有一个持有者——**不可能 double-free**。
 *  2) 通知一律使用 broadcast 而非 signal。signal 只唤醒一个等待者，
 *     而「队列非空」是**众多消费者共享**的状态：唤醒一个之后若队列仍非空，
 *     其余消费者就会带着已经失效的等待条件继续睡死——这就是经典的
 *     **丢唤醒（lost wakeup）**，在多消费者下表现为某个消费者永久阻塞、
 *     程序死锁，且极难复现。broadcast 让每个等待者都重新检查条件，
 *     正确性优先于惊群开销。
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
 * 带超时的接收（v0.9）：至多等待 timeout_ms 毫秒。
 *
 * 返回 NULL 的情形只有两种：
 *   1) 超时（队列一直空着）
 *   2) 通道已关闭且已空（EOF）
 * 而「等待期间恰好来了元素」时**绝不会**返回 NULL —— 超时与到达同时发生时
 * 以「有元素」为准，绝不吞掉一个刚到的元素（while 循环会重新检查条件）。
 *
 * 用 is_null(x) 判别是否真的拿到了值。
 *
 * 底层直接用 pthread_cond_timedwait + 绝对时刻实现，不引入任何额外状态机。
 * 时钟取 CLOCK_MONOTONIC：系统时间被 NTP 调整（往前跳会让等待瞬间超时、
 * 往后跳会让等待被无限延长）不该影响程序的正确性。
 */
void *dial_channel_recv_timeout(DialChannel ch, int64_t timeout_ms);

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

/*
 * ============================================================================
 *  多路复用原语（v0.91）：全局事件广播 + 就绪通道挑选
 * ============================================================================
 *
 *  select 需要同时监听多个通道。它不用「一把大锁锁住所有队列」（那会引入
 *  复杂的锁顺序问题），而是走**事件驱动**路线：
 *
 *      任何通道的状态变化（send / recv / close）都递增一个全局 epoch 计数，
 *      并在**全局条件变量**上广播。select 等待时先记住 epoch，再检查各通道
 *      是否就绪，若都不就绪就睡到 epoch 变化为止。
 *
 *  两个关键性质：
 *
 *  1) **不会丢唤醒。** epoch 是「先读后查」的：等待前若已有事件发生，epoch
 *     已经变了，等待条件立即不成立，绝不会带着过期的假设睡死。
 *  2) **不会死锁。** 通知（notify）一律在**释放通道本地锁之后**才调用，
 *     于是全局锁与通道锁**永不嵌套**：
 *       - 持通道锁 -> 取全局锁：不存在（notify 在放锁之后）
 *       - 持全局锁 -> 取通道锁：不存在（挑选就绪通道时不持全局锁）
 *     「持通道锁时取全局锁」是这类设计最经典的死锁成因，这里从结构上排除。
 *
 *  Windows 说明：Windows 没有 pthread，本组函数在 Windows 后端里对应
 *  `WakeConditionVariable` / `SetEvent`（见 ARCHITECTURE.md §0.2 的线程后端规划）。
 *  Linux / Termux 用 pthread_cond_broadcast + pthread_cond_wait。
 *
 *  惊群：一次广播会唤醒全部 select 等待者（惊群）。基线实现选择
 *  **逻辑正确 + 低延迟**优先于惊群开销——惊群只造成少量无谓唤醒，
 *  而轮询方案会造成持续 CPU 空转与毫秒级延迟。未来在 Linux 上可用
 *  epoll / eventfd 消除惊群（见 README §27.10 的展望）。
 */

/* 取当前全局事件纪元（每次通道状态变化 +1）。 */
int64_t dial_chan_epoch(void);

/* 等待直到全局 epoch 发生变化（即至少有一个通道的状态变了）。 */
void    dial_chan_wait(int64_t since);

/*
 * 在就绪的通道中挑一个（多通道同时就绪时**随机**挑选，防饿死）。
 *
 * 返回值：
 *   >= 0  就绪通道在数组中的下标（调用方据此取元素）
 *   -1    当前没有任何通道就绪（但也没有全部 EOF —— 应当等待）
 *   -2    **全体 EOF**（所有通道都已关闭且已空）—— select 语句就此结束
 *
 * 数组元素为 DialChannel；元素本身不会被本函数加引用计数，
 * 调用方需保证数组在调用期间有效。
 */
int64_t dial_channel_pick_ready(DialChannel *chs, int64_t n);

/*
 * ---- 选择器对象（v0.91 的 select 用它）----
 *
 *  IR 侧不构造通道数组（那需要在 LLVM 里摆弄 [n x i8*] 数组类型），
 *  而是把「挑一个就绪通道 + 从它取走一个元素」这两件事整体交给 C 侧的
 *  选择器对象。IR 侧只持有一个 i8* 句柄，逐个 add 通道即可。
 *
 *  生命周期：dial_sel_new -> (add)* -> pick/recv* -> dial_sel_free。
 *  IR 侧保证每条控制流路径上**恰好 free 一次**（元素取走成功后才 free，
 *  因为「被别的消费者抢走」需要重试，selector 还得留着）。
 */
typedef struct DialSelector DialSelector;

DialSelector *dial_sel_new(void);
void          dial_sel_add(DialSelector *s, DialChannel ch);
int64_t       dial_sel_pick(DialSelector *s);        /* >=0 就绪下标 / -1 无就绪 / -2 全体 EOF */
void         *dial_sel_recv(DialSelector *s, int64_t idx);  /* 取元素；可能返回 NULL（被抢走） */
void          dial_sel_free(DialSelector *s);

#endif /* DIAL_CHANNEL_H */
