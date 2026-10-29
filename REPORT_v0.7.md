# Dial v0.7 交付报告 —— 可恢复异常（try/catch/throw）+ 三重开源许可

> 版本：`0.7.0`（十进制递增，用户显式指定 v0.7，授权跳版；见 README §23）
> 日期：2026-10-30
> 范围：**可恢复异常** + **开源协议**。（`move_to` 所有权转移与多线程并发按用户拍板**推迟**，不在本版）

---

## 1. 本次新增能力

| 能力 | 说明 |
| --- | --- |
| `throw Error("msg");` | 抛出可恢复异常；**只能**抛 `Error` 类型（裸 int/string 编译期拒绝） |
| `try { } catch e { }` | 捕获并绑定错误对象（`e` 类型自动推导为 `error`）；**catch 后无圆括号** |
| `try { } catch { }` | 省略变量名，忽略具体错误，只求不崩溃 |
| 内建 `Error` 类型 | `error`，不透明指针，**受 ARC 管理**；`Error("msg")` 构造，`.msg()` 取消息 |
| `open`/`read`/`write` 改抛 Error | 业务失败不再返回错误标志，改为抛出可恢复异常 |

---

## 2. 用户四条原则的落实情况（逐条自检）

| # | 用户原则 | 落实 | 证据 |
| --- | --- | --- | --- |
| 1 | **极简语法**；严禁抛非 Error | ✅ | `throw 42;` → 编译期拒绝「只能抛出 Error 类型，禁止抛出裸的 int」；`catch (e)` → 拒绝「catch 后不得使用圆括号」；`throw` 不在 try 内 → 拒绝「throw 必须出现在 try 块内」 |
| 2 | **底层防漏**：跳转前自动 release 所有活跃 ARC 局部 | ✅ | `emit_throw` 在 `dial_throw_to`（内含 longjmp）**之前**，按「try 入口作用域深度」精确释放 `scopes[depth..]` 内所有 ARC 局部与结构体堆字段；被抛出的 payload 标记 `moved` 跳过，避免 UAF |
| 3 | **失败与业务分离**：系统崩溃走 ud2，业务失败走 throw | ✅ | 越界/除零仍 `int3`/`ud2`（D/E 组回归全绿）；文件不存在等由 C 侧返回 NULL/-1，Dial 层转 `throw` |
| 4 | **零隐式负担**：无异常代码零开销 | ✅ | `nm` 实测：`error_zero_cost` 产物中**没有任何** `setjmp`/`exbuf`/`dial_throw`/`dial_error` 符号 |

---

## 3. 关键实现决策（含一次踩坑与修正）

### 3.1 setjmp 必须由「包含 try 的函数」亲自调用（重要）

初版把 `setjmp` 包进 C 助手函数 `dial_try(buf)`，编译通过但**运行当场 SIGTRAP 崩溃（exit 133）**。

原因：`jmp_buf` 里保存的是 `setjmp` 调用点的**栈指针**。`dial_try` 一返回，那个栈帧就没了；
之后再 `longjmp` 就是跳进一个已死的栈帧——**未定义行为**。

修正：

- IR 侧**直接**调用 `setjmp(dial_exbuf_env_ptr(buf))`，并打上 `returns_twice` 属性；
- 跳转方向改为：`longjmp` 由 C 侧 `dial_throw_to` 发起（此时 try 所在函数的栈帧仍然存活，合法）。

> 这条已写进 `stdlib/dial_exc.h` 与 README §26.6 的注释，防止后人重蹈。

### 3.2 释放范围用「作用域深度」而非「变量名集合」

`exbuf_stack` 每项记为 `(buf_alloca, depth)`，`depth` = try 压栈前的 `scopes.len()`。
抛出时只释放 `scopes[depth..]`。比记录外层变量名集合更稳健：天然兼容同名遮蔽，
不会误放外层作用域的同名变量。

### 3.3 Error 是 ARC 类型

`Error` 带引用计数（`rc`），与 string/int[]/map 同等对待。这样 `catch e` 拿到的是**所有权**，
catch 作用域结束时自动 `release`，不会 double free，也不会泄漏。

