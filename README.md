# dialc — Dial 语言编译器 v0.3

Dial 是一门**静态强类型、显式标注、无 GC** 的编译型语言。编译器前端（Rust + inkwell）
只负责把 Dial 源码翻译成 LLVM IR；**所有容器与运行时能力都由 C 语言实现**，
Dial 代码对这些能力的一切访问都降级为对 C 导出函数的调用。

本文件是 v0.3 的**规范性文档**：描述的语言行为与 `src/` 中的实现逐条对应，
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
├── runtime/                # C 运行时（引用计数原语）
│   └── dial_rt_mem.h / .c  #   refcount 分配 / retain / release / COW clone
├── stdlib/                 # C 标准库（Dial 语言的实际能力来源）
│   ├── dial_string.h / .c  #   String
│   ├── dial_array.h / .c   #   Array<int>
│   ├── dial_map.h / .c     #   Map（C 侧已实现，v0.2 语言层未暴露）
│   └── dial_io.h / .c      #   打印 / 读行
├── examples/               # 示例（*.dial + 同名 *.expected 黄金输出）
├── tests/
│   ├── run_tests.sh        # 端到端测试
│   └── bad/                # 必须编译失败的负向用例
└── README.md               # 本文件
```

---

## 3. 构建与使用

### 3.1 构建编译器

```bash
export LLVM_SYS_181_PREFIX=/usr/lib/llvm-18   # 按本机 LLVM 18 安装路径调整
cargo build --release                          # 产物：target/release/dialc
```

`dialc` 自身链接 libLLVM——这是**编译器**的依赖，与用户程序无关。

### 3.2 编译 Dial 程序

```bash
dialc build <源文件.dial> [-o 可执行文件] [--no-link]
dialc run   <源文件.dial> [-o 可执行文件]
```

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

### 3.3 产物依赖契约

用户编译出的可执行文件**只允许**依赖 C Runtime：

```bash
$ ldd examples/hello
    linux-vdso.so.1
    libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6
    /lib64/ld-linux-x86-64.so.2
```

`tests/run_tests.sh` 的 C 组会把"产物不得依赖 libLLVM"作为**硬断言**执行。

---

## 4. 词法结构

### 4.1 注释

`//` 到行尾。无块注释。

### 4.2 标识符

以字母或 `_` 开头，后接字母、数字或 `_`。

### 4.3 关键字

```
var  let  fn  struct  if  else  while  return
and  or   not
true false
int  float  bool  string  void
```

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
| `string` | LLVM `ptr`（不透明） | 由 C Runtime 管理的引用计数字符串 |
| `int[]` | LLVM `ptr`（不透明） | 由 C Runtime 管理的动态数组，**元素类型固定为 `int`** |
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

示例：

```dial
let i: int = 10;
let f: float = int_to_float(i) + 2.5;   // 必须先显式转换，再参与浮点运算
let j: int = float_to_int(3.7);          // j == 3（截断）
```

