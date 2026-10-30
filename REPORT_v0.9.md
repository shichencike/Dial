# Dial v0.9 交付报告 —— 判空 · 多消费者安全 · 超时接收

- 版本：`0.81.0 → 0.9.0`（十进制递增；用户明确指定 v0.9）
- 日期：2026-10-30
- 规范位置：README [§27.8](README.md#278-判空多消费者安全与超时v09)

---

## 1. 本版定位

v0.81 补上了 EOF，但并发底座还剩三处硬伤。本版把它们一次清干净：

| # | 硬伤 | 本版对策 |
| --- | --- | --- |
| 1 | `recv` 在 EOF 时返回空指针，而 Dial **没有任何判空能力**（无 `Option`、无 `null` 字面量）——用户无从分辨「这是没有值」还是「这是一个真实值」 | 新增 `is_null(x)` |
| 2 | 通道通知用 `pthread_cond_signal`，**多消费者下会丢唤醒 → 永久死锁** | 全部改 `broadcast`；新增测试组 F 做实证 |
| 3 | `recv` 无期限等待，生产者异常退出时消费者永久挂起 | 新增 `recv_timeout(ch, ms)` |

---

## 2. 最重要的一件事：修掉了一个真实的死锁

v0.8/v0.81 的 `dial_channel_send` / `dial_channel_recv` 用的是
`pthread_cond_signal`——**只唤醒一个**等待者。

单消费者时这没问题；多消费者时，「队列非空」是**众多消费者共享的状态**：

> 唤醒其中一个消费者后，若队列里**还剩元素**，其余消费者会带着**已经失效的等待条件**
> （「队列为空」）继续睡死——直到下一次 `send` 才被叫醒，而那次 `send` 又只唤醒一个。
> 队列深度 > 1 且消费者 > 1 时，必然有消费者永久阻塞。

### 对照实验（`tests/stress/channel_mt.c`：8 消费者 / 100 万元素 / 队列深 64）

| 版本 | 结果 |
| --- | --- |
| `pthread_cond_signal`（v0.8 / v0.81） | **25 秒后超时挂死**（exit 124） |
| `pthread_cond_broadcast`（v0.9） | **4.25 秒跑完**，`received=1000000 duplicate=0 out_of_range=0` |

这不是推演，是跑出来的。修复只有一处改动：所有「队列状态变化」的通知改用 `broadcast`，
让每个等待者都重新检查条件。**正确性优先于惊群开销**——Dial 的并发规模远不值得
为省一点唤醒去换一次死锁。

---

## 3. 新增内建

### 3.1 `is_null(x)`

准入名单：`string` / `int[]` / `map` / `error` / `channel<T>` / `thread` / `file`。

`int` / `float` / `bool` 是**值类型，永远非空**，对它们判空是纯噪音——编译期直接拒绝，
而不是让它变成一段恒为 `false` 的死代码。

IR 侧只是一次指针比较（`ptrtoint` + `icmp eq 0`，优化器折叠回 `icmp eq ptr %p, null`），
**零函数调用、零引用计数操作**。

### 3.2 `recv_timeout(ch, ms)`

- 底层就是 `pthread_cond_timedwait` + 一个绝对时刻，**没有引入任何状态机**。
- 超时是**上限不是下限**：元素提前到了就立刻返回。
- **超时不会吞掉元素**：`ETIMEDOUT` 后回到 `while` 条件重新判定，
  此刻队列非空则正常取走——「元素恰好在超时这一刻到达」也能被拿到。
- 所有权规则与 `recv` 完全一致：取出的元素归接收者所有，不做 retain / release。

### 3.3 一个跨平台性的取舍

超时基准钟最初选的是 `CLOCK_MONOTONIC`（避免系统时间被 NTP 调整影响），
但 **`pthread_condattr_setclock` 在 macOS 上并不存在**——为一个「极端情况下超时点偏移」
的收益牺牲跨平台可移植性不划算，遂改回 `CLOCK_REALTIME`（`pthread_cond_timedwait` 默认基准）。

代价可控：系统时间前跳会让等待提前结束（多返回一次 NULL），后跳会让等待被延长，
但**正确性不受影响**——超时判定后 `while` 循环会重新检查队列，
绝不会因此吞掉元素或把元素重复交给两个消费者。

---

## 4. 多消费者怎么写才安全

```dial
while (true) {
    var r: string = recv(ch);     // 只在「已关闭且已空」时返回空指针
    if (is_null(r)) { break; }     // 这才是 EOF
    ... 处理 r ...
}
```

**不能用 `try_recv` 判结束**：`try_recv` 在「队列暂时空」时也返回空指针，
那不是 EOF，只是「现在没有」。多消费者下用 `try_recv` 会漏收元素。

单消费者场景仍推荐 `while (not done(ch))`（§27.7）——连一次多余的 `recv` 都不需要。

### ARC 会计：不变，但这次被验证了

环形队列的每一次出队都在**同一个互斥量**下完成，因此每个元素只会被弹出一次，
也就只会有一个持有者——**结构上不可能 double-free**。

验证手段：
- `tests/stress/channel_mt.c`：位图 + 原子 `__atomic_fetch_or` 逐个记录领取情况，
  重复领取即判失败；总数必须守恒。
- `examples/channel_multiconsumer.dial`：两个消费者抢同一个通道，各自发到自己的报告通道，
  主线程取数后相加——**总数恒为 100**（各消费者分到多少不确定，和是确定的）。
- Dial 层百万次移交（1 生产者 / 3 消费者 / 队列深 64）：**1.11 秒，RSS 1972 KB 稳定不增长**，
  无崩溃、无挂起。

---

## 5. 端到端验证

```
== A) 正向 ==      ✅ 新增 is_null_demo / recv_timeout / channel_multiconsumer
== B) 负向 ==      ✅ 新增 8 例
== D) Fail-Fast == ✅ 回归不变
== E) --fast ==    ✅ 回归不变
== C) 仅依赖 libc == ✅ 回归不变
== F) 并发压测 ==  ✅ 新增：received=1000000 expected=1000000 duplicate=0 out_of_range=0
```

三个新示例各**连跑 10 次**，输出逐字节一致。

---

## 6. 交付物清单

| 文件 | 说明 |
| --- | --- |
| `stdlib/dial_channel.{h,c}` | 通知改 `broadcast`；新增 `dial_channel_recv_timeout`；补齐线程安全契约注释 |
| `src/semantic.rs` | 新增 `is_null` / `recv_timeout` 内建 arm；`is_nullable_type` 准入名单；保留名补 2 个 |
| `src/codegen.rs` | `is_null` 走 LLVM 指针比较；`recv_timeout` 调用 C 包装；`declare_stdlib` 补声明 |
| `tests/stress/channel_mt.c` | **新增测试组 F**：C 层多消费者压测（100 万元素 / 8 消费者） |
| `tests/run_tests.sh` | 新增 F 组；注释补齐六组说明 |
| `examples/is_null_demo.dial` + `.expected` | EOF / 超时 / 真实值三种情形 |
| `examples/recv_timeout.dial` + `.expected` | 超时返回空值；超时与到达同时发生以「有元素」为准 |
| `examples/channel_multiconsumer.dial` + `.expected` | 两消费者抢同一通道，总数守恒 = 100 |
| `tests/bad/*`（8 个） | `is_null` 作用于标量 / 参数个数 / `recv_timeout` 四种错用 / 两个保留名 |
| `README.md` | 新增 §27.8；边界表改 §27.9；§18 测试章节补 F 组；§17.2 同步 |
| `CHANGELOG.md` / `web/*.html` / `Cargo.toml` | `[0.9.0]` / 官网重生成 / `0.9.0` |

---

## 7. 下一版预告

按既定路线，下一版（v0.91 或 v0.95）上 **`select` 多路复用**——同时监听多个通道。

它的难点在于：需要一个「同时等多个队列」的等待原语，而 Dial 的通道各自持有自己的
互斥量与条件变量；直接用一把大锁去轮询所有队列会引入新的锁顺序问题。
本次已把**单通道**的语义（EOF / 超时 / 多消费者 / 判空）全部做扎实并压测到百万级，
正是为了在动 `select` 之前，把地基验干净。

其余待办：`recv` 超时的精确时钟、线程池、协程/异步、包分发（§25 草案）、
标量元素通道（需装箱）。
