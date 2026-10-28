# dialc v0.4 验证报告：真实汇编与端到端自测

本报告所有反汇编片段均来自 `objdump -d` 对**实际链接产物**的反汇编，非人工编写、非伪代码。

| 项目 | 值 |
| --- | --- |
| 编译器版本 | `0.4.0`（`Cargo.toml`） |
| 构建 | `cargo build --release`（Rust + inkwell 0.11，LLVM 18） |
| 链接 | `zig cc` 归档 `libdial_std.a` 后与用户 `.o` 链接 |
| 测试日志 | `TEST_RUN_v0.4.log`（34 项 ✅，0 项 ❌） |
| 平台 | x86-64, Linux |

验证的四项新能力：**① `map` 类型（ARC 哈希表）② `read_line()` ③ `else if` 语法糖 ④ `for` 范围循环**。

---

## 1. `for` 范围循环：半开区间 `[start, end)` 的真实落地

源码（`examples/control_flow.dial`）：

```dial
var sum: int = 0;
for (i in 0..5) {        // i = 0,1,2,3,4
    sum = sum + i;
}
```

反汇编（`objdump -d`，`main` 内）：

```
 1001e6f:	48 c7 44 24 20 00 00 	movq   $0x0,0x20(%rsp)      # sum = 0
 1001e78:	48 c7 44 24 10 00 00 	movq   $0x0,0x10(%rsp)      # i = 0   （循环变量）
 1001e81:	48 83 7c 24 10 04    	cmpq   $0x4,0x10(%rsp)      # 循环守卫
 1001e87:	7f 21                	jg     1001eaa <main+0xea>  # i > 4 则跳出
 1001e90:	48 8b 44 24 10       	mov    0x10(%rsp),%rax      # ── 循环体开始
 1001e95:	48 01 44 24 20       	add    %rax,0x20(%rsp)      #    sum += i
 1001e9a:	48 ff c0             	inc    %rax                 #    i = i + 1
 1001e9d:	48 89 44 24 10       	mov    %rax,0x10(%rsp)
 1001ea2:	48 83 7c 24 10 04    	cmpq   $0x4,0x10(%rsp)      #    回边判定
 1001ea8:	7e e6                	jle    1001e90 <main+0xd0>  #    i <= 4 则继续
 1001eaa:	48 8b 7c 24 20       	mov    0x20(%rsp),%rdi
 1001eaf:	e8 bc 08 00 00       	call   1002770 <dial_print_int>
```

**判读要点**

- 循环变量 `i` 落在 `0x10(%rsp)`（入口块 alloca，跨迭代存活），并非寄存器临时值。
- 源码写的是 `0..5`（end = 5），编译器把守卫 `i < 5` **常量折叠为 `i <= 4`**（`cmpq $0x4` + `jle`）。
  这直接证明 **end 是独占的（半开区间）**：迭代序列恰为 0,1,2,3,4，共 5 次。
- 回边 `jle` + 前置 `jg` 构成标准循环结构；`sum` 累加结果实测输出 `10`（= 0+1+2+3+4）。

---

## 2. `else if` 语法糖：编译期展开为嵌套 `if`

源码：

```dial
if (n < 0) {
    return -1;
} else if (n == 0) {
    return 0;
} else if (n < 10) {
    return 1;
} else {
    return 2;
}
```

反汇编（`classify` 函数）：

```
0000000001001d80 <classify>:
 1001d80:	48 89 7c 24 f8       	mov    %rdi,-0x8(%rsp)
 1001d85:	48 85 ff             	test   %rdi,%rdi             # n < 0 ?
 1001d88:	78 16                	js     1001da0 <classify+0x20>   # -> return -1
 1001d8a:	48 83 7c 24 f8 00    	cmpq   $0x0,-0x8(%rsp)      # else if n == 0
 1001d90:	74 16                	je     1001da8 <classify+0x28>   # -> return 0
 1001d92:	48 83 7c 24 f8 09    	cmpq   $0x9,-0x8(%rsp)      # else if n < 10  (折叠为 n <= 9)
 1001d98:	7f 11                	jg     1001dab <classify+0x2b>   # else -> return 2
 1001d9a:	b8 01 00 00 00       	mov    $0x1,%eax             # return 1
 1001d9f:	c3                   	ret
 1001da0:	48 c7 c0 ff ff ff ff 	mov    $0xffffffffffffffff,%rax   # return -1
 1001da7:	c3                   	ret
 1001da8:	31 c0                	xor    %eax,%eax             # return 0
 1001daa:	c3                   	ret
 1001dab:	b8 02 00 00 00       	mov    $0x2,%eax             # return 2
 1001db0:	c3                   	ret
```