> `int_to_float` / `float_to_int` 是纯编译器内联的 LLVM 指令，**不经过任何 C 运行时函数**，
> 因此不会引入额外依赖（见 §14）。

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
while (cond) { ... }
return e;                  // 或 return;（仅 void 函数）
```

### 8.2 `if` / `while`

条件**必须带括号**，且类型必须是 `bool`：

```dial
if (x > 0) { ... } else { ... }
while (i < n) { ... }
```

无 `else if` 语法糖，需嵌套书写；无 `for`、无 `break`、无 `continue`。

### 8.3 块与作用域

`{ ... }` 只能作为函数体或 `if`/`while` 的分支体，**不能**作为独立的裸块语句。

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
  编译期插入对称的 `retain`，作用域结束时插入对称的 `release`（详见 §13.4）。

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

**无边界检查**。越界读写是未定义行为（与 C 一致）。

---

## 12. 字符串 `string`

`string` 是不透明指针，内存布局 `[refcount][len][data...]` 由 C 侧私有维护。

### 12.1 方法

| 方法 | 等价于 C 调用 | 返回 | 说明 |
| --- | --- | --- | --- |
| `s.length()` | `dial_string_length` | `int` | 字节长度 |
| `s.concat(t)` | `dial_string_concat` | `string` | 拼接，产生新对象 |
| `s.get(i)` | `dial_string_get` | `int` | 取第 `i` 个字符的**整数码**（v0.2 无 `char` 类型） |
| `s.substring(from, to)` | `dial_string_substring` | `string` | 半开区间 `[from, to)` |
| `s.clone()` | `dial_string_clone` | `string` | COW 感知克隆 |
| `s.eq(t)` | `dial_string_eq` | `bool` | 按内容比较 |

`==` / `!=` 对 `string` 按**内容**比较（编译为 `dial_string_eq`），而非比较指针地址。

### 12.2 不可变语义

v0.2 未提供任何原地修改字符串的接口：所有操作（`concat`、`substring`）都产生新对象。
字符串因此天生满足"只读推断"，跨函数共享永远是 O(1)。

---

## 13. 内存与所有权模型

这是 Dial 相对 C 的核心增值点：**尽可能在编译期把内存管理开销消掉**。

### 13.1 不透明指针契约

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

### 13.2 引用计数对象的布局

```
[ int64 refcount ][ payload ... ]
                    ^
                    └─ Dial 看到的"不透明指针"指向这里
```

`dial_rc_alloc` 返回 payload 指针；`dial_rc_retain/release` 操作其前 8 字节。

### 13.3 编译期三种绑定策略

Sema 阶段对每条 `let/var x = y;` 做数据流分析，判定下列三种策略之一：

| 策略 | 触发条件 | 生成的代码 | 开销 |
| --- | --- | --- | --- |
| **移动语义（Move）** | `y` 在此之后再未被引用，且不在循环/分支内 | 只复制指针，**不生成任何 retain/release** | 零 |
| **只读共享（Share）** | `y` 之后仍被读取但从未被写入 | 一次 `retain`，双方共享同一指针 | O(1) |
| **COW 兜底（Copy）** | 静态分析无法证明独占（如穿过函数边界） | 交给 C Runtime，在**写入瞬间**按 refcount 决定是否深拷贝 | 仅在此刻可能 O(n) |

前两种在编译期即消除了运行时开销；只有第三种可能真的复制内存。

判定"最后一次引用"时，位于循环或分支内的引用被视为**不安全上下文**，
不参与移动判定（因为该语句可能被执行零次或多次）。

### 13.4 栈分配与逃逸分析

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

### 13.5 COW 的触发时机

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

### 13.6 自赋值安全

`x = x` 必须安全。编译器采用"**先取得新值所有权，再释放旧值**"的顺序：

```dial
x = x;   // 先 retain（计数 1→2），再 release（2→1），绝不会先 free 再 use
```

若顺序颠倒，计数会先归零被 `free`，随后的 `retain` 即 use-after-free。

### 13.7 返回值不被提前释放

`return` 的处理顺序是**强制**的：

1. 求值返回表达式；
2. 若返回的是局部变量，标记"已移动"（所有权随返回值转移）；
3. 释放其余**未被转移**的局部变量；
4. 生成 `ret`。

若在求值之前就释放全部局部变量，返回的对象会被提前销毁。

---

## 14. FFI 契约：Dial 语法 → C 函数

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
| `print(x)` | `dial_print_int` / `dial_print_float` / `dial_print_bool` / `dial_print_string` |
| 变量绑定（共享） | `dial_string_retain` / `dial_array_retain` |
| 作用域结束 | `dial_string_release` / `dial_array_release` |

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
DialString dial_read_line(void);            // C 侧已实现，v0.2 语言层未暴露
```

**`stdlib/dial_map.h`**（C 侧已实现，v0.2 语言层**未暴露**类型，暂无 Dial 语法可访问）

```c
DialMap  dial_map_new(void);
void     dial_map_put(DialMap m, int64_t key, int64_t val);
int64_t  dial_map_get(DialMap m, int64_t key);   // 未命中返回 -1
```

---

## 15. 内置 `print`

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

## 16. 未定义行为与已实现边界

