# Dial v0.81 交付报告 —— 通道关闭与 EOF 语义（`close` / `done` / `try_recv`）

- 版本：`0.8.0 → 0.81.0`（十进制递增：+0.01 = 小功能 / 新语法，本版无新类型、无新引擎）
- 日期：2026-10-30
- 规范位置：README [§27.7](README.md#277-通道关闭与-eofv081)

---

## 1. 补的是什么缺口

v0.8 落地了并发，但留了一个**功能级**而非体验级的缺口：**通道没有 EOF**。

```dial
fn producer(ch: channel<string>): void {
    ... move_to(s, ch); ...
}                       // 生产者退出

fn main(): int {
    let t: thread = spawn(producer, ch);
    var r: string = recv(ch);   // 生产者不再发送，但通道从未被告知「结束了」
    ...                          // ← 消费者永远卡在这里
}
```

生产者退出后，消费者不知道「不会再有数据了」，`recv` 会一直等一个永远不会来的元素。
这不是「跑出一个错误结果」，而是**程序死锁**——而且是最难排查的那种：没有崩溃、没有日志、
进程只是静静挂着。

**后果**：「生产者结束 → 消费者收尾」这个并发里最常见的范式，在 v0.8 里根本写不出来。

v0.81 把它补上。

---

## 2. 新增内建

| 内建 | 签名 | 语义 |
| --- | --- | --- |
| `close(ch)` | `(channel<T>) -> void` | 关闭通道（幂等），广播唤醒所有等待者。**不释放通道对象** |
| `done(ch)` | `(channel<T>) -> bool` | **已关闭 且 已空** —— EOF 的精确定义 |
| `closed(ch)` | `(channel<T>) -> bool` | 是否已调用过 `close` |
| `len(ch)` | `(channel<T>) -> int` | 队列中当前的剩余元素数 |
| `try_recv(ch)` | `(channel<T>) -> T` | 非阻塞取出；队列空立即返回，绝不等待 |

C Runtime 侧对应 `dial_channel_close` / `closed` / `len` / `done` / `try_recv`。

于是完整范式第一次可写：

```dial
fn producer(ch: channel<string>): void {
    var a: string = "one";
    move_to(a, ch);
    var b: string = "two";
    move_to(b, ch);
    close(ch);                 // 宣告结束 + 唤醒消费者
}

fn main(): int {
    let ch: channel<string> = channel(4);
    let t: thread = spawn(producer, ch);
    while (not done(ch)) {     // 单调判据 -> 必然终止
        var r: string = recv(ch);
        print(r);
    }
    join(t);
}
```

---

## 3. 关键实现决策

### 3.1 `done` 为什么必须是「已关闭 **且** 已空」

只判断 `closed` 会**丢数据**（关闭时队列里可能还剩几个元素）；只判断 `len == 0`
会**漏收**（生产者还没来得及发第一个时它也成立）。只有二者的合取才是「不会再有元素了」。

更关键的是 `done` 具有**单调性**：一旦为真就永远为真。因为

1. 关闭不可逆（没有 reopen）；
2. 关闭后 `send` 被禁止（见 §3.4）。

所以元素只会越取越少，绝不会出现「刚判断完又冒出一个」。单调性是
`while (not done(ch))` 这个循环**必然终止**的唯一保障——如果 `done` 会在真假之间反复，
这个写法就会随机漏收或死锁。

### 3.2 `closed` 标志**故意不用** atomic

`done` 必须与元素计数 `n` 在**同一个互斥临界区**内判定。如果把 `closed` 拆成独立的
`_Atomic`，两次读之间状态已经变了，会读到「关闭前 + 取空后」这种从未存在过的组合。
因此 `closed` 是普通 `int`，全程只在 mutex 保护下读写。

### 3.3 EOF 返回空指针，不是默认值

`recv` / `try_recv` 在「已关闭且已空」时返回 **NULL**，而不是空串 / 空数组。

理由：NULL 表示「没有值」。一旦被继续使用，Runtime 会在**使用点**当场 Fail-Fast
（例如给 `dial_print_string` 加了 `DIAL_HEAP_ASSERT`，打印诊断后 `ud2`），
而不是让它伪装成空串悄悄流进业务逻辑——这是 §20 Fail-Fast 铁律的直接要求。
若返回空串，一次 EOF 误用会变成「业务逻辑算出一个诡异的空值」，排查成本极高。

正常写法碰不到它：`while (not done(ch))` 守住了循环，`done` 为真时压根不进去。

### 3.4 关闭后再 `send` 必须 Fail-Fast

这是最危险的一种误用：元素会**永久滞留**在队列里，而只要队列非空 `done` 就永远为假，
消费者的收尾循环将永不结束——**一次「关了还发」会把 v0.81 好不容易补上的 EOF 语义整个废掉**，
让程序重新退化成死锁。

因此 ReleaseSafe 及以上当场终止；只有 `--fast` 才关闭该检查（后果自负，已由 E 组验证）。

### 3.5 一次设计回退的反转

v0.8 曾**刻意**不给通道加 `close`，担心「关掉之后变量还在、作用域结束再 `release` 一次」
造成二次释放，并在报告中记为「设计回退」。本次复核确认**该担心不成立**：

- `close` 只**置标志 + 广播**，既不释放通道对象，也不动引用计数；
- 通道对象的销毁仍然**只由原子 ARC 归零触发一次**。

二者职责不重叠，因此 `close` 之后继续持有变量、继续 `recv` 残留元素、最后正常退出作用域，
全部安全。压力测试（§4.3）已验证。

### 3.6 保留名校验补漏

`close` 此前**已经在 `Expr::Call` 里按名字分派为内建**，却没被列入 `RESERVED_NAMES`——
也就是说用户可以定义一个名为 `close` 的函数去遮蔽内建，走到未定义行为。本次一并补齐，
并把 `closed` / `len` / `done` / `try_recv` 加入保留表。

---

## 4. 端到端验证

### 4.1 五组测试全绿

```
== A) 黄金输出 ==      ✅ 新增 channel_close / channel_try_recv
== B) 负向编译失败 ==  ✅ 新增 11 例
== D) Fail-Fast ==     ✅ 新增 channel_send_after_close（信号 4 终止 + 诊断）
== E) --fast 对照 ==   ✅ 同一例打印哨兵 9999，证明陷阱确由编译器插入
== C) 仅依赖 libc ==   ✅ 产物仍无 libLLVM
```

### 4.2 并发输出确定性

两个新示例各**连跑 10 次**，逐字节一致。遵守 §27.6 铁律：子线程不 `print`，
单生产者，结果汇聚回 main 后按序打印。

### 4.3 所有权会计压力测试

沙箱无 valgrind、clang-18 的 ASan 运行时缺失，改用**内存 + 死锁双重压力测试**：

```dial
fn producer(ch: channel<string>): void {
    var i: int = 0;
    while (i < 200000) {
        var s: string = "0123456789abcdefghijklmnopqrstuvwxyz";
        move_to(s, ch);
        i = i + 1;
    }
    close(ch);                 // ← 本次新增的收尾路径
}
fn main(): int {
    ... while (not done(ch)) { var r: string = recv(ch); c = c + 1; } ...
}
```

结果：`200000` 全部收齐、**RSS 稳定 1.4 MB 不增长**（若漏释放应约 7 MB）、
**无挂起**（若 `done` 判据有误必然死锁，测试会被 timeout 杀掉）。
即：EOF 路径下所有权会计依然平衡，无泄漏、无 double-free。

### 4.4 零隐式负担

`nm examples/hello.dial` 的产物：`dial_channel_*` / `dial_thread_*` / `pthread*` / `*thunk*`
**一个都没有**。不用并发的代码，一个字节都不为本次新增付出代价。
`channel_try_recv` 的产物中则精确出现 `dial_channel_close/closed/done/len/try_recv`。

---

## 5. 交付物清单

| 文件 | 说明 |
| --- | --- |
| `stdlib/dial_channel.{h,c}` | 新增 `close` / `closed` / `len` / `done` / `try_recv`；`recv` 加 EOF 分支；`send` 加关闭检查 |
| `stdlib/dial_string.c` | `dial_print_string` 加 NULL 断言（EOF 误用在使用点 Fail-Fast） |
| `src/semantic.rs` | `close` 扩展到 channel；`recv|try_recv` 合并校验；新增 `closed`/`done`/`len`；保留名补 5 个 |
| `src/codegen.rs` | 新 C 函数声明；`close` 按静态类型分派；`recv|try_recv` 分派；`closed|done` 收口为 i1 |
| `examples/channel_close.dial` + `.expected` | 生产者 close → 消费者 done 收尾（并发） |
| `examples/channel_try_recv.dial` + `.expected` | `len` / `try_recv` / `closed` / `done` 状态机（单线程） |
| `tests/bad/*`（11 个） | 错用四个新内建、元素类型未定就 `try_recv`、保留名定义函数与结构体 |
| `tests/trap/channel_send_after_close.dial` | 关闭后 send 必须 Fail-Fast（D 组） |
| `README.md` | 新增 §27.7；边界表改 §27.8 并更新；§17.2 / §24 同步 |
| `CHANGELOG.md` / `web/*.html` / `Cargo.toml` | `[0.81.0]` / 官网重生成 / `0.81.0` |

---

## 6. 仍未做（需用户拍板）

| 能力 | 状态 |
| --- | --- |
| `select` / 多路复用 | 未实现——`try_recv` 已落地，但无法同时监听多个通道 |
| 通道超时（`recv_timeout`） | 未实现 |
| 多消费者协调 | `done` 的单调性只对单消费者保证；多消费者仍可能有一个卡在 `recv` |
| 线程池、协程、异步 | 未实现 |
| 包分发（`.dialpkg` / `import`） | 草案见 README §25，未实现 |
| 标量元素通道（`channel<int>`） | 编译期拒绝（需装箱，与「零分配移交」冲突） |
