# Dial v0.91 交付报告 —— 多路复用 `select`

- 版本：`0.9.0 → 0.91.0`（十进制递增：+0.01 = 小功能 / 新语法）
- 日期：2026-10-30
- 规范位置：README [§27.9](README.md#279-多路复用-selectv091)

---

## 1. 语法：Dial 风格，不是 Go 风格

```dial
select {
    case recv(chA) as a {
        print(a);
    }
    case recv(chB) as b {
        print(b);
    }
    default {
        print("暂时没有就绪的通道");
    }
}
```

用户明确要求「绝不能照搬 Go」，落实为三条硬规则：

| 要求 | 落实 |
| --- | --- |
| 废弃 `<-` 语法，接收永远是函数调用 | `case recv(ch) as v { ... }`；Sema 强制 case 必须是 `recv(ch)` 调用（用 `try_recv` 也拒绝） |
| 绑定用 `as` 关键字 | 新增 `as` 关键字；parser 报「case 后必须用 as 绑定变量名」 |
| 必须是语句，不能是表达式 | `select` 是 `Stmt::Select`；`let x = select {...}` 编译期拒绝；各 case 绑定变量类型可互不相同（不存在统一返回类型） |

绑定变量的类型**自动推导**为通道元素类型，生命周期**严格限制**在 case 的 `{}` 内
（作用域外使用 → 编译期拒绝）。

---

## 2. 三条语义

| # | 语义 |
| --- | --- |
| 1 | **case 只拿真实值**——分支体内绝不会拿到空指针 |
| 2 | **全体 EOF → select 直接结束**，一个分支都不执行（包括 `default`） |
| 3 | **`default` 使 select 非阻塞** |

### 关于「case 要不要用 is_null 判断」

用户要求在 README 写「必须在 case 里判断 is_null，为空则 break」。**本实现下这条不适用**，
理由是技术性的：

- case 用的是**非阻塞**取。若在挑中某通道之后、真正取之前，那个元素被别的消费者抢走，
  select 会**自动重挑一个**（重试整个 pick 循环），而不是把空指针交给用户；
- EOF 时（已关闭且已空）`pick` 返回 -2，select **直接结束**，根本不会进入 case。

所以 **case 里的 `is_null(v)` 恒为 `false`**；EOF 的判据是「select 语句结束了」。
这一点在 README §27.9.2 如实写明，并给出了两者的对照表。

### 真正需要警告的是 `default`

用户担心的「100% CPU 空转」确实存在，但触发条件是 **`default`**（因为它让 select 永不阻塞），
而且危害比空转更严重——**逻辑错误 + 死锁**：

| | 结果 |
| --- | --- |
| 阻塞式 select（无 default），50 万次收发 | **wall 0.19 s**，CPU 0.36 s ✅ |
| 改用 `default` 顶替真实接收，同样工作量 | 空转 0.38 s 后**卡死在 join**（队列塞满、生产者永久阻塞），120 s 超时 ❌ |

README §27.9.5 因此给出了**反例 + 正例**两段代码：反例演示用 default 冒充接收导致虚假计数，
正例演示 default 只记空闲、不影响计数。

---

## 3. 底层：事件驱动，不是轮询

用户明确否掉了轮询方案（「轮询 + sleep 是底层并发设计的毒药」）。实现走**事件驱动**：

```
全局事件纪元 g_epoch（原子）+ 全局条件变量 g_ev_cond
  send / recv / recv_timeout / try_recv / close
        → 递增 g_epoch → 在 g_ev_cond 上 broadcast
select 等待：
        e = epoch()        ← ① 先读
        idx = pick_ready() ← ② 再查就绪
        若都不就绪 → wait(e)  ← ③ 睡到 epoch 变化
```

| 性质 | 保证方式 |
| --- | --- |
| **不丢唤醒** | epoch 是「先读后查」的：等待前若已有事件发生，epoch 已经变了，等待条件立即不成立 |
| **不死锁** | notify 一律在**释放通道本地锁之后**调用——「持通道锁 → 取全局锁」与「持全局锁 → 取通道锁」两种嵌套**都不存在** |
| **微秒级唤醒** | `pthread_cond_broadcast`（Windows 侧对应 `WakeConditionVariable`） |

### 惊群：接受它，并记录未来优化

一次广播会唤醒所有 select 等待者，多数醒来发现「不是我」。按用户指示，
基线实现**选择逻辑正确 + 低延迟优先于惊群开销**（惊群只造成少量无谓唤醒；
轮询方案则是持续 CPU 空转 + 毫秒级延迟）。

README §27.9.6 记录了未来方向：**Linux 上用 epoll + eventfd 替代条件变量，把惊群降到零**，
属 v0.92 之后的优化，不影响本版语义。

### 随机选取：防饿死

`pick_ready` 从一个**原子递增**计数器取随机起点开始扫描（不能用 `rand()`，它不是线程安全）。
`examples/select_fairness.dial` 断言两个通道**都**被处理过（若存在饥饿，其中一个必为 0）。

---

## 4. 实现要点

### 4.1 IR 结构

```
setup   → 建 selector、登记各 case 的通道
loop    → 取 epoch、pick 一个就绪通道
chk     → 三路分支的中间块（LLVM 条件分支只有两路）
dispatch→ switch 按就绪下标分派
take_i  → 从第 i 个通道非阻塞取；取到 NULL = 被抢走 → 回 loop 重挑
case_i  → 绑定变量 + 执行分支体
nowait  → 无就绪：有 default 走 default，否则 wait(epoch) 后回 loop
eof     → 全体 EOF：释放 selector、select 直接结束
end     → 统一续跑点
```

**关键细节**：IR 侧不构造通道数组（那需要在 LLVM 里摆弄 `[n x i8*]` 数组类型），
而是把「挑一个就绪通道 + 从它取走元素」整体交给 C 侧的选择器对象
（`dial_sel_new` / `add` / `pick` / `recv` / `free`），IR 只持有一个 `i8*` 句柄。

**释放时机**：selector 在**元素取走成功后立刻释放**（在执行分支体之前），
因此分支体里即使 `return` 也不会泄漏；而「被抢走需重试」的路径不释放，
保证每条控制流路径上**恰好释放一次**。

### 4.2 修掉一个自己造出来的编译器崩溃

首版实现里 select 的 setup 块**没有接上调用方当前块**，导致 `while` 的 body 块变成
无终结指令的空块——LLVM 在分支概率分析阶段段错误（`BranchProbabilityInfo::calculate`）。

定位方式：给 codegen 加了 `module.verify()` 诊断开关，直接报出
`Basic Block in function 'main' does not have terminator! label %while.body`。
修复：select 生成时先补一条「当前块 → setup」的跳转。

---

## 5. 端到端验证

```
== A) 正向 ==      ✅ 新增 select_two_channels / select_default / select_eof / select_fairness
== B) 负向 ==      ✅ 新增 9 例
== D) Fail-Fast == ✅ 回归不变
== E) --fast ==    ✅ 回归不变
== C) 仅依赖 libc == ✅ 回归不变
== F) 并发压测 ==  ✅ 回归不变：received=1000000 duplicate=0
```

四个新示例各**连跑 10 次**，输出逐字节一致。

双通道并发断言（`select_two_channels`）：`nA=2, nB=2, wrong=0`
——既验证了 select 正确分派，也验证了「收到的确实是本通道的元素」（没有串通道）。

---

## 6. 交付物清单

| 文件 | 说明 |
| --- | --- |
| `src/lexer.rs` | 新增 `Select` / `Case` / `Default` / `As` 关键字 |
| `src/ast.rs` | `Stmt::Select { arms, default_body, line, col }` 与 `SelectArm { var, chan, body }` |
| `src/parser.rs` | `parse_select`：`case recv(ch) as v { }` / `default { }`；缺 `as`、空 select、两个 default 均拒绝 |
| `src/semantic.rs` | case 必须是 `recv(ch)`；元素类型必须已确定；绑定变量类型 = 元素类型且作用域限于 case 块内；`check_moved_stmt` / `collect_refs` / `set_moves` 均已递归进 select |
| `src/codegen.rs` | select 的完整基本块结构；`declare_stdlib` 补 7 个 C 声明；新增 `module.verify()` 诊断开关（`DIALC_DUMP_IR` 可 dump IR） |
| `stdlib/dial_channel.{h,c}` | 全局事件机制（epoch + 广播）、`dial_channel_pick_ready`（随机起点）、selector 对象；send/recv/recv_timeout/try_recv/close 五处 notify |
| `examples/select_*.dial` ×4 + `.expected` | 双通道并发 / default / 全体 EOF / 防饥饿 |
| `tests/bad/select_*.dial` ×9 | 负向用例 |
| `README.md` | 新增 §27.9；边界表改 §27.10；§17.2 / §4.3 关键字 / §18 示例清单同步 |
| `CHANGELOG.md` / `web/*.html` / `Cargo.toml` | `[0.91.0]` / 官网重生成 / `0.91.0` |

---

## 7. 下一版预告

按既定路线，下一版（v0.92 或 v0.95）可做：

- **epoll + eventfd 后端**：消除 select 的惊群（Linux 专用，Termux 亦可用）；
- **`select` 的超时变体**：`select_timeout`；
- **Windows 线程后端**：让 `spawn` / `channel` / `select` 在 Windows 上可用
  （见 [ARCHITECTURE.md §0.2](ARCHITECTURE.md#02-平台差异对-c-runtime-的约束实现路线备忘)）；
- **包分发 §25 落地**：`.dialpkg` 容器 + `dialc install` + `import` + Target Triple 索引。
