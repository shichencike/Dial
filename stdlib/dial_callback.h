#ifndef DIAL_CALLBACK_H
#define DIAL_CALLBACK_H

#include <stdint.h>

/*
 * ============================================================================
 *  dial_callback —— C → Dial 反向回调（v1.21，推翻 GUI 层「绝不反向回调」宪法）
 * ============================================================================
 *
 *  宪法边界（推翻后的新契约，见 README §32.7）：
 *    * 仍无闭包：回调入口是「具名函数 + 显式 rawptr<void> 状态」（§32 既有 ABI）。
 *    * 回调由 C 侧（含平台线程）**主动**调用 Dial 函数——这正是此前 GUI 层永久否决、
 *      本版起受控开放的「反向回调」。
 *    * 跨线程的 ARC 安全：回调参数/返回值一律是标量（int 等位模式），state 是
 *      rawptr<void>（用户自己管理生命周期）。**回调线程绝不触碰非原子 dial_rc_* 对象**；
 *      需要传复杂数据请用 channel<T>（原子 ARC）在线程间移交所有权。
 *
 *  实现：全局注册表（mutex 保护）id → (thunk, state)。C 侧持有 id（整数句柄），
 *  不持有任何 Dial 堆对象；fire 时查表取 thunk + state，构造 argcell 调用 thunk。
 * ============================================================================
 */

typedef int64_t DialCallbackId;      /* >0 有效；<=0 表示注册失败 */

/* 回调入口：与 §32 的 C 回调 ABI 一致。argcell 布局：
 *   slot[0] = payload(int, 8 字节)
 *   slot[1] = state(rawptr<void>)
 *   slot[2] = result 回写槽（thunk 写入 fn 的 int 返回值）
 */
typedef void (*DialCallbackFn)(void *argcell);

/* 注册回调（thunk + 显式 state）。返回 id（>0），失败 Fail-Fast。 */
DialCallbackId dial_callback_register(DialCallbackFn thunk, void *state);

/* Dial 侧同步触发（同线程，用于验证 thunk 正确性）。返回 thunk 写入的 int 结果。 */
int64_t dial_callback_fire_sync(DialCallbackId id, int64_t payload);

/* 启动一个 C 平台线程：sleep delay_ms 后**主动**回调 Dial（真反向回调）。
 * 返回 pthread 句柄（Dial 侧用 join 等待；回调结果经 callback_result 取）。 */
void *dial_callback_thread_fire(DialCallbackId id, int64_t payload, int64_t delay_ms);

/* join 一个 callback_thread 句柄并释放（与 dial_thread_join 同款契约）。 */
void dial_callback_join(void *tid);

/* 读取最近一次回调的 int 结果（线程安全）。 */
int64_t dial_callback_result(DialCallbackId id);

/* 最近一次回调是否已完成（非阻塞）。 */
int dial_callback_done(DialCallbackId id);

/* 阻塞等待最近一次回调完成。 */
void dial_callback_wait(DialCallbackId id);

/* 注销回调（释放注册表槽位）。注销后句柄失效。 */
void dial_callback_free(DialCallbackId id);

/* 测试/诊断：当前注册表条目数。 */
int64_t dial_callback_count(void);

#endif /* DIAL_CALLBACK_H */