---

## 4. 端到端测试（全绿）

`bash tests/run_tests.sh` → **== 全部测试通过 ✅ ==**

| 组 | 结果 |
| --- | --- |
| **A 正向** | 全部黄金输出一致，含新增 `try_catch_demo`（3 行：caught / ignored / after）与 `error_zero_cost`（13 / 30 / 5） |
| **B 负向** | 新增 3 个用例全部如期编译失败：`throw_non_error`、`catch_with_paren`、`throw_outside_try` |
| **D Fail-Fast** | 越界/除零仍信号终止（回归不变，证明异常机制没有稀释 Fail-Fast） |
| **E --fast** | 对照组不变 |
| **C 契约** | 产物仅依赖 libc，**无 libLLVM** |

`try_catch_demo` 实际输出：

```
caught: 无法打开文件（路径不存在或无访问权限）
ignored
after
```

---

## 5. 开源许可：三重许可（本次新增）

`GPL-3.0 OR MIT OR Apache-2.0` —— **任选其一**遵守。随源码与发布包**完整分发**四份文件：

| 文件 | 内容 |
| --- | --- |
| `LICENSE` | **说明文档**：解释三份协议为何共存、各场景该选哪份、义务是什么 |
| `LICENSE-GPLv3` | GNU GPL v3 全文（674 行，取自 gnu.org 官方原文，未改动） |
| `LICENSE-MIT` | MIT 全文 |
| `LICENSE-APACHE` | Apache License 2.0 全文（202 行，取自 apache.org 官方原文，未改动） |

`Cargo.toml` 同步为 `license = "GPL-3.0 OR MIT OR Apache-2.0"`。

**发布包强制校验**：新增 `tools/package.sh`，打包时**硬性校验**这四份许可文件与 `.git` 均在包内，
任一缺失即**拒绝出包**——杜绝「上一版压缩包里 0 个许可文件」的情况再次发生。

---

## 6. 文档与官网同步

- **README.md**：顶部三重许可说明；§4.3 关键字补 `try`/`catch`/`throw`；§5.1 类型表补 `error`；
  §17.2 补回 `move_to`/并发为「规划中」；§24.2 改为「失败即抛 Error」；
  **新增 §25 包分发与依赖管理（生态规划草案）**、**§26 可恢复异常**。
- **CHANGELOG.md**：新增 `[0.7.0]`。
- **官网**：`tools/build_web.py` 更新（v0.7、三重许可、可恢复异常卡片、下载链接指向 v0.7），
  `web/` 三个页面已重新生成。
- **新增示例**：`examples/try_catch_demo.dial`、`examples/error_zero_cost.dial`（均带 `.expected`）。

---

## 7. 交付物清单

| 交付物 | 状态 |
| --- | --- |
| 编译器源码（Rust + inkwell） | ✅ `cargo build --release` 通过，无 error |
| C Runtime | ✅ 新增 `stdlib/dial_error.{h,c}`、`stdlib/dial_exc.{h,c}`；`dial_io.c` 调整 read 失败语义 |
| 示例/负向用例 | ✅ 2 个正向 + 3 个负向 |
| README / CHANGELOG | ✅ 已同步 |
| 官网 `web/` | ✅ 已重新生成 |
| 开源许可四份文件 | ✅ 已入库并随包分发 |
| `tools/package.sh` | ✅ 新增，强制校验许可与 `.git` |
| git 提交 | ✅ |
| 发布压缩包 `dialc-v0.7.zip`（含 `.git` 与四份许可） | ✅ |

---

## 8. 后续路线图（需用户拍板方可推进）

1. **`move_to` 所有权转移 + 多线程并发（thread / channel）**——本版按拍板推迟；
   `move_to` 需先有并发原语（Channel 等）作为转移目标，建议作为独立版本推进。
2. **包分发与依赖管理**（README §25 草案）——`.dialpkg` 格式、自动解压、`.diali` 签名、
   SHA256 校验、零版本冲突。需先有 `import` 语法。
3. 其余未实现项见 README §17.2。