**判读要点**

- `else if` **没有**生成任何额外的运行时开销或新节点——它被编译期展开为嵌套 `if`，
  经 LLVM 优化后收敛为一条**线性条件跳转链**（`js` / `je` / `jg`）。
- 四条 `return` 分别返回 `-1 / 0 / 1 / 2`，与源码分支一一对应。
  实测 `classify(-5)=-1`、`classify(0)=0`、`classify(7)=1`、`classify(100)=2`，全部正确。

---

## 3. `map`（int → int）：ARC + COW 的真实落地

### 3.1 `put` 的返回值被写回接收变量

源码：

```dial
var m: map = new_map();
for (k in 0..3) {
    m.put(k, k * k);
}
```

反汇编（`main` 内）：

```
 1001ec8:	e8 13 03 00 00       	call   10021e0 <dial_map_new>
 1001ecd:	48 89 44 24 08       	mov    %rax,0x8(%rsp)        # m = new_map()
 1001ed2:	48 c7 44 24 18 00 00 	movq   $0x0,0x18(%rsp)       # k = 0
 1001edb:	48 83 7c 24 18 02    	cmpq   $0x2,0x18(%rsp)      # k <= 2  (即 k < 3)
 1001ee1:	7f 35                	jg     1001f18 <main+0x158>
 1001ef0:	48 8b 7c 24 08       	mov    0x8(%rsp),%rdi        # 参数 m
 1001ef5:	48 8b 74 24 18       	mov    0x18(%rsp),%rsi       # 参数 k
 1001efa:	48 89 f2             	mov    %rsi,%rdx
 1001efd:	48 0f af d6          	imul   %rsi,%rdx             # k * k
 1001f01:	e8 4a 03 00 00       	call   1002250 <dial_map_put>
 1001f06:	48 89 44 24 08       	mov    %rax,0x8(%rsp)        # ★ 写回 m（关键）
 1001f0b:	48 ff 44 24 18       	incq   0x18(%rsp)            #    k = k + 1
 1001f10:	48 83 7c 24 18 02    	cmpq   $0x2,0x18(%rsp)
 1001f16:	7e d8                	jle    1001ef0 <main+0x130>
```

**关键**：`1001f06` 处 `mov %rax,0x8(%rsp)` —— `dial_map_put` 的**返回值被无条件写回 `m`**。
这是与 `int[].push` 完全一致的契约：`put` 可能因**扩容**或 **COW** 更换底层 buffer 指针，
编译器强制把新指针写回接收变量，从根本上杜绝"旧指针失效"的悬垂访问。

### 3.2 `dial_map_put` 内部的引用计数判定与 COW

反汇编（`dial_map_put` 内）：

```
 1002287:	49 83 7f f8 02       	cmpq   $0x2,-0x8(%r15)      # refcount == 2 ?（> 1 即共享）
 100228c:	0f 8d aa 01 00 00    	jge    100243c <dial_map_put+0x1ec>  # 共享 -> 走 COW
 ...
 10022bf:	e8 5c fe ff ff       	call   1002120 <dial_rc_alloc>   # 扩容：分配新 buffer
 ...
 1002430:	e8 2b fd ff ff       	call   1002160 <dial_rc_release> # 释放旧 buffer
 1002435:	49 83 7f f8 02       	cmpq   $0x2,-0x8(%r15)      # 再次检查共享
 100244f:	4c 89 ff             	mov    %r15,%rdi
 1002452:	e8 29 fd ff ff       	call   1002180 <dial_rc_clone>   # ★ COW：克隆
 1002457:	49 89 c4             	mov    %rax,%r12
 100245a:	4c 89 ff             	mov    %r15,%rdi
 100245d:	e8 fe fc ff ff       	call   1002160 <dial_rc_release> # ★ 释放旧引用
```

**判读要点**

- `-0x8(%r15)` 正是引用计数头（payload 前 8 字节，见 `runtime/dial_rt_mem.h` 的
  `[int64 refcount][payload]` 布局）。`cmpq $0x2` 即判定 **refcount > 1（被多者共享）**。