诚实列出边界，避免"文档写了、实现没有"的冲突。

### 16.1 未定义行为（与 C 一致的部分）

| 场景 | 行为 |
| --- | --- |
| 数组/字符串下标越界 | 未定义（无边界检查） |
| 整数除零 | 未定义（陷阱，进程终止） |
| 整数溢出 | 按 i64 二进制补码回绕 |

### 16.2 v0.2 未实现（不要指望能编译）

| 能力 | 状态 |
| --- | --- |
| 哈希表 / Map | C 侧已实现 `dial_map_*`，但 Dial 语言层**无 Map 类型**，暂不可访问 |
| 读行 `read_line` | C 侧已实现 `dial_read_line`，语言层**未暴露** |
| 文件 IO | 未实现 |
| 多维数组 `int[][]` | 编译期拒绝 |
| 非 `int` 元素数组 | 编译期拒绝 |
| `char` 类型 | 未实现，字符以 `int` 表示 |
| `else if` / `for` / `break` / `continue` | 不支持 |
| 裸块 `{ ... }` | 不支持 |
| 模块 / 包 / import | 不支持 |
| 泛型、接口、trait、闭包 | 不支持 |
| 指针、地址取值、指针算术 | **永不提供**（与不透明指针契约冲突） |
| 运算符重载 | **永不提供** |
| 全局变量 | 不支持 |
| 数组/结构体字面量作为常量表达式 | 不支持（需先赋给变量） |

---

## 17. 测试

```bash
./tests/run_tests.sh
```

三组检查，全部通过才算绿：

| 组 | 内容 |
| --- | --- |
| **A. 正向** | `examples/*.dial` 必须编译成功，且运行输出与同名 `.expected` 黄金文件**逐字节一致** |
| **B. 负向** | `tests/bad/*.dial` 必须编译失败，用于锁定诊断信息不被弱化 |
| **C. 契约** | 产物可执行文件不得依赖 libLLVM |

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

### 负向用例清单（节选，全部位于 `tests/bad/`）

| 用例 | 锁定行为 |
| --- | --- |
| `let_push.dial` | `let` 变量调用 `push` 必须在编译期拒绝 |
| `implicit_conv.dial` | `let y: float = x`（x 为 int）隐式转换必须在编译期拒绝 |
| `mixed_arith.dial` | `1 + 2.0` 这类 int/float 混合算术必须在编译期拒绝 |

---

## 18. v0.3 变更摘要（相对 v0.2）

本版只修改**语义与诊断**，未改变任何架构契约（不透明指针、两步构建、产物零 LLVM 依赖、C 承担一切容器）。

| 修正点 | v0.2 行为 | v0.3 行为 | 动机 |
| --- | --- | --- | --- |
| **`and` / `or` 求值** | 两侧都求值，不短路 | **短路求值**，与 C 的 `&&` / `\|\|` 一致 | 现代语言常识；不短路会让"判空后访问"类代码在左操作数为假时崩溃 |
| **`int` ↔ `float` 转换** | 曾允许 `int→float` 隐式提升（算术/比较中混用） | **零隐式转换铁律**：混用即编译错误，必须写 `int_to_float` / `float_to_int` | 杜绝静默精度丢失；比 C 更严格 |
| **`let` 变量调 `push` / `set`** | 未明确定义 | **编译期拒绝**（`let` 不可重新赋值，无法接收 COW 换回的新指针）；接收者须为顶层 `var` | 把"容器被原地修改但接收者无法接收新指针"的悬垂 Bug 提前到编译期 |
| **新增内置函数** | — | `int_to_float(int): float`（LLVM `sitofp`）、`float_to_int(float): int`（LLVM `fptosi`），纯编译期内联、不经 C 运行时 | 零隐式转换铁律的"显式安全阀" |

相关文档同步更新：§5.3（零隐式转换铁律）、§6.2（可变性判定含 `push`/`set`）、§7.2（类型规则）、§7.3（语义细则表，修正 `and`/`or` 与混合算术两行）、§11.3–§11.4（接收者须为 `var`）。
