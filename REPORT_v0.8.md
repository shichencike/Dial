# Dial v0.8 交付报告 —— `move_to` 所有权转移 + 多线程并发（thread / channel）

- 版本：`0.7.0 → 0.8.0`（十进制递增：+0.1 = 新引擎 / 重大特性）
- 日期：2026-10-30
- 规范位置：README [§27](README.md#27-所有权转移与多线程并发move_to--thread--channel)

---

## 1. 本次新增能力

| 能力 | 说明 |
| --- | --- |
| `thread` 类型 | 线程句柄，不透明指针，**不受 ARC 管理**，显式 `join` / `detach` |
| `channel<T>` 类型 | 通道句柄，不透明指针，跨线程共享 → **原子**引用计数 ARC |
| `spawn(fn, args...)` | 创建并立即启动线程；实参在 `pthread_create` 之前入 argcell |
| `move_to(x, ch)` | 编译期 intrinsic：O(1) 把 `x` 的所有权移交给通道 |
| `join` / `detach` | 终结线程句柄，二者选一且仅一次 |
| `channel(cap)` / `recv(ch)` | 创建有界通道 / 阻塞取出一个元素（返回类型即 `T`） |

底层新增 `stdlib/dial_thread.{h,c}` 与 `stdlib/dial_channel.{h,c}`，全部只依赖 libc + pthread，
由 `main.rs` 的目录 glob 自动编入 `libdial_std.a`，产物**不引入任何新的动态库依赖**。

---

## 2. 用户原始需求逐条自检

| 用户要求 | 落实 | 证据 |
| --- | --- | --- |
| 内置函数 `move_to(变量, 目标)` | ✅ 落为 `move_to(x, ch)` | 三条 IR 指令，见 §3.1 |
| Sema 标记 `Moved`，再用即编译错误「变量已转移，无法使用」 | ✅ | `semantic.rs` 的 `VarState.moved` + `check_moved_stmt`；错误文案与用户原文逐字一致 |
| Codegen 不生成函数调用栈，直接发 IR 推指针 + 清零原槽 | ✅ | `gen_expr` 的 `"move_to"` 分支；`nm` 产物中**无** `move_to` 符号 |
| O(1) 指令级操作 | ✅ | 一次 load + 一次入队调用 + 一次 store null，无循环、无分配 |
| 因所有权已移交，ARC 不做 retain / release | ✅ | `move_to` 分支内零 retain / 零 release；C 侧 `dial_channel_send` 也只搬指针 |
| 新线程（接收方）用完负责最终释放 | ✅ | 见 §3.3 会计表 + 20 万次压力测试 RSS 不增长 |
| 「取代锁机制的最强武器」 | ✅ | `send` / `recv` 全程零 retain/release，队列里的指针永远只有一个持有者 |

---

## 3. 关键实现决策

### 3.1 `move_to` 的 IR（用户要求的 O(1)）

```llvm
%v = load ptr, ptr %x_slot                       ; 取指针（不 retain）
call void @dial_channel_send(ptr %ch, ptr %v)    ; 入队
store ptr null, ptr %x_slot                      ; 原槽清零
```

没有调用栈、没有参数编组、没有引用计数操作。`moved` 集合随后记下该槽位指针，
使作用域结束时跳过它的 release（所有权已随指针出走）。

### 3.2 `spawn` 的 thunk + argcell（零竞争）

C 的 `pthread_create` 只认 `void (*)(void*)`，而 Dial 函数实参类型各异，因此编译器为
每个被 spawn 的函数生成私有 thunk，按 **8 字节槽宽**从 argcell 取参后调用真函数。

关键顺序：**所有实参 store 都在 `dial_thread_new`（内含 `pthread_create`）之前完成**，
因此不存在「先启动、再移交」的竞争窗口。这也正是**不支持**「move_to 到一个已启动线程」的原因——
那条路必然有竞争，Dial 选择在编译期就把它关掉；`spawn(fn, x)` 本身就是「move_to 到线程」。

### 3.3 所有权会计（最易错处，逐条推演）

| 场景 | 引用计数 |
| --- | --- |
| `spawn(worker, s)`，worker 不再移交 | 移交 rc=1 → worker 入口 retain(2) → 出口 release(1) → thunk release(0) ✅ |
| worker 内 `move_to(a0, ch)` | 入口 retain(2) → `moved` 使出口**跳过** → 2 → thunk release(1，归通道) → 接收方释放(0) ✅ |
| `channel` 作 spawn 实参（共享） | 原子 retain(2) → worker 入口(3) → 出口(2) → main 作用域结束(1) → thunk release(0) ✅ |
| `let r : string = recv(ch)` | `recv` 是调用非变量引用，VarDecl 不 retain → 直接取得所有权(1) → 作用域结束(0) ✅ |

thunk **无条件**按形参类型释放，不查 `moved`、不复用调用方的判定。

### 3.4 顺带修掉的两个既有隐患

1. **`moved` 由「变量名」改为「栈槽指针」**：变量名会跨作用域 / 跨函数重名，按名字记账会让
   同名变量互相误伤（该 release 的被跳过 → 泄漏）。改按 alloca 槽位记账后天然精确。
   `move_to` 是用户日常 API，这一位必须是准的。
2. **结构体字段 ARC 补全**：`retain_struct_fields` / `release_struct_fields` 此前漏了
   `Type::Error`（与两侧不对称），本次一并补上 `Error` 与 `Channel`。

### 3.5 一次设计回退

初版计划让 `close` 同时服务于 `file` 与 `channel`。实现前复核发现：通道生命周期已由原子 ARC
完整管住，再给一个手动 `close` 只会引入「关了之后变量还在、作用域结束再 release 一次」的
**二次释放**风险。故回退——`close` 仍只服务于 `file`，通道不支持关闭（也不提供 EOF 语义）。
该边界已写入 README §27.7 与 §17.2。

---

## 4. 端到端验证

### 4.1 五组测试全绿

| 组 | 内容 | 结果 |
| --- | --- | --- |
| A 正向黄金 | 既有 18 例 + 新增 5 例（`thread_spawn` / `thread_scalar` / `channel_move` / `channel_thread` / `channel_array`） | 全部逐字节一致 |
| B 负向编译失败 | 既有 26 例 + 新增 19 例 | 全部如期被拒 |
| D Fail-Fast | 越界 / 除零仍 `int3` / `ud2` 终止 | 5/5 不变 |
| E `--fast` 对照 | 关闭检查后恢复 C 的静默行为 | 5/5 不变 |
| C 契约 | 产物依赖 | **仅 libc**（无 LLVM、无额外 .so） |

### 4.2 并发输出确定性

多线程 `print` 交错会让逐字节 diff 随机变红且极难复现，因此立下纪律：
**子线程一律不 `print`**，结果经 channel 汇聚回 main 后按序打印；需要确定顺序时用单生产者。
五个新增示例各连跑 **10 次**，输出与黄金文件**完全一致**。

### 4.3 所有权会计压力测试（替代 valgrind / ASan）

沙箱缺 valgrind，clang-18 的 ASan 运行时也未安装，故改用内存压力测试验证会计是否平衡：

- 程序：单生产者循环 20 万次「构造 36 字节 string → `move_to` 入通道 → main `recv` → 释放」
- 结果：`Maximum resident set size = 1356 KB`，**不增长**；无 abort、无 double-free
- 判据：若漏释放，20 万 × 36 字节 ≈ 7 MB 的泄漏量在此分辨率下会立刻显形

### 4.4 零隐式负担

`nm` 抽查 `hello.dial` 产物：**不含**任何 `pthread` / `dial_channel` / `dial_thread` / `thunk` 符号——
不用并发的代码，一点代价都不付。含并发的 `channel_thread` 则正常出现
`pthread_create` / `dial_channel_send` / `dial_channel_recv` / `__dial_thunk_producer`。

---

## 5. 交付物清单

| 文件 | 改动 |
| --- | --- |
| `Cargo.toml` | `0.7.0 → 0.8.0` |
| `src/ast.rs` | `Type::Thread` / `Type::Channel(Box<Type>)` / `Expr::FuncRef` |
| `src/parser.rs` | `parse_type` 的 `channel<T>` / `thread` 特判；`spawn` 首参解析为 `FuncRef` |
| `src/semantic.rs` | `VarState{mutable,moved}` + `check_moved_stmt` + 6 个内建 arm + 元素白名单 + 保留名校验 |
| `src/codegen.rs` | `move_to` / `spawn` / `join` / `detach` / `channel` / `recv` 生成；`gen_spawn_thunk`；`moved` 改槽位指针；`release_value` |
| `stdlib/dial_thread.{h,c}` | NEW：argcell + `pthread_create` 封装 + join/detach |
| `stdlib/dial_channel.{h,c}` | NEW：有界环形队列 + 互斥量 + 条件变量 + 原子 ARC |
| `src/main.rs` | C 编译加 `-pthread`、链接加 `-lpthread` |
| `examples/*.dial` + `.expected` | NEW 5 组 |
| `tests/bad/*.dial` | NEW 19 个 |
| `README.md` | §4.3 关键字、§5.1 类型表、§17.2 边界、**§27 新增章节** |
| `CHANGELOG.md` / `tools/build_web.py` / `web/*.html` | 同步 v0.8 |
| `REPORT_v0.8.md` | 本报告 |
| `dialc-v0.8.zip` | 发布包（含 `.git` + 四份许可文件） |

---

## 6. 后续路线图（需用户拍板方可推进）

| 项 | 说明 |
| --- | --- |
| 通道 `close` 与 EOF 语义 | 需要「元素空值工厂」解决指针类型的默认值问题 |
| `try_recv` / `select` / 多路复用 | 非阻塞接收 |
| 线程池、协程、异步 | — |
| 标量装箱入通道 | 与「零分配移交」立场冲突，需先定调 |
| 包分发与依赖管理 | README §25 草案，尚未实现 |
