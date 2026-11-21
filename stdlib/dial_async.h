#ifndef DIAL_ASYNC_H
#define DIAL_ASYNC_H

#include <stdint.h>
#include <stddef.h>

/*
 * ============================================================================
 *  dial_async —— Dial 的协作式协程 / 有界线程池 / async 调度器（v1.21）
 * ============================================================================
 *
 *  宪法一致性（与 README §25 / §27 / §37.2 完全对齐）：
 *    * 绝无闭包：所有入口都是**具名函数 + 显式 argcell 状态**（与 spawn 同款）。
 *      C 侧不捕获、不保存任何 Dial 环境对象。
 *    * 绝无用户可见可变全局状态：协程 / 线程池 / 任务全部是不透明句柄，
 *      生命周期由用户显式 new → use → join/close/free 掌管（与 thread/channel 同级）。
 *    * 绝无用户可见原子类型：内部同步用 pthread mutex + cond（C 私有实现），
 *      不向 Dial 暴露任何 atomic 类型或 CAS 内建。
 *
 *  三条能力共用同一个入口签名 DialCoEntry = void(void* argcell)：
 *    - 线程 / 线程池任务跑在 worker 线程上；
 *    - 协程 / async 任务跑在**有栈协程**上（POSIX ucontext / Windows Fiber），
 *      yield 可在任意调用深度发生，局部变量与 ARC 引用随栈帧自然保留。
 *
 *  argcell 布局（async / pool 任务的 thunk 由编译器生成，见 lower.rs）：
 *    [0]      result 槽（8 字节，thunk 在目标函数返回后写入；void 入口写 0）
 *    [1..n]   n 个形参槽（每个 8 字节）
 *  这让运行时的 dial_task_result() 只需读 slot[0]，无需知道形参数目。
 * ============================================================================
 */

/* ---------- 有栈协程 ---------- */
typedef void *DialCoroutine;

/* 协程入口：与线程入口同签名，复用 argcell 显式状态。 */
typedef void (*DialCoEntry)(void *argcell);

/* 创建协程（不立即运行）。stack_bytes=0 使用默认栈（256KB）。失败 Fail-Fast。 */
DialCoroutine dial_co_new(DialCoEntry fn, void *argcell, int64_t stack_bytes);

/* 恢复协程运行直到它 yield 或结束。返回 1=已结束，0=仍挂起。 */
int dial_co_resume(DialCoroutine co);

/* 当前协程主动让出（协作式）。在协程外调用为 Fail-Fast。 */
void dial_co_yield(void);

/* 状态：0=ready 1=running 2=suspended 3=done */
int dial_co_status(DialCoroutine co);

/* 释放协程句柄（须在 done 后；argcell 由 task 统一释放，这里不碰）。 */
void dial_co_free(DialCoroutine co);

/* ---------- 有界线程池 ---------- */
typedef void *DialPool;
typedef void *DialTask;

/* 创建含 nworkers 个 worker 线程的线程池（nworkers<1 归一为 1）。 */
DialPool dial_pool_new(int64_t nworkers);

/* 提交任务（具名函数 + argcell）。argcell 所有权移交给任务。返回 task 句柄。 */
DialTask dial_pool_submit(DialPool pool, DialCoEntry fn, void *argcell);

/* 等待线程池中全部已提交任务完成。 */
void dial_pool_join(DialPool pool);

/* join + 回收线程池（之后句柄失效）。 */
void dial_pool_close(DialPool pool);

/* ---------- 任务（pool / async 通用） ---------- */
/* 任务是否完成（非阻塞）。 */
int dial_task_done(DialTask task);

/* 阻塞等待任务完成。 */
void dial_task_wait(DialTask task);

/* 读取任务结果槽 slot[0]（须在 done 后；由 lower 按目标类型解释）。 */
uint64_t dial_task_result(DialTask task);

/* 释放任务句柄（须在 done 后；同时释放其拥有的 argcell）。 */
void dial_task_free(DialTask task);

/* ---------- async executor（单线程协作调度） ---------- */
/* 创建并登记一个 async 任务（协程），返回 task 句柄；需 run_all 驱动。 */
DialTask dial_async_spawn(DialCoEntry fn, void *argcell);

/* 等待任务完成：未完成则让出当前协程，由 run_all 调度其它任务。
 * 必须在 async 协程上下文（async fn）内调用，否则 Fail-Fast。 */
void dial_async_await(DialTask task);

/* 驱动 executor：round-robin resume 所有活跃 async 任务直到全部完成。 */
void dial_run_all(void);

#endif /* DIAL_ASYNC_H */