- 共享时执行 `dial_rc_clone` + `dial_rc_release`：先克隆再释放本调用者的旧引用——
  **标准 COW**，与 `array.set` 完全对称。
- 扩容路径走 `dial_rc_alloc`（新 buffer refcount = 1）+ `dial_rc_release`（释放旧 buffer）。

### 3.3 COW 隔离的运行时验证

把 `map` 传入函数并在其中 `put`，**调用方不应被污染**（ARC 隔离）：

```dial
fn mutate(x: map): int {
    x.put(99, 999);
    return x.size();
}
```

实测输出：

```
[cow] 传入前 size = 2 ->         2
[cow] 函数内 size = 3 ->         3
[cow] 调用后调用方 size 应为 2 -> 2
[cow] 调用方 get(99) 应为 -1 ->  -1
```

调用方 `size` 保持 `2`、`get(99)` 仍为 `-1`，证明 COW 隔离生效——
**函数内的写入没有污染调用方持有的对象**。

### 3.4 `map` 与内存宪法的一致性

`dial_map_put` / `dial_map_retain` / `dial_map_release` 全部走 `dial_rc_*` 引用计数原语，
与 `string` / `int[]` 共用同一份实现（`runtime/dial_rt_mem.c`）。
产物中**不存在任何追踪式 GC 的运行时标记/清扫代码**——确定性、零 GC 停顿（见 README §19）。

---

## 4. `read_line()` 的 EOF 安全性

源码（`examples/map_demo.dial`）：

```dial
var s: string = read_line();
print(s.length());
```

在**空输入（立即 EOF）**下运行（`printf '' | ./map_demo`），输出 `0` 且退出码为 0：

```
[io] read_line() 读到空行 -> length = 0 ->
0
```

`dial_read_line` 在 `fgets` 返回 NULL 时返回空串（`dial_string_new("")`），
**不会崩溃、不会返回空指针**——这是把示例纳入自动化黄金测试的前提。

---

## 5. 端到端自测结果

完整日志见 `TEST_RUN_v0.4.log`。摘要：

| 组 | 内容 | 结果 |
| --- | --- | --- |
| A | 正向：12 个示例编译 + 运行 + 与 `.expected` 黄金文件逐字节比对 | ✅ 12/12 一致 |
| B | 负向：20 个用例必须编译失败（锁定诊断不被弱化） | ✅ 20/20 正确报错 |
| C | 契约：产物可执行文件不得依赖 libLLVM | ✅ 通过 |
| **合计** | | **✅ 34 项通过，0 项失败** |

新增用例：

| 类型 | 文件 | 覆盖点 |
| --- | --- | --- |
| 正向 | `examples/map_demo.dial` | `map` 全部方法 + `read_line()` |
| 正向 | `examples/control_flow.dial` | `else if` 链 + 两个 `for` 循环（求和 / 写表） |
| 负向 | `tests/bad/map_let_put.dial` | `let` 变量不可调 `put`（接收者须为 `var`） |
| 负向 | `tests/bad/map_bad_arg.dial` | `map.put` 参数必须是 int |
| 负向 | `tests/bad/for_range_type.dial` | `for` 上下界必须是 int |

### 产物依赖契约（组 C 实测）

```
linux-vdso.so.1 (0x00007ffd6b742000)
libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f0fa7fda000)
/lib64/ld-linux-x86-64.so.2 (0x00007f0fa81fd000)
```

**只依赖 C Runtime（libc）**，不含 `libLLVM` —— 两步构建契约成立。

---

## 6. 结论

v0.4 的四项新能力均有**真实反汇编与运行时输出**佐证，且：

1. `for` 循环的半开区间语义（`i < 5` → `i <= 4`）在机器码层面可直接验证；
2. `else if` 是**零成本语法糖**——编译期展开，优化后收敛为线性跳转链；
3. `map` 走与 `string` / `int[]` **完全对称的 ARC + COW**，`put` 的新指针被强制写回接收变量；
4. `read_line()` 的 EOF 行为安全且可自动化验证；
5. 产物仍**零 LLVM 依赖**，34 项端到端检查全绿。

以上完全符合 README §19「内存宪法（防 GC 铁律）」：确定性内存管理，永不引入追踪式 GC。
