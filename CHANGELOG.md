# 更新日志 / Changelog

本文件记录 Dial 语言编译器 `dialc` 的每个发布版本的相对变更。

> **版本号规则（强制）**
>
> 1. **最多两位小数**的单一十进制数：`v0.1`、`v0.2`、…、`v0.9`、`v0.91`、`v0.92`……
>    不再使用语义化版本的「主.次.修订」三段式。
> 2. **版本标题一律带 `v` 前缀、不写补丁位**：写作 `[v0.91]`，**不是** `[0.91.0]`。
>    （源码 `Cargo.toml` 的 `version` 字段受 Cargo 限制仍写作 `0.91.0`，那是存储格式，
>    文档与官网一律显示为 `v0.91`。）
> 3. **版本号必须严格按时间递增，绝不许倒流**：后发版本的日期不得早于先前版本。
>
> 详见 [README §23](#23-版本号规则)。

---

## [v0.94] — 2026-10-31

本版落地 **`dialc lsp` 语言服务器骨架**（见 README [§28](#28-语言服务器-dialc-lspv094-骨架)），
把「写代码」这件事的最后一环补上：编辑器里能看到红线、能看到高亮，
而且**红线与 `dialc build` 的报错是同一条错误、同一个位置、同一句文案**。

至此用户指定的「开发闭环」三项（char 类型 → import 机制 → LSP 骨架）全部打通。

### Added（新增）
- **`dialc lsp` 子命令**：stdio 上的 JSON-RPC 服务（`Content-Length` 分帧）。
  不引入 LSP 框架——骨架只需要「读一条 JSON、写一条 JSON」，
  异步运行时与依赖树的代价远超收益。
- **诊断**：`didOpen` / `didSave` 时发布；词法 / 语法 / 语义三阶段任一失败即停，
  只报第一个错误（与 `dialc build` 行为一致）。改正保存后自动清空，关文件后不留残留红线。
- **语义高亮**：`textDocument/semanticTokens/full`，token 由**编译器自己的词法器**产出
  （不另写一套高亮扫描器——一旦有两份词法，高亮与编译必然慢慢分道扬镳）。
- **VS Code 扩展** `editors/vscode/`：**不需要 npm install**（LSP 客户端是手写的 200 行），
  拷进扩展目录即可；另附 Neovim 原生 LSP 的三行配置。

### Changed（变更）
- **`If` / `While` / `For` / 表达式语句补上位置**（此前只有 `var` / `return` 这类带）：
  所有语句现在都带 `line` / `col`，语义诊断因此能落回源码。
- **语义错误的语句级位置兜底**：`check_stmt_at` 把内部产生的「无位置」错误
  盖到出错那一条语句上（只补缺失的，已精确定位的原样放行）。
  修掉的是一个真实缺陷：表达式错误原先一律报「第 0 行第 0 列」，
  在编辑器里就是**第一行一个红点**，等于没定位。
  B 组负向用例的输出也因此全部带上了「第 N 行第 M 列」。
- **`char` 参与算术的专门诊断**：此前落到通用的「要求两个操作数类型完全相同」那条，
  会让人以为补个转换就能做算术。现在单独说明：
  `'a' + 1` 的意图无法判定，请先 `char_to_int` 再 `int_to_char`。
- **`dialc install` 回显包导出函数**：装完直接列出 `extern fn` 签名与其 `///` 文档，
  不必再去翻 `.diali` 文件。

### Added（测试）
- 测试组 **I**（`tests/lsp_smoke.py`）：真起一个 `dialc lsp` 子进程按协议往返，
  覆盖初始化能力协商、诊断定位到出错那一行、语义 token 的**差分解码 + 源码子串反查**、
  `shutdown` 后干净退出、以及 stderr 不污染协议流。
- 其中 UTF-16 换算一条**特意用 emoji 而非中文**做用例：
  中文在 BMP 内（1 char = 1 码元）**测不出**换算错误，只有 emoji（1 char = 2 码元）能。
  已反向验证过：删掉换算代码重跑，确实只有 emoji 那条变红。

### 刻意未做
补全 / 跳转定义 / 悬停 / 重命名 / 格式化——这些都建立在**跨文件符号表**之上，
而 Dial 目前一次编译一个文件，没有符号表可查。硬做只能做出「猜」的结果，比不做更糟。

### Docs（文档）
- **[§3.1 运行环境要求](#31-运行环境要求linux--动态-glibc)**：明确 Dial **只支持 Linux（x86_64 · 动态 glibc）**，
  并只在**近两年内发布的发行版**上验证（Ubuntu 24.04 LTS / Fedora 42 / Debian 13 等）；
  **明确排除 musl 系统（Alpine 等）**——编译器本体与产物都依赖 glibc，跨 ABI 用不了。
  列出工具链版本（Rust ≥ 1.80、LLVM 18 开发库、`zig cc` 或 `clang`）与运行 `dialc` 本体所需的
  `LD_LIBRARY_PATH`（仅影响编译器，不影响产物）。
- **[§3.2 从源码构建](#32-从源码构建克隆--进入--编译)**：把流程钉死为
  **克隆 → 进入（`cd Dial`，仓库根即编译器本体）→ 编译（`cargo build --release`）**，
  并说明仓库根就是 `Cargo.toml` 与 `src/` 所在之处（没有嵌套的 `dialc/` 子目录），避免读者多进一层目录。
- 官网「快速开始」同步加了 Linux/glibc 提示、`LD_LIBRARY_PATH` 步骤与九组自测命令。

## [v0.93] — 2026-10-31

本版落地 **`import` 与 `.dialpkg` 包分发**（见 README [§25.9](#259-用法import-与-dialc-installv092-已实现)），
完成了 ARCHITECTURE.md 实现路线的**阶段 1 / 3 / 4** 与阶段 2 的本地安装部分。
这是让「标准库能跑起来」的第一块地基。

### Added（新增）
- **`import "包名";` 语句**（词法 / 语法 / 两阶段解析）。包名是字符串字面量而非标识符——
  刻意避开为命名空间再引入一套 `.` / `::` 解析规则。
- **新增模块 `src/package.rs`**：`.dialpkg` 解包、缓存管理、manifest 解析、`.diali` 解析。
- **`dialc install <包.dialpkg>`**：解压到 `~/.dial/packages/<name>/`，**项目目录零侵入**；
  缓存是可重建的派生数据。
- **`.dialpkg` 解包**：zip 容器 + **路径穿越防护**（拒绝绝对路径、`..` 逃逸、Windows 盘符）。
- **`manifest.txt` 解析**（JSON）：`name` / `version` / `abi` / `iface` / `license` /
  `targets`（Triple 映射字典）/ `sha256`（逐文件哈希）。字段缺失一律报错，绝不静默补默认值。
- **按 Target Triple 选库**：编译器按自己的 triple 索引 `targets`，
  查不到就报错并**列出本包支持哪些 triple**；交叉编译到 Windows 时自动链 `.lib`。
- **SHA256 强制校验**：`.diali` 与**本次实际要用的那份**机器码各自校验（§5.1）。
  残缺或被篡改的二进制**绝不进入链接器**。
- **`.diali` 解析**：`type X;`（不透明句柄）与 `extern fn f(a: T): R;`，
  签名接入 Sema 做类型检查；未声明又非内建的类型名一律报错。
- **新类型 `Type::Opaque(name)`**：包提供的不透明句柄，IR 为 `ptr`、**不受 ARC 管理**
  （生命周期由包自己的 open/close 掌管），与 `file` / `thread` 同一阵营。
- **链接期自动链库**：把选定平台的 `.a`（或 `.lib`）加入链接命令，用户不写路径。
- **许可可见**（§25.8）：`manifest.txt` 的 `license` 为必填字段，构建时回显到日志。

### Changed（变更）
- **两阶段解析**：先解析一遍取 `import` 列表 → 解析包（拿到不透明类型名）→ 再重新解析，
  使 `var h: DemoHandle = ...` 这类包类型名在**语法阶段**就被识别为 `Opaque` 而非结构体。
- **`analyze` 与 `compile` 接受外部函数表**：包函数对调用方与本地函数别无二致，
  区别仅是「函数体在别人的 `.a` 里」。名字冲突当场报错。
- **void 函数调用可作语句**：`close(f);` / `demo_free(h);` 这类「只做事、不给值」的调用
  此前在 codegen 被拒（「返回 void，暂不支持用作表达式」），现已放行。
  「把 void 当值用」仍由**语义层**挡住——两条规则各司其职。
- **首次引入第三方依赖** `zip` / `serde_json` / `sha2`：都只服务于「读外部数据」这一处；
  哈希与容器解析不手写（手写出 bug 的代价是「恶意包被判为可信」）。

### Added（测试）
- 测试组 **H**：用 `tools/make_sample_pkg.sh` 现场构建示例包 → 安装 → `import` 后编译运行并比对输出
  → **故意篡改一个字节**验证 SHA256 确实拦得住。
- `tests/pkg/use_demo.dial` 覆盖四种签名形态：纯数值、无参、返回不透明句柄、
  以不透明句柄为参数，以及 void 释放函数。

### 尚未实现（本版刻意留白）
- `dialc install <URL>`（网络下载；本地文件安装已可用）。
- 一键安装脚本（`install-dial.sh` / `.ps1`）、Windows 线程后端、官方标准库的 `.dialpkg` 化。
- `.diali` 接入 LSP（补全 / 悬停）——待 LSP 骨架落地。

### 验证
- A/B/D/E/C/F/G **七组原有测试全绿**，新增 H 组通过；端到端实测
  `demo_add`→7、`demo_version`→42、不透明句柄 `demo_new`+`demo_get`→99、void `demo_free` 正常。

---

## [v0.92] — 2026-10-31

本版落地 **`char` 单字符类型**（见 README [§5.4](#54-char单字符类型v092)），
并集中修复了一批**宪法级文档自相矛盾**与一个真实的运行时缺陷。

### Added（新增）
- **`char` 类型**：一个 Unicode 码点。值类型、无 ARC、无堆分配、无空指针，
  IR 层与 `int` 同为 `i64`，但**类型独立**。
- **字符字面量** `'a'` / `'中'` / `'\n'`，支持 `\n` `\t` `\r` `\0` `\\` `\'` `\"` 转义；
  空 `''` 与多字符 `'ab'` 在**词法阶段**即被拒绝（它们根本不是一个字符）。
- **比较运算**：`==` `!=` `<` `>` `<=` `>=` 按码点序比较（`'a' < 'b'` 为真）。
- **零成本显式转换** `char_to_int(c): int` 与 `int_to_char(n): char`：
  二者**不产生任何 LLVM 指令**（char 与 int 同表示），纯粹是编译期的类型视图切换。
- **C Runtime** `dial_print_char`：按 UTF-8 编码输出 1~4 字节。
  非法码点（负数、> `0x10FFFF`、UTF-16 代理区 `D800~DFFF`）当场打印诊断并终止。
- 正向示例 `examples/char_demo.dial`（含多字节 UTF-8 `中` / `é`）；
  负向用例 5 个（char 算术 / char 与 int 混用 / `char_to_int` 传 int / `int_to_char` 传 char / int 赋给 char 变量）。

### Changed（语义与规则变更）
- **统一容器写回规则**：废除 `m = m.put(1, 1)` 这类手写赋值，与数组 `push` 统一——
  **所有原地修改并触发 COW 的方法（`push` / `put`），编译器自动写回，严禁用户手写赋值**。
  现在 `a = a.push(...)` / `m = m.put(...)` 会在**编译期被拒绝**
  （把结果赋给**另一个**变量仍然允许，那是取新容器而非就地修改）。
- **版本号规则重写**：废弃此前的「十进制递增 / 三段式」表述，改为
  **最多两位小数**（`v0.1`…`v0.9`、`v0.91`、`v0.92`），版本标题统一为 **`[v0.92]`** 而非 `[0.92.0]`，
  且**版本号必须严格按时间递增、绝不倒流**。
- **许可隔离声明**（README §3.3.1 / §25.8 / LICENSE）：明确**按组件划分**——
  编译器前端为三重许可，但被静态链入用户程序的 `libdial_std.a`
  **强制锁定为 MIT / Apache-2.0，绝不允许以 GPL 条款链接进闭源程序**。

### Fixed（修复）
- **未捕获异常打印出乱码**（真实缺陷）：`dial_error_msg` 返回的是不透明 `DialString` 句柄，
  却被直接当作 `const char*` 传给 `dial_panic`，导致 panic 只打印出一个数字（实测 `9`）而非错误消息。
  已补上 `dial_string_cstr` 转换，现在正确输出
  `dial: 未捕获的异常（panic）: 无法打开文件（路径不存在或无访问权限）`。
  新增测试组 **G** 锁定该契约。
- **§27.9.5 的 select 空转教程自相矛盾**：原文称「`while (true) { select { ... } }」本身安全，
  实则全体 EOF 时 select 只是**直接结束**、不会跳出循环，于是每轮立刻结束又立刻重入，
  构成 **100% CPU 空转死循环**（实测 5 秒超时、`exit 124`）。
  已改为演示两种安全退出写法，并各配一个可运行正例（连跑 10 次输出稳定）：
  `examples/select_exit_condition.dial`（`while (not done(ch))`）与
  `examples/select_exit_break.dial`（case 内 `break`）。
- **§26.1 异常语法漏洞**：原文笼统断言「throw 必须出现在 try 块内」，混淆了两种情形。
  现明确区分（§26.1.1）：**显式 `throw Error(...)`** 不在 try 内 → **编译期报错**；
  **内建函数的隐式抛出**（`open` / `read` / `write` 失败）允许写在任何地方 → **编译通过**，
  运行到该处若无捕获则触发未捕获异常 panic 终止。
- **首页示例使用 `+` 拼接字符串**：Dial 的字符串没有 `+` 运算符，已改为 `print("hello, ".concat(name))`
  （同步修正生成器 `tools/build_web.py` 与产物 `web/index.html`，避免重新生成时复发）。

### 验证
- A/B/D/E/C/F **六组原有测试全绿**，新增 G 组（未捕获异常契约）通过。
- 新增 5 个 char 负向用例 + 2 个写回规则负向用例 + 2 个 select 安全退出正例，均纳入自测。

---

## [v0.91] — 2026-10-30

本版落地**多路复用 `select`**（见 README [§27.9](README.md#279-多路复用-selectv091)）——
同时监听多个通道，任一就绪即执行对应分支。

### Added（新增）
- **新语法**（4 个关键字）：
  ```dial
  select {
      case recv(chA) as a { ... }
      case recv(chB) as b { ... }
      default { ... }
  }
  ```
  **刻意不照搬 Go**：接收永远是**函数调用** `recv(ch)`（没有 `<-` 运算符），
  绑定变量用 **`as`** 关键字（不是 `:=`）。`select` 是**语句**不是表达式——
  各 case 的绑定变量类型可以互不相同，不存在统一返回类型，编译期拒绝 `let x = select {...}`。
- **内建语义三条**：
  1. **case 只拿真实值**：分支体内绝不会拿到空指针（底层非阻塞取，元素被抢走会自动重挑）；
  2. **全体 EOF → select 直接结束**：所有通道都「已关闭且已空」时，select 语句结束，
     **一个分支都不执行**（包括 `default`）；
  3. **`default` 使 select 非阻塞**：没有任何通道就绪时立刻执行 default，绝不等待。
- **事件驱动的等待原语**（C Runtime）：全局事件纪元 `epoch` + 全局条件变量广播。
  - **不丢唤醒**：先读 epoch → 再查就绪 → 再睡到 epoch 变化，过期的等待假设永不成立；
  - **不死锁**：notify 一律在**释放通道本地锁之后**调用，
    「持通道锁 → 取全局锁」与「持全局锁 → 取通道锁」两种嵌套**都不存在**。
- **随机选取就绪通道（防饿死）**：多通道同时有数据时随机挑一个，
  起点用**原子递增**计数器取模（不用 `rand()`——它不是线程安全的）。
- **C Runtime**：`dial_chan_epoch` / `dial_chan_wait` / `dial_channel_pick_ready`，
  以及 selector 对象（`dial_sel_new` / `add` / `pick` / `recv` / `free`）。

### Added（测试与示例）
- `examples/select_two_channels.dial`：**双通道并发**——两个生产者往不同通道发数据，
  一个消费者同时监听；顺带校验「收到的确实是本通道的元素」（防串通道）。
- `examples/select_default.dial`：`default` 的非阻塞语义。
- `examples/select_eof.dial`：全体 EOF 时 select 直接结束。
- `examples/select_fairness.dial`：随机选取的防饿死断言（两个通道都必须被处理过）。
- 负向用例新增 9 个（case 非 channel / 元素类型未定 / 缺 `as` / 绑定变量作用域外使用 /
  case 不是 recv 调用 / select 为空 / 两个 default / select 当表达式 / case 用 try_recv）。

### Fixed（修复）
- **IR 生成缺一条跳转导致编译器段错误**：select 的 setup 块原先没有接上调用方当前块，
  使 `while` 的 body 块变成无终结指令的空块，LLVM 在分支概率分析时段错误。
  已补上「当前块 → setup」的跳转。

### 性能数据

| 场景 | 结果 |
| --- | --- |
| 50 万次 send + select（阻塞式，事件驱动） | **wall 0.19 s**，CPU 0.36 s |
| 同等工作量改用 `default`（非阻塞轮询） | 空转 0.38 s 后**卡死在 join**（队列塞满、生产者永久阻塞），120 s 超时 |
| 8 消费者 / 100 万元素并发压测（F 组） | 回归不变：`received=1000000 duplicate=0` |

### 已知限制与未来方向
- **惊群**：全局广播会唤醒所有 select 等待者，多数醒来后发现「不是我」。
  基线实现选择「逻辑正确 + 低延迟」优先于惊群开销（惊群只造成少量无谓唤醒，
  轮询方案则持续 CPU 空转 + 毫秒级延迟）。
  Linux 上可用 **epoll + eventfd** 消除惊群——v0.92 之后的优化方向，不影响本版语义。
- `select` **不带超时**：要「等一会儿再看」请用 `recv_timeout` 单独接收。

---

## [v0.9] — 2026-10-30

本版做的是**并发底座的安全加固**：判空、多消费者安全、超时接收
（见 README [§27.8](#278-判空多消费者安全与超时v09)）。

> **最重要的不是新语法，而是修掉了一个真实存在的死锁。**
> v0.8/v0.81 的通道通知用的是 `pthread_cond_signal`（只唤醒一个等待者）。
> 单消费者看不出问题；多消费者下，「队列非空」是众多消费者共享的状态——
> 唤醒一个之后若队列仍非空，其余消费者会带着已经失效的等待条件继续睡死。
> 对照实验（8 消费者 / 100 万元素 / 队列深 64）：`signal` 版本 **25 秒后超时挂死**，
> `broadcast` 版本 **4.25 秒跑完**，总数恰好 100 万、零重复、零丢失。

### Added（新增）
- **内建 `is_null(x)`**：句柄判空。准入名单 `string` / `int[]` / `map` / `error` /
  `channel<T>` / `thread` / `file`；`int` / `float` / `bool` 是值类型、永远非空，
  编译期直接拒绝（而不是让它变成恒为 false 的死代码）。
  IR 层只是一次指针比较（`ptrtoint` + `icmp eq 0`，优化器折叠回 `icmp eq ptr %p, null`），
  **零函数调用、零引用计数操作**。
- **内建 `recv_timeout(ch, ms)`**：带超时的接收。超时或已关闭且已空时返回空指针
  （用 `is_null` 判别），**绝不无限期阻塞**。
  所有权规则与 `recv` 完全一致。底层就是 `pthread_cond_timedwait` + 一个绝对时刻，
  **没有引入任何状态机**。
- **测试组 F**：绕开编译器直压 `stdlib/dial_channel.c` 的并发压测
  （`tests/stress/channel_mt.c`）：8 个消费者抢同一个通道、100 万元素、队列深 64。
  用「位图 + 原子 `fetch_or`」逐个记录领取情况，重复领取即判失败；总数必须守恒。

### Fixed（修复）
- **多消费者丢唤醒（死锁）**：通道的所有状态变化通知由 `pthread_cond_signal`
  改为 `pthread_cond_broadcast`。单消费者下无差别，多消费者下前者会让某个消费者
  永久阻塞。正确性优先于惊群开销。
- **超时不会吞掉元素**：`pthread_cond_timedwait` 返回 `ETIMEDOUT` 后回到 `while` 条件
  重新判定，若此刻队列非空则正常取走——「元素恰好在超时这一刻到达」也能被拿到。
- **超时基准钟从 `CLOCK_REALTIME` 切回 `CLOCK_MONOTONIC`**：底层超时取用的绝对时刻
  原先取自 `CLOCK_REALTIME`，会被 NTP 校时跳变干扰（校时回拨可能让等待被提前唤醒、
  前拨则拖长等待）。现已切回单调钟 `CLOCK_MONOTONIC`，借助 Linux vDSO 直接在用户态读取、
  无需系统调用，时间读取更快，且彻底不受 NTP 校时影响；因彻底放弃 macOS，
  无需为 `pthread_condattr_setclock` 的兼容性做任何妥协。

### Added（测试与示例）
- `examples/is_null_demo.dial`：EOF / 超时 / 真实值三种情形下 `is_null` 的判定。
- `examples/recv_timeout.dial`：超时返回空值；超时与到达同时发生时以「有元素」为准。
- `examples/channel_multiconsumer.dial`：**两个消费者抢同一个通道**。
  每个消费者把收到的元素转发到自己的报告通道，主线程分别取数后相加——
  各消费者分到多少不确定，但**总数恒为 100**（每个元素只被弹出一次）。
  注意多消费者必须用 `recv` + `is_null` 判结束，**不能用 `try_recv`**：
  后者在「队列暂时空」时也返回空指针，那不是 EOF，会漏收元素。
- 负向用例新增 8 个（`is_null` 作用于标量 / 参数个数 / `recv_timeout` 的四种错用 /
  两个新名字撞保留名）。

### Changed（文档）
- README §27.8 新增「判空、多消费者安全与超时」，§27.9 边界表同步；
  §18 测试章节补 F 组与新示例清单；§17.2 未实现清单同步。

### 验证数据

| 项目 | 结果 |
| --- | --- |
| C 层并发压测（8 消费者 / 100 万元素） | 4.25 s，`received=1000000 duplicate=0 out_of_range=0`，RSS 2.0 MB |
| 同上，`signal` 版本对照 | **25 s 超时挂死**（exit 124）——坐实丢唤醒 |
| Dial 层百万次移交（1 生产者 / 3 消费者） | 1.11 s，RSS 1972 KB **稳定不增长**，无崩溃无挂起 |
| 三个新示例 | 各连跑 10 次输出逐字节一致 |

---

## [v0.81] — 2026-10-30

本版补上 v0.8 并发模型**唯一的功能缺口：通道的关闭与 EOF**
（见 README [§27.7](#277-通道关闭与-eofv081)）。

> **缺口本身**：v0.8 的通道没有 EOF。生产者一旦退出，消费者就永远卡在 `recv` 上——
> 程序不是跑错，而是**死锁**。也就是说「生产者结束 → 消费者收尾」这个最常见的并发范式
> 在 v0.8 里**根本写不出来**。v0.81 让它可以写了。

### Added（新增）
- **内建 `close(ch)`**：关闭通道（幂等），广播唤醒所有阻塞在 `send` / `recv` 上的线程。
  同时服务于 `file` 与 `channel`（二者都是「显式终结」），按实参静态类型分派。
- **内建 `done(ch): bool`**：**已关闭 且 已空** —— EOF 的精确定义，且**单调**为真，
  因此 `while (not done(ch)) { r = recv(ch); }` 这个循环**必然终止**。
- **内建 `closed(ch): bool`** 与 **`len(ch): int`**：是否已关闭 / 队列中当前的剩余元素数。
- **内建 `try_recv(ch): T`**：非阻塞取出，队列空立即返回，绝不等待
  （配合 `len(ch) > 0` 使用；单选消费者下该条件保证必然取到）。
- **C Runtime**：`dial_channel_close` / `closed` / `len` / `done` / `try_recv`。
  `closed` 标志**不设 atomic**——因为 `done` 必须与元素计数在同一互斥临界区内判定，
  拆成两个原子读反而会读到不同时刻的状态。

### Changed（语义变更）
- **`recv` 具备 EOF 语义**：「已关闭且已空」时**立即返回空指针**，不再永久阻塞。
  未关闭时行为完全不变（既有代码不受影响）。
- **`send` 到已关闭通道 → Fail-Fast**：元素会永久滞留，使 `done` 永远为假，
  消费者的收尾循环永不结束——一次「关了还发」会把 EOF 语义整个废掉。
  ReleaseSafe 及以上当场终止；`--fast` 下关闭该检查。
- **`print` 一个 NULL `string` → Fail-Fast**：EOF 之后误用取回的值时，
  在使用点打印诊断并 `ud2`，而不是解引用 NULL 变成一次无解释的段错误。
- **保留名校验补齐**：`close` / `closed` / `len` / `done` / `try_recv`
  不能用作函数名或结构体名（此前 `close` 虽按名字分派内建却未列入保留表）。

### 修正的设计回退
- v0.8 曾刻意**不**把 `close` 扩展到 `channel`，担心「关掉之后变量还在、作用域结束再
  `release` 一次」造成二次释放。复核后确认该担心**不成立**：`close` 只置标志 + 广播，
  既不释放通道对象也不动引用计数；对象销毁仍**只由原子 ARC 归零触发一次**。二者职责不重叠。

### 测试
- A 组新增 `examples/channel_close.dial`（生产者 `close` → 消费者 `done` 收尾）与
  `examples/channel_try_recv.dial`（`len` / `try_recv` / `closed` / `done` 状态机），
  各连跑 10 次输出逐字节一致。
- B 组新增 11 个负向用例（错用 `close` / `closed` / `done` / `len` / `try_recv`、
  元素类型未定就 `try_recv`、用保留名定义函数与结构体）。
- D 组新增 `tests/trap/channel_send_after_close.dial`：关闭后 `send` 必须信号终止且不打印哨兵；
  E 组 `--fast` 下恢复 UB（打印哨兵），证明陷阱确由编译器插入。
- 压力测试：20 万次移交 + EOF 收尾，RSS 稳定 1.4 MB 不增长，无死锁、无泄漏。

---

## [v0.8] — 2026-10-30

本版落地**所有权转移 `move_to` 与多线程并发（`thread` / `channel`）**
（见 README [§27](#27-所有权转移与多线程并发move_to--thread--channel)）。
核心主张：**用「转移所有权」取代「共享内存 + 锁」**——数据不共享、只移交，
队列里流动的指针永远只有一个持有者，因此不存在数据竞争，也就不需要任何互斥锁。

### Added（新增）
- **类型**：`thread`（线程句柄，显式 `join` / `detach`，不受 ARC 管理）与
  `channel<T>`（通道句柄，跨线程共享 → **原子**引用计数 ARC）。
- **内建 `spawn(fn, args...)`**：创建并**立即**启动线程。所有实参在 `pthread_create`
  **之前**写进 argcell —— 零竞争窗口，这也是「move_to 到线程」的**唯一**合法形式。
- **内建 `move_to(x, ch)`**：编译期 intrinsic，把 `x` 的所有权移交给通道。
- **内建 `join` / `detach` / `channel(cap)` / `recv(ch)`**。
- **C Runtime**：新增 `stdlib/dial_thread.{h,c}`（argcell + `pthread_create` 封装）与
  `stdlib/dial_channel.{h,c}`（有界环形队列 + 互斥量 + 条件变量 + 原子引用计数）。
- **Moved 状态进 Sema**：被移交的变量此后**读取 / 写入 / 再次转移**全部编译期拒绝
  （「变量已转移，无法使用」）。标记落在声明该变量的作用域，分支内移交也会被保守拦下。
- **`spawn` thunk**：编译器为每个被 spawn 的函数生成私有 thunk（`void f(ptr argcell)`），
  按 8 字节槽宽取出实参后调用真函数，并负责释放被移交进来的那一份所有权。
- **保留名校验**：`spawn` / `join` / `detach` / `channel` / `recv` / `move_to` / `thread`
  不能用作函数名或结构体名。
- **示例**：`examples/{thread_spawn,thread_scalar,channel_move,channel_thread,channel_array}.dial`；
  **负向用例** 19 个（含 `move_use_after` / `move_twice` / `spawn_non_void_ret` /
  `spawn_struct_param` / `channel_int_elem` / `fn_named_move_to` 等）。

### Changed（变更）
- **`moved` 集合由「变量名」改为「栈槽指针」**：变量名会跨作用域 / 跨函数重名，按名字记账
  会让同名变量互相误伤（该 release 的被跳过 → 泄漏）。改为按 alloca 槽位记账后天然精确。
- **结构体字段 ARC 补全**：`retain_struct_fields` / `release_struct_fields` 此前漏了 `Type::Error`
  （与两侧不对称），本次一并补上 `Error` 与 `Channel`。
- **链接**：C 编译追加 `-pthread`、链接追加 `-lpthread`（glibc ≥ 2.34 已并入 libc，此处为兼容老 glibc）。

### 设计约束（本版刻意不做）
- **通道元素只能是句柄**（`string` / `int[]` / `map` / `error` / `channel`）：标量需装箱，
  编译期拒绝。
- **不支持 `move_to` 到已启动的线程**：那条路必然有竞争，移交只能在 `spawn` 时完成。
- **不支持 `close(ch)` / `try_recv` / `select` / 线程池 / 原子类型**。
- **线程入口必须返回 `void`、禁止 `struct` 形参**；`spawn` 首参只能是顶层具名函数（Dial 无一等函数）。

### 验证
- A/B/D/E/C 五组全绿（新增 5 正向 + 19 负向），C 组产物仍**仅依赖 libc**（无 LLVM、无额外 .so）。
- **零隐式负担**：`hello.dial` 产物中不含任何 `pthread` / `dial_channel` / `dial_thread` 符号。
- **所有权会计压力测试**：20 万次「构造 → move_to → recv → 释放」后 RSS 稳定在 1.3 MB 不增长
  （若漏释放应约 7 MB），且无 double-free abort。

---

## [v0.7] — 2026-10-30

本版落地**可恢复异常（try / catch / throw）**（见 README [§26](#26-可恢复异常trycatchthrow)），
补齐了 v0.6 只有 `ud2`（不可恢复崩溃）的那一半；同时正式声明**三重开源许可**。

### Added（新增）
- **可恢复异常语法**：`throw Error("msg");` 抛出；`try { } catch e { }` 捕获（**catch 后无圆括号**，
  `e` 类型自动推导为 `error`）；`try { } catch { }` 省略变量名表示忽略具体错误。
- **内建 `Error` 类型**：`error`，底层 `ptr`（不透明），**受 ARC 管理**；构造 `Error("msg")`，
  方法 `.msg()` 取消息字符串；`print(e)` 等价于打印 `e.msg()`。
- **C Runtime**：新增 `stdlib/dial_error.{h,c}`（引用计数错误对象）与
  `stdlib/dial_exc.{h,c}`（`jmp_buf` 容器 `DialExBuf` + `dial_throw_to` / `dial_exbuf_take_err` / `dial_panic`）。
- **失败与业务分离**：`open` / `read` / `write` 失败由「返回错误标志」改为**抛出 Error**
  （判据在 C 侧、抛出在 Dial 侧）；越界 / 除零**仍走** `ud2` / Fail-Fast，绝不混入异常。
- **三重开源许可**：`GPL-3.0 OR MIT OR Apache-2.0`（任选其一）。三份协议原文
  `LICENSE-GPLv3` / `LICENSE-MIT` / `LICENSE-APACHE` 与说明文档 `LICENSE` 随源码及发布包分发。
- **打包脚本** `tools/package.sh`：发布压缩包**强制校验**四份许可文件与 `.git` 均在包内，缺一即拒绝打包。
- **生态规划草案**：README 新增 [§25 包分发与依赖管理](#25-包分发与依赖管理生态规划--草案)
  （`.dialpkg` 专属格式、自动解压到 `~/.dial/packages/`、接口签名文件 `.diali`、SHA256 强制校验、零版本冲突）。
- **示例与负向用例**：`examples/try_catch_demo.dial`、`examples/error_zero_cost.dial`；
  `tests/bad/{throw_non_error,catch_with_paren,throw_outside_try}.dial`。

### Changed（变更）
- `dial_file_read` 的失败语义由「返回空串」改为**返回 `NULL`**，使「读取失败」与「读到空内容」不再混淆
  （成功读取 0 字节仍返回空串）。
- `readme` 同步：顶部三重许可说明、§4.3 关键字（补 `try`/`catch`/`throw`）、§5.1 类型表（补 `error`）、
  §17.2（补回 `move_to` / 并发为「规划中」）、§24.2（文件 IO 现抛 Error）、新增 §25 与 §26。

### 设计约束（用户拍板）
- **极简语法**：只 `throw Error(...)` 与 `try/catch`；**严禁抛出非 `Error` 类型**（裸 int/string 编译期拒绝）。
- **catch 后不得有圆括号**：括号在 C 骨架里属于函数或条件专属，用在 `catch` 后属视觉污染。
- **底层防漏**：跳转前**必须**自动为 try 作用域内所有活跃 ARC 局部插入 `release`，绝不泄漏。
- **失败与业务分离**：系统崩溃走 `ud2`（Fail-Fast），业务失败走 `throw`（可恢复）。
- **零隐式负担**：无异常的代码运行时开销为零；已用 `nm` 实测验证（见 §26.6）。

### 推迟（不在本版）
- `move_to` 所有权转移与多线程并发（thread / channel）——需先有并发原语，留待后续专属版本。

---

## [v0.61] — 2026-10-29

本版落地**文件 IO（file 句柄）**（见 README [§24](#24-文件-iofile-句柄)），并正式确立**版本号规则**（最多两位小数、禁止跳版、严格按时间递增，见 README [§23](#23-版本号规则)）。

### Added（新增）
- **`file` 不透明指针类型**：新增 `file` 类型，IR 中为 `ptr`，封装 C 的 `FILE*`，**不受 ARC 管理**；用户必须显式 `open` / `close`（与内存宪法显式指针回收契约一致）。
- **`open` / `read` / `write` / `close`**：仅封装 C 标准库 `fopen` / `fread` / `fwrite` / `fclose`，Rust 前端不重写任何文件逻辑；只提供最基础的 C 风格 API，无链式流、无额外语法糖。
- **C 侧实现**：`stdlib/dial_io.c` 新增 `dial_file_open` / `dial_file_read` / `dial_file_write` / `dial_file_close`；`stdlib/dial_string.c` 新增 `dial_string_cstr` / `dial_string_from_bytes`（供 `read` 从字节缓冲区构造字符串）。
- **示例与负向用例**：`examples/file_demo.dial`（写后读回并打印，黄金输出 `dialc-file-io`）；`tests/bad/io_bad_arg.dial`（open 参数类型错误须在编译期拒绝）。

### Changed（变更）
- 遵循版本号规则：本版属「小功能 / 新语法」，版本号 `v0.6 → v0.61`（Cargo `version` 字段按三段式写作 `0.61.0`，语义等同 `v0.61`）。
- README 同步：§4.3 关键字、§5.1 基本类型、§15 FFI 表与 C 导出函数、§17.2（从「未实现」移除文件 IO）、新增 [§24 文件 IO](#24-文件-iofile-句柄) 与 [§22 版本变更摘要](#22-版本变更摘要)。

### 设计约束（用户拍板）
- **绝不引入全局变量**：文件句柄显式传递。
- **底层走 C 标准库**：直接封装 `FILE*`，不在 Rust 前端重写文件系统逻辑。
- **错误处理（无异常）**：打开失败返回空指针、读失败返回空串、写失败返回 `-1`；不引入 `Result` 封装。
- **不搞花哨语法**：只做 `open` / `read` / `write` / `close` 基础 API，无链式文件读取流。

---

## [v0.6] — 2026-10-29

本版把 Zig 式 **Fail-Fast** 确立为与内存宪法并列的语言底线（见 README §20）：
**宁可当场崩溃，绝不产生未定义行为或静默错误数据。**

### Added（新增）

- **Fail-Fast 陷阱基础设施**：新增 `runtime/dial_rt_trap.h` / `.c`，提供 `dial_trap()`（`__builtin_trap`，
  x86 生成 `ud2`）、`dial_bounds_fail` / `dial_null_fail` / `dial_assert_fail`（先打印诊断再 trap）。
  诊断走 `write(2)` 直写 stderr，并在陷阱前冲刷 stdout 缓冲，保证崩溃上下文完整。
- **LLVM Codegen 硬插入检查**：
  - 数组下标读（`Expr::Index`）与写（`a[i] = v`）前插入边界检查，用一次**无符号比较**
    同时判定 `idx < 0` 与 `idx >= len`（负数的位模式必然 >= len）。
  - 整数除法前插入除零检查——LLVM 的 `sdiv` 除数为 0 时结果是 poison（UB），
    优化器可据此把后续代码整段删掉，比崩溃危险得多。
  - 失败分支发射 `llvm.debugtrap`（Debug → x86 `int3`，可被调试器当断点捕获）
    或 `llvm.trap`（ReleaseSafe → x86 `ud2`，指令更短），随后 `unreachable`。
- **C Runtime 边界检查**：`dial_array_get` / `dial_array_set` / `dial_string_get` /
  `dial_string_substring` 全部加入边界检查，越界调用 `dial_bounds_fail`。
  另新增：OOM 不再返回 NULL（`dial_assert_fail`）、引用计数下溢检测（重复释放 /
  use-after-free 当场终止）、`dial_array_length` / `dial_map_size` 的内部一致性断言。
- **三档安全等级**：`--safety debug|safe|fast`，简写 `--release`（safe）、`--fast`（fast）。
  等级**同时**作用于 LLVM IR 与 C Runtime（`-DDIAL_SAFETY=N`），是编译期常量，
  ReleaseFast 下检查代码被编译器完全消除。默认永远是 Debug——安全是默认值。
- **测试组 D / E**：新增 `tests/trap/`（5 个用例），D 组断言必须当场因信号终止且
  **不得打印哨兵 9999**；E 组用 `--fast` 作对照组，证明陷阱确由编译器插入而非碰巧崩溃。

### Changed（变更）

- `dialc build` / `run` 新增 `--safety` / `--release` / `--fast` 参数。
- C 静态库按安全等级分目录构建（`.dialc_build/safety-<N>/`），避免不同等级的 `.o` 互相污染。
- README §17.1「未定义行为」重写：越界 / 除零在默认等级下**不再是 UB**，而是确定性崩溃。
- README §11.5 越界行为：由「无边界检查」改为「默认有边界检查，仅 `--fast` 回落到 UB」。

### Added（v0.6 补充）

- **`--fast` 危险警告**：编译器在使用 `--fast`（ReleaseFast）时**当场打印醒目警告**，
  说明该模式会关闭所有边界检查、除零检查和空指针保护，越界将退化为 C 的 UB。
  文档侧同步写入 README §3.2 与 §20.4，并出现在官网首页卡片。
  安全性是默认值，选择放弃它的人必须当场被告知代价。

### 实测对照（同一份越界代码，三档行为）

| 等级 | `a[99]`（长度 3） | `100 / 0` |
| --- | --- | --- |
| Debug（默认） | SIGTRAP(5)，`int3` 当场终止 | SIGTRAP(5) 当场终止 |
| ReleaseSafe | SIGILL(4)，`ud2` 当场终止 | SIGILL(4) 当场终止 |
| ReleaseFast | 读出 `0` 并继续执行，打印哨兵 | 打印 `0` 并继续执行（**静默错误结果**） |

---

## [v0.5] — 2026-10-29

本版补齐循环控制流，并把**代码编写纪律**正式写入规范（见 README §20）。

### Added（新增）
- **`break`**：跳出最内层循环；仅允许出现在 `while` / `for` 循环体内，写在别处是编译错误（见 README §8.4）。
- **`continue`**：进入下一次迭代。关键语义——`for` 的 `continue` **先执行 `i = i + 1` 再回到条件判定**：
  实现上把自增放在独立的 `for.inc` 块，`continue` 跳到该块而非条件块；若跳到条件块就会跳过自增造成死循环。
- 新增示例 `examples/loop_control.dial`（覆盖 for+continue、for+break、while+continue/break 三种组合）。
- 新增负向用例 `tests/bad/break_outside_loop.dial`、`tests/bad/continue_outside_loop.dial`。

### Docs（文档）
- README 新增 [§20 代码编写纪律（防臃肿与允许冗余原则）](#20-代码编写纪律防臃肿与允许冗余原则)：
  ① 绝对拒绝过度抽象（宁可 WET，绝不 DRY 过头）② 不引入无用依赖 ③ 允许安全与性能的冗余
  ④ 保持代码直白（能用一个循环和一个 if 解决的，绝不套设计模式）。
- README §8.1 语句一览、§8.4 `break` / `continue`、§17.2 未实现清单（移除已落地的 break/continue）同步更新。
- 新增 `REPORT_v0.5.md`（`objdump` 真实反汇编证据）与 `TEST_RUN_v0.5.log`。
- 官网 `web/` 重新生成，首页版本同步 v0.5。
- 版本号升级至 `v0.5`。

---

## [v0.4] — 2026-10-29

本版落地**部分标准库**与**控制流语法糖**，并正式写入[内存宪法](#内存宪法防-gc-铁律)（见 README §19）。

### Added（新增）
- **`map` 类型（int → int）**：新增哈希表类型，内存布局 `[int64 cap][int64 count][entries...]`，
  与 `string` / `int[]` 共用同一套 **ARC 引用计数**（`dial_map_retain` / `dial_map_release`）。
  采用线性探测开放寻址，删除用左移紧凑；负载因子超过 3/4 自动扩容并重新哈希。
- **`map` 方法**：`put(k,v)`（返回可能更新的指针，编译器写回接收变量）、`get(k)`（未命中 -1）、
  `contains(k)`（bool）、`remove(k)`（返回其值，未命中 -1）、`size()`。
- **`new_map()`**：无参内置函数，构造空表。
- **`read_line()`**：无参内置函数，从标准输入读一行；EOF 返回空串（绝不崩溃）。
- **`else if` 语法糖**：`else if (c) {..}` 编译期展开为嵌套 `if`，不引入新 AST 节点（见 README §8.2）。
- **范围 `for` 循环**：`for (i in start..end)` 循环变量 `i` 为 int，半开区间 `[start, end)`，
  可在循环体内重新赋值（见 README §8.3）。
- 新增示例 `examples/map_demo.dial`、`examples/control_flow.dial`；
  新增负向用例 `tests/bad/map_let_put.dial`、`tests/bad/map_bad_arg.dial`、`tests/bad/for_range_type.dial`。

### Changed（变更）
- `dial_map.c` 由旧版「非 ARC 的 `Map*` 堆分配（进程退出时由 OS 回收）」重写为与 `dial_array.c`
  **完全对称**的 inline-entries ARC 实现：指针因扩容 / COW 可能更换，编译器负责把新指针写回接收变量。
  这消除了旧实现与[内存宪法](#内存宪法防-gc-铁律)（防 GC 铁律）的矛盾。
- Map 与 `read_line` 从「未实现」清单移除（v0.4 已暴露）。

### Fixed（修复）
- 修正 `0..5` 这类范围字面量此前被数字词法器误识别为浮点 `0.5` 的问题：仅当小数点后紧跟数字才视为浮点，
  `..` 现被正确识别为范围运算符。

### Docs（文档）
- README 新增 [§12 哈希表 `map`](#12-哈希表-mapint--int)、[§8.2 `else if`](#82-if--else-if--while)、[§8.3 范围 `for`](#83-范围-for-循环)、
  [§19 内存宪法](#19-内存宪法防-gc-铁律)；并将 FFI 表与 C 导出函数清单补齐 `dial_map_*` / `dial_read_line`。
- 官网文档（`web/`）随 README / CHANGELOG 重新生成。
- 版本号升级至 `v0.4`。

---

## [v0.3] — 2026-10-28

本版只修改**语义与诊断**，未改变任何架构契约（不透明指针、两步构建、产物零 LLVM 依赖、C 承担一切容器）。

### Added（新增）
- 内置类型转换函数 `int_to_float(int): float`，编译为 LLVM `sitofp`（x86-64：`cvtsi2sd`）。
- 内置类型转换函数 `float_to_int(float): int`，编译为 LLVM `fptosi`（x86-64：`cvttsd2si`，向零截断，与 C 的 `(int)` 一致）。
- 两者均为**纯编译期内联**，不经过任何 C 运行时函数，因此不引入额外依赖。
- 新增示例 `examples/short_circuit.dial`（证明短路求值）、`examples/conversions.dial`（证明显式转换）。
- 新增负向用例 `tests/bad/let_push.dial`、`tests/bad/implicit_conv.dial`、`tests/bad/mixed_arith.dial`。

### Changed（变更）
- **`and` / `or` 改为短路求值**，与 C 的 `&&` / `||` 一致：先求左操作数，若已能决定结果则不再求值右操作数。
  这保证了"判空后访问"类代码在左操作数为假时不会因右操作数被盲目求值而崩溃。
- **严格执行"零隐式转换铁律"**：算术、相等、比较运算均要求两个操作数类型**完全相同**；`int` 与 `float`
  之间不再有隐式提升或窄化，混用一律编译错误，必须显式调用 `int_to_float` / `float_to_int`。
- `let` 变量调用 `push` / `set` 等"原地修改且可能更换指针"的方法现在会在语义阶段被**明确拒绝**；
  此类方法的接收者必须是顶层 `var` 变量（详见 README §6.2、§11.3–§11.4）。

### Fixed（修复）
- 修复了此前 `and` / `or` 不短路可能导致的判空崩溃隐患（如 `p != null and p.x > 0` 在 `p` 为空时仍会求值右操作数）。
- 修复了 `int → float` 隐式转换可能造成的静默精度丢失（如 `1 + 2.0` 曾默默将 `int` 提升为 `double`）。
- 修复了 `let` 变量调用 `push` 时"容器被原地修改但接收者无法接收新指针"的悬垂风险，将其提前到编译期拒绝。

### Docs（文档）
- README 同步更新：§5.3 零隐式转换铁律、§6.2 可变性判定（含 `push`/`set`）、§7.2 类型规则、
  §7.3 语义细则表（修正 `and`/`or` 与混合算术两行）、§11.3–§11.4 接收者须为 `var`、§18 v0.3 变更摘要。
- 新增 `REPORT_v0.3.md`：基于 `objdump -d` 真实反汇编的短路求值与 `int_to_float`/`float_to_int` 落地证据，
  以及端到端自测结果与产物零 LLVM 依赖契约校验。

---

## [v0.2] — 基线版本（本仓库首个规范化基线）

> 注：本仓库的历史从 v0.3 的源码状态起记。`v0.2` 及以下版本的特性以下列条目作为基线描述，
> 具体提交不在本仓库历史中。

- 编译器前端（Rust + inkwell）把 Dial AST 翻译为 LLVM IR；IR 全程在内存中构建并直接 `write_to_file` 写出 `.o`。
- 两步构建：`zig cc` 把 `stdlib/` 与 `runtime/` 编译归档为 `libdial_std.a`，再与用户 `.o` 链接。
- 不透明指针契约：`string` / `int[]` 在 IR 中只是 `ptr`，用户代码无法触碰其内存布局。
- 引用计数 + COW 所有权模型，Sema 阶段做数据流/逃逸分析以静态消除运行时开销。
- 基本类型 `int` / `float` / `bool` / `string` / `int[]` / 结构体（值类型）。
- `and` / `or` 在 v0.2 中为**非短路**（旧行为，v0.3 已改为短路）；`int→float` 在 v0.2 中存在隐式提升（v0.3 已废除）。

---

## 未实现（-Unreleased-，v0.2/v0.3 语言层均未暴露，v0.4 起部分已落地）
- 多维数组、非 `int` 元素数组、`char` 类型。
- 裸块 `{ ... }`。
- 模块 / 包 / import、泛型、接口、trait、闭包。
- 指针、地址取值、指针算术（永不提供）；运算符重载（永不提供）；全局变量。
