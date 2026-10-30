# dialc — Dial 语言编译器 v0.94

> **GitHub Repository：** https://github.com/shichencike/Dial

> ## 开源许可：三重许可（任选其一）
>
> Dial 以 **GPL-3.0、MIT、Apache-2.0** 三份协议**同时**发布，用 `OR` 连接：
>
> ```
> GPL-3.0 OR MIT OR Apache-2.0
> ```
>
> **你可以任选其中一份遵守**，无需同时满足三者（详见 [`LICENSE`](LICENSE)）。
> 三份协议原文随源码与发布压缩包**完整分发**：[`LICENSE-GPLv3`](LICENSE-GPLv3)、
> [`LICENSE-MIT`](LICENSE-MIT)、[`LICENSE-APACHE`](LICENSE-APACHE)。
>
> 注意：本许可只覆盖 **Dial 编译器与标准库的源码**。你用 Dial **写出来的程序**
> 只链接 libc，不链接 LLVM 运行库，因此**不受本许可约束**（见 [§3.4 产物依赖契约](#34-产物依赖契约)）。

Dial 是一门**静态强类型、显式标注、无 GC** 的编译型语言。编译器前端（Rust + inkwell）
只负责把 Dial 源码翻译成 LLVM IR；**所有容器与运行时能力都由 C 语言实现**，
Dial 代码对这些能力的一切访问都降级为对 C 导出函数的调用。

本文件是 v0.94 的**规范性文档**：描述的语言行为与 `src/` 中的实现逐条对应，
不描述任何尚未实现的特性。凡文档中未出现的写法，即为不支持。

---

## 1. 设计原则

| 原则 | 含义 |
| --- | --- |
| **静态强类型，无推断** | 每个变量、参数、返回值都必须显式标注类型；不存在 `auto` / 类型推导 |
| **不透明指针（Opaque Pointer）** | `string` 与 `int[]` 在 IR 中只是一个 `ptr`；用户代码永远无法触碰其内存布局 |
| **C 承担一切容器** | 字符串、动态数组、哈希表、IO、内存管理全部由 `stdlib/` 与 `runtime/` 的 C 代码实现 |
| **编译期所有权优化** | Sema 阶段做数据流分析，尽可能把引用计数开销静态消除；只有静态分析失效时才由 C Runtime 的 COW 兜底 |
| **产物零 LLVM 依赖** | 用户编译出的可执行文件只依赖 C Runtime（libc），**绝不**依赖 libLLVM |
| **比 C 更严格** | 强制初始化、强制不可变性、禁止隐式窄化转换、禁止省略结构体字段——把 C 中大量"未定义行为"变成"编译期错误" |
| **无 GC 底线（内存宪法）** | 永不引入追踪式 GC；确定性内存管理靠 ARC + 编译期分析 +（未来）显式裸指针，见 §19 |
| **Fail-Fast 底线** | 宁可当场崩溃，绝不产生未定义行为或静默错误数据；越界/除零必须当场终止，见 §20 |

---

## 2. 目录结构

```
dialc/
├── src/                    # 编译器前端（Rust + inkwell）
│   ├── main.rs             #   驱动：两步构建（emit .o / 链接）
│   ├── lexer.rs            #   词法分析
│   ├── parser.rs           #   语法分析（递归下降 + 优先级）
│   ├── ast.rs              #   抽象语法树与类型定义
│   ├── semantic.rs         #   语义分析 + 数据流分析（移动/共享判定）
│   ├── codegen.rs          #   LLVM IR 生成（inkwell，内存中构建，直接 emit .o）
│   └── error.rs            #   带行列号的错误类型
├── runtime/                # C 运行时（引用计数原语 + Fail-Fast 陷阱）
│   ├── dial_rt_mem.h / .c  #   refcount 分配 / retain / release / COW clone
│   └── dial_rt_trap.h / .c #   Fail-Fast：ud2 陷阱 + 越界/空指针/断言诊断（§20）
├── stdlib/                 # C 标准库（Dial 语言的实际能力来源）
│   ├── dial_string.h / .c  #   String
│   ├── dial_array.h / .c   #   Array<int>
│   ├── dial_map.h / .c     #   Map（int → int，ARC）
│   └── dial_io.h / .c      #   打印 / 读行
├── examples/               # 示例（*.dial + 同名 *.expected 黄金输出）
├── tests/
│   ├── run_tests.sh        # 端到端测试（A 正向 / B 负向 / D Fail-Fast / E 对照 / C 契约）
│   ├── bad/                # 必须编译失败的负向用例
│   └── trap/               # 必须当场因信号终止的 Fail-Fast 用例
└── README.md               # 本文件
```

---

## 3. 构建与使用

### 3.1 运行环境要求（Linux · 动态 glibc）

Dial **只在 Linux 上开发与验证**，并且要求系统使用 **glibc 动态链接**（不是 musl）。

> **为什么强调「动态 glibc」？**
> - `dialc` 编译器本体链接 `libLLVM.so`，而 libLLVM 是 **glibc ABI** 的产物；
>   musl 系统（如 Alpine）里既没有它，跨 ABI 也用不了。
> - 用户编译出的可执行文件**动态链接 libc**（见 [§3.4 产物依赖契约](#34-产物依赖契约)），
>   因此它也只能在 glibc 系统上跑。
> - 所以：**请用带 glibc 的常规发行版**；**Alpine Linux、Void(musl)、Gentoo(musl profile) 等 musl 系统不在支持范围。**

#### 3.1.1 操作系统：近两年内的新发行版本

我们只在**近两年内发布、且采用 glibc 的发行版**上验证过。下表是已知可工作的范围
（x86_64 架构）：

| 发行版 | 推荐版本（发布年份） | glibc 动态链接 |
| --- | --- | --- |
| **Ubuntu** | 24.04 LTS（2024）/ 24.10 / 25.04 / 25.10 | ✅ |
| **Debian** | 13 trixie（2025）/ 12 bookworm（2023，下限） | ✅ |
| **Fedora** | 40 / 41 / 42（2024–2025） | ✅ |
| **Linux Mint** | 22（2024，基于 Ubuntu 24.04） | ✅ |
| **openSUSE** | Leap 15.6（2024）/ Tumbleweed（滚动） | ✅ |
| **Arch Linux** | 滚动（持续在近两年内） | ✅ |
| ❌ **Alpine** | 任意版本 | ❌ musl，不支持 |
| ❌ **Void / Gentoo（musl）** | 任意版本 | ❌ musl，不支持 |

> 一句话：装一台 **2024 年之后发布的、带 glibc 的 x86_64 Linux**（例如 Ubuntu 24.04 LTS 或 Fedora 42），
> 就满足操作系统层面的要求。更老的系统若 glibc 版本过低（< 2.35）也可能无法运行预编译产物，
> 因此「近两年」既是验证范围，也是稳妥下限。

#### 3.1.2 工具链

| 组件 | 版本要求 | 用途 | 备注 |
| --- | --- | --- | --- |
| **Rust 工具链**（`cargo` / `rustc`） | stable（≥ 1.80） | 编译 `dialc` 本体 | 用 `rustup` 安装最省事 |
| **LLVM 18 开发库** | 18.x | `dialc` 链接 `libLLVM.so` | 提供 `libLLVM.so.18`、`llvm-config` |
| **C 编译器**：`zig cc` 优先，缺失时回退 `clang` | zig ≥ 0.11 **或** clang ≥ 18 | 链接阶段把 `stdlib/` + `runtime/` 编成 `libdial_std.a` 并链接 | 见 [§3.3](#33-编译-dial-程序) |

安装示例（Ubuntu 24.04）：

```bash
# LLVM 18（含 libLLVM.so 与 clang）
sudo apt-get update
sudo apt-get install -y llvm-18-dev clang-18 libclang-18-dev

# Rust（若尚未安装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# 链接阶段用的 C 编译器：优先 zig；装了就无需 clang
#   sudo apt-get install -y zig        # 可选，未装则自动回退 clang-18
```

#### 3.1.3 运行 `dialc` 本体时的动态库要求

`dialc` 自身依赖 `libLLVM.so.18`。如果该库不在系统默认库路径里，运行 `dialc` 前需要把它加进
`LD_LIBRARY_PATH`（**仅影响编译器，不影响你编出来的程序**）：

```bash
export LD_LIBRARY_PATH=/usr/lib/llvm-18/lib:$LD_LIBRARY_PATH   # 按本机路径调整
dialc --version
```

> 把 `libLLVM.so.18` 装进 `/usr/lib` 或 `/usr/lib/x86_64-linux-gnu` 等默认路径后，
> 这一行就可以省掉——它只是给「编译器启动」用的，与最终产物的依赖无关。

### 3.2 从源码构建：克隆 → 进入 → 编译

```bash
# 1) 克隆源码
git clone https://github.com/shichencike/Dial.git
cd Dial                                  # 进入工程根目录（编译器本体就在这里：Cargo.toml 与 src/ 都在根）

# 2) 编译（编译器本体就位于仓库根，无需再进子目录）
export LLVM_SYS_181_PREFIX=/usr/lib/llvm-18   # 指向本机 LLVM 18（见 §3.1.2）
cargo build --release                          # 产物：target/release/dialc

# 3) 让编译器本体能找到 libLLVM（见 §3.1.3）
export LD_LIBRARY_PATH=/usr/lib/llvm-18/lib:$LD_LIBRARY_PATH

# 4) 验证
./target/release/dialc --version
```

`dialc` 自身链接 libLLVM——这是**编译器**的依赖，与用户程序无关。

### 3.3 编译 Dial 程序

```bash
dialc build <源文件.dial> [-o 可执行文件] [--no-link] [--safety debug|safe|fast]
dialc run   <源文件.dial> [-o 可执行文件] [--safety debug|safe|fast]
```

安全等级（Fail-Fast，详见 §20）：

| 参数 | 等级 | 行为 |
| --- | --- | --- |
| （默认） | Debug | 崩溃保护全开，陷阱用 `llvm.debugtrap`（x86 `int3`） |
| `--release` / `--safety safe` | ReleaseSafe | 保留防 UB 检查，陷阱用 `llvm.trap`（x86 `ud2`） |
| `--fast` / `--safety fast` | ReleaseFast | 关闭全部检查，越界/除零即 UB（与 C 一致）——⚠️ **危险，详见下方警告** |

等级同时作用于 LLVM IR 与 C Runtime（`-DDIAL_SAFETY=N`），两侧口径一致。

> ⚠️ **关于 `--fast` 模式（ReleaseFast）：此模式会关闭所有边界检查、除零检查和空指针保护，
> 以换取极致的性能。除非你是正在编写底层核心算法、且完全确信你的数组下标绝对不会越界，
> 否则请永远不要使用此模式。在 `--fast` 下发生的越界，将退化为 C 语言的未定义行为（UB），
> 可能导致静默的错误数据甚至灾难性后果。**
> （详见 [§20.4](#204-性能模式的切换)）

`dialc build` 严格分成两步，这是架构约束而非实现细节：

| 步骤 | 负责组件 | 是否需要 LLVM | 产物 |
| --- | --- | --- | --- |
| **第一步：代码生成** | inkwell 在**内存中**构建 IR，再由 `TargetMachine::write_to_file` 直接写出机器码 | **需要** | `<源文件>.o` |
| **第二步：链接** | `zig cc`（缺失时回退 `clang`）把 `stdlib/` 与 `runtime/` 的 C 代码编译并归档为 `libdial_std.a`，再与用户 `.o` 链接 | **不需要** | 可执行文件 |

要点：

- 第一步不导出 `.ll` 文本、不调用外部命令，IR 全程在内存中。
- 第二步完全交给 `zig cc`，此时 LLVM 已退出舞台。
- `--no-link` 只执行第一步，便于交叉编译或自行接管链接。
- 中间产物落在工程根的 `.dialc_build/`（C 的 `.o` 与 `libdial_std.a`）。

### 3.4 产物依赖契约

用户编译出的可执行文件**只允许**依赖 C Runtime：

```bash
$ ldd examples/hello
    linux-vdso.so.1
    libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6
    /lib64/ld-linux-x86-64.so.2
```

`tests/run_tests.sh` 的 C 组会把"产物不得依赖 libLLVM"作为**硬断言**执行。

#### 3.4.1 许可隔离：标准库绝不以 GPL 链接进用户程序（防法务传染）

上表的 `libc` 之外还有一个**被静态链入**用户产物的组件：**`libdial_std.a`**
（`stdlib/` 与 `runtime/` 编译归档而成的 C Runtime，见 [§15](#15-ffi-契约dial-语法--c-函数)）。
它随每一个 Dial 程序一起进入最终可执行文件，因此它的许可条款**直接作用于用户的程序**：

> **Dial 编译器前端（`dialc` 本体）采用三重许可：`GPL-3.0 OR MIT OR Apache-2.0`，任选其一。**
> **但 Dial 的标准库（`libdial_std.a`）必须额外豁免，并强制锁定为 `MIT` 或 `Apache-2.0` 二选一；
> 绝对不允许以 GPL 协议的条款把它链接进用户的程序（尤其是闭源程序），以避免法务传染。**

为什么必须这样切分：

| 事实 | 后果 |
| --- | --- |
| `libdial_std.a` 是**静态链接**进用户可执行文件的 | 它构成用户程序的**一部分**，用户的产物是它的衍生作品 |
| **GPL-3.0 具有传染性** | 若标准库按 GPL-3.0 授权，**任何**链接它的程序（含闭源商业软件）都必须整体以 GPL-3.0 开源 |
| 而用户只是想用一门语言写程序 | 一旦被传染，等于**强制公开自己的全部源码**——这事实上否决了 Dial 的一切闭源与商业用途 |

因此本项目的执行口径是**按组件划分**，而非全仓库一刀切：

| 组件 | 许可 | 理由 |
| --- | --- | --- |
| **编译器前端**（`src/`，`dialc` 本体） | `GPL-3.0 OR MIT OR Apache-2.0` 三重许可 | 它只是**工具**，不进入用户产物，无传染风险 |
| **标准库 / C Runtime**（`stdlib/`、`runtime/` → `libdial_std.a`） | **强制 `MIT` 或 `Apache-2.0`** | 它被**静态链入**用户程序，必须免传染 |
| 用户用 Dial 写出的程序 | **不受 Dial 许可约束** | 前提是上述隔离成立；用户的代码是用户自己的 |

> **一句话：工具可以是 GPL，被链进你程序里的运行时绝不能是。**
> 为标准库选择许可时，请在 MIT 与 Apache-2.0 之间选，不要选 GPL-3.0。

---

## 4. 词法结构

### 4.1 注释

`//` 到行尾。无块注释。

### 4.2 标识符

以字母或 `_` 开头，后接字母、数字或 `_`。

### 4.3 关键字

```
var  let  fn  struct  if  else  while  for  in  return
try  catch  throw
select  case  default  as
and  or   not
true false
int  float  bool  char  string  void  file  error  thread  channel
```

> `char` 是 v0.92 引入的单字符类型：**一个 Unicode 码点**，底层与 `int` 同为 `i64`，
> 但类型独立——不参与算术，只可比较或经 `char_to_int` / `int_to_char` 显式转换
> （见 [§5.1](#51-基本类型) 与 [§17.2](#172-尚未实现的能力截至-v092)）。

> `try` / `catch` / `throw` 是 v0.7 引入的可恢复异常关键字（见 [§26](#26-可恢复异常trycatchthrow)）。
> 注意 **`error` 不是关键字**——它是内建构造函数名（与 `open` / `print` 同类），写作 `Error("...")`。
> 同理 `catch` 后**不跟圆括号**：`catch e { }`，而非 `catch (e) { }`。
>
> `thread` / `channel` 在**词法上也不是关键字**，只是 `parse_type` 对标识符的特判——
> 否则 `channel(8)` 这个内建调用会被当成类型名而无法解析。并发内建（`spawn` / `join` /
> `detach` / `channel` / `recv` / `move_to`）全部按名字匹配，**不能用作函数名或结构体名**
>（见 [§27](#27-所有权转移与多线程并发move_to--thread--channel)）。

### 4.4 字面量

| 类别 | 写法 | 说明 |
| --- | --- | --- |
| 整数 | `123` | 十进制，对应 `int`（i64） |
| 浮点 | `3.14` | 必须含小数点，对应 `float`（f64） |
| 布尔 | `true` / `false` | 对应 `bool` |
| 字符串 | `"hello"` | 双引号；转义仅支持 `\n` `\t` `\\` `\"` |

### 4.5 运算符与标点

```
+  -  *  /  ==  !=  <  >  <=  >=  =  .  [  ]  ;  ,  (  )  {  }  :
```

> **注意**：单独的 `!` 是词法错误。逻辑非必须写关键字 `not`。

---

## 5. 类型系统

### 5.1 基本类型

| 类型 | 底层表示 | 说明 |
| --- | --- | --- |
| `int` | LLVM `i64` | 整数 |
| `float` | LLVM `double` | 双精度浮点 |
| `bool` | LLVM `i1` | 布尔 |
| `char` | LLVM `i64` | **单个 Unicode 码点**（v0.92）。**值类型**：无 ARC、无堆分配、无空指针。与 `int` 同表示但**类型独立**——不参与 `+ - * /`，只可比较，或经 `char_to_int` / `int_to_char` 显式转换。字面量写作 `'a'`、`'\n'`、`'中'` |
| `string` | LLVM `ptr`（不透明） | 由 C Runtime 管理的引用计数字符串 |
| `int[]` | LLVM `ptr`（不透明） | 由 C Runtime 管理的动态数组，**元素类型固定为 `int`** |
| `file` | LLVM `ptr`（不透明） | 文件句柄，封装 C 的 `FILE*`；显式 `open` / `close`，**不受 ARC 管理**（见 [§24](#24-文件-iofile-句柄)） |
| `error` | LLVM `ptr`（不透明） | **可恢复异常对象**（v0.7），由 C Runtime 管理的 ARC 类型；由 `Error("msg")` 构造、`throw` 抛出、`catch` 捕获（见 [§26](#26-可恢复异常trycatchthrow)） |
| `thread` | LLVM `ptr`（不透明） | 线程句柄（v0.8）；显式 `join` / `detach`，**不受 ARC 管理**（见 [§27](#27-所有权转移与多线程并发move_to--thread--channel)） |
| `channel<T>` | LLVM `ptr`（不透明） | 通道句柄（v0.8），元素类型仅编译期可见；跨线程共享 → **原子**引用计数 ARC。元素只能是 `string` / `int[]` / `map` / `error` / `channel`（见 [§27](#27-所有权转移与多线程并发move_to--thread--channel)） |
| `void` | LLVM `void` | **仅可用作函数返回类型** |
| 结构体 | LLVM 具名 struct | 值类型，栈分配 |

### 5.2 类型良构性约束

以下写法在编译期即被拒绝（而非运行期静默降级）：

| 写法 | 结果 |
| --- | --- |
| `float[]` | ❌ 错误。v0.2 数组元素只支持 `int` |
| `int[][]` | ❌ 错误。不支持嵌套数组 |
| `var v: void = ...` | ❌ 错误。`void` 不能作为变量类型 |

> 之所以在编译期拒绝 `float[]`：C 侧 `dial_array_*` 以 `int64` 存储元素，
> 若允许 `float[]`，元素会在跨越 FFI 边界时被 `fptosi` **静默截断**。
> 宁可拒绝，不可静默损坏数据。

### 5.3 类型转换

**零隐式转换铁律。**

Dial **不存在任何**隐式类型转换——既没有 `int → float` 的隐式提升，也没有 `float → int` 的隐式窄化，
更没有 `int`/`float` 在算术或比较中的隐式混用。两个操作数只要类型不完全相同，就一律编译错误。
这比 C 更严格：C 允许 `1 + 2.0` 把 `int` 默默提升为 `double`，正是大量隐蔽精度 bug 的温床，
Dial 在编译期即切断。

唯一的合法转换通道是**命名清晰的内置函数**，它们把"发生过转换"这件事明明白白写出来：

| 内置函数 | 签名 | 编译为 | 说明 |
| --- | --- | --- | --- |
| `int_to_float` | `int_to_float(x: int): float` | LLVM `sitofp` | 整数 → 双精度浮点 |
| `float_to_int` | `float_to_int(x: float): int` | LLVM `fptosi` | 浮点 → 整数（向零截断，与 C 的 `(int)` 一致） |
| `char_to_int` | `char_to_int(c: char): int` | **无指令**（恒等） | 字符 → 码点（v0.92） |
| `int_to_char` | `int_to_char(n: int): char` | **无指令**（恒等） | 码点 → 字符（v0.92）；非法码点在**打印时**才会被 C Runtime 拦截 |

示例：

```dial
let i: int = 10;
let f: float = int_to_float(i) + 2.5;   // 必须先显式转换，再参与浮点运算
let j: int = float_to_int(3.7);          // j == 3（截断）
```

> `int_to_float` / `float_to_int` 是纯编译器内联的 LLVM 指令，**不经过任何 C 运行时函数**，
> 因此不会引入额外依赖（见 §15）。
>
> `char_to_int` / `int_to_char` 更彻底：**连一条指令都不产生**——`char` 与 `int` 在 IR 层就是
> 同一个 `i64`，转换纯粹是编译期的类型视图切换（零成本）。它仍然不是「隐式转换」，
> 因为源码里必须写出函数名。

### 5.4 `char`：单字符类型（v0.92）

`char` 是**一个 Unicode 码点**——不是字节、不是 UTF-8 片段。它补齐了 Dial 在字符层面的空白，
是字符串操作的基础类型。

```dial
var c: char = 'a';
print(c);                  // a        （打印字符本身）
print(char_to_int(c));     // 97       （打印码点）
print(int_to_char(98));    // b
print('中');                // 中       （多字节 UTF-8，输出时按码点编码）
print('\n');               // 转义：\n \t \r \0 \\ \' \" 均支持
```

**字面量规则**（词法阶段即校验）：

| 写法 | 结果 |
| --- | --- |
| `'a'`、`'中'`、`'é'` | ✅ 单个字符（可为任意 Unicode 码点） |
| `'\n'`、`'\t'`、`'\0'`、`'\\'`、`'\''`、`'\"'` | ✅ 转义序列 |
| `''` | ❌ 词法错误：字符字面量不能为空 |
| `'ab'` | ❌ 词法错误：字符字面量只能包含一个字符 |
| `'a`（未闭合） | ❌ 词法错误：缺少结束的单引号 |

**能做什么 / 不能做什么**：

| 操作 | 是否允许 | 说明 |
| --- | --- | --- |
| 赋值、作参数、作返回值 | ✅ | 值语义，与 `int` 一致 |
| `==` `!=` `<` `>` `<=` `>=` | ✅ | 按**码点序**比较：`'a' < 'b'` 为真 |
| `char_to_int` / `int_to_char` | ✅ | 与 `int` 互转，零成本 |
| `+ - * /` 算术 | ❌ **编译期拒绝** | `'a' + 1` 语义可疑（下一个字母？还是数值计算？），一律拒绝 |
| 与 `int` 混用（`'a' == 97`） | ❌ **编译期拒绝** | 零隐式转换铁律；请写 `char_to_int('a') == 97` |
| 作为 `channel<T>` 的元素类型 | ❌ **编译期拒绝** | 通道元素只能是不透明句柄（§27.2） |
| 判空 `is_null(c)` | ❌ **编译期拒绝** | `char` 是值类型，永远非空，与 `int` / `bool` 同理 |

**为什么不让 char 参与算术**：`'a' + 1` 的意图无法判定——是「下一个字母」还是「拿码点做加法」？
Dial 的选择是让源码把意图写清楚：要码点就 `char_to_int('a') + 1`，要下一个字母就 `int_to_char(char_to_int('a') + 1)`。
**宁可多写一次转换，也不接受一个含混的运算符。**

**打印语义与 Fail-Fast**：`print(c)` 输出的是**字符**（按 UTF-8 编码为 1~4 字节），不是码点数字。
若码点非法（负数、> `0x10FFFF`、或落在 UTF-16 代理区 `D800~DFFF`），
C Runtime 会在打印时**当场打印诊断并终止**，绝不输出半个坏字符——
静默产出乱码比当场崩溃更危险，它会在下游变成无法复现的数据损坏（Fail-Fast 铁律 §20）。

---

## 6. 变量与可变性

### 6.1 声明

```dial
var x: int = 10;      // 可变
let y: int = 20;      // 不可变
```

规则：

1. **必须显式标注类型**，无类型推断。
2. **必须初始化**，不存在未初始化变量（这一点比 C 严格）。
3. `let` 变量**不可重新赋值**，也不可经字段/下标写入。违反即编译错误：

   ```
   语义错误：变量 'n' 由 let 声明，不可重新赋值
   ```

4. 同一作用域内不可重复声明。
5. `if` / `while` 体内构成内层作用域，允许声明与外层同名的变量（遮蔽外层）。

### 6.2 可变性判定

写入操作的合法性取决于**根变量**的可变性：

| 操作 | 要求 |
| --- | --- |
| `x = e` | `x` 必须是 `var` |
| `x.f = e` | `x` 必须是 `var` |
| `x[i] = e` | `x` 必须是 `var` |
| `x.push(v)` / `x[i] = v`（原地修改） | `x` 必须是顶层 `var`；`let` 变量、字段、下标均不可作接收者（见 §11.3） |

> **为什么 `let` 变量不能调用 `push` / `set`**：这两种操作会原地修改容器，且因 COW 可能**更换底层指针**；
> 编译器必须把返回的新指针写回接收者。接收者若不可重新赋值（`let`）或根本不是可写回的顶层变量
> （字段、下标），就无法可靠地完成这次"写回"，于是编译器在**语义阶段即拒绝**，而非把旧指针留在原地酿成悬垂。
> 若只是想获得一个新容器而不就地修改，请以表达式形式使用：`let b: int[] = a.push(v);`

---

## 7. 表达式与运算符

### 7.1 优先级（由低到高）

| 层级 | 运算符 | 结合性 |
| --- | --- | --- |
| 1 | `or` | 左 |
| 2 | `and` | 左 |
| 3 | `not`（前缀） | 右 |
| 4 | `==` `!=` | 左 |
| 5 | `<` `>` `<=` `>=` | 左 |
| 6 | `+` `-` | 左 |
| 7 | `*` `/` | 左 |
| 8 | `-`（一元负号） | 右 |
| 9 | `.field` `.method(args)` `[index]` | 左（可链式） |
| 10 | 字面量、变量、调用、数组/结构体字面量 | — |

因此 `not a == b` 解析为 `not (a == b)`。

### 7.2 类型规则

| 运算符 | 操作数要求 | 结果类型 |
| --- | --- | --- |
| `+` `-` `*` `/` | 两侧**必须同为 `int`**（→`int`）或同为 `float`（→`float`），**不可混合** | `int` 或 `float` |
| `==` `!=` | 两侧类型完全相同：同为数值（`int` 或 `float`，不可混合）、或同为 `bool`、或同为 `string` | `bool` |
| `<` `>` `<=` `>=` | 两侧类型完全相同且均为数值（`int` 或 `float`，不可混合） | `bool` |
| `and` `or` | 两侧均为 `bool`，**短路求值** | `bool` |
| `-`（一元） | `int` 或 `float`（与原操作数同类型） | 同操作数 |
| `not` | `bool` | `bool` |

> 所有二元运算都要求操作数**类型完全相同**。特别地，`1 + 2.0` 在 C 里合法（int 提升为 double），
> 在 Dial 里是**编译错误**——必须先写 `int_to_float(1) + 2.0`。这把"隐式混用"从运行期隐患提前到编译期拒绝。

违反以上任一条即编译错误。

### 7.3 语义细则（与 C 的重要差异）

| 行为 | Dial 的规定 | 对比 C |
| --- | --- | --- |
| 整数除法 `7 / 2` | 整数除法，结果 `3` | 同 C |
| 混合算术 `7 / 2.0` | ❌ **编译错误**：`int` 与 `float` 无隐式转换，必须先 `int_to_float(7) / 2.0` | C 隐式把 `int` 提升为 `double`，结果 `3.5` |
| `and` / `or` 求值 | **短路求值**（与 C 的 `&&` / `\|\|` 一致） | C 同样短路 |
| 字符串 `==` | 按**内容**比较（`dial_string_eq`） | C 的 `==` 比指针 |
| 数组 `==` | ❌ 不支持 | — |
| 结构体 `==` | ❌ 不支持 | — |
| 运算符重载 | ❌ 不存在。字符串拼接必须写 `concat()` | — |

> `and` / `or` 的短路求值是一项**强制语义**：先求左操作数，若已能决定结果（`and` 遇假、`or` 遇真）则不再求值右操作数。
> 这保证了诸如"判空后访问"这类依赖短路的代码不会因右操作数被盲目求值而崩溃——这是现代语言的常识。
> 若需要两侧副作用都必然发生，请显式写成两条独立语句。

---

## 8. 语句与控制流

### 8.1 语句一览

```dial
var x: int = 1;            // 变量声明
x = 2;                     // 赋值
p.x = 3;                   // 字段赋值
a[0] = 4;                  // 下标赋值
print(e);                  // 打印语句
f(1, 2);                   // 表达式语句
if (cond) { ... } else { ... }
if (cond) { ... } else if (c2) { ... } else { ... }   // else if 语法糖
for (i in 0..n) { ... }    // 范围 for 循环（end 独占）
while (cond) { ... }
break;                     // 跳出最内层循环（仅循环体内）
continue;                  // 进入下一次迭代（仅循环体内）
return e;                  // 或 return;（仅 void 函数）
```

### 8.2 `if` / `else if` / `while`

条件**必须带括号**，且类型必须是 `bool`：

```dial
if (x > 0) { ... } else { ... }

// else if 是语法糖：编译期展开为嵌套 if，无需新语法节点
if (x < 0) { ... }
else if (x == 0) { ... }
else if (x < 10) { ... }
else { ... }

while (i < n) { ... }
```

循环体内可用 `break` / `continue`（见 §8.4）与 `return` 提前退出。

### 8.3 范围 `for` 循环

`for (i in start..end)`：循环变量 `i` 为 `int`，从 `start` 迭代到 `end - 1`（`end` **独占**，半开区间）。
`start` / `end` 必须为 `int` 表达式，`i` 在循环体内为可变变量（可被重新赋值）。

```dial
var sum: int = 0;
for (i in 0..5) {        // i = 0,1,2,3,4
    sum = sum + i;       // 0+1+2+3+4 = 10
}
```

循环变量作用域限定在循环体内；`start`/`end` 在每次迭代开始前各求值一次（不支持 C 式「每次迭代重新求值边界」的语义，等价于把上下界快照到循环前）。

### 8.4 `break` / `continue`

`break` 跳出**最内层**循环；`continue` 进入下一次迭代。两者**只允许出现在 `while` / `for` 循环体内**，
写在别处（包括 `if` 但外层无循环）都是编译错误。

```dial
var sum: int = 0;
for (i in 0..6) {
    if (i == 3) {
        continue;      // 跳过 3
    }
    sum = sum + i;     // 0+1+2+4+5 = 12
}
```

**关键语义**：`for` 的 `continue` 会**先执行 `i = i + 1` 再回到条件判定**。
实现上自增落在独立的 `for.inc` 块，`continue` 跳到该块而非条件块——若跳到条件块就会跳过自增造成死循环。
`while` 的 `continue` 则直接回到条件判定，因此**自增语句必须写在 `continue` 之前**（否则同样死循环）：

```dial
var n: int = 0;
while (n < 10) {
    n = n + 1;         // 自增在前：continue 不会漏掉它
    if (n == 2) { continue; }
    ...
}
```

### 8.5 块与作用域

`{ ... }` 只能作为函数体或 `if`/`while`/`for` 的分支体，**不能**作为独立的裸块语句。

---

## 9. 函数

### 9.1 定义

```dial
fn add(a: int, b: int): int {
    return a + b;
}

fn log(msg: string): void {
    print(msg);
    return;
}
```

- 参数与返回类型均须显式标注。
- 返回类型可以是 `void`。

### 9.2 入口函数

程序**必须**定义 `main`，且返回类型必须是 `int`：

```dial
fn main(): int {
    return 0;
}
```

`main` 的返回值作为进程退出码（C ABI 下为 `i32`）。

### 9.3 参数传递与所有权

| 参数类型 | 传递方式 | 被调方所有权 |
| --- | --- | --- |
| `int` `float` `bool` | 按值 | 无 |
| `string` `int[]` | 不透明指针按值传递 | 入口 `retain`，出口 `release`（借用语义） |
| 结构体 | 按值拷贝（含其堆字段指针的复制） | 入口递归 `retain` 堆字段，出口递归 `release` |

### 9.4 返回值的确定性

非 `void` 函数若执行到函数体末尾仍无 `return`，返回该类型的**零值**：

| 类型 | 零值 |
| --- | --- |
| `int` | `0` |
| `float` | `0.0` |
| `bool` | `false` |
| `string` / `int[]` | 空指针 |
| 结构体 | 各字段取其零值 |

> 这比 C 严格：C 中"非 void 函数落到末尾"是**未定义行为**，Dial 定义为确定的零值。

---

## 10. 结构体

### 10.1 定义

只能在**顶层**定义，写在函数体内是语法错误：

```dial
struct Point {
    x: int,
    y: int,
}
```

字段类型可以是 `int` / `float` / `bool` / `string` / `int[]` / 另一个结构体。

### 10.2 字面量

```dial
let p: Point = Point { x: 3, y: 4 };
let q: Point = Point { y: 10, x: 20 };   // 顺序无关
```

规则：

1. **必须显式列出全部字段。** 不支持"省略即零初始化"——C 允许 `Point p = {.x=1}`，
   Dial 不允许，以此杜绝漏初始化字段。
2. **字段顺序与声明顺序无关**，字面量按**字段名**定位下标（不会因顺序不同而错位）。
3. 字段类型必须与该字段声明类型兼容。

### 10.3 访问与赋值

```dial
print(p.x);            // 读
p.x = 100;             // 写（p 必须是 var）
print(r.origin.x);     // 支持链式
```

### 10.4 语义

结构体是**值类型**：

- 在 LLVM 中以 `alloca` 落在栈上，字段访问编译为 `getelementptr`，零运行时开销。
- 赋值、传参、返回均为**值拷贝**。
- 若结构体含有 `string` / `int[]` 字段，值拷贝时这些字段按**共享引用**处理：
  编译期插入对称的 `retain`，作用域结束时插入对称的 `release`（详见 §14.4）。

---

## 11. 动态数组 `int[]`

`int[]` 是不透明指针，内存布局 `[refcount][len][cap][elems...]` 由 C 侧私有维护。

### 11.1 构造

```dial
var a: int[] = [10, 20, 30];
```

空数组字面量 `[]` 不支持（无法推断元素类型）。

### 11.2 方法

| 方法 | 等价于 C 调用 | 返回 | 说明 |
| --- | --- | --- | --- |
| `a.length()` | `dial_array_length` | `int` | 元素个数 |
| `a.get(i)` | `dial_array_get` | `int` | 按下标读 |
| `a.push(v)` | `dial_array_push` | `int[]` | 追加；**返回（可能更新后的）数组指针** |
| `a.set(i, v)` | `dial_array_set` | `int[]` | 按下标写；被共享时内部触发 COW，**返回新指针** |
| `a.clone()` | `dial_array_clone` | `int[]` | COW 感知克隆 |

下标语法 `a[i]` 等价于 `a.get(i)`；`a[i] = v` 等价于 `a = a.set(i, v)`。

### 11.3 为什么 `push` / `set` 要返回指针

这两种操作在需要扩容或存在多个持有者时，会产生**新的**不透明指针。
编译器强制把返回值写回接收变量，杜绝"旧指针失效"的悬垂问题：

```dial
var a: int[] = [10, 20, 30];
a.push(40);     // 编译器自动写回 a，无需（也不允许）手写 a = a.push(40)
a[0] = 99;      // 同理
```

> 此处**不**对旧指针做 `release`：`dial_array_push` / `dial_array_set` 已经在
> C 侧完成了所有权交接（独占时返回同一指针、共享时克隆并释放本调用者的引用）。
> 若编译器再补一次 `release`，会造成悬垂指针或双重释放。

### 11.4 接收者必须是可变变量（`var`）

`push` / `set` 会修改容器并可能更换指针，因此**接收表达式必须是顶层可变变量**：

| 调用形式 | 是否允许 |
| --- | --- |
| `var a: int[] = ...; a.push(v);` | ✅ 允许（编译器把新指针写回 `a`） |
| `let a: int[] = ...; a.push(v);` | ❌ 编译错误（`let` 不可重新赋值，见 §6.2） |
| `rec.field.push(v);` | ❌ 编译错误（字段不可被可靠写回） |
| `arr[i].push(v);` | ❌ 编译错误（下标不可被可靠写回） |
| `let b: int[] = a.push(v);` | ✅ 允许（以表达式形式获取新容器，不就地修改原变量） |

> 这一约束把"容器被原地修改但接收者无法接收新指针"这一类悬垂 Bug **提前到编译期拒绝**，
> 而不是留到运行期靠运气。

### 11.5 越界行为

默认等级（Debug / ReleaseSafe）下**有边界检查**：越界读写当场触发陷阱终止，
由 codegen 插入的检查（§20.2）与 C Runtime 的检查（§20.3）双重保证。
仅在 `--fast`（ReleaseFast）下才回落到"无边界检查、越界即 UB"的 C 行为。

---

## 12. 哈希表 `map`（int → int）

`map` 是 Dial 的哈希表类型，键与值均固定为 `int`（v0.4）。在 IR 中它同样只是一个 `ptr`（`i8*`），
其内存布局 `[int64 cap][int64 count][entries...]` 由 C 侧私有维护，并走与 `string` / `int[]`
**同一套 ARC 引用计数**（`dial_map_retain` / `dial_map_release`）。这完全契合 [§19 内存宪法](#19-内存宪法防-gc-铁律)：
确定性、零 GC 停顿、所有权可控。

每条 entry 24 字节：`{ int64 key; int64 val; int64 used; }`，采用线性探测（linear probing）开放寻址，
删除采用「左移紧凑」维护簇连续性。负载因子超过 3/4 时自动扩容并重新哈希。

### 12.1 构造

```dial
var m: map = new_map();   // 创建空表，返回 map 类型（ARC 引用计数 1）
```

`new_map()` 无参数，在 C 侧调用 `dial_map_new`，分配一个 `cap = 8` 的空表。

### 12.2 方法

| 方法 | 签名 | 返回 | 说明 |
| --- | --- | --- | --- |
| `put` | `m.put(k: int, v: int)` | `map`（可能更新的指针，**由编译器自动写回**） | 写入键值对；命中覆盖值，未命中追加 |
| `get` | `m.get(k: int)` | `int` | 返回值；**未命中返回 -1** |
| `contains` | `m.contains(k: int)` | `bool` | 命中返回 `true` |
| `remove` | `m.remove(k: int)` | `int` | 移除并返回其值；未命中返回 -1 |
| `size` | `m.size()` | `int` | 当前条目数 |

```dial
var m: map = new_map();
m.put(1, 100);
m.put(2, 200);
print(m.get(2));        // 200
print(m.contains(2));   // true
print(m.size());        // 2
m.remove(2);
print(m.get(2));        // -1（已删除）
```

### 12.3 为什么 `put` 要返回指针

与 `int[].push` / `int[].set` **完全一致**（见 [§11.3](#113-为什么-push-set-要返回指针)）：`put` 在底层可能因
**扩容**或**写时复制（COW）**更换底层 buffer 指针。

**统一规则（强制）**：

> **所有原地修改并触发 COW 的方法（`push` / `put`），编译器自动写回接收变量，
> 严禁用户手写赋值。**

```dial
var m: map = new_map();
m.put(1, 1);       // ✅ 正确：编译器自动把新指针写回 m

m = m.put(1, 1);   // ❌ 禁止：严禁手写赋值（与数组规则统一，见下）
```

为什么废弃 `m = m.put(...)` 这种写法：

| 理由 | 说明 |
| --- | --- |
| **与数组规则不统一** | 数组写 `a.push(40)`，map 却要写 `m = m.put(...)`——这是纯粹的不一致，没有任何技术理由支撑 |
| **诱导错误的所有权直觉** | 手写赋值看起来像「由用户把返回值赋回去」，实则**无论写不写，编译器都会写回**；两种写法并存只会让人误以为二者有区别 |
| **ARC 归属容易被误判** | `dial_map_put` 已在 C 侧完成所有权交接（独占时返回同一指针、共享时克隆并释放本调用者的引用）。让用户侧再参与赋值，只会模糊 retain/release 的归属 |

> 与数组一致：此处**不**对旧指针做 `release`，C 侧已完成所有权交接；
> 若编译器再补一次 `release`，会造成悬垂指针或双重释放（同 §11.3 的说明）。

若 `m` 当前被多者共享（例如已作为实参传入某函数），`dial_map_put` 会先 COW 克隆再修改，
**调用方持有的旧指针不受影响**（ARC 隔离，见 [§13.5](#145-cow-的触发时机)）。

### 12.4 接收者必须是可变变量（`var`）

与数组一致：`put` 可能更换指针，因此接收者必须是顶层 `var` 变量。`let` 变量、字段、下标均不可作接收者：

```dial
let m: map = new_map();
m.put(1, 1);   // ❌ 编译错误：let 不可调用原地修改方法
```

### 12.5 ARC 与确定性内存

`map` 与 `string` / `int[]` 共享同一份引用计数实现（`runtime/dial_rt_mem.h` 的 `dial_rc_*`）。
作用域结束时编译器插入对称 `release`，归零即释放，**绝不引入追踪式 GC**。

---

## 13. 字符串 `string`

`string` 是不透明指针，内存布局 `[refcount][len][data...]` 由 C 侧私有维护。

### 13.1 方法

| 方法 | 等价于 C 调用 | 返回 | 说明 |
| --- | --- | --- | --- |
| `s.length()` | `dial_string_length` | `int` | 字节长度 |
| `s.concat(t)` | `dial_string_concat` | `string` | 拼接，产生新对象 |
| `s.get(i)` | `dial_string_get` | `int` | 取第 `i` 个字符的**整数码**；想要 `char` 请显式转换 `int_to_char(s.get(i))`（v0.92 起） |
| `s.substring(from, to)` | `dial_string_substring` | `string` | 半开区间 `[from, to)` |
| `s.clone()` | `dial_string_clone` | `string` | COW 感知克隆 |
| `s.eq(t)` | `dial_string_eq` | `bool` | 按内容比较 |

`==` / `!=` 对 `string` 按**内容**比较（编译为 `dial_string_eq`），而非比较指针地址。

### 13.2 不可变语义

v0.2 未提供任何原地修改字符串的接口：所有操作（`concat`、`substring`）都产生新对象。
字符串因此天生满足"只读推断"，跨函数共享永远是 O(1)。

---

## 14. 内存与所有权模型

这是 Dial 相对 C 的核心增值点：**尽可能在编译期把内存管理开销消掉**。

### 14.1 不透明指针契约

Dial 代码中的 `string` / `int[]` 只是 `i8*`：

```llvm
; s.length() 的唯一形态
%len = call i64 @dial_string_length(ptr %s)
```

用户代码**永远无法**：

- 计算其偏移量、做指针算术；
- 读取 refcount 或容量字段；
- 直接 `free` 它。

一切都必须经由 C 导出的 `dial_*` 函数。

### 14.2 引用计数对象的布局

```
[ int64 refcount ][ payload ... ]
                    ^
                    └─ Dial 看到的"不透明指针"指向这里
```

`dial_rc_alloc` 返回 payload 指针；`dial_rc_retain/release` 操作其前 8 字节。

### 14.3 编译期三种绑定策略

Sema 阶段对每条 `let/var x = y;` 做数据流分析，判定下列三种策略之一：

| 策略 | 触发条件 | 生成的代码 | 开销 |
| --- | --- | --- | --- |
| **移动语义（Move）** | `y` 在此之后再未被引用，且不在循环/分支内 | 只复制指针，**不生成任何 retain/release** | 零 |
| **只读共享（Share）** | `y` 之后仍被读取但从未被写入 | 一次 `retain`，双方共享同一指针 | O(1) |
| **COW 兜底（Copy）** | 静态分析无法证明独占（如穿过函数边界） | 交给 C Runtime，在**写入瞬间**按 refcount 决定是否深拷贝 | 仅在此刻可能 O(n) |

前两种在编译期即消除了运行时开销；只有第三种可能真的复制内存。

判定"最后一次引用"时，位于循环或分支内的引用被视为**不安全上下文**，
不参与移动判定（因为该语句可能被执行零次或多次）。

### 14.4 栈分配与逃逸分析

- **结构体**：值类型，一律在栈上 `alloca`，字段访问是 `getelementptr`。
  这是确定的，不依赖逃逸分析结果。
- **所有局部变量的栈槽**：统一分配在函数的 **entry 块**上。
  这不只是优化，而是**正确性要求**：局部变量声明可能出现在循环体内，
  若在当时所在块直接 `alloca`，每轮迭代都会多压一个栈槽，
  迭代次数一多就会耗尽栈空间（段错误）。提到 entry 块后每个变量只占一个
  固定栈槽、跨迭代复用，**栈用量与循环次数无关**。
- **结构体中的堆字段**：结构体值被复制时，其 `string` / `int[]` 字段按共享引用处理，
  编译期插入**对称**的 `retain`；作用域结束时插入对称的 `release`。
  `retain` 与 `release` 的数量严格相等——不多（泄漏）、不少（双重释放）。

  验证方式：把循环次数从 1 万提到 100 万，峰值 RSS 基本不变即为平衡
  （见 `examples/ownership_stress.dial`）。

### 14.5 COW 的触发时机

```
                     ┌─ 编译期能证明独占 ──→ 移动语义：零开销
let/var x = y;  ─────┤
                     ├─ 编译期能证明只读 ──→ 共享：一次 retain，O(1)
                     └─ 静态分析失效 ─────→ 交给 C Runtime
                                              │
                                    写入瞬间检查 refcount
                                              │
                            ┌─────────────────┴─────────────────┐
                      refcount == 1                        refcount > 1
                      原地写入，零拷贝                    先克隆再写（真实复制）
```

C 侧 `dial_rc_clone` 是这一兜底的落点：唯一持有时直接返回自身，否则深拷贝。

### 14.6 自赋值安全

`x = x` 必须安全。编译器采用"**先取得新值所有权，再释放旧值**"的顺序：

```dial
x = x;   // 先 retain（计数 1→2），再 release（2→1），绝不会先 free 再 use
```

若顺序颠倒，计数会先归零被 `free`，随后的 `retain` 即 use-after-free。

### 14.7 返回值不被提前释放

`return` 的处理顺序是**强制**的：

1. 求值返回表达式；
2. 若返回的是局部变量，标记"已移动"（所有权随返回值转移）；
3. 释放其余**未被转移**的局部变量；
4. 生成 `ret`。

若在求值之前就释放全部局部变量，返回的对象会被提前销毁。

---

## 15. FFI 契约：Dial 语法 → C 函数

编译器前端把 Dial 语法**逐条**翻译为对 C 导出函数的调用，不重新实现任何容器：

| Dial 写法 | 生成的 LLVM IR |
| --- | --- |
| `"abc"` | `call ptr @dial_string_new(ptr @str)` |
| `s.length()` | `call i64 @dial_string_length(ptr %s)` |
| `s.concat(t)` | `call ptr @dial_string_concat(ptr %s, ptr %t)` |
| `s.get(i)` | `call i64 @dial_string_get(ptr %s, i64 %i)` |
| `s.substring(a, b)` | `call ptr @dial_string_substring(ptr %s, i64 %a, i64 %b)` |
| `s.clone()` | `call ptr @dial_string_clone(ptr %s)` |
| `s == t` | `call i64 @dial_string_eq(ptr %s, ptr %t)` 后与 0 比较 |
| `[1, 2, 3]` | `call ptr @dial_array_new()` + 三次 `dial_array_push` |
| `a.length()` | `call i64 @dial_array_length(ptr %a)` |
| `a.get(i)` / `a[i]` | `call i64 @dial_array_get(ptr %a, i64 %i)` |
| `a.push(v)` | `call ptr @dial_array_push(ptr %a, i64 %v)` 并写回 `%a` |
| `a[i] = v` | `%a = call ptr @dial_array_set(ptr %a, i64 %i, i64 %v)` |
| `a.clone()` | `call ptr @dial_array_clone(ptr %a)` |
| `new_map()` | `call ptr @dial_map_new()` |
| `m.put(k, v)` | `%m = call ptr @dial_map_put(ptr %m, i64 %k, i64 %v)` 并写回 `%m` |
| `m.get(k)` | `call i64 @dial_map_get(ptr %m, i64 %k)` |
| `m.contains(k)` | `call i64 @dial_map_contains(ptr %m, i64 %k)` 后与 0 比较 |
| `m.remove(k)` | `call i64 @dial_map_remove(ptr %m, i64 %k)` |
| `m.size()` | `call i64 @dial_map_size(ptr %m)` |
| `read_line()` | `call ptr @dial_read_line()` |
| `open(p, m)` | `call ptr @dial_file_open(ptr %p, ptr %m)`（失败返回 null） |
| `read(f, n)` | `call ptr @dial_file_read(ptr %f, i64 %n)`（读最多 n 字节，失败返回空串） |
| `write(f, c)` | `call i64 @dial_file_write(ptr %f, ptr %c)`（返回写入字节数，失败 -1） |
| `close(f)` | `call void @dial_file_close(ptr %f)`（空指针安全） |
| `print(x)` | `dial_print_int` / `dial_print_float` / `dial_print_bool` / `dial_print_string` |
| 变量绑定（共享） | `dial_string_retain` / `dial_array_retain` / `dial_map_retain` |
| 作用域结束 | `dial_string_release` / `dial_array_release` / `dial_map_release` |

### C Runtime 导出函数

**`runtime/dial_rt_mem.h`**（引用计数原语）

```c
void *dial_rc_alloc(int64_t payload_size);          // refcount 初始为 1
void  dial_rc_retain(void *p);                      // +1
void  dial_rc_release(void *p);                     // -1，归零则释放
void *dial_rc_clone(void *p, int64_t payload_size); // 唯一持有时返回自身（COW 命中）
```

**`stdlib/dial_string.h`**

```c
DialString dial_string_new(const char *lit);
int64_t    dial_string_length(DialString s);
DialString dial_string_concat(DialString a, DialString b);
int64_t    dial_string_get(DialString s, int64_t i);          // 返回字符的整数码
int64_t    dial_string_eq(DialString a, DialString b);        // 相等返回 1
DialString dial_string_substring(DialString s, int64_t from, int64_t to);
DialString dial_string_clone(DialString s);
void       dial_string_retain(DialString s);
void       dial_string_release(DialString s);
void       dial_print_string(DialString s);
```

**`stdlib/dial_array.h`**

```c
DialArray dial_array_new(void);
DialArray dial_array_push(DialArray a, int64_t v);            // 返回（可能更新的）指针
int64_t   dial_array_get(DialArray a, int64_t i);
DialArray dial_array_set(DialArray a, int64_t i, int64_t v);  // 共享时内部 COW
int64_t   dial_array_length(DialArray a);
DialArray dial_array_clone(DialArray a);
void      dial_array_retain(DialArray a);
void      dial_array_release(DialArray a);
```

**`stdlib/dial_io.h`**

```c
void       dial_print_int(int64_t v);
void       dial_print_float(double v);
void       dial_print_bool(int64_t b);      // 0/1 → false/true
void       dial_print_string(DialString s);
DialString dial_read_line(void);                  // 已暴露为 read_line()

// 文件 IO（v0.61）：文件句柄是不透明的 FILE*（经 void* 传递），显式 open/close，不受 ARC 管理。
typedef void *DialFile;
DialFile  dial_file_open(DialString path, DialString mode);   // 已暴露为 open(p, m)，失败返回 NULL
DialString dial_file_read(DialFile f, int64_t n);            // 已暴露为 read(f, n)，读最多 n 字节
int64_t   dial_file_write(DialFile f, DialString content);   // 已暴露为 write(f, c)，返回写入字节数
void      dial_file_close(DialFile f);                       // 已暴露为 close(f)，空指针安全
```

**`stdlib/dial_map.h`**（与 `string` / `int[]` 共用 ARC；已暴露为 `map` 类型）

```c
DialMap  dial_map_new(void);                           // 已暴露为 new_map()
DialMap  dial_map_put(DialMap m, int64_t key, int64_t val); // 返回（可能更新的）指针；已暴露为 m.put(k,v)
int64_t  dial_map_get(DialMap m, int64_t key);         // 未命中返回 -1；已暴露为 m.get(k)
int64_t  dial_map_contains(DialMap m, int64_t key);    // 命中返回 1；已暴露为 m.contains(k)
int64_t  dial_map_remove(DialMap m, int64_t key);      // 移除并返回其值，未命中返回 -1；已暴露为 m.remove(k)
int64_t  dial_map_size(DialMap m);                    // 已暴露为 m.size()
void     dial_map_retain(DialMap m);                  // 与 string/array 对称
void     dial_map_release(DialMap m);
```

---

## 16. 内置 `print`

```dial
print(e);
```

- `print` 是**语句**，返回 `void`，因此**不能**出现在赋值右侧、返回值或任何需要值的位置：

  ```dial
  let n: int = print("hi");   // ❌ 编译错误：void 与 int 不兼容
  ```

- 恰好接受 1 个参数。
- 支持的参数类型：`int`、`float`、`bool`、`string`。
- **不支持** `int[]` 与结构体——必须逐元素 / 逐字段打印。
  若放任会把二进制 payload 当字符串吐出乱码，故在编译期直接拒绝。
- 输出格式：

  | 类型 | 格式 |
  | --- | --- |
  | `int` | 十进制整数 + 换行 |
  | `float` | `%f`（6 位小数）+ 换行 |
  | `bool` | `true` / `false` + 换行 |
  | `string` | 内容 + 换行 |

---

## 17. 未定义行为与已实现边界

诚实列出边界，避免"文档写了、实现没有"的冲突。

### 17.1 未定义行为

默认（Debug / ReleaseSafe）等级下，下列曾经的 UB **已被 Fail-Fast 消除**——它们不再是 UB，
而是"确定性崩溃"：

| 场景 | Debug / ReleaseSafe 行为 | ReleaseFast 行为 |
| --- | --- | --- |
| 数组下标越界（读/写） | **当场终止**（`int3` / `ud2`），见 §20 | 未定义（回落到 C 的行为） |
| 字符串下标越界 | **当场终止**（C Runtime 侧检查），见 §20.3 | 未定义 |
| 负下标 | **当场终止**（无符号比较一次判定） | 未定义 |
| 整数除零 | **当场终止**（LLVM `sdiv` 的 poison 是 UB） | 未定义（实测打印错误结果后继续跑） |
| 内存分配失败（OOM） | **当场终止**，绝不返回 NULL | 同左（这条检查不关闭） |
| 重复释放 / use-after-free | **当场终止**（引用计数下溢检测） | 未定义 |

仍是 UB、与 C 一致的：

| 场景 | 行为 |
| --- | --- |
| 整数溢出 | 按 i64 二进制补码回绕（Debug 等级亦不检查） |

### 17.2 尚未实现的能力（截至 v0.94）

> 已落地：`Map`（int → int，ARC 管理）、`read_line()`、短路求值、零隐式转换铁律、
> `let` + 原地修改方法（push/set/put）的接收者约束、`else if` 语法糖、`for` 范围循环、`break` / `continue`、
> Fail-Fast 运行时检查（§20）、文件 IO（§24）、可恢复异常（§26）、
> 所有权转移与多线程并发（§27，含 v0.81 的通道关闭与 EOF、v0.9 的判空与多消费者安全）、
> **`char` 类型（v0.92，见 §5.1）**、**`import` 与 `.dialpkg` 包分发（v0.93，见 §25）**、
> **`dialc lsp` 语言服务器骨架（v0.94，见 §28）**。
> 其余仍按计划未实现：

| 能力 | 状态 |
| --- | --- |
| 多维数组 `int[][]` | 编译期拒绝 |
| 非 `int` 元素数组 | 编译期拒绝 |
| 裸块 `{ ... }` | 不支持 |
| 模块 / 包 / import | **已支持**（v0.92）：`import "包名";` + `dialc install <包.dialpkg>`，见 [§25](#25-包分发与依赖管理生态规划--最终规范)。**尚未实现**：`dialc install <URL>`（本地文件安装已可用）、源码形式的模块拆分（无 `mod`） |
| 泛型、接口、trait、闭包 | 不支持（`channel<T>` 是内建特化语法，不是用户泛型） |
| 指针、地址取值、指针算术 | **永不提供**（与不透明指针契约冲突） |
| 运算符重载 | **永不提供** |
| 全局变量 | 不支持 |
| 数组/结构体字面量作为常量表达式 | 不支持（需先赋给变量） |
| `select` 的超时 / 多路复用超时 | **未实现**——`select` 本身已落地（v0.91），但它**不带超时**；要「等一会儿再看」请用 `recv_timeout` 单独接收 |
| 通道关闭的「反向」操作（重新打开） | **永不提供**——关闭不可逆，这是 `done` 单调性的前提 |
| 线程池、协程、异步 | 未实现 |
| 原子类型、无锁数据结构 | 未实现 |
| `move_to` 到**已启动的**线程 | 编译期不支持——移交只能在 `spawn` 时完成（见 [§27.3](#273-零竞争移交与-o1-论证)） |
| 通道元素为标量（`int` / `float` / `bool`） | 编译期拒绝（需装箱，见 [§27.2](#272-channel-与所有权移交)） |
| 一等函数 / 函数指针 | 不支持——`spawn` 首参只能是顶层具名函数 |
| LSP 的补全 / 跳转定义 / 悬停 / 重命名 / 格式化 | **未实现**（v0.94 只有诊断与语义高亮，见 [§28.4](#284-刻意没做的部分)）——这些都要跨文件符号表，不是「再写几个请求」的事 |

---

## 18. 测试

```bash
./tests/run_tests.sh
```

十组检查，全部通过才算绿：

| 组 | 内容 |
| --- | --- |
| **A. 正向** | `examples/*.dial` 必须编译成功，且运行输出与同名 `.expected` 黄金文件**逐字节一致** |
| **B. 负向** | `tests/bad/*.dial` 必须编译失败，用于锁定诊断信息不被弱化 |
| **D. Fail-Fast** | `tests/trap/*.dial` 必须**当场因信号终止**（SIGILL/SIGTRAP/SIGSEGV），且**不得打印哨兵 9999**——证明它在越界那一行就停了，而不是带病跑完 |
| **E. 对照组** | 同一批用例在 `--fast` 下应当关闭检查并跑完（证明陷阱确由编译器插入，而非碰巧崩溃） |
| **C. 契约** | 产物可执行文件不得依赖 libLLVM |
| **F. 并发压测**（v0.9 新增） | 绕开编译器直压 `stdlib/dial_channel.c`：8 个消费者抢同一个通道、100 万元素、队列深 64。校验「每个元素恰好被取走一次」（位图 + 原子 `fetch_or`，重复领取即失败）与「总数守恒」。若发生丢唤醒，消费者 `join` 会永远挂住，由 timeout 判失败 |
| **G. 未捕获异常**（v0.92 新增） | 内建函数的隐式抛出允许写在 `try` 外：必须编译通过，运行期 panic 终止并打印**真实消息**（锁定的是「消息没被当成整数打印」这个曾经的缺陷）。不参与 E 组对照——异常不是安全等级检查，`--fast` 不该关掉它 |
| **H. 包分发**（v0.93 新增） | 构建示例 `.dialpkg` → 安装 → `import` 后编译运行比对黄金输出 → **篡改一个字节后必须被 SHA256 拦下**，绝不允许流到链接器 |
| **I. LSP**（v0.94 新增） | 真起一个 `dialc lsp` 子进程、按 JSON-RPC 往返：初始化能力、诊断定位到**出错那一行**、语义 token 的差分解码与**源码子串反查**（含中文的 UTF-16 换算）、`shutdown` 后干净退出。见 `tests/lsp_smoke.py` |
| **J. 包安装三条硬规则**（v0.94 收尾） | `dialc install`：① 同名包 **ABI 相同则覆盖、不同则报错要求先卸载**；② `targets` 的库文件**只允许 `.a` / `.lib` 静态库，拒绝 `.so` / `.dll` 动态库**；③ 卸载即 `rm -rf ~/.dial/packages/<名>/`（缓存是纯派生数据）。三条均被 J 组锁死 |

### 示例清单

| 示例 | 覆盖点 |
| --- | --- |
| `hello.dial` | 最小程序 |
| `flow.dial` | 变量、`while`、`if/else`、函数调用 |
| `struct_demo.dial` | 结构体：定义、字面量、字段读写、嵌套、作为参数与返回值 |
| `string_demo.dial` | 字符串全部方法、`==`/`!=` 内容比较 |
| `array_demo.dial` | 数组字面量、`push`/`get`/`set`、下标读写、作为函数参数 |
| `cow_demo.dial` | 移动语义、只读共享、COW 触发与隔离验证 |
| `record_demo.dial` | 结构体持有 `string` 与 `int[]` 的组合场景 |
| `ownership_stress.dial` | 所有权压力测试：栈槽复用 + 引用计数平衡 |
| `short_circuit.dial` | `and` / `or` 短路求值（以"副作用是否被触发"证明） |
| `conversions.dial` | `int_to_float` / `float_to_int` 显式转换（零隐式转换铁律） |
| `thread_spawn.dial` | 线程创建、`spawn` 移交 string、`join` |
| `channel_move.dial` | `move_to` 把所有权移交进通道，`recv` 取回 |
| `channel_thread.dial` | 子线程 `move_to` → 主线程 `recv` → `join` |
| `channel_array.dial` | `channel<int[]>` 传数组 |
| `thread_scalar.dial` | 标量实参按值拷贝（源变量不 Moved，之后仍可用） |
| `channel_close.dial` | 生产者 `close` → 消费者 `done` 收尾（EOF 全流程） |
| `channel_try_recv.dial` | `len` / `try_recv` / `closed` / `done` 状态机 |
| `is_null_demo.dial` | `is_null` 判别 EOF / 超时 / 真实值三种情形 |
| `recv_timeout.dial` | `recv_timeout` 超时返回空值；超时不会吞掉元素 |
| `channel_multiconsumer.dial` | **两个消费者抢同一个通道**，元素总数守恒（多消费者安全） |
| `select_two_channels.dial` | **双通道 select**：两个生产者往不同通道发数据，一个消费者同时监听；顺带校验没有串通道 |
| `select_default.dial` | `default` 分支（非阻塞语义）：队列空时每一轮都走 default |
| `select_eof.dial` | **全体 EOF 时 select 直接结束**，不执行任何分支 |
| `select_fairness.dial` | 多通道同时就绪时的**随机选取**（防饿死）：两个通道都必须被处理过 |

### 负向用例清单（节选，全部位于 `tests/bad/`）

| 用例 | 锁定行为 |
| --- | --- |
| `let_push.dial` | `let` 变量调用 `push` 必须在编译期拒绝 |
| `implicit_conv.dial` | `let y: float = x`（x 为 int）隐式转换必须在编译期拒绝 |
| `mixed_arith.dial` | `1 + 2.0` 这类 int/float 混合算术必须在编译期拒绝 |

---

## 19. 内存宪法（防 GC 铁律）

> 这是 Dial 区别于 Java / Go / C# 的**核心底线**，也是本语言不可动摇的内存契约：
> **绝不背追踪式垃圾回收的历史包袱。** 以下四条在任何版本中都不得违反。

### 19.1 永远拒绝 GC

Dial 的设计**绝不引入任何形式的追踪式垃圾回收（Tracing GC）**——没有 mark-sweep、没有 STW（Stop-The-World）停顿、没有分代回收。
内存回收的时机与开销在 Dial 程序里必须是**确定性的、可推理的**，而不是由运行时在某次不可预期的 GC 周期里"替你决定"。

这是 Dial 与 Java / Go / C# 最根本的分道扬镳：后者把内存管理的不可预测性藏进运行时，Dial 把它摊在编译期与显式契约上。

### 19.2 编译期接管内存

Dial 依托**不透明指针 + C Runtime 的 ARC（自动引用计数）**，结合编译器前端的**逃逸分析与移动语义**，实现"零 GC 停顿的确定性内存管理"：

- 值类型（如结构体）在栈上分配，作用域结束即回收，**零运行时参与**；
- 引用类型（`string` / `int[]` / `Map`）由 ARC 管理；编译器在编译期通过数据流分析，尽量把 `retain` / `release` 静态消除
  （移动语义零开销、只读共享 O(1)），只有跨边界等静态分析失效处，才由 C Runtime 在**写入瞬间**按引用计数兜底（COW，见 §14）；
- **没有任何后台线程、没有并发回收、没有不可预期的暂停。**

### 19.3 显式指针回收（未来裸指针的契约）

未来若引入裸指针（`raw ptr`），Dial 坚持**"谁申请、谁释放"**的显式契约：

- 裸指针**绝对不受 ARC 保护**，不会被任何引用计数自动回收；
- 它的生命周期完全交由开发者显式管理，例如调用底层 `dial_raw_free` 归还；
- 这一设计与 §19.2 的 ARC 容器**互补**：日常用 ARC 容器获得确定性安全，需要极致性能或底层控制时再用裸指针并自行负责回收；
- 裸指针与 ARC 容器之间**不允许**隐式转换——这正是"不透明指针契约"的延伸，避免把受管指针与不受管指针混为一谈而引入悬挂。

### 19.4 为什么这是底线

| 语言 | 内存回收方式 | Dial 的立场 |
| --- | --- | --- |
| Java / Go / C# | 追踪式 GC（存在 STW 停顿） | ❌ **永不引入** |
| C / C++ | 手动 `free` / RAII | 借鉴"确定性"，但用 ARC + 编译期分析消除大部分手写负担 |
| **Dial** | ARC + 编译期移动/逃逸分析 +（未来）显式裸指针 | ✅ 确定性、零 GC 停顿、可控 |

> 这条底线写进语言本身，而非仅仅写在文档里：任何试图为 Dial 添加追踪式 GC 的提案，都在原则上被否决。

---

## 20. Fail-Fast 铁律（宁可崩溃，绝不静默损坏）

> **宁可当场段错误（Segfault / Illegal Instruction），也绝对不允许产生未定义行为（UB）或静默产生错误数据。**

这是借鉴 Zig 的错误处理哲学，与 [§19 内存宪法](#19-内存宪法防-gc-铁律) 并列为 Dial 的两条工程底线：
内存宪法管"内存怎么回收"，Fail-Fast 管"出错时怎么办"。

### 20.1 拒绝 C 语言的静默损坏

C 的越界访问不会崩溃，而是**悄悄修改相邻内存然后继续跑**，Bug 往往在几万行之后
才以完全无关的形态爆发，排查成本极高。Dial 拒绝这种模式：

| 非法状态 | C 的行为 | Dial 的行为 |
| --- | --- | --- |
| 数组越界读 | 返回相邻内存的垃圾值，程序继续 | **当场终止**，不返回任何值 |
| 数组越界写 | 悄悄改坏别人的数据，程序继续 | **当场终止** |
| 负下标 | 读写对象头 / 前一块内存 | **当场终止** |
| 字符串越界 | 读到 `'\0'` 之后的字节或越界 | **当场终止** |
| 整数除零 | x86 上 SIGFPE；但 LLVM 层是 **poison（UB）** | **当场终止**（见 20.3） |
| 内存分配失败 | 返回 NULL，上层带病运行 | **当场终止**，绝不返回 NULL |
| 重复释放 / use-after-free | 静默损坏堆管理结构 | **当场终止**（引用计数下溢检测） |

### 20.2 LLVM Codegen 的硬性插入

在生成 LLVM IR 时，编译器**主动插入**运行时检查；失败分支用 LLVM 内建函数发射陷阱指令：

| 安全等级 | 陷阱 intrinsic | x86 机器码 | 进程信号 |
| --- | --- | --- | --- |
| Debug（默认） | `llvm.debugtrap` | `int3` (0xCC) | SIGTRAP（5），可被调试器当断点捕获 |
| ReleaseSafe | `llvm.trap` | `ud2` (0F 0B) | SIGILL（4） |
| ReleaseFast | —— | —— | 无（检查被完全消除） |

非法指令由操作系统立刻接管并 Core Dump，调试器能**精确停在越界的那一条指令上**。

真实反汇编（`a[99]`，长度 3，Debug 等级）：

```asm
1001f76: call dial_array_length      # 取长度
1001f7b: cmp  $0x63,%rax             # 与下标 99 比较（0x63 = 99）
1001f7f: jbe  1001fb2                # ★ 越界则跳转
1001f81: mov  $0x63,%esi
1001f89: call dial_array_get         # 合法才真正访问
...
1001fb2: int3                        # ★★ llvm.debugtrap：当场终止
```

### 20.3 边界检查与断言（C Runtime 侧）

底层 C Runtime 的所有下标访问**必须**先做边界检查，越界直接触发底层 trap。
这是第二道防线——即便 codegen 检查被关闭，Runtime 仍不放过越界：

```asm
# dial_string_get（C Runtime，DIAL_SAFETY >= 1）
1001e3a: mov  (%rdi),%rsi            # len = s[0]
1001e3d: test %rbx,%rbx              # idx < 0 ?
1001e40: js   1001e47                #   是 -> fail
1001e42: cmp  %rbx,%rsi              # len > idx ?
1001e45: jg   1001e4f                #   是 -> 合法路径
1001e47: call dial_bounds_fail       # ★ 打印诊断后 ud2 终止
1001e4f: movzbl 0x8(%r14,%rbx,1),%eax
```

为什么**除零也必须检查**：LLVM 的 `sdiv` 在除数为 0 时结果是 **poison**（未定义行为），
优化器有权据此认为该路径不可达，进而把后续代码整段删掉。**实测**：关闭检查后
`100 / 0` 打印出 `0` 并继续执行——程序没崩，却给出了完全错误的结果。这比崩溃危险得多。

### 20.4 性能模式的切换

```bash
dialc build app.dial                 # Debug（默认）：崩溃保护全开
dialc build app.dial --release       # ReleaseSafe：保留防 UB 检查，陷阱用 ud2
dialc build app.dial --fast          # ReleaseFast：关闭全部检查，越界即 UB
dialc build app.dial --safety debug  # 显式指定（debug / safe / fast）
```

> <span id="warn-fast">⚠️ **关于 `--fast` 模式（ReleaseFast）**</span>
>
> **此模式会关闭所有边界检查、除零检查和空指针保护，以换取极致的性能。**
> **除非你是正在编写底层核心算法、且完全确信你的数组下标绝对不会越界，否则请永远不要使用此模式。**
> **在 `--fast` 下发生的越界，将退化为 C 语言的未定义行为（UB），可能导致静默的错误数据甚至灾难性后果。**

同一个等级**同时**作用于 LLVM IR 与 C Runtime（通过 `-DDIAL_SAFETY=N` 注入），
保证两侧检查口径一致。等级是编译期常量，ReleaseFast 下检查代码被编译器完全消除：

```asm
# ReleaseFast 下的 a[99]：无长度调用、无比较、无跳转、无陷阱
1001e14: call dial_array_length      # 这一条来自源码里显式的 a.length()
1001e2a: call dial_array_get         # 直接访问
1001e40: call dial_array_get
```

> **ReleaseFast 的后果由使用者自行承担**：它换来的性能，代价是回到 C 的静默损坏模式。
> 默认等级永远是 Debug——安全性是默认值，性能才是需要显式选择的东西。

### 20.5 崩溃时保留上下文

`dial_bounds_fail` / `dial_null_fail` / `dial_assert_fail` 在触发陷阱前会：
① 冲刷 stdout 缓冲（否则崩溃前已经 print 的内容会随之消失，让人误判崩溃位置）；
② 用 `write(2)` 直接写 stderr 打印原因（不用 stdio，避免缓冲来不及 flush）。

```
dial: 下标越界（Fail-Fast 终止）: index = 99, length = 3
[1]    12345 illegal hardware instruction (core dumped)  ./app
```

---

## 21. 代码编写纪律（防臃肿与允许冗余原则）

本节约束**编译器前端（Rust）与 C Runtime 自身的实现代码**，而非 Dial 用户代码。
它与 [§19 内存宪法](#19-内存宪法防-gc-铁律) 同为工程底线：一个是内存模型的底线，一个是代码形状的底线。

### 21.1 绝对拒绝过度抽象

严禁为了复用而把代码抽象成极度复杂的高阶函数、宏或泛型。
**如果复制粘贴几行代码能让逻辑更直观、调试更方便，就果断复制粘贴——宁可 WET，绝不 DRY 过头。**

- 典型体现：`array_method_type` 与 `map_method_type` 是**两份各自独立的直白函数**，
  不合并成一个带泛型的"通用方法签名解析器"；`dial_array.c` 与 `dial_map.c` 也各自独立实现，
  不为省几行而引入共享的"通用容器抽象层"。
- 判据：新增一个抽象前先问——它是让**读代码的人**更快理解，还是只让**写代码的人**少敲几行？后者一律不做。

### 21.2 不引入无用依赖

Rust 前端与 C Runtime 的代码**尽量只依赖标准库**。任何新引入的 Crate 或 C 库，
必须能证明它**不可替代且极其轻量**。

- 当前事实：`Cargo.toml` 的唯一依赖是 `inkwell`（LLVM 绑定，不可替代——需要它 emit 目标文件）；
  C Runtime 只用 `<stdlib.h>` / `<string.h>` / `<stdio.h>` / `<stdint.h>`。
- 判据：能用 `for` + `if` 写完的，绝不引一个 Crate；能自己算的哈希，绝不引哈希库。

### 21.3 允许安全与性能上的冗余

明确**允许**两类冗余，它们不是"多余代码"：

| 冗余类型 | 允许的形式 | 现实例子 |
| --- | --- | --- |
| 边界防御 | 跨语言（Dial ↔ C）边界处的少量防御性判断，防止崩溃 | `dial_map_put` 在共享时先 COW 再改；`dial_read_line` 在 `fgets` 返回 NULL 时返回空串而非空指针；C Runtime 每个下标访问前都做边界检查（§20.3） |
| 性能冗余 | C 数据结构中为内存对齐、预分配留出的冗余空间 | `dial_map` 的 entry 用 `int64_t used`（而非 `int8_t`）保持 8 字节对齐；`new_map()` 直接预分配 `cap = 8` |

> Fail-Fast 的检查本身也是**被明确允许的冗余**：它让"安全"成为默认值，
> 把性能留给需要的人显式选择（`--fast`）。这与 §20 是一致的，而非冲突。

### 21.4 保持代码直白

**能用一个循环和一个 `if` 解决的事情，绝对不要用设计模式套壳。**

- 线性探测就是一层 `while` 循环查找空槽；删除就是一层"左移紧凑"循环——不套迭代器、不套策略模式。
- 编译器的 pass 就是顺序执行的几个函数（收集签名 → 检查函数体 → 数据流分析 → 代码生成），
  不引入 visitor 框架或 IR 中间层的插件机制。

---

## 22. 版本变更摘要

### 22.0 v0.61（相对 v0.6）

本版落地**文件 IO（file 句柄）**，并正式确立[版本号规则](#23-版本号规则)（最多两位小数、禁止跳版、严格按时间递增）：

| 新增 / 修正点 | 说明 |
| --- | --- |
| **`file` 不透明指针类型** | 新增 `file` 类型，IR 中为 `ptr`；封装 C 的 `FILE*`，**不受 ARC 管理**，由用户显式 `open` / `close`（与内存宪法 [§19](#19-内存宪法防-gc-铁律) 显式指针回收一致） |
| **`open` / `read` / `write` / `close`** | 仅封装 C 标准库 `fopen` / `fread` / `fwrite` / `fclose`，Rust 前端不重写任何文件逻辑；最基础的 C 风格 API，无链式流、无额外语法糖 |
| **错误处理（无异常）** | 严格遵循「无异常」原则：打开失败返回**空指针**、读失败返回**空串**、写失败返回 **-1**；不为其引入 Result 封装或自动抛错 |
| **无全局变量** | 文件句柄必须显式传递，绝不引入全局文件状态 |
| 示例与负向用例 | 新增 `examples/file_demo.dial`（写后读回并打印）；新增 `tests/bad/io_bad_arg.dial`（open 参数类型错误须在编译期拒绝） |

> 版本号按[版本号规则](#23-版本号规则)：本版属「小功能 / 新语法」，递增 `+0.01`（`v0.6 → v0.61`）。

### 22.1 v0.6（相对 v0.5）

本版把 Zig 式 **Fail-Fast** 确立为与内存宪法并列的语言底线（[§20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)）：
宁可当场崩溃，绝不产生未定义行为或静默错误数据。

| 新增 / 修正点 | 说明 |
| --- | --- |
| **Codegen 硬性插入陷阱** | 数组下标读（`Expr::Index`）、写（`a[i] = v`）、整数除法前插入检查，失败分支发射 `llvm.debugtrap`（Debug，`int3`）或 `llvm.trap`（ReleaseSafe，`ud2`），由 OS 接管 Core Dump |
| **C Runtime 边界检查** | `dial_array_get/set`、`dial_string_get`、`dial_string_substring` 全部加入边界检查，越界调用 `dial_bounds_fail` → `ud2`；空指针、OOM、重复释放同样当场终止 |
| **除零检查** | LLVM `sdiv` 除数为 0 时结果是 **poison（UB）**，优化器可据此删掉后续代码；现改为先检查再除。实测关闭检查时 `100 / 0` 打印 `0` 后继续跑——静默错误结果 |
| **三档安全等级** | 默认 Debug（`int3`）/ `--release`（ReleaseSafe，`ud2`）/ `--fast`（ReleaseFast，关闭全部检查）；等级同时注入 LLVM IR 与 C Runtime（`-DDIAL_SAFETY=N`） |
| **崩溃上下文** | 陷阱前先冲刷 stdout 缓冲，再用 `write(2)` 直接写 stderr 打印原因（如 `index = 99, length = 3`） |
| **新增测试组 D / E** | D：`tests/trap/*.dial` 必须当场因信号终止且不得打印哨兵；E：同一批用例在 `--fast` 下应关闭检查（证明陷阱确由编译器插入） |

相关文档同步更新：§1（设计原则表）、§2（目录结构）、§3.3（安全等级参数）、§11.5（越界行为）、
§17.1（UB 清单重写）、§18（测试组）、§21.3（冗余原则与 Fail-Fast 的关系）。

### 22.2 v0.5（相对 v0.4）

本版补齐循环控制流，并把[代码编写纪律](#21-代码编写纪律防臃肿与允许冗余原则)正式写入规范。

| 新增 / 修正点 | 说明 |
| --- | --- |
| **`break`** | 跳出最内层循环（仅允许出现在 `while` / `for` 体内，见 §8.4） |
| **`continue`** | 进入下一次迭代；`for` 中先执行 `i = i + 1` 再回到条件判定（自增落在独立的 `for.inc` 块，避免死循环） |
| **代码编写纪律** | 新增 [§21 代码编写纪律](#21-代码编写纪律防臃肿与允许冗余原则)：拒绝过度抽象（宁可 WET）、不引入无用依赖、允许安全与性能冗余、保持代码直白 |

相关文档同步更新：§8.1（语句一览）、§8.4（`break` / `continue`）、§17.2（从「未实现」移除 break/continue）。

### 22.3 v0.4（相对 v0.3）

本版落地**部分标准库**与**语言特性扩充**，并正式写入[内存宪法](#19-内存宪法防-gc-铁律)：

| 新增 / 修正点 | 说明 |
| --- | --- |
| **`map` 类型（int → int）** | 新增哈希表类型，内存布局 `[cap][count][entries...]`，与 `string` / `int[]` 共用同一套 **ARC 引用计数**（`dial_map_retain` / `dial_map_release`）；线性探测 + 左移紧凑删除；负载超 3/4 自动扩容重哈希 |
| **`map` 方法** | `put(k,v)`（返回可能更新的指针，编译器写回接收变量）、`get(k)`（未命中 -1）、`contains(k)`（bool）、`remove(k)`（返回其值，未命中 -1）、`size()` |
| **`new_map()` / `read_line()`** | 两个无参内置函数：`new_map()` 构造空表；`read_line()` 从标准输入读一行（EOF 返回空串，绝不崩溃） |
| **`else if` 语法糖** | `else if (c) {..}` 编译期展开为嵌套 `if`（不引入新 AST 节点），见 [§8.2](#82-if-else-if-while) |
| **`for` 范围循环** | `for (i in start..end)`：循环变量 `i` 为 int，半开区间 `[start, end)`，可在体内重新赋值，见 [§8.3](#83-范围-for-循环) |
| **内存宪法写入 README** | 新增 [§19 内存宪法（防 GC 铁律）](#19-内存宪法防-gc-铁律)：永不追踪式 GC；确定性内存靠 ARC + 编译期分析 +（未来）显式裸指针 |

> 设计约束：Map 的 ARC 实现与 `int[]` 完全对称（inline-entries、put 触发扩容 / COW 并写回指针），**不引入任何非 ARC 的堆管理**，与内存宪法一致。

相关文档同步更新：§5.1（基本类型新增 `map`）、§12（哈希表专章）、§15（FFI 表与 C 导出函数新增 `dial_map_*` / `dial_read_line`）、§17.2（从「未实现」移除 Map 与 read_line）。

### 22.4 v0.3（相对 v0.2）

本版只修改**语义与诊断**，未改变任何架构契约（不透明指针、两步构建、产物零 LLVM 依赖、C 承担一切容器）。

| 修正点 | v0.2 行为 | v0.3 行为 | 动机 |
| --- | --- | --- | --- |
| **`and` / `or` 求值** | 两侧都求值，不短路 | **短路求值**，与 C 的 `&&` / `\|\|` 一致 | 现代语言常识；不短路会让"判空后访问"类代码在左操作数为假时崩溃 |
| **`int` ↔ `float` 转换** | 曾允许 `int→float` 隐式提升（算术/比较中混用） | **零隐式转换铁律**：混用即编译错误，必须写 `int_to_float` / `float_to_int` | 杜绝静默精度丢失；比 C 更严格 |
| **`let` 变量调 `push` / `set`** | 未明确定义 | **编译期拒绝**（`let` 不可重新赋值，无法接收 COW 换回的新指针）；接收者须为顶层 `var` | 把"容器被原地修改但接收者无法接收新指针"的悬垂 Bug 提前到编译期 |
| **新增内置函数** | — | `int_to_float(int): float`（LLVM `sitofp`）、`float_to_int(float): int`（LLVM `fptosi`），纯编译期内联、不经 C 运行时 | 零隐式转换铁律的"显式安全阀" |

相关文档同步更新：§5.3（零隐式转换铁律）、§6.2（可变性判定含 `push`/`set`）、§7.2（类型规则）、§7.3（语义细则表，修正 `and`/`or` 与混合算术两行）、§11.3–§11.4（接收者须为 `var`）。

---

## 23. 版本号规则

Dial 目前处于 v0.x 快速演进阶段。版本号采用**最多两位小数**的单一十进制数，
**不再使用**语义化版本的「主.次.修订」三段式。

| 变更类型 | 版本增量 | 演进示例 |
| --- | --- | --- |
| **重大更新 / 新引擎 / 大重构** | `+0.1` | `v0.6 → v0.7` |
| **小功能 / 新语法** | `+0.01` | `v0.9 → v0.91` |
| **补丁 / 次要改动** | `+0.01`（与上一种同档） | `v0.91 → v0.92` |

约定与约束（强制）：

1. **最多两位小数**：`v0.1`、`v0.2`、…、`v0.9`、`v0.91`、`v0.92`……
   不存在 `v0.601` 这类三位小数的版本——补丁不再单独占一个数量级，
   与小功能同按 `+0.01` 推进，避免版本号无意义地膨胀、也避免「补丁」与「小功能」纠缠不清。
2. **标题带 `v`、不写补丁位**：文档与 CHANGELOG 的版本标题一律写作 **`[v0.91]`**，
   **不是** `[0.91.0]`。源码 `Cargo.toml` 的 `version` 字段受 Cargo 三段式限制仍写作 `0.91.0`，
   那只是**存储格式**，对外一律显示为 `v0.91`。
3. **不可跳版**：例如不能从 `v0.6` 直接到 `v0.7`；必须按 `v0.61`、`v0.62`……逐档推进，
   除非用户明确要求跳版。
4. **版本号必须严格按时间递增，绝不许倒流**：后发版本的日期不得早于先前版本。
   CHANGELOG 中一旦出现日期倒流（例如 `v0.4` 的日期晚于 `v0.5`），即视为文档缺陷，必须当场修正。
5. **每档一次完整交付**：每一个版本号都对应一次完整交付——代码实现 + 端到端测试（A/B/D/E/C 全绿）
   + README/CHANGELOG 文档 + 官网重新生成 + git 提交 + 含 `.git` 目录的发布压缩包。

---

## 24. 文件 IO（file 句柄）

文件 IO 在 v0.61 落地。设计严守两条底线：**绝不引入全局变量**、**不重写文件逻辑（只封装 C 标准库）**。
文件句柄是显式传递的不透明指针（`file` 类型，IR 中为 `ptr`），封装 C 的 `FILE*`，**不受 ARC 管理**——
用户必须自己 `open` 后 `close`，这与[内存宪法 §19](#19-内存宪法防-gc-铁律)的「显式指针回收」契约一致。

### 24.1 API

| 函数 | 签名 | 返回 | 说明 |
| --- | --- | --- | --- |
| `open` | `open(path: string, mode: string): file` | `file` | 打开文件；底层 `fopen(path, mode)`。**失败（打不开）→ 抛出 `Error`** |
| `read` | `read(f: file, n: int): string` | `string` | 从 `f` 读取**最多 `n` 字节**，返回字符串。**失败 → 抛出 `Error`** |
| `write` | `write(f: file, content: string): int` | `int` | 把 `content` 写入 `f`，返回**实际写入字节数**。**失败 → 抛出 `Error`** |
| `close` | `close(f: file): void` | `void` | 关闭文件（`fclose`）；`f` 为空指针时安全无操作。**v0.81 起也可用于 `channel`**（关闭广播，见 [§27.7](#277-通道关闭与-eofv081)），二者按实参静态类型分派 |

`open` 的 `mode` 直接透传给 C 的 `fopen`，即 `"r"` / `"w"` / `"a"` / `"rb"` 等标准模式。

### 24.2 错误处理（v0.7 起）：失败即抛出 `Error`

> **v0.7 变更**：文件 IO 由「返回错误标志」改为「**失败即抛出 `Error`**」。
> 这是[§26.3 失败与业务分离](#263-失败与业务分离两码事)的直接落地——
> 文件不存在属于**业务失败**，不该让程序崩溃，也不该逼用户去判空指针。

- `open` 打不开文件 → 抛出 `Error`（消息形如「无法打开文件（路径不存在或无访问权限）」）。
- `read` 失败（句柄无效 / 内存不足）→ 抛出 `Error`。
- `write` 失败（句柄无效 / 写入中断）→ 抛出 `Error`。
- `close` 从不失败（空句柄上安全无操作）。

未被 `try` 包裹时，抛出的 `Error` 属于**未捕获异常**——程序打印诊断并终止（§26.5），
**绝不静默继续**。要恢复，用 `try { } catch e { }` 包住：

```dial
try {
    let f: file = open("no_such_file.txt", "r");
    close(f);
} catch e {
    print("caught: ".concat(e.msg()));
}
```

> 底层仍是「C Runtime 返回错误标志（NULL / -1）+ Dial 层转成 `throw`」的两段式：
> 判据在 C 侧，抛出在 Dial 侧，不含任何 LLVM 运行时依赖，也不破坏
> [代码编写纪律 §21](#21-代码编写纪律防臃肿与允许冗余原则)对依赖与抽象的限制。

### 24.3 用后即焚：必须 `close`

`file` 句柄**不受 ARC 管理**，编译器不会在作用域结束时自动关闭或释放它。忘记 `close` 会造成文件描述符泄漏
（与 C 的 `FILE*` 完全一致——这是显式指针回收的代价，也是它确定性的来源）。示例见 `examples/file_demo.dial`。

### 24.4 完整示例

```dial
fn main(): int {
    let path: string = "/tmp/dialc_io_demo.txt";
    var wf: file = open(path, "w");
    write(wf, "dialc-file-io");
    close(wf);                 // 写完务必关闭

    var rf: file = open(path, "r");
    var content: string = read(rf, 100);
    print(content);           // dialc-file-io
    close(rf);
    return 0;
}
```

`file` 句柄不能 `print` 直接打印（会编译期报错），请先 `read` 成 `string` 再打印。

---

## 25. 包分发与依赖管理（生态规划 · 最终规范）

> **状态：规范已定稿；v0.92 起核心链路已实现。**
> 本章是 Dial 生态**最终规范**的决策摘要。**完整规范见
> [ARCHITECTURE.md《Dial 标准库与包分发最终规范》](ARCHITECTURE.md)**——
> 那里有完整的平台矩阵、包内目录树、JSON 映射字典格式、安装流程、
> 与旧草案的逐条修订记录。
>
> **v0.92 已落地**：`import "包名";` 语句、`.dialpkg` 解包（含路径穿越防护）、
> `manifest.txt` 解析、按 Target Triple 选库、**SHA256 强制校验**、
> `.diali` 签名解析接入 Sema、链接期自动链入对应平台的机器码。
> 见 [§25.9](#259-用法import-与-dialc-installv092-已实现)。
> **尚未实现**：`dialc install <URL>`（网络下载；本地文件安装已可用）、一键安装脚本、
> 官方标准库的 `.dialpkg` 化、Windows 线程后端。
>
> 之所以现在就把规范写死：让「不透明指针 + C ABI」这条底层设计（[§14.1](#141-不透明指针契约)、[§15](#15-ffi-契约dial-语法--c-函数)）
> 从第一天起就为包分发服务，而不是等生态长歪了再回头打补丁。规范先于实现，是防止依赖地狱的唯一低成本时机。

Dial 的包分发只解决一件事：**把别人写好的机器码 + 函数签名，安全、确定地接进你的程序**。

它**不是**一个包管理器：不做版本求解、不做依赖树仲裁、不做语义化版本区间匹配。
它是一套**极简、防呆、零歧义**的分发格式——把复杂度的根部直接砍断（见 [§25.3](#253-绝对零版本冲突底层兼容)）。

### 25.0 目标平台

| 目标 | 状态 |
| --- | --- |
| **Windows**（x64 / ARM64） | ✅ 目标平台 |
| **Linux**（x86_64 / ARM64，glibc 与 musl） | ✅ 目标平台 |
| **Termux**（Android ARM64） | ✅ 目标平台 |
| macOS / Darwin / Apple | ❌ **不在目标范围内**，不为其做设计妥协 |

平台只有三个变体集合，这个事实直接决定了下一节的决策：**既然平台数这么少，
把它们的机器码全量打进同一个包，体积代价（几十 MB）完全可以接受。**

### 25.1 专属格式与自动解压（防呆设计）

| 决策 | 规定 |
| --- | --- |
| 容器格式 | **拒绝**直接用 `.zip` / `.tar.gz` 裸分发 |
| 专属扩展名 | **`.dialpkg`**（Dial Package） |
| 用户是否手动解压 | **永远不需要** |
| 解压位置 | 本地缓存目录 `~/.dial/packages/<包名>/`，**绝不污染当前项目目录** |
| 分发内容 | **不分发源码**——标准库与第三方库统一分发**预编译好的机器码** |
| 触发时机 | 源码中出现 `import` 时，`dialc` 自动识别并**静默**解压；或由 `dialc install <URL>` 显式安装 |

要点：

- **必须有专属扩展名。** 不用 `.dpkg`——它会与 Debian 的 `dpkg` 撞名，造成工具链与人的双重误判；
  `.dialpkg` 全局唯一，双击/命令行都不会被当成别的东西。
- **用户永远不手动解压。** 用户把包下载到本地，在代码里 `import`，`dialc` 自动识别 `.dialpkg` 格式、
  解压到全局缓存 `~/.dial/packages/`。**项目目录里不会出现任何解压产物**——
  既不污染版本控制，也不会让「第三方代码」混进用户的源码树。
- **静默但有痕。** 解压是静默的（不打满屏日志），但缓存目录是真实可读的目录，用户可以 `ls` 查看、
  也可以整目录删除后让编译器重新解压——缓存永远是**可重建的派生数据**，不是权威来源。

> 与[代码编写纪律 §21](#21-代码编写纪律防臃肿与允许冗余原则)一致：不引入「包管理器框架」这种抽象，
> `dialc` 自己认识自己的格式，一步到位。

### 25.2 包内结构与接口签名（供 LSP 与 Sema 使用）

解压后的包**必须**包含以下三个核心部分：

| 部件 | 作用 | 是否含实现代码 |
| --- | --- | --- |
| `lib/` **目录** | **全平台**预编译静态库（`linux-x86_64.a` / `termux-aarch64.a` / `windows-x86_64.lib` …），平铺存放，**不限制包体积** | 是（二进制） |
| `xxx.diali` | **唯一的**接口签名文件：`extern fn` 签名、类型定义、注释。**平台无关**，供 Sema 与 LSP 使用 | **否**（纯文本，只有声明） |
| `manifest.txt` | **映射字典**：Target Triple → 机器码文件名；另含包名、ABI 版本号、各文件的 sha256 | 否（纯文本 / JSON） |

```
sqlite.dialpkg
├── manifest.txt          # 映射字典：Target Triple -> 机器码文件
├── sqlite.diali          # 唯一的接口声明（平台无关）
└── lib/                  # 全平台预编译静态库
    ├── linux-x86_64.a
    ├── linux-x86_64-musl.a
    ├── linux-aarch64.a
    ├── termux-aarch64.a
    ├── windows-x86_64.lib
    └── windows-arm64.lib
```

**三件套的职责切分**：`manifest.txt` 回答「有哪些平台、机器码在哪、可信吗」；
`xxx.diali` 回答「这些函数长什么样」；`lib/` 负责「给我当前平台的那份机器码」。

> **为什么全量打包而不是一个平台一个包**：机器码是**编译产物**，不是源码——
> 它不会被人阅读、不会被手工编辑、不会产生「版本分叉」的困扰；而几十 MB 的下载成本
> 在有 CDN 的前提下可以忽略。既然如此，把「选哪个平台的机器码」这件事从**用户侧**
> 移到**编译器侧**：用户只管写代码，一个包走天下。详见
> [ARCHITECTURE.md §2](ARCHITECTURE.md#2-多平台机器码揉捏与字典映射)。

#### `xxx.diali` —— 签名即契约

`.diali` 是**纯文本**的接口签名文件，只含声明，**不含任何实现代码**。
`dialc` 与 LSP **只读取这个文件**来完成：类型检查（Sema）、代码补全、悬停提示（hover）。

```dial
// sqlite.diali —— 接口签名文件（纯文本，无实现）
// dialc 用它做类型检查；LSP 用它做补全与悬停提示。

/// 不透明句柄：外部代码只拿到指针，永远不知道内部布局（见 §14.1）
type Db;

/// 打开数据库；失败返回空句柄。
/// 句柄不受 ARC 管理，必须显式 db_close（§19.3 显式指针回收契约）。
extern fn db_open(path: string): Db;

/// 执行 SQL，返回受影响行数
extern fn db_exec(db: Db, sql: string): int;

/// 关闭数据库；空句柄上安全无操作
extern fn db_close(db: Db): void;
```

由此得到两个直接收益：

- **Sema 不需要读二进制。** 类型检查只依赖 `.diali`，不必解析 `.a` 里的符号表——
  交叉编译、跨架构检查因此成为可能（`.diali` 与架构无关）。
- **LSP 与编译器共用同一份真相。** 补全/悬停与编译期检查读的是同一个文件，
  杜绝「编辑器说能过、编译器说不能过」的经典分裂。文档注释（`///`）就是悬停提示的来源。

#### `manifest.txt` —— 映射字典与元信息

采用 **JSON**（映射字典的本质是「键 → 值」的嵌套结构，JSON 表达力足够，
且几乎所有语言的 JSON 解析都是三行代码，不引入新依赖）：

```json
{
  "name": "sqlite",
  "version": "1.4.0",
  "abi": 1,
  "iface": "sqlite.diali",

  "targets": {
    "x86_64-linux-gnu":       { "lib": "lib/linux-x86_64.a" },
    "x86_64-linux-musl":       { "lib": "lib/linux-x86_64-musl.a" },
    "aarch64-linux-gnu":       { "lib": "lib/linux-aarch64.a" },
    "aarch64-linux-android":   { "lib": "lib/termux-aarch64.a" },
    "x86_64-pc-windows-msvc":  { "lib": "lib/windows-x86_64.lib" },
    "aarch64-pc-windows-msvc": { "lib": "lib/windows-arm64.lib" }
  },

  "sha256": {
    "sqlite.diali":            "3a7e04...（省略）...d8",
    "lib/linux-x86_64.a":      "9f2b7c...（省略）...c1",
    "lib/termux-aarch64.a":    "a20f91...（省略）...19",
    "lib/windows-x86_64.lib":  "7d3ab8...（省略）...45"
  }
}
```

| 字段 | 含义 |
| --- | --- |
| `name` | 包名（同时决定缓存子目录名） |
| `version` | 包作者的版本号（**仅用于展示与人工核对，编译器不做版本求解**） |
| `abi` | **ABI 版本号**；这才是编译器唯一关心的兼容性判据（见 §25.3） |
| `iface` | 唯一的接口声明文件名 |
| `targets` | **映射字典**：Target Triple → 该平台的机器码文件。**Target Triple 是唯一寻址键**——「架构」不足以定位一份机器码（同为 x86_64，glibc / musl / MSVC / MinGW 四者 ABI 互不兼容） |
| `sha256` | **每个文件各自**的哈希：`.diali` 一份，`lib/` 下每个平台机器码各一份。**选中哪个平台就校验哪个平台的**；其余用于整包分发完整性 |

### 25.3 绝对零版本冲突（底层兼容）

这是整套规范的核心赌注，也是它敢不做版本求解的底气：

> 因为 Dial 的底层**严格采用不透明指针（Opaque Handle）**，对外只暴露**标准 C ABI 函数**
> （[§14.1](#141-不透明指针契约)、[§15](#15-ffi-契约dial-语法--c-函数)），
> 外部代码只是**把句柄传给底层函数**来操作，完全不知道底层内存布局。

由此推出：

| 结论 | 说明 |
| --- | --- |
| **内部结构随便改** | 库作者可以任意重构内部数据结构、增删字段、换算法——只要函数签名与语义不变 |
| **旧代码零改动** | 调用方只需**链接新的 `.a` 文件**即可完美运行，无需重新编译、无需改一行源码 |
| **依赖地狱被消灭** | 不存在「A 要 v1、B 要 v2 所以装不上」的局面：同一进程里只有一个 ABI，一个包只有一个版本 |
| **判据是 `abi` 字段** | 只要 `manifest.txt` 里的 **ABI 版本号不变**，就承诺二进制兼容；ABI 变更必须递增该号，编译器据此拒绝 |

- **一个包 = 一个 ABI = 无版本矩阵。** 规范里 `version` 只用于人类阅读与人工核对；
  编译器的兼容性判据**只有 `abi`**。这从根上取消了「版本区间求解」这个 NP 难题的存在理由。
- **ABI 变更是显式事件。** 库作者改坏了兼容性，就必须递增 `abi`；`dialc` 加载时若发现
  调用方期望的 ABI 与包声明不符，**当场报错**，而不是让不匹配的二进制悄悄链进产物。
- **跨平台也是同一个版本号。** `.diali` 只有一份、ABI 号只有一个，
  所以「Windows 版 1.4.0」与「Linux 版 1.4.0」在兼容性判据上是**同一个 1.4.0**，
  不需要任何 per-platform 的版本矩阵。

> 这不是「尽力兼容」，而是**由类型系统强制的不可能冲突**：
> 你拿不到布局，就无法依赖布局；无法依赖布局，内部结构怎么改都不会破坏你。

### 25.4 SHA256 强制校验（防断网与防篡改）

所有 `.dialpkg` **必须自带 SHA256 哈希校验**，由 `dialc` 在**解压前**强制验证整包；
解压之后、进入链接器**之前**，还要对**本次实际要用的那个平台的机器码**再校验一次。

| 场景 | 后果 |
| --- | --- |
| 下载时网络中断 → 文件残缺 | **当场拦截并报错**，绝不让残缺二进制进入链接器 |
| 文件被恶意篡改（植入后门） | **当场拦截并报错** |
| 哈希比对通过 | 正常解压、链接 |

铁律：

- **校验先于一切。** 残缺或被篡改的二进制**绝不允许**流到链接阶段——否则就会产生
  「链接通过、运行随机崩」的幽灵 Bug：它不报编译错、不报链接错，只在客户机器上偶发，
  极难复现，最终全部变成无效的客服纠纷。
- **每个文件都要校验。** `.diali`（签名）与各平台机器码**各自**都有独立哈希。
  只校验其中一个是不够的：`.diali` 被篡改同样致命——它会让「类型检查通过」建立在谎言之上
  （签名说返回 `int`，真实库返回指针）。
- **两层校验分工明确**：整包哈希回答「这个包完整吗、没被改过吗」；
  单文件哈希回答「我要用的这一份机器码可信吗」。两者不可互相替代。
- **失败即终止，不做降级。** 与 [Fail-Fast 铁律 §20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)同调：
  宁可让编译当场失败并给出明确原因，也绝不带病产出可执行文件。

### 25.5 分发渠道：拒绝中心化服务器

| 决策 | 规定 |
| --- | --- |
| **中心化服务器** | **拒绝**。不设官方包仓库、不做账号体系、不做中央索引、不做自建 CDN |
| **分发渠道** | **所有包（包括官方标准库）均通过 GitHub Releases 分发** |
| **成本** | 零运维成本，**利用 GitHub 的免费 CDN** |

这条决策与 Dial 的整体哲学一致：

- **不设账号 = 没有「谁可以发布这个包」的问题**，也就不需要审核、签名体系、信任根。
- **不用中央服务器 = 没有单点故障、没有带宽成本、没有「服务关停后生态归零」的风险。**
- **包的信任由哈希而非服务器担保**：GitHub Releases 只负责「把字节送到你面前」，
  「这些字节是不是我要的那个」由 SHA256 独立回答。**分发渠道与信任判定解耦**——
  换任何一个 CDN 或镜像，都不影响安全性。

### 25.6 一键安装与无缝链接

**一键安装**（Linux / Termux：`install-dial.sh`；Windows：`install-dial.ps1`）：

```
用户把 sqlite.dialpkg 丢进项目目录
            │
            ▼
   运行一键脚本
            │
            ├── 初始化 Dial 环境：建 ~/.dial/、挂 PATH、写配置
            │
            └── 重新扫描 → 找到 sqlite.dialpkg → 执行安装命令
                              │
                              ▼
              dialc install sqlite.dialpkg
                              │
                              ▼
              SHA256 校验 → 解压到 ~/.dial/packages/sqlite/
```

**无缝链接**（`dialc build` 自动完成，用户零配置）：

```
dialc build main.dial
        │
        ├─ ① 扫描包来源：~/.dial/packages/*/ 与项目内 *.dialpkg
        ├─ ② 读源码里的 import，确定被引用的包
        ├─ ③ 读 manifest.txt
        │      ├─ 按【当前编译 Target Triple】查 targets 映射字典
        │      │     └─ 查不到 → 当场报错（并列出本包支持哪些 triple）
        │      ├─ 校验 .diali 与该平台机器码的 SHA256
        │      └─ 读 .diali → 喂给 Sema 做类型检查（供 LSP 补全 / 悬停）
        ├─ ④ 链接阶段：按 triple 自动选中机器码加入链接命令
        │      ├─ 编译到 Windows     → 自动链接包内的 .lib
        │      └─ 编译到 Linux/Termux → 自动链接包内的 .a
        └─ ⑤ 产出可执行文件（仍只依赖 libc / 平台运行库）
```

**「无缝」的含义**：用户不写依赖列表、不写路径、不写平台；包未解压就自动解压，
哈希不符就自动拦截，目标缺失就明确报错而不是静默降级。
**接口契约由 `.diali` 单独统一提供，与底层是哪个平台的机器码完全解耦。**

#### 25.6.1 三条硬规则（已落地）

**① 强制静态库——拒绝一切动态库。**
`targets` 映射的库文件，后缀**必须为 `.a`（Linux/Termux）或 `.lib`（Windows）**；
形如 `.so` / `.dll` 的动态库一律在安装解析 `manifest.txt` 时**当场拒绝**。
理由：Dial 产物的依赖契约是「**只链 libc，绝不引入运行时动态依赖**」（见 [§3.4 产物依赖契约](#34-产物依赖契约)）。
一旦允许 `.so` / `.dll` 混进来，用户程序就会多出一份运行期必须随附的动态依赖，
既违背「零运行时依赖」的承诺，也会让 `ldd` 契约检查（测试 C 组）失效。

**② 重复安装——ABI 相同覆盖，ABI 不同报错。**
缓存目录 `~/.dial/packages/<name>/` 下已存在同名包时，`dialc install` 会先读出旧 `manifest.txt` 比对 `abi`：

| 情形 | 行为 |
| --- | --- |
| **ABI 相同** | 直接**覆盖**——整目录删除后重新解压，避免残留旧平台的机器码或陈旧的 `.diali` |
| **ABI 不同** | **报错并拒绝**，提示先卸载旧包再装（见下），防止不兼容的二进制悄悄链进用户程序 |

> 「为什么不让不同 ABI 共存？」因为同一包名在 `import` 侧只凭名字引用，
> 若允许同名不同 ABI 的包同时躺在缓存里，编译器无法判断该链哪一份——版本冲突的口子就开了。
> Dial 选择「同名即唯一」，把冲突暴露成「先卸载再装」的显式动作，而非静默选错。

**③ 卸载——直接删目录即可。**
Dial 目前**没有** `dialc uninstall` 命令，因为缓存是**纯派生数据**：包的所有内容都来自
`.dialpkg`，删了随时能重装。要卸载一个包，直接在终端删除它的缓存目录即可：

```bash
rm -rf ~/.dial/packages/<包名>/
```

> 注意这条与 ② 的报错提示是自洽的：ABI 冲突时，运行上面这行删掉旧包，再 `dialc install`
> 新包即可，无需任何额外工具。

### 25.7 这套规范刻意不做的事

把边界写清楚，和规范本身同等重要：

| 不做 | 原因 |
| --- | --- |
| 版本区间求解 / 语义化版本匹配 | ABI 不变即兼容，冲突不存在，求解无对象（§25.3） |
| 传递依赖自动解析 | 包内不含依赖声明；依赖由使用者显式 `import`，保持可见与可控 |
| 中央仓库 / 账号体系 / 官方索引服务 | §25.5：全部走 GitHub Releases，零中心化 |
| 源码分发 / 就地编译 | 只分发**机器码 + 签名**，用户机器无需装对方的工具链 |
| 包内执行构建脚本 | 无脚本、无钩子，包是**纯数据**，解压即可用，无可执行的副作用面 |
| 覆盖/污染用户项目目录 | 一律解压到 `~/.dial/packages/`，项目目录零侵入（§25.1） |
| 二进制差分 / 增量更新 | 一个包几十 MB，一次下载即可；增量更新的复杂度远超收益 |
| 自建 CDN / 镜像站 | §25.5：直接用 GitHub 的免费 CDN |

### 25.8 许可隔离：第三方包同样绝不以 GPL 链接进用户程序

包分发的机器码（`lib/` 下的 `.a` / `.lib`）会被**静态链入**用户的可执行文件，
因此**每一个包**都与 [§3.4.1](#341-许可隔离标准库绝不以-gpl-链接进用户程序防法务传染) 的标准库
面临完全相同的法务传染问题，并且风险更大——包的作者不是 Dial 自己，用户无从替他做担保。

> **包内的机器码与 `.diali` 接口签名，其许可由包作者自行决定；
> 但 Dial 官方标准库（`libdial_std.a`）必须额外豁免，并强制锁定为 `MIT` 或 `Apache-2.0`，
> 绝对不允许以 GPL 协议的条款链接进用户的闭源程序，避免造成法务传染。**

| 组件 | 许可要求 | 说明 |
| --- | --- | --- |
| **Dial 官方标准库**（`libdial_std.a`） | **强制 `MIT` 或 `Apache-2.0`** | 官方运行时被链进**每一个** Dial 程序，必须免传染 |
| **第三方包**（`~/.dial/packages/*/lib/*.a`） | 由包作者声明，**`dialc` 不做审查** | 但 `manifest.txt` **必须**含 `license` 字段，`dialc` 在链接时把它打印到构建日志 |

因此 `manifest.txt` 增加一项**必填**字段：

```json
{
  "name": "sqlite",
  "abi": 1,
  "license": "MIT OR Apache-2.0",
  "...": "..."
}
```

| 字段 | 说明 |
| --- | --- |
| `license` | **必填**。SPDX 许可表达式。`dialc` **不校验**其相容性（那是法务判断，不是编译器职责），但**必须存在且非空**——缺失即拒绝加载该包 |

> **编译器的职责边界**：`dialc` 负责让许可**可见**（强制声明 + 构建日志回显），
> 但**不替用户做法务判断**。是否接受某个包的条款，由使用者自己决定。
> 唯一被**强制**的，是 Dial 自己的标准库：它必须是 MIT 或 Apache-2.0，绝不可以是 GPL。

### 25.9 用法：`import` 与 `dialc install`（v0.92 已实现）

**第一步：安装一个包**（本地 `.dialpkg` 文件）

```bash
$ dialc install sqlite.dialpkg
✅ 已安装包 'sqlite' v1.4.0（ABI 1）
   许可: MIT OR Apache-2.0
   位置: /home/you/.dial/packages/sqlite
   支持平台: x86_64-linux-gnu, aarch64-linux-gnu, x86_64-pc-windows-msvc, ...
```

包**永远**解压到 `~/.dial/packages/<name>/`，项目目录零侵入。
缓存是可重建的派生数据，整个删掉后编译器会重新解压。

**第二步：源码里 `import`，然后照常编译**

```dial
import "sqlite";                 // 包名是字符串字面量

fn main(): int {
    var db: Db = db_open("app.db");   // Db 是包声明的**不透明句柄**
    let n: int = db_exec(db, "delete from t");
    db_close(db);                     // void 调用：只作语句，不能当值用
    print(n);
    return 0;
}
```

```bash
$ dialc build main.dial -o main
ℹ️  包 'sqlite' v1.4.0（ABI 1，许可 MIT OR Apache-2.0）
ℹ️  链接 sqlite -> /home/you/.dial/packages/sqlite/lib/linux-x86_64.a（目标 x86_64-linux-gnu）
✅ 已生成可执行文件: main
```

**用户不写路径、不写平台、不写版本**——编译器按自己的 Target Triple 索引，
交叉编译到 Windows 时自动改为链包内的 `.lib`。

**`.diali` 里能写什么**（v0.92 支持两种声明，其余一律报错）：

| 声明 | 含义 | Dial 侧的类型 |
| --- | --- | --- |
| `type Db;` | 不透明句柄 | `Type::Opaque("Db")`——IR 为 `ptr`，**不受 ARC 管理**，生命周期由包自己的 open/close 掌管 |
| `extern fn f(a: T, ...): R;` | 外部函数签名 | 按签名类型检查；调用与本地函数无差别 |

参数与返回类型可用：`int` / `float` / `bool` / `char` / `string` / `void` / `map` / `file` /
`error` / `thread` / `channel<T>` / `T[]`，以及本包声明的不透明类型名。
**未声明又非内建的类型名一律报错**——让它悄悄退化成某个默认类型，
等于把「签名不匹配」这个能在编译期抓住的错误推迟成运行期的内存损坏。

**安全保证（已实现并纳入自测）**：

| 场景 | 行为 |
| --- | --- |
| 机器码被篡改 / 下载残缺 | **SHA256 校验失败，当场拦截**，绝不进入链接器 |
| `.diali` 被篡改 | 同上——被篡改的签名会让「类型检查通过」建立在谎言之上 |
| 包内路径含 `../` 或绝对路径 | **拒绝解压**（路径穿越防护） |
| 当前平台不在 `targets` 里 | 报错并**列出本包支持哪些 triple** |
| 导入的函数与本程序函数重名 | 编译期报错（避免「到底链了哪一个」的悬案） |

> 自测覆盖：`tests/run_tests.sh` 的 **H 组**会用 `tools/make_sample_pkg.sh`
> 现场构建一个示例包、安装、`import` 后编译运行并比对输出，
> 再**故意篡改一个字节**验证 SHA256 确实拦得住。

---

> 本章为**生态规划**，不含任何实现承诺与时间表。**完整规范**（平台矩阵、包内目录树、
> JSON 映射字典格式、安装流程、与旧草案的逐条修订记录、实现路线）见
> **[ARCHITECTURE.md《Dial 标准库与包分发最终规范》](ARCHITECTURE.md)**。
> 规范一旦在真实包上落地，将同步更新本章、ARCHITECTURE.md 与 [CHANGELOG.md](CHANGELOG.md)；
> 在此之前，`import` 仍按 [§17.2](#172-尚未实现的能力截至-v091) 视为**不支持**。

---

## 26. 可恢复异常（try/catch/throw）

v0.7 落地。Dial 至此拥有**两套互补的错误机制**，各管一半，绝不混用：

| 类别 | 例子 | 机制 | 能否恢复 |
| --- | --- | --- | --- |
| **系统崩溃**（程序员的错） | 数组越界、除零、重复释放 | Fail-Fast：`int3` / `ud2` **当场终止**（[§20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)） | **否**（也不该恢复） |
| **业务失败**（运行环境的错） | 文件不存在、网络超时、磁盘满 | **可恢复异常**：`throw Error(...)` / `try ... catch` | **是** |

> v0.6 只有 `ud2`（不可恢复）这一半；v0.7 补上另一半，但**绝不**用它去接越界/除零——
> 那些是 bug，不是业务分支，恢复它们只会把 crash 变成更贵的幽灵 bug。

### 26.1 语法（catch 后无圆括号）

```dial
// 1) 捕获并拿到错误对象：catch e { }   —— 注意没有圆括号
try {
    let f: file = open("no_such_file.txt", "r");
    close(f);
} catch e {
    print(e.msg());          // e 的类型自动推导为内建 Error
}

// 2) 忽略具体错误：catch { }            —— 只要捕获到不崩溃即可
try {
    throw Error("boom");
} catch {
    print("ignored");
}
```

| 写法 | 含义 |
| --- | --- |
| `throw Error("msg");` | 抛出一个错误对象；**只能**抛 `Error` 类型 |
| `try { ... } catch e { ... }` | 捕获，并把错误对象绑定到 `e`（`e` 自动推导为 `error`） |
| `try { ... } catch { ... }` | 捕获但**忽略**具体错误（错误对象被立即释放，不泄漏） |

语法上的两条硬性约束（违反即编译期拒绝）：

- **`catch` 后不得写圆括号。** `catch (e) { }` 会报
  「语法错误：catch 后不得使用圆括号，直接写变量名，例如 `catch e { ... }`」。
  理由与[代码编写纪律 §21](#21-代码编写纪律防臃肿与允许冗余原则)一致：括号在 C 骨架里属于函数或条件专属，
  用在 `catch` 后面只会造成视觉污染，且对解析器毫无帮助。
- **显式 `throw Error(...)` 必须出现在 `try` 块内。** 否则报
  「throw 必须出现在 try 块内（没有捕获点，异常无法被处理）」。
  注意这条**只约束你手写的 `throw`**，不约束内建函数的隐式抛出——见 [§26.1.1](#2611-显式-throw-与内建函数的隐式抛出两条不同的规则)。

#### 26.1.1 显式 `throw` 与内建函数的隐式抛出：两条不同的规则

「必须写在 `try` 里」这条约束**只适用于你自己手写的 `throw`**。
Dial 里有**两种**产生异常的方式，它们的编译期待遇**完全不同**，必须严格区分：

| | ① **显式 throw**（你自己抛） | ② **内建函数的隐式抛出**（运行环境的错） |
| --- | --- | --- |
| 写法 | `throw Error("msg");` | `open(...)` / `read(...)` / `write(...)` 失败 |
| 不在 `try` 块内时 | **编译期报错，拒绝编译** | **编译通过** |
| 运行到该处且无捕获时 | （已编译失败，不存在此情形） | 触发**未捕获异常（panic）**，打印消息并终止进程 |
| 为什么 | 你明确写了一个异常却没有准备捕获点——这是**代码缺陷**，应在编译期挡住 | 失败取决于运行时的文件系统/网络，编译器**无从预知**，无权替你决定「这里一定要捕获」 |

**① 显式 throw —— 编译期强制必须有捕获点**

```dial
fn bad(): void {
    throw Error("boom");      // ❌ 编译错误：throw 必须出现在 try 块内
}

fn good(): void {
    try {
        throw Error("boom");  // ✅ 有捕获点
    } catch e {
        print(e.msg());
    }
}
```

**② 内建函数的隐式抛出 —— 允许写在任何地方**

```dial
fn h(): void {
    let f: file = open("maybe_missing.txt", "r");   // ✅ 编译通过（无论是否在 try 内）
    close(f);
}
```

上面 `h()` **能通过编译**。`open` 会不会失败，取决于运行那一刻文件是否存在，编译期无法判断。
一旦它真的失败、且调用链上没有任何 `try` 捕获，进程会这样终止（**实测输出**）：

```
dial: 未捕获的异常（panic）: 无法打开文件（路径不存在或无访问权限）   ← stderr
（进程以 SIGABRT 终止，退出码 134）
```

同样的失败若被 `try` 接住，则只是**一次普通的业务分支**，程序继续运行（**实测输出**）：

```
caught:
无法打开文件（路径不存在或无访问权限）
still alive
```

> 这不是「编译器漏检」，而是**刻意的语义分工**：
>
> - 显式 `throw` 是你**写死的控制流**，没有捕获点就是写错了 → **编译期拒绝**；
> - 内建函数的失败是**运行环境的分支**，编译器放行到运行期；
>   未被捕获就以 panic **当场明确终止**，而不是让你静默拿到一个坏句柄继续跑
>   （与 [Fail-Fast 铁律 §20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)同调）。
>
> 想要「不终止」，就在调用处包一层 `try / catch`。**是否捕获是你的选择，
> 编译器只负责让失败可见、且绝不静默。**

### 26.2 `Error` 类型：内建、ARC 管理

| 项 | 说明 |
| --- | --- |
| 类型名 | `error`（底层 LLVM `ptr`，**不透明**——与 string/int[] 同一契约，见 [§14.1](#141-不透明指针契约)） |
| 构造 | `Error("消息")`，**唯一**的构造方式（不是关键字，与 `open`/`print` 同类的内建函数） |
| 内存 | **受 ARC 管理**（带引用计数），与 `string` / `int[]` / `map` 同等对待 |
| 方法 | `.msg()` → 返回消息字符串（`string`） |
| 打印 | `print(e)` 等价于打印 `e.msg()` |
| 存储 | 可赋给变量、可作参数/返回值，引用计数自动维护 |

**严禁抛出非 `Error` 类型**——这条由 Sema 强制，编译期即拒绝：

```dial
throw 42;                 // ❌ 语义错误：只能抛出 Error 类型，禁止抛出裸的 int
throw "oops";             // ❌ 同上（string 也不行）
throw Error("oops");      // ✅
```

### 26.3 失败与业务分离（两码事）

判据只有一条：**这是 bug，还是运行环境的正常分支？**

| 判为「系统崩溃」走 `ud2` | 判为「业务失败」走 `throw` |
| --- | --- |
| 下标越界（数组 / 字符串） | 文件不存在 / 无访问权限 |
| 整数除零 | 网络超时、连接被拒 |
| 重复释放 / use-after-free | 磁盘写满、配额不足 |
| 内存分配失败（OOM） | 外部输入格式非法 |

实现上严格两段式，**判据在 C 侧、抛出在 Dial 侧**：

1. C Runtime 只返回**错误标志**（`open` 返回 `NULL`、`write` 返回 `-1`），绝不自己 longjmp；
2. Dial 层（`open`/`read`/`write` 内置的 codegen）识别该标志，构造 `Error` 并 `throw`。

> 这样 C Runtime 保持纯粹（可单独复用、可测试），Dial 层独占控制流的跳转权。

### 26.4 抛出前自动释放 ARC 引用（防漏核心）

异常最容易埋的雷是**内存泄漏**：控制流一跳走，try 体里的 `string` / `int[]` / `error` 局部
就随栈展开而"丢失"，引用计数永不清零。

Dial 的做法：**在 `longjmp` 之前，编译器自动为「try 作用域内」所有活跃 ARC 局部插入 `release`**。

```dial
try {
    let s: string = "很长的字符串……";   // rc = 1
    let a: int[] = [1, 2, 3];           // rc = 1
    throw Error("炸了");                 // ← 跳转前：先 release(s)、release(a)，再 longjmp
} catch e {
    print(e.msg());                      // s 与 a 已正确释放，无泄漏
}
```

释放范围的精确界定（实现细节）：

- 编译器在 `try` 入口记录**当时的作用域深度** `depth`；抛出时只释放 `scopes[depth..]`——
  即「try 体 + 其中嵌套块」的局部，**绝不触碰外层作用域**的变量。
  比"记录变量名集合"更稳健：天然兼容同名遮蔽，不会误放外层的同名变量。
- **被抛出的错误对象本身不被释放**：若它来自某个局部变量（如 `throw myErr;`），
  该变量在 `throw` 处被标记 `moved`，所有权随异常转移给 `catch`，因此不会被误放（避免 UAF）。
- 结构体虽是值类型，但其**堆字段**（string/int[]）同样被递归释放。

### 26.5 未捕获异常：终止，绝不静默继续

`throw` 若不在任何 `try` 内（或 `catch` 块里再次抛出且外层无 `try`），属于**未捕获异常**。
此时程序打印诊断并**终止进程**：

```
dial: 未捕获的异常（panic）: boom
```

> 与 [Fail-Fast §20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)同调：宁可当场死，
> 也绝不让一个没人处理的错误被悄悄吞掉——那才是真正昂贵的失败模式。
> 注意编译器已在 Sema 阶段直接拒绝「`throw` 不在 `try` 内」，所以这条通常只在
> 运行期由 `open`/`read`/`write` 等业务失败触发。

### 26.6 零隐式负担（零成本抽象）

**没有异常的代码，运行时开销严格为零。**

| 代码里有 try/catch 吗 | 生成的东西 |
| --- | --- |
| **没有** | **完全没有**异常相关指令：无 `setjmp`、无异常缓冲区分配、无 ARC 释放插入 |
| 有 | 进入时一次 `dial_exbuf_new` + `setjmp`；正常结束时一次 `dial_exbuf_free`；**仅当真的抛出时**才做 ARC 释放 |

实测（`nm` 检查产物符号，见 `examples/error_zero_cost.dial`）：

```
# 无 try/catch 的程序（error_zero_cost）：
$ nm ./error_zero_cost | grep -iE "setjmp|exbuf|dial_throw|dial_error"
（无任何输出 —— 零开销）

# 有 try/catch 的程序（try_catch_demo）：
$ nm ./try_catch_demo | grep -iE "setjmp|exbuf|dial_throw|dial_error"
0000000001002bb0 T dial_exbuf_new
0000000001002c10 T dial_throw_to
...
```

> 底层用 `setjmp` / `longjmp`（经 C Runtime 封装 `jmp_buf`，对 IR 只暴露为 `i8*`）。
> **关键约束**：`setjmp` 必须由「包含 try 的那个函数」亲自调用——
> `jmp_buf` 里存的是调用点的栈指针，若把它包在某个 C 助手函数里、助手一返回，
> 之后再 `longjmp` 就是跳进一个已死的栈帧（未定义行为）。
> 因此 IR 直接发起 `setjmp`（并标 `returns_twice` 属性），而 `longjmp` 由 C 侧的 `dial_throw_to` 发起。

### 26.7 完整示例

见 `examples/try_catch_demo.dial`（黄金输出 `try_catch_demo.expected`）：

```dial
fn main(): int {
    // 1) 捕获「业务失败」：open 打不开文件 -> 抛 Error -> 被 catch e 捕获
    try {
        let f: file = open("no_such_file.txt", "r");
        close(f);
    } catch e {
        let prefix: string = "caught: ";
        print(prefix.concat(e.msg()));
    }

    // 2) catch 省略变量名：忽略具体错误，只要捕获到不崩溃即可
    try {
        throw Error("boom");
    } catch {
        print("ignored");
    }

    // 3) 捕获之后程序继续正常执行
    print("after");
    return 0;
}
```

输出：

```
caught: 无法打开文件（路径不存在或无访问权限）
ignored
after
```

### 26.8 当前边界

| 边界 | 说明 |
| --- | --- |
| `catch` 不做类型/条件过滤 | 只有一个 `catch`，捕获一切 `Error`；不做多 catch 分支匹配 |
| `Error` 只有 `.msg()` | 不提供错误码、错误链、自定义子类 |
| `finally` | 不支持（用 try 之后的代码即可，Dial 无异常穿透的清理需求） |
| 跨函数抛出 | `try` 只捕获**本函数内**（含被调函数）抛出的异常 |

---

## 27. 所有权转移与多线程并发（`move_to` · `thread` · `channel`）

> v0.8 起可用。**Dial 用「转移所有权」取代「共享内存 + 锁」**：
> 数据不共享，只移交。队列里流动的指针永远只有一个持有者，因此根本不存在数据竞争，
> 也就不需要任何互斥锁——这是本节全部设计的出发点。

### 27.1 三个新东西

| 名字 | 类别 | 说明 |
| --- | --- | --- |
| `thread` | 类型 | 线程句柄（不透明指针）。**不受 ARC 管理**，必须显式 `join` 或 `detach` 之一 |
| `channel<T>` | 类型 | 通道句柄（不透明指针）。跨线程共享 → **原子**引用计数 ARC |
| `spawn(fn, args...)` | 内建 | 创建并**立即**启动线程，返回 `thread` |
| `join(t)` / `detach(t)` | 内建 | 等待线程结束 / 分离线程；二者选一且仅一次 |
| `channel(cap)` | 内建 | 创建容量为 `cap` 的有界通道 |
| `recv(ch)` | 内建 | 阻塞取出一个元素，返回类型即 `T`，接收方成为其唯一所有者 |
| `move_to(x, ch)` | 内建（intrinsic） | 把 `x` 的所有权**移交**给通道 `ch` |

### 27.2 `channel<T>` 与所有权移交

```dial
fn producer(ch: channel<string>): void {
    var s: string = "from-thread";
    move_to(s, ch);          // 移交：s 此后不可再用
}

fn main(): int {
    let ch: channel<string> = channel(2);
    let t: thread = spawn(producer, ch);
    let r: string = recv(ch);
    join(t);
    print(r);                // from-thread
    return 0;
}
```

- **元素类型白名单**：只能是 `string` / `int[]` / `map` / `error` / `channel<...>`。
  通道传的是所有权，只有句柄能做到 O(1) 的指针搬运；`int` / `float` / `bool` 每发一次都要
  装箱（malloc 8 字节），既砸掉「零分配移交」的立身之本，又让 C 侧无法判断槽里的东西
  该 `free` 还是直接丢——因此在**编译期**就拒绝。
- **元素类型是编译期的**：`channel(cap)` 返回 `channel<void>`（未定占位），声明处写出
  `let ch: channel<string> = ...` 才确定 `T`。运行时已擦除（队列只存 `void*`），
  `recv` 的返回类型从变量自身的静态类型取出。
- **`send` / `recv` 不做任何 retain / release**：指针进出队列，引用计数一个数都不动。

### 27.3 零竞争移交与 O(1) 论证

`move_to(x, ch)` **不是函数调用**，是编译期 intrinsic。Codegen 只发三条 IR 指令：

```llvm
%v = load ptr, ptr %x_slot             ; 取指针（不 retain）
call void @dial_channel_send(ptr %ch, ptr %v)
store ptr null, ptr %x_slot            ; 原槽清零
```

- **O(1)**：没有调用栈、没有参数编组、没有引用计数操作，就是一次指针入队加一次清零。
- **零竞争**：`spawn` 的所有实参都在 `pthread_create` **之前**写进线程的 argcell，
  因此不存在「先启动、再移交」的窗口。这也是为什么**不支持**「move_to 到一个已启动的线程」——
  那条路必然有竞争，Dial 选择在编译期就把它关掉。`spawn(fn, x)` 本身就是「move_to 到线程」。

### 27.4 移交之后：编译器的死命令

一旦 `move_to`（或 `spawn` 的堆类型实参移交）发生，该变量即进入 **Moved** 状态，
此后**读取、写入、再次转移**全部当场编译失败：

```
[dialc 错误] 第 N 行: 语义错误：变量 's' 已转移，无法使用
```

Moved 标记落在**声明该变量的那个作用域**上（而非最内层），所以「if 分支里移交、
主流程再用」也会被保守拦下。生命周期交接的会计规则：

| 实参类型 | `spawn` 时的行为 |
| --- | --- |
| `int` / `float` / `bool` | **按值拷贝**（8 字节直存），源变量不受影响、之后照用 |
| `string` / `int[]` / `map` / `error` | **移交**：源槽清零 + 标 Moved，**不 retain 不 release** |
| `channel<T>` | **共享**：原子 `retain` +1，源变量仍可用（通道天生要被两端持有） |

被移交的那一份所有权，由**接收方**负责最终释放：

| 场景 | 引用计数推演 |
| --- | --- |
| `spawn(worker, s)`，worker 不再移交 | main 移交 rc=1 → worker 入口 retain(2) → worker 出口 release(1) → thunk release(0) ✅ |
| worker 内 `move_to(a0, ch)` | 入口 retain(2) → `moved` 使出口**跳过** release → 2 → thunk release(1，归通道) → 接收方释放(0) ✅ |
| `channel` 作 spawn 实参 | 原子 retain(2) → worker 入口(3) → 出口(2) → main 作用域结束(1) → thunk release(0) ✅ |
| `let r : string = recv(ch)` | `recv` 是调用而非变量引用，VarDecl **不会** retain → 直接取得所有权(1) → 作用域结束(0) ✅ |

### 27.5 `spawn` 的底层：thunk + argcell

C 的 `pthread_create` 只认统一签名 `void (*)(void*)`，而 Dial 函数实参类型各异。
编译器为每个被 spawn 的函数生成一个**私有 thunk**：

```llvm
define private void @__dial_thunk_producer(ptr %cell) {
  %p0 = getelementptr ptr, ptr %cell, i64 0
  %a0 = load ptr, ptr %p0                       ; 取出 channel 实参
  call void @producer(ptr %a0)
  call void @dial_channel_release(ptr %a0)      ; 移交来的那一份由本线程最终释放
  ret void
}
```

- argcell **每槽固定 8 字节**：指针 / `i64` / `f64` 直接 `store`；`bool(i1)` 存前 `zext`、
  取后 `trunc`。
- 线程句柄内部**只存 `pthread_t`**，启动参数包由子线程第一时间取走并释放，
  因此 `join` / `detach` 可以立即回收句柄，无需与子线程做原子握手。
- `join` / `detach` 之后 IR 会把句柄槽置 `null`，二次 `join` 触发 `DIAL_HEAP_ASSERT` 当场 Fail-Fast，
  而不是退化成 use-after-free。

### 27.6 确定性纪律（写并发代码时必须遵守）

> **子线程一律不要 `print`**，要观察结果就走 channel 汇聚回 main 后按序打印。
> 多线程输出的交错顺序不确定，会让「逐字节比对」的黄金测试随机变红且极难复现。
> 多生产者通道的到达顺序同样不确定——需要确定顺序时用**单生产者**。

### 27.7 通道关闭与 EOF（v0.81）

v0.8 的通道有一个致命缺口：**没有 EOF**。生产者一旦退出，消费者就永远卡在 `recv` 上——
程序不是跑错，而是**死锁**。v0.81 把这块补上，新增四个内建：

| 内建 | 签名 | 语义 |
| --- | --- | --- |
| `close(ch)` | `(channel<T>) -> void` | 关闭通道（幂等），广播唤醒所有等待者。**不释放通道对象** |
| `done(ch)` | `(channel<T>) -> bool` | **已关闭 且 已空** —— EOF 的精确定义 |
| `closed(ch)` | `(channel<T>) -> bool` | 是否已调用过 `close` |
| `len(ch)` | `(channel<T>) -> int` | 队列中当前的剩余元素数 |
| `try_recv(ch)` | `(channel<T>) -> T` | 非阻塞取出；队列空立即返回，绝不等待 |

#### `done` 为什么是 EOF 的正确判据

`done = closed && len == 0`。它具有一个关键性质：**单调**——一旦为真就永远为真。
因为关闭不可逆，且关闭后 `send` 被禁止，元素只会越取越少，绝不会「刚判断完又冒出一个」。
单调性保证了这个循环**必然终止**：

```dial
while (not done(ch)) {          // 注意：Dial 的逻辑非写作 not，不是 !
    var r: string = recv(ch);
    print(r);
}
```

若 `done` 为假，则要么队列里有元素（`recv` 立刻返回），要么还有生产者会送来或最终关闭
（`recv` 阻塞等一个一定会来的事件）。因此这个循环**既不漏收，也不死锁**。

> 不要用 `len(ch) == 0` 当结束条件——生产者还没来得及发第一个元素时它也成立，会漏收。
> 也不要用 `closed(ch)`——关闭后可能还有残留元素，会提前退出、丢数据。

#### `close` 与 ARC 为什么不会打架

v0.8 曾刻意不给通道加 `close`，担心「关掉之后变量还在、作用域结束再 `release` 一次」造成二次释放。
复核后确认该担心不成立，关键在于**职责划分**：

- `close` 只**置标志 + 广播**，既不释放通道对象，也不动引用计数；
- 通道对象的销毁仍然**只由原子 ARC 归零触发一次**。

二者不重叠，因此 `close` 之后继续持有变量、继续 `recv` 残留元素、最后正常退出作用域，都是安全的。

#### EOF 之后拿到的是什么

`recv` / `try_recv` 在「已关闭且已空」时返回**空指针**（不是空串、不是默认值）。
这是刻意的：NULL 意味着「没有值」，一旦被继续使用，Runtime 会在**使用点**当场 Fail-Fast
（例如 `print` 一个 NULL string 会打印诊断并 `ud2`），而不是让它伪装成空串悄悄流进业务逻辑——
这正是 [§20 Fail-Fast 铁律](#20-fail-fast-铁律宁可崩溃绝不静默损坏) 的要求。

正常写法压根碰不到它：`while (not done(ch))` 守住了循环，`done` 为真时根本不会进去。

#### 关闭后 `send` = Fail-Fast

关了还发是最危险的一种误用：元素会**永久滞留**在队列里，而只要队列非空 `done` 就永远为假，
消费者的收尾循环将永不结束——一次「关了还发」会把 EOF 语义整个废掉，让程序重新退化成死锁。
因此 ReleaseSafe 及以上当场终止；只有 `--fast` 才关闭该检查（后果自负）。

> `close` 同时服务于 `file` 与 `channel`（二者都是「显式终结」），按实参静态类型分派。

### 27.8 判空、多消费者安全与超时（v0.9）

v0.81 补上了 EOF，但并发底座还剩三处硬伤。v0.9 一次把它们清干净。

#### 27.8.1 `is_null(x)`：Dial 终于有了判空

`recv` / `try_recv` / `recv_timeout` 在 EOF 或超时后返回的是**空指针**。而 Dial 没有 `Option`
类型、没有 `null` 字面量——用户此前**无从分辨**「这是没有值」还是「这是一个真实值」，
直接使用会撞上 Runtime 的 Fail-Fast 断言（[§20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)）。

```dial
var r: string = recv(ch);
if (is_null(r)) {
    // 没有值（EOF 或超时）
} else {
    // r 是一个真实的值
}
```

准入名单：`string` / `int[]` / `map` / `error` / `channel<T>` / `thread` / `file`。
`int` / `float` / `bool` 是**值类型，永远非空**，对它们判空是纯噪音——编译期直接拒绝，
而不是让它变成一段恒为 `false` 的死代码。

落到 IR 上只是一次指针比较（`ptrtoint` + `icmp eq 0`，优化器会折叠回 `icmp eq ptr %p, null`），
**不产生任何函数调用，也不碰引用计数**。

#### 27.8.2 多消费者：一个被实证过的死锁

v0.8/v0.81 的通道通知用的是 `pthread_cond_signal`——只唤醒**一个**等待者。
单消费者时看不出问题；多消费者时，「队列非空」是**众多消费者共享的状态**：

> 唤醒其中一个消费者后，若队列里**还剩元素**，其余消费者会带着**已经失效的等待条件**
> （「队列为空」）继续睡死——直到下一次 `send` 才被叫醒。而那次 `send` 又只唤醒一个。
> 于是只要队列深度大于 1、消费者多于 1，就会有消费者永久阻塞。

这就是经典的**丢唤醒（lost wakeup）**。本仓库用对照实验坐实了它（`tests/stress/channel_mt.c`）：

| 版本 | 8 个消费者抢 100 万个元素（队列深 64） |
| --- | --- |
| `pthread_cond_signal`（v0.8 / v0.81） | **25 秒后超时挂死**（exit 124） |
| `pthread_cond_broadcast`（v0.9） | **4.25 秒跑完**，总数恰好 100 万，零重复、零丢失 |

修复只有一处：所有「队列状态变化」的通知一律改用 `broadcast`，让每个等待者都重新检查条件。
正确性优先于惊群开销——Dial 的并发规模远不值得为省一点唤醒去换一次死锁。

**多消费者的标准写法**：

```dial
while (true) {
    var r: string = recv(ch);     // 只在「已关闭且已空」时返回空指针
    if (is_null(r)) { break; }     // 这才是 EOF
    ... 处理 r ...
}
```

注意**不能**用 `try_recv` 判结束：`try_recv` 在「队列暂时空」时也返回空指针，
那不是 EOF，只是「现在没有」。多消费者下用 `try_recv` 会漏收元素。

> 单消费者场景仍推荐 `while (not done(ch))`（[§27.7](#277-通道关闭与-eofv081)）——
> 它连一次多余的 `recv` 都不需要。

#### 27.8.3 `recv_timeout(ch, ms)`：带超时的接收

```dial
var s: string = recv_timeout(ch, 500);   // 最多等 500 毫秒
if (is_null(s)) {
    // 超时（或者通道已关闭且已空）
}
```

- **超时是上限，不是下限**：元素提前到了就立刻返回。
- **超时不会吞掉元素**：`pthread_cond_timedwait` 返回 `ETIMEDOUT` 后会回到 `while` 条件
  重新判定，若此刻队列非空则正常取走——「元素恰好在超时这一刻到达」也能被拿到。
- 所有权规则与 `recv` 完全一致：取出的元素归接收者所有，不做 retain / release。
- 底层实现就是 `pthread_cond_timedwait` + 一个绝对时刻，**没有引入任何状态机**。

> 超时是**边界情况下的兜底**，不是常态。正常收尾仍应走 `close` + `recv` 的 EOF 路径——
> 超时会让「生产者在慢、消费者在快」这种正常节奏也变成一次失败的等待。

#### 27.8.4 ARC 会计：不变，但被验证了

多消费者不会破坏所有权会计：环形队列的每一次出队都在**同一个互斥量**下完成，
因此每个元素只会被弹出一次，也就只会有一个持有者——**结构上不可能 double-free**。

`tests/stress/channel_mt.c` 用「位图 + 原子 `fetch_or`」逐个记录每个序号的领取情况，
重复领取即判失败；`examples/channel_multiconsumer.dial` 从 Dial 层验证同一件事
（两个消费者抢同一个通道，元素总数守恒）。

### 27.9 多路复用 `select`（v0.91）

同时监听多个通道，**任一就绪即执行对应分支**。

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

#### 27.9.1 语法：Dial 风格，不是 Go 风格

Go 写的是 `case v := <-ch:`。Dial **刻意不照搬**，有两条硬性适配：

| | Go | Dial |
| --- | --- | --- |
| 接收动作 | `<-` 运算符 | **函数调用** `recv(ch)`——与 `move_to` / `spawn` 同为内建 |
| 绑定变量 | `:=` | **`as` 关键字** |

```dial
case recv(ch) as v { ... }
```

- **`select` 是语句，不是表达式。** 各 case 绑定的变量类型可以互不相同，
  因此不存在「统一返回类型」可言——绝不能写 `let x = select { ... }`（编译期拒绝）。
- 绑定变量的类型**自动推导**为通道的元素类型（`channel<string>` → `string`），
  生命周期**严格限制**在 case 的 `{}` 内，出了块再用即编译错误。
- `default` 至多一个，且必须放在所有 case 之后。

#### 27.9.2 三条语义

| # | 语义 | 说明 |
| --- | --- | --- |
| 1 | **case 只拿真实值** | 分支体内拿到的**一定是一个真实元素**，不会是空指针 |
| 2 | **全体 EOF → select 直接结束** | 所有通道都「已关闭且已空」时，select 语句结束，**一个分支都不执行**（包括 `default`） |
| 3 | **`default` 使 select 非阻塞** | 没有任何通道就绪时**立刻**执行 `default`，绝不等待 |

> **第 1 条与 `recv` 不同**：`recv` 在 EOF 时返回**空指针**，需要用 `is_null` 判别；
> 而 `select` 的 case 永远拿不到空指针——EOF 的判据是「**select 语句结束了**」。
> 若在 case 里写 `is_null(v)`，它恒为 `false`。
>
> 之所以能保证：底层用**非阻塞**取元素，若在我们挑中某个通道之后、真正取之前，
> 那个元素被别的消费者抢走了，select 会**自动重挑一个**（而不是把空指针交给用户）。

#### 27.9.3 事件驱动：底层怎么等

select **不用**「一把大锁锁住所有队列」（那会引入复杂的锁顺序问题），
而是走**事件驱动**路线：

- 任何通道的状态变化（`send` / `recv` / `close`）都递增一个**全局事件纪元 `epoch`**，
  并在**全局条件变量**上广播；
- select 等待时**先读 epoch → 再查各通道是否就绪 → 若都不就绪就睡到 epoch 变化为止**。

两个关键性质：

| 性质 | 保证方式 |
| --- | --- |
| **不丢唤醒** | epoch 是「先读后查」的：等待前若已有事件发生，epoch 已经变了，等待条件立即不成立，绝不会带着过期的假设睡死 |
| **不死锁** | 通知（notify）一律在**释放通道本地锁之后**才调用 —— 「持通道锁 → 取全局锁」与「持全局锁 → 取通道锁」两种嵌套**都不存在**，全局锁与通道锁永不死锁 |

实测：50 万次「send + select」往返，**wall 0.19 s**，CPU 0.36 s。

> **Windows 说明**：Windows 没有 pthread，对应实现是 `WakeConditionVariable` / `SetEvent`
> （见 [ARCHITECTURE.md §0.2](ARCHITECTURE.md#02-平台差异对-c-runtime-的约束实现路线备忘) 的线程后端规划）。

#### 27.9.4 随机选取：防饿死

多个通道同时有数据时，select **随机**挑一个就绪的 case 执行。
若固定总挑第一个，第二个通道的数据会被一直压着——这就是经典的**饥饿**。

底层用一个**原子递增**的计数器取随机起点（不能用 `rand()`，它不是线程安全的）。
`examples/select_fairness.dial` 断言「两个通道都被处理过」。

#### 27.9.5 ⚠️ 警告：`default` 是空转与死锁的来源

> **`default` 使 select 永不阻塞。** 因此把带 `default` 的 select 放进 `while` 循环里，
> 就是一个 **100% CPU 的空转循环**——每一轮都立刻返回，什么也没等到。
>
> 更糟的是：如果用 `default` 分支去「顶替」真实接收（比如在 default 里也把计数器加一），
> 就会产生**虚假计数**，进而让接收方的期望量与生产方的实际量脱节——
> 生产者把队列塞满后**永久阻塞**，接收方却以为已经收够了，整个程序死锁。

**反例（错误）**：

```dial
// ❌ 错：default 立即返回 -> 100% CPU 空转；且计数是虚假的
var n: int = 0;
while (n < 1000) {
    select {
        case recv(ch) as x {
            n = n + 1;          // 只有真的收到才加
        }
        default {
            n = n + 1;          // ❌ 什么都没收到也算数
        }
    }
}
```

这个反例在本仓库实测：**空转 0.38 s 后卡死在 `join`**（队列被塞满、生产者永久阻塞），
120 秒都没跑完。

**正例（正确）**：

```dial
// ✅ 对：default 只做「记录一次无事发生」，绝不影响计数
var n: int = 0;
var idle: int = 0;
while (n < 1000) {
    select {
        case recv(ch) as x {
            n = n + 1;
        }
        default {
            idle = idle + 1;    // ✅ 只记空闲，不冒充收到
            // 想避免空转，就在这里让出 CPU / 退避
        }
    }
}
```

#### 27.9.5.1 ⚠️ `while (true) { select { ... } }` 是空转陷阱

不带 `default` 的 select 是**阻塞**的——没有就绪通道时它会睡到有事件为止，
单看一次 select 确实不空转。**但把它放进 `while (true)` 就不是了**：

> 全体通道 EOF 时，select 只是**直接结束**（一个分支都不执行），
> 它**不会**替你跳出外层循环。外层若写成 `while (true)`，
> 那么 EOF 之后每一轮 select 都立刻结束、循环立刻重入 ——
> **这就是一个 100% CPU 的空转死循环**（本仓库实测：5 秒超时挂死，`exit 124`）。

因此 **select 循环必须自带退出条件**，只有下面两种正确写法：

**写法一（推荐）：把退出条件写进 `while` 条件**

```dial
// ✅ 对：以「未收到 EOF」为循环条件，通道关空后自然收敛
while (not done(ch)) {
    select {
        case recv(ch) as x {
            ... 处理 x ...
        }
    }
}
```

**写法二：在 case 内主动 `break`**

```dial
// ✅ 对：满足结束条件时在分支里跳出（break 跳出的是 select 外层的那个循环）
while (true) {
    select {
        case recv(ch) as x {
            if (x.eq("quit")) { break; }
            ... 处理 x ...
        }
    }
}
```

两种写法各自对应一个可运行的正例，均已连跑 10 次输出稳定（`got=2` 后正常退出，退出码 0）：

| 写法 | 示例 |
| --- | --- |
| 写法一（`while (not done(ch))`） | `examples/select_exit_condition.dial` |
| 写法二（case 内 `break`） | `examples/select_exit_break.dial` |

**凡是写不出「某条路径能让循环终止」的 select 循环，一律视为缺陷。**

> 若确实需要在「等待 + 超时」之间做选择，用
> [`recv_timeout`](#278-判空多消费者安全与超时v09) 单独接收，而不是给 select 加 default。

#### 27.9.6 未来展望：消除惊群

当前的全局广播有一个已知代价：**惊群**——一次广播会唤醒所有 select 等待者，
其中大多数醒来后发现「不是我」，于是重新睡下。

基线实现选择**逻辑正确 + 低延迟**优先于惊群开销：惊群只造成少量无谓唤醒，
而轮询方案会造成持续 CPU 空转与毫秒级延迟。

在 Linux 上可以用 **epoll + eventfd** 替代条件变量，实现「只唤醒真正相关的那一个」，
把惊群降到零。这是 v0.92 之后的优化方向，不影响本版的语义。

### 27.10 当前边界

| 边界 | 说明 |
| --- | --- |
| 线程入口必须返回 `void` | 返回值无处安放；堆类型返回值还会无人释放地泄漏，故编译期拒绝 |
| 线程入口禁止 `struct` 形参 | 值语义聚合体塞不进 8 字节实参槽 |
| 无一等函数 | `spawn` 首参只能是顶层具名函数（`spawn(worker, ...)`），不能传闭包或函数值 |
| 关闭不可逆 | 没有「重新打开」；关闭后 `send` 一律 Fail-Fast |
| `recv` 仍会阻塞 | 未关闭时队列空就一直等；要非阻塞请用 `try_recv`，要限时请用 `recv_timeout` |
| `select` 多路复用 | **已支持**（v0.91）。但它**不带超时**——要「等一会儿再看」，用 `recv_timeout` 单独接收 |
| `select` 有惊群开销 | 全局广播会唤醒所有等待者，多数醒来后发现「不是我」。Linux 上可用 epoll/eventfd 消除（见 [§27.9.6](#2796-未来展望消除惊群)） |
| `select` 不支持表达式 | 它是**语句**：各 case 的绑定变量类型可以互不相同，没有统一返回类型 |
| 无 `Option` 类型 | 「有没有取到」用 `is_null(x)` 判断；没有 `Some` / `None` 这类包装 |
| 多消费者 | **已支持**（v0.9）。前提是消费者用 `recv` + `is_null` 判结束；用 `try_recv` 判结束会漏收。`select` 的 case 则永远拿不到空值 |
| 无线程池 / 无异步 | 一个 `spawn` 就是一个真线程 |
| `join` 后句柄失效 | 二次 `join` / `detach` 触发 Fail-Fast 断言 |

---

## 28. 语言服务器 `dialc lsp`（v0.94 骨架）

```bash
dialc lsp
```

它不从终端读命令、也不往终端写报告：**stdin 收 JSON-RPC，stdout 回 JSON-RPC**，
按 `Content-Length` 分帧。你不会手敲这条命令——它由编辑器启动
（[`editors/vscode/`](editors/vscode/) 下有一个免安装依赖的 VS Code 扩展）。

```
编辑器  ──stdin──▶  dialc lsp  ──stdout──▶  编辑器
                        │
                        └─ 词法 → 语法 → 语义（与 dialc build 同一套代码）
```

### 28.1 为什么是「同一套代码」而不是独立的解析器

编辑器里的诊断最容易出的毛病不是报错太少，而是**与编译器不一致**：
编辑器说没问题、编译器说有；或者反过来。根因几乎总是——编辑器侧另写了一套解析器，
两套实现慢慢长歪。

`dialc lsp` 不写第二套：它直接调用 `lexer::lex` / `parser::parse_with` / `semantic::analyze`。
因此**编辑器里的红线与 `dialc build` 的报错是同一条错误、同一个位置、同一句文案**。
代价是每次诊断都要跑一遍完整前端；收益是「所见即所编」，这个代价值得。

### 28.2 已支持

| 能力 | 时机 | 说明 |
| --- | --- | --- |
| **诊断** | `didOpen`、`didSave` | 词法 / 语法 / 语义三阶段任一失败即停，只报**第一个**错误（与 `dialc build` 一致：第一个错误之后的位置信息已经失真，报一串连锁错误只会淹没真正的问题） |
| **诊断清空** | `didSave`（改正后）、`didClose` | 关掉文件后不留残留红线 |
| **语义高亮** | `textDocument/semanticTokens/full` | 关键字 / 字符串 / 数字 / 运算符 / 类型 / 标识符 |
| 能力协商 | `initialize` | 声明 `textDocumentSync = 1`（全量同步）与语义 token 的 legend |

刻意**不**在 `didChange` 时发诊断：打字到一半的代码必然是错的，
边打边报会让红线在半句话上反复闪。诊断留给保存时——这也是本节标题写「保存时诊断」的原因。

### 28.3 两个容易出错、已经处理好的细节

**行列基准不同。** dialc 内部的 `line` / `col` 都从 **1** 起，LSP 的 `line` / `character`
都从 **0** 起。少减一个 1，整列高亮就会偏移一格。

**列的计量单位不同。** 词法器按 **char** 记列，LSP 按 **UTF-16 码元**记列。

这里有个容易搞错的直觉：很多人以为「有中文就要换算」，**并不对**。
中文「你」在 BMP 内，1 个 char 就是 1 个码元，`"你好"` 连引号是 4 个 char 也是 **4 个码元**——
它根本测不出换算错误。真正分叉的是 BMP **之外**的字符（emoji 等）：
`"🙂"` 是 **3 个 char 但 4 个码元**，少算那一个码元，这一行后面所有 token 会整体右偏一格。

I 组测试因此**同时**放了两行：中文那行证明长度本身没算错，emoji 那行才真正锁定换算。
我们验证过这个测试不是摆设：把换算那一行代码删掉重跑，只有 emoji 那条变红（中文那条依然绿）——
正是上面这段分析所预期的样子。

### 28.4 刻意没做的部分

| 没做 | 原因 |
| --- | --- |
| 补全、跳转定义、悬停、重命名、格式化 | 都建立在**跨文件符号表**之上。Dial 目前一次编译一个文件，还没有符号表可查——硬做只能做出「猜」的结果，那比不做更糟 |
| 区分变量名与函数名（高亮上） | 同上，需要符号表。当前标识符一律标 `variable`：**宁可不标，也不标错** |
| 行注释高亮 | 词法器直接丢弃注释、不产 token，语义 token 也就拿不到它。注释高亮交给编辑器的 TextMate 语法（已在扩展里提供），那才是它的主场 |
| 增量文档同步 | 全量同步（`textDocumentSync = 1`）对当前规模足够了；增量同步省下的那点带宽，换不来它带来的偏移换算复杂度 |
| 引入 LSP 框架（tower-lsp 等） | 本骨架只需要「读一条 JSON、写一条 JSON」。框架带来的异步运行时与依赖树，远超它省下的那点代码 |

### 28.5 诊断位置（v0.94 顺带修掉的一个缺陷）

Dial 的 `Expr` 不携带行列，因此表达式里的语义错误（比如 `'a' + 1`）天然「不知道在哪」，
原先一律报 `第 0 行 第 0 列`——在编辑器里就是**第一行一个红点**，等于没定位。

v0.94 的处理：给**所有**语句补上位置（连 `If` / `While` / `For` / 表达式语句都补了，
此前只有 `var` / `return` 这类带），然后在语义检查的语句边界上做**兜底包装**：

```rust
// 只补「缺失」的位置，已经精确定位的错误原样放行
check_stmt(...).map_err(|e| if e.line == 0 && e.col == 0 {
    DialcError::new(e.message, line, col)   // line/col = 本语句的位置
} else { e })
```

于是 `let c: char = 'a' + 1;` 的诊断落在**这一行**，而不是文件开头。
代价是精度只到「语句级」而非「表达式级」——这是不做 Expr span 的必然结果，
也是当前规模下正确的取舍：语句级定位已经够用，而给每个 Expr 加 span 会牵动
解析器与代码生成的每一处构造点。

顺带一提，也是这次改动让 B 组负向用例的输出全部带上了「第 N 行 第 M 列」。

---
