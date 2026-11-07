# 更新日志 / Changelog

本文件记录 Dial 语言编译器 `dialc` 的每个发布版本的相对变更。

> **版本号规则（强制）**
>
> 1. **最多两位小数**的单一十进制数：`v0.1`、`v0.2`、…、`v0.9`、`v0.91`、`v0.92`……
>    不再使用语义化版本的「主.次.修订」三段式。
>    **唯一例外是第三位的补丁号**（如 `v0.98.1`）：它是「两位小数版本 + 一位修复补丁位」，
>    而非「第三位连续小数」；仅用于**修复发布**（只修缺陷、不加新特性），
>    适用条件见 [README §23](README.md#23-版本号规则) 第 6 条。
> 2. **版本标题一律带 `v` 前缀、不写补丁位**：写作 `[v0.91]`，**不是** `[0.91.0]`。
>    （源码 `Cargo.toml` 的 `version` 字段受 Cargo 限制仍写作 `0.91.0`，那是存储格式，
>    文档与官网一律显示为 `v0.91`。）
>    **带补丁位的版本例外**：补丁发布写作 `[v0.98.1]`，补丁位必须显示——
>    否则读者无从判断自己装的是不是修复过的版本。
> 3. **版本号必须严格按时间递增，绝不许倒流**：后发版本的日期不得早于先前版本。
>
> 详见 [README §23](README.md#23-版本号规则)。

---

## [v1.04] — 2026-11-06

**主题：FFI 成为语言一等公民。** 详见 [README §33](README.md#33-v104-ffi外部函数接口)。

> **版本定位**：`v1.0` 里程碑已在 `[v1.03]` 达成、标签 `v1.0` 已打并推送；
> `[v1.00]` ~ `[v1.03]` 是「达成 v1.0」过程中的分版记录（见下方里程碑大标题）。
> `[v1.04]` 是**里程碑达成之后的第一个增量分版**，不归入上面那批分版记录。
> 按本轮交付约定：**只做本地提交，不推送 GitHub、不打 tag**。
> 本版后续还并入了「语言层动态链接」「工程工具链增补」「DWARF 调试信息（-g）」
> 「性能基准（4 语言 × 28 维）」与「真实程序缺口补齐（B/C 组，第一批）」
> （见 [README §34](README.md#34-v104-增补工程工具链并入-v104版本不变)、§35、§36、§37），
> 均不新增分版记录，编译器版本 `1.4.0` 保持不变。
> **最后一批起 `STDLIB_ABI 5 → 6`**（新增 `dial_eprint_*` / `get_or` 等导出符号，
> 按既有判据递增，见 §37.6）；术语「绝不引入全局变量」统一为
> 「绝不引入可变全局状态」，并把「函数名重载」「闭包」写入永久否决（§37.1–37.2）。

### Added

- **语言层 `extern fn`**：直接写在 `.dial` 源码里，编译期生成 LLVM 外部声明、
  链接期由 C 编译器解析，不再强制绕 `.diali` → `.a` → `.dialpkg` → `install` → `import`。
  现有包机制完整保留、行为不变（见 [README §15](README.md#15-ffi-契约)）。
- **语言层 `link "...";`**：显式链入用户 C 扩展库。相对路径以源文件所在目录为基准解析。
- **语言层动态链接**：`link` 除静态归档外也接受 `.so` / `.dll` / `.dylib`
  （含 `libx.so.1` 带版本号变体），按**扩展名**校验 ELF `\x7fELF` / PE `MZ` /
  Mach-O 32、64 位或 fat magic；ELF / Mach-O 自动注入 `-Wl,-rpath,<库目录>`，
  产物无需手工设 `LD_LIBRARY_PATH`。PE 无 rpath，`.dll` 需放 exe 同目录或 `PATH`。
- **`extern struct`（C ABI / `repr(C)`）**：字段顺序、对齐、padding、sizeof 与 C 完全一致；
  只经 `rawptr<Struct>` 读写字段，不按值传递、不能用 `StructLit` 构造。
- **`.diali` 类型白名单打穿**：`byte` / `bytes` / `rawptr<T>` / `fn(A, B): R` 全部放行；
  形参表改为按**顶层逗号**切分，嵌套泛型/函数类型签名不会被切坏。
- **完整数据总线**：`string.as_c_str()` / `bytes.as_ptr()` 返回**借用**的 `rawptr<byte>`
  （由 C Runtime 跳过 8 字节长度头，用户不写 `+8` offset hack）；
  `string_from_cstr(p)` / `bytes_from_ptr(p, n)` 从 C 缓冲区**复制**出受 ARC 管理的新对象。
- **C 回调**：`Type::FnPtr` 生成标准 C calling convention，C 侧可安全调用 Dial 具名函数；
  跨回调状态经 `rawptr<void>` 显式透传，无闭包、无隐藏环境对象。
- **工程工具链（并入 `[v1.04]` 的增补，见 [README §34](README.md#34-v104-增补工程工具链并入-v104版本不变)）**：
  - 新增 `dialc check / fmt / test / new / list / uninstall / pack / help` 八个子命令；
    `build / run / check / test` **共用同一条前端流水线**（`main.rs::frontend()`）；
  - **本地源码模块**：`import "./util.dial";` 或裸名 `import "util";`（同目录存在
    `util.dial` 即视为本地模块），递归合并编译，带循环引用 / 同名函数 / 缺失文件诊断；
  - **顶层 `test "名" { ... }` 块**：`dialc test` 逐块独立进程运行、`--filter` 筛选，
    测试代码不进 `build` 产物（刻意不新增 `test` 关键字，避免破坏已有标识符）；
  - **`dialc pack` + `dial.toml`**：通用产包命令，校验 `.diali` 与静态库 ar 魔数、
    生成 `manifest.txt` 与逐文件 SHA256、打包后就地自检；`dialc new` 生成项目骨架，
    `list` / `uninstall` 补齐包管理。`tools/make_stdlib_pkg.py` 保留给 CI 的三平台 stdlib 包。
- **DWARF 调试信息（`-g` / `--debug-info`，见 README §35）**：`dialc build / run` 加 `-g`
  即为 Dial 代码生成标准 DWARF——`DW_TAG_compile_unit` / `DW_TAG_subprogram`（每个 Dial 函数）/
  `DW_TAG_formal_parameter`（形参）/ `DW_TAG_variable` + `llvm.dbg.declare`（`Stmt::VarDecl`
  局部变量）/ `.debug_line` 行表（`gen_stmt` 的语句行列）。GDB / LLDB 因此能按 `.dial`
  源码行下断点、`print counter` 读标量值、`info args` 看形参、`bt` / `list` 直接指向
  Dial 源码，不需要调试器插件。`DIFile` 用绝对路径，`finalize()` 在 `module.verify()` 前调用。
  标量（`int` / `float` / `bool` / `char` / `byte`）给出精确 DWARF 宽度与编码；复合类型
  在 IR 里本就是不透明 `i8*`，如实映射为指向 `dial_object` 的指针，不伪装布局。
  不带 `-g` 时完全不创建 DIBuilder，零开销。
- **性能基准（4 语言 × 28 维，见 README §36）**：`bench/` 下新增 C / Go / Python / Dial
  四份等价实现，28 个纯整数微基准（算术/递归/数组/字符串/位与控制流五类），
  每语言每项产出**完全相同的校验和**（112/112）作为等价计算的正确性锚点。
  - `tools/bench_harness.py`：外部统一墙钟计时 + 单次进程内 `repeat` 自校准
    （摊薄启动开销）+ 扣除 `noop` 基线，给出**绝对值**（单次纯耗时 ms）与
    **绝对比值**（C = 1.00× 基准），并采集 6 大工程维度（速度 / 峰值内存 /
    编译耗时 / 启动耗时 / 产物体积 / 源码行数），产出 `bench/data.json`。
  - `tools/build_bench_web.py`：由数据生成独立静态页 `web/bench.html`——
    绝对值表 + 比值表 + 六维表 + 底部**六维雷达图**（手写 SVG，4 语言叠加，
    按各维最优语言归一化，越靠外越优），无外部 CDN / JS 依赖、离线可看。
    触及计时下限（或被编译器常量折叠）的项以 `†` 诚实标注。
  - `tools/check_bench_data.py`：校验 28×4 数据完整、校验和与真值一致、
    比值基准、6 维 6 轴，及网页与数据同步（CI 与本地共用）。
  - 官网导航 / 首页 CTA / 安全公告页均加入口；`build_web.py` 不生成 `bench.html`，
    不影响官网产物同步校验。

- **真实程序缺口补齐（B/C 组，第一批）**：按 `REPORT_v1.04_gaps.md` 的裁决落地 5 项，
  规范见 [README §37](README.md#37-v104-增补真实程序缺口补齐与宪法裁决并入-v104分-3-批)。
  - **B9 块注释 `/* ... */`**：lexer 层跳过、跨行同步行号；**未闭合直接 Fail-Fast 报错**，
    绝不静默吞掉余下源码。
  - **B10 `!` 一元逻辑非**：词法层复用 `TokenKind::Not`（`parse_not` / 语义 / IR
    全链路零改动），`!=` 分支优先匹配不受影响；不是新运算符语义，只是同一语法的另一种拼写。
  - **B11 `a.slice(from, to)`**：与切片语法 `a[from..to]` 共用 `dial_array_slice`，
    返回新数组、不改接收者；补齐「能力在但方法名不直觉」的绕路。
  - **C6 `get_or(k, default)`**：普通 map 与泛型 map（标量 V）一次探测，把「未命中」
    与「真实值恰等于默认值」彻底分开。`dial_map_get_or` / `dial_kvmap_get_or`
    两个新 C 导出；句柄 V 由容器类型系统更早拦截，方法层的拒绝分支是纵深防御。
  - **C8 `eprint` / `eprint_raw`**：stderr 输出族（`dial_eprint_*`），与 `print`
    逐类型对称、variadic 规则一致；**不引入可变全局状态**（没有「当前输出流」开关、
    没有全局 `FILE*`），四个分派函数各自独立实现以守 §21.1。
  - **C8 配套 Fail-Fast 加固**：输出族内建（`print` / `print_raw` / `write_char` /
    `eprint` / `eprint_raw`）按名字硬编码分派，此前未列入 `RESERVED_NAMES`——
    用户写 `fn eprint(...)` 能编译却在调用点被内建分支**静默绕过**，违反 §20。
    本轮把这 5 个名字补进保留名清单，同名定义编译期直接拒绝；既有 `print` 调用
    行为不变。`.diali` 自描述补齐 `dial_array_slice` / `dial_map_get_or` /
    `dial_kvmap_get_or` 与 `type DialKvMap;`（新增导出符号不漏登记）。
  - **T 组回归**：stdout/stderr 严格分流、未闭合块注释 Fail-Fast、`get_or`
    类型严格校验、函数重载（B5 永久否决）与输出族保留名负向锁定；A–S 扩为 A–T。
  - **架构记录**：`ARCHITECTURE.md` 追加 §14（宪法裁决与 B/C 组补齐的分层约束、
    ABI 递增判据、3 批落地纪律），明确「规范先于实现，但绝不把未实现项标成已支持」。
- **真实程序缺口补齐（B/C 组，第二批）**：按 §37.5「C11 走独立包」的裁决，本批
  只做**标准库内建**（不进 `stdlib.dialpkg`），全部落在 `stdlib/*.c` +
  `src/semantic.rs` + `src/codegen.rs` + `stdlib/stdlib.diali` 四层。
  - **C1 字符串大小写 / 子串**：`to_upper` / `to_lower` / `contains` / `starts_with` /
    `ends_with`（`dial_string_*`）逐类型安全、UTF-8 字节级。
  - **C4 Unicode 码点**：`char_count` / `chars()` / `char_at(i)` / `char_slice(from,to)`
    按 **UTF-8 码点**而非字节；`length()` 保持字节语义不动，避免破坏既有 416 项回归。
  - **C2 数值严格解析**：`parse_int` / `parse_float` 非法输入 **Fail-Fast**（绝不静默
    返回 0），`dial_parse_int` / `dial_parse_float` 在 C 侧 `dial_assert_fail` 抛 Error；
    `is_int` / `is_float` 提供显式探测。
  - **C3 类型安全格式化**：`format_float(v: float, digits: int)` 拒绝 printf 式运行期
    格式串（§5.3 零隐式转换），`int` 参数编译期被拒；对应 `dial_array_new(1)` + `snprintf`。
  - **C5 数组 API 扩充**：`insert` / `index_of`（标量元素；`string[]` 因位模式歧义被拒）/
    `reverse` / `reduce`（复用 v1.03 函数指针 + `rawptr<void>` thunk，无闭包）/
    `join` / `sort` 放宽到 `string[]`（字典序）/ `clear`，新增 `dial_array_*` C 导出。
  - **C7 文件 IO**：`file_exists` / `file_size`（打不开 `-1`）/ `file_remove` /
    `file_read_all`（FNF→抛 Error）/ `file_read_line(f)`（逐行）/ `eof(f)`；
    刻意用 `file_read_line` 而非 `read_line(f)`，避免与 stdin 版 `read_line()` 撞名
    （B5 否决重载），所有文件操作为不透明句柄、显式 `close`、不进 ARC。
  - **C9 环境**：`has_env` / `getenv`（缺失返回空串）/ `program_name`（argv[0] 只读
    快照，非 `var` 全局）/ `cwd`，无 `setenv`（进程级可变全局状态，否决）。
  - **C10 时间**：`local_parts()`（int[8]，自取分量）/ `date_string()`（固定
    `YYYY-MM-DD HH:MM:SS`）/ `monotonic_ms()`，拒绝 printf 式格式串，组件由调用方
    显式拼（§5.3）；新增 `dial_local_parts` / `dial_date_string` / `dial_monotonic_ms`。
  - **Fail-Fast 加固**：第 2 批 18 个内建名补进 `RESERVED_NAMES`（延续第 1 批 `eprint`
    族加固），同名定义编译期拒绝，杜绝内建分支静默绕过（§20）。
  - **U 组回归**：8 组 API 端到端输出断言、C2 解析失败 Fail-Fast、C4 `char_at` 越界
    Fail-Fast、`string[].index_of` 编译期拒绝、C3 `format_float(int,..)` 零隐式转换拒绝、
    第 2 批内建同名定义被拒；A–T 扩为 A–U。
- **B/C 组缺口补齐 第 3 批：语言机制（B7 / B8 / B6）**，落在 `src/parser.rs` +
  `src/semantic.rs` + `src/codegen.rs` + `runtime/dial_rt_mem.c` + `stdlib/dial_kvmap.c`：
  - **B7 文件级只读 `const`**：支持标量 / 字符串 / 数组 / 嵌套数组 / 字符串数组 /
    结构体字面量，以及字面量上的常量表达式（可常量折叠）；const 可引用其他 const
    （解析期内联展开）；**顶层 `var` 全局变量编译期拒绝**（禁止可变全局状态红线）；
    const 重赋值 / 非编译期常量值 / 类型不匹配 / 局部变量遮蔽 const 均被拒。
  - **B8 结构体字段默认值**：`struct Point { x: int = 0, y: int = 0 }`；默认值必须是
    编译期常量；省略有默认值的字段取默认值、省略**无默认值**的字段编译期报错
    （不静默零初始化）；默认值类型不匹配按零隐式转换铁律拒绝。
  - **B6 结构体进容器**：`Point[]` 与 `map<string, Struct>`。值类型结构体装不进 8 字节
    数组 / map 值槽，故**装箱**为引用计数对象（`dial_struct_box`，布局
    `[int64 refcount][payload]`，与既有 Dial 堆对象同构）后入槽，读出时**解箱按值**拷贝；
    `put` / `get` / `contains` / `remove` / `get_or` / `values()` 全通路打通；
    map 侧新增 `KV_STRUCT(6)` 值种类（与 `KV_STRING` 同走 retain/release），
    未命中返回空指针并由编译器还原零值结构体（绝不解引用空指针）。
  - **边界（如实标注）**：B6 本批只放开「**字段全为标量**」的结构体；含句柄字段的
    结构体进容器需要字段级 ARC（装箱 / 释放时逐字段 retain/release），该能力尚未
    落地，故语义层**明确拒绝**而非让它「看起来能用」却在释放时泄漏或悬垂。
  - **顺带修好的通用缺陷**：字段访问此前只支持「变量 / 字段链」作基对象，
    `Pt{}.x` / `make().y` / `arr[i].f` 一律报「仅支持对变量/字段进行赋值」；现让
    `gen_expr` 字段分支把非 lvalue 基物化到栈上临时槽再取字段指针
    （`gen_lvalue_ptr` 保持只服务数组元素写，避免破坏 `a[i] = v` 的写回语义）。
    B7 的 const 结构体内联后「取字段」成为常态路径，此修复是其必要前提。
  - **V 组回归**（新增第 22 组）：B8 / B7 / B6 正向端到端、顶层 `var` 被拒、
    省略无默认值字段被拒、默认值类型不匹配被拒、含句柄字段结构体作数组元素 /
    map 值被拒；A–U 扩为 A–V。内存专项：3M 次 `Point[]` 建放、1M 次
    `map<string,Point>` 读写循环均在虚拟内存上限内通过（装箱对象随容器释放，无泄漏）。
  - **本批明确不碰（按裁决划清界限）**：B1（`impl` 方法，暂用 `fn` 替代）、
    B2（用户泛型，死守单态化底线）、B3（枚举 ADT，不引入 Tagged Union）；
    B5（函数名重载）与 B4（闭包）维持**永久否决**。C11（`http.dialpkg` 独立包）待落地。
- **C11 独立包第一包：`http.dialpkg`**（§37.5「常用库走独立包」的落地），
  落在 `packages/http/` + `third_party/`：
  - **按领域拆包，本轮只落地 `http`**：正则 / JSON / 摘要 / Base64 各自成包
    （`text` / `json` / `crypto`），后续批次逐个补。
  - **协议层自研**：`packages/http/src/dial_http.c` 实现 URL 解析、请求行与 Header
    拼装、响应报文解析（状态行 / 头部 / `Content-Length` / `chunked` / 读到关闭）、
    Socket 读写——线级细节留在自己手里，为后续的连接指纹定制留出空间。
  - **密码学绝不手搓**：TLS 由 **mbedTLS 3.6.2** 提供并**已接线上层**
    （`https://` 走 mbedTLS，证书校验 fail-closed：系统根 + 自定义 CA 双信任链，
    `mbedtls_ssl_get_verify_result()` 非 0 即拒绝），gzip / deflate 解压由
    **zlib 1.3.1** 提供；
    两者 **vendor 进仓库**（`third_party/`，保留上游 LICENSE 与版权声明），
    构建离线可复现；zlib 裁掉 `gz*.c`（gzip **文件** API，依赖 POSIX `lseek`，
    与 HTTP 内存解压无关且编不过 Windows / musl 目标）。
  - **两层 API**：底层「流式」（`connect` / `write` / `write_line` / `read_line` /
    `read_all`，调用方自己拼报文）+ 高层「一次请求」（`get` / `post` +
    `HttpResponse` 访问器：状态码 / 响应体 / 按名取头 / 按序号遍历）。
  - **错误与所有权口径**：包不抛 Dial 异常（不假定宿主装了异常栈），失败返回空句柄
    （`is_null` 可判），原因取自**线程局部**最近错误槽；不透明句柄**不进 ARC**，
    释放必须显式（`dial_http_close` / `dial_http_resp_free`）。
  - **链接顺序修正**（编译器侧）：静态归档由链接器从左到右扫描，符号只有在其归档
    被扫描时仍处于「未定义」状态才会被拉入。包要向 Dial 返回 `string` / `bytes`
    就必须引用标准库的 `dial_string_new` / `dial_bytes_new`，故顺序由
    `obj → runtime → stdlib → packages` 改为 `obj → runtime → packages → stdlib`。
  - **两条写包必备的 FFI 约定**（已写进包内注释）：Dial 的 `string` 跨到 C 传的是
    **载荷指针**（`[int64 len][bytes...]`），须用 `dial_string_cstr()` 取文本；
    带缓冲的读取器必须先取缓冲再读 socket，否则会漏掉已吞进缓冲的响应体并误报
    「连接提前关闭」。
  - **一处刻意不动语言**：Dial 字符串字面量只支持 `\n \t \\ \"`、**不支持 `\r`**，
    而 HTTP 行结束符是 CRLF——因此包提供 `dial_http_write_line` 负责补 CRLF，
    不为发一个 HTTP 包顺带改语言。
  - **W 组回归**（新增第 23 组）：三平台静态库构建、打包 + 安装（manifest /
    逐文件 SHA256 自检）、`import "http"` 编译运行、本地 HTTP 服务器端到端
    （200 / 404 / POST 回显 / chunked / gzip 解压 / 多响应头含同名多值 / 低层流式）、
    明文三条错误路径（https 连不可达主机 / URL 缺主机名 / 连接失败）必须明确失败；
    **TLS 1.3 子组**（同组第 7 步）：openssl 自签 CA + 本地 TLS 服务器，验证 TLS 1.3
    协商（`TLSv1.3`）、证书校验 fail-closed（清空自定义 CA 后系统根拒签自签）、
    keep-alive 连接池复用（`pool hits > 0`）、404 / 500 全绿。
- **宪法裁决入文档（§37）**：`STDLIB_ABI 5 → 6`（`make_stdlib_pkg.py` 默认、
  CI 两处、`dialc new` 骨架同步，后者改为引用 `package::STDLIB_ABI` 消除硬编码）；
  术语统一为「绝不引入可变全局状态」；「函数名重载」「闭包」写入**永久否决**；
  B7（文件级 `const` 只读常量、禁 `var` 全局）与 B8（结构体显式默认值、
  未声明默认值仍报错）的精确语义先于实现写清；C11 常用库确立为**独立 `.dialpkg`**。

### Changed

- **所有 Dial 函数的 LLVM 签名统一为 C ABI 宽度**：`bool` 内部 i1 → 边界 i32（C 默认整型提升），
  `byte` 内部 i64 → 边界 i8（C 的 `uint8_t`）；函数体内部仍是 i1 / i64，逻辑零改动。
  函数入口 / 出口 / 调用点 / HOF thunk / spawn thunk 五处做**对称** `abi_adapt` 转换，
  靠 A–P 全量回归兜底。这是「C 能安全调用任意 Dial 函数」的前提。
- `STDLIB_ABI` **4 → 5**（新增 `stdlib/dial_ffi.c` 四个数据总线函数），
  编译器版本 `1.3.0 → 1.4.0`；`make_stdlib_pkg.py` 默认 `--abi 5`、CI `--abi 5`。
- 系统库（libc / libm）继续由 C 编译器**默认动态解析**，不进 `link`、不进 LTO 链路；
  所有 `.dialpkg` 仍**强制 `.a` / `.lib` 静态归档**并校验 ar 魔数 `!<arch>\n`，
  J 组三条硬规则一字未动。语言层 `link` 则**静态 / 动态并存**：动态库按扩展名验
  ELF / PE / Mach-O 魔数并注入 rpath，由源码作者显式承担运行期依赖；不写 `link`
  动态库的程序仍满足「只链 libc」的默认产物契约（测试 C 组照常断言）。
- **运行时诊断带位置（A7）**：`panic` / `assert` 失败 / 未捕获异常现在输出
  `文件:行:列`（文件名与行列是编译期常量，运行期零额外开销）；新增
  `dial_panic_loc` / `dial_assert_fail_loc` 两个 C 入口，旧符号保留给其它调用点。
  首行文案与旧版完全一致，G 组回归不受影响。
- **Fail-Fast 收紧（A3）**：旧版 `dialc build a.dial b.dial` 打印「未知参数」后
  **仍以 `rc=0` 产出 a 的可执行文件**（多余输入被静默吞掉）；现在任何未知参数一律
  非零退出，多出来的是 `.dial` 时附带 §34.2 本地模块写法的指引。同一规则推广到全部新子命令。
- **`-g` 隐含 O0**：`emit_object()` 在 `debug_info` 且非 `--fast` 时用
  `OptimizationLevel::None`，避免默认 O2 把栈槽变量优化成 `<optimized out>` 而让
  `print var_name` 失效；`-g --fast` 仍走用户明确要求的 O3 + SIMD/LTO，并打印兼容提示。
  安全等级、运行期检查、C Runtime 注入与 `.dialpkg` 契约均不变。

### Fixed

- 函数实参类型不匹配的诊断：参数序号此前被误填成形参类型名（显示「第 rawptr 个参数」），
  且泛型类型只显示 `rawptr` 而非完整签名；现改为正确序号 + `Type::display()` 完整类型。
- **D 组文档 / 诊断漂移**（按 `REPORT_v1.04_gaps.md` 实测校正）：
  `examples/string_demo.dial` 的「`+` 不对 string 重载」注释、README §16「print 恰好 1 参」、
  §5.5 / §15「只有 `str(char)`」、§17.2「截至 v1.02 / 函数指针不支持 / 源码模块未实现」、
  `Point[]` 报错里「元素类型只支持 int 与句柄」全部改为与实现一致；
  「找不到已安装的包」补上本地源码模块指引（LSP 稳定错误码与文案前缀不变）。
- `dialc test` 逐块链接时的 stdlib 编译日志改为静默（`LinkOpts.quiet`），
  避免把测试结果淹没在重复的 ℹ️ 行里。

### 测试

- 4 个正向示例：`ffi_hello`（语言层 extern + 系统库 + C 变参 + 双向字符串）、
  `ffi_buffer`（rawptr 喂 `memcpy` / `memcmp`）、`ffi_struct`（C ABI 字段读写）、
  `ffi_callback`（libc `atexit` 反调 Dial 函数）。
- 19 个负向用例：伪装的 `.so` / `.dll` / `.dylib`、动态库缺失、非归档后缀、
  `extern struct` 的 string/char/map 字段、保留名与撞名、C 变参不在末尾、
  `rawptr<byte>` 字段访问、`as_c_str` 带参、方法用错类型、数据总线参数个数/类型、
  struct 撞名、`extern` 后跟错关键字。
- 新增 **P 组**：把 C 侧专项库编成 `.a`，逐项比对 `sizeof` / `offsetof` 与 C 完全一致，
  验证 Dial↔C 双向字段读写、C 回调状态透传；另编真实共享库做动态链接端到端
  （显式 `link` + `extern fn`，并用 `readelf -d` 断言 `RUNPATH` 已注入），再用伪装的
  `.a` / `.so` / `.dll` / `.dylib` 验证魔数拒绝不是靠「文件不存在」蒙混过关。
- 新增 **Q 组（工程工具链专项）**：全部新子命令、A3 非零退出且不产物、`fmt` 幂等与
  token 流不变、`test` 通过 / 失败 / 过滤、多文件 import 合并与循环 / 冲突 / 缺失诊断、
  `new → pack → install → import → uninstall` 全链路闭环、`pack` 拒绝动态库与缺字段、
  A7 运行时位置断言。回归脚本由 **A–P 扩为 A–Q**（后续由 R 组扩为 A–R，见下）。
- 新增 **R 组（DWARF 调试信息专项）**：`llvm-dwarfdump` 核对 `.debug_info` / `.debug_line`
  段与 `add` / `main` / `a` / `b` / `sum` / `counter` / `note` 七个 Dial 名字、行表含
  `.dial` 源文件；真实 `gdb -batch` 在 `.dial:9` 断点断言 `print counter = 42`、
  `bt` 显示源码行；无 `-g` 不产 Dial 调试符号的负向断言、`-g --fast` 兼容断言。
  环境缺 `gdb` / `llvm-dwarfdump` 时对应子项明确跳过（⏭️）而非误报失败，
  保证 CI 不被工具缺失阻断。回归脚本由 **A–Q 扩为 A–R**。
- **性能基准与 CI 集成（v1.04 增补）**：`build-linux` 作业新增 Go 安装与
  「性能基准采集」步骤，实跑 4 语言编译 + 28 项计时 + 112 校验和一致性，
  生成 CI 机器上的数据/网页并经 `check_bench_data.py` 校验，同时验证仓库内
  交付快照（`bench/data.json` + `web/bench.html`）配套；产物上传为
  `bench-4lang-28dim` artifact。CI 只写 `/tmp` 再上传，绝不回写 `web/bench.html`，
  避免机器相关的实测数据让官网同步校验误报。

---

## [v1.0 里程碑] — 分版记录（v1.00 ~ v1.03）

> **里程碑与标签**：`v1.00` 是从 `v0.99.1` 跳版而来的语言特性补齐里程碑，当时
> 「回调 / 可变参数」等收尾特性尚未完成。到 `[v1.03]` 这些收尾特性已全部落地并
> 回归通过（A–O 全绿、330 ✅ / 0 ❌、零编译警告），**里程碑标签 `v1.0` 已打并推送；
> 在后续 CI 修复完成后，该标签被删除并重新指向里程碑收尾的最新提交**（见
> 上方「CI 修复与 v1.0 标签重打」章节）。下面四个版本条目统一归到本里程碑之下，
> 作为达成过程中的**分版记录**，而不是四个各自独立的「正式 v1.0」。
>
> | 分版 | 主题 |
> | --- | --- |
> | `[v1.00]` | 语言特性补齐（五组）+ 泛型容器 |
> | `[v1.01]` | 按「真实程序」盘点后的缺口补齐 |
> | `[v1.02]` | `goto` / `with` / 裸指针，并补齐不透明句柄契约 |
> | `[v1.03]` | 回调（函数指针）与可变参数 |
>
> **两类标签互不替代**：`v1.0` 标记里程碑达成；按版本号的发布标签（`v*`）用于
> 触发 CI 产出带全部资产的 Release。一键安装脚本**不依赖精确版本标签**——它通过
> GitHub API 解析最新 Release 的真实资产名并走 `releases/latest/download/`，
> 因此发新版时无需改脚本、也无需补打配套标签，详见下方「安装与发布流程调整」。

---

## [CI 修复与 v1.0 标签重打] — 2026-11-06

**目标**：修掉上一节留下的两个红色测试作业，让 `release` 作业能真正产出带
`stdlib.dialpkg` 的 `v*` Release；随后删除并重打 `v1.0` 标签触发新发布，
使 `releases/latest` 指向这个新页面。纯修复，**编译器版本 `1.3.0` 与
`STDLIB_ABI 4` 均不变**。

### Fixed

- **Windows 核心冒烟**：`stdlib/dial_env.c` 原先依赖 POSIX 的
  `srandom()` / `random()` / `clock_gettime()` / `nanosleep()`；MinGW-w64 只提供
  `rand`（`RAND_MAX` 仅 32767），`clock_gettime` / `nanosleep` 则靠 winpthreads
  才勉强可用，属于脆弱依赖。现改为：
  - 自实现 `xorshift64*` PRNG（纯 64 位整数运算），一次产出 64 位、掩掉符号位
    得到 `[0, 2^63)` 的非负随机数；三平台行为一致，不再依赖任何 libc 随机函数；
  - `dial_now()` / `dial_sleep()` 在 `_WIN32` 下改用 Win32 原生
    `GetSystemTimeAsFileTime`（FILETIME → Unix 纪元秒）与 `Sleep`，非 Windows
    仍走 `clock_gettime` / `nanosleep`。
- **Linux 全量测试**：`tools/cve_check.py` 的 `check_110_leak`（CVE-2026-DIAL-007）
  编译清单漏了 `stdlib/dial_array.c`，导致 `dial_string_split()` →
  `undefined reference to dial_array_new`，而检测器把「编译 / 链接失败」误判成
  「❌ 仍可利用」并阻断发布。补全清单后该用例输出 `FIXED`（3/3 残留元素已释放），
  全套 **8/8 个 CVE 已确认修复**。

### 验证

- `zig cc -target x86_64-windows-gnu` 编译 `dial_env.c` 通过，`_WIN32` /
  `__MINGW32__` 宏确认定义，预处理确认选中 `GetSystemTimeAsFileTime` + `Sleep`
  分支；`-target aarch64-linux-musl` 与 `x86_64-linux-gnu` 全量编译链接通过
  （zig 裸 windows 目标缺 `pthread.h` 是验证环境限制，CI 已安装 winpthreads）；
- `bash tools/build_stdlib.sh` 全量归档通过；`examples/env_demo.dial` 端到端输出一致；
  C 级验证：PRNG 连续 8 次取值互异且非负、`now()` 在 `sleep(12)` 后严格增大；
- `python3 tools/cve_check.py --repeat 1` 8/8 通过；`bash tests/run_tests.sh` A–O 全绿。

### 发布

- 删除本地与远程 `refs/tags/v1.0`（该标签此前**没有**关联任何 Release，
  CI 的 `release` 作业因测试作业失败被 skip，删除是安全的）；
- 在本节修复后的最新提交上重新创建 `v1.0` 附注标签并推送，触发新的 `v*` CI：
  三平台构建 → `release` 作业上传 zip / `.vsix` / `stdlib.dialpkg` →
  `releases/latest` 指向该页面。安装脚本零改动即可生效。

---

## [安装与发布流程调整] — 2026-11-06

**目标**：安装脚本不再硬编码任何版本 / 标签，下载地址改为能重定向到最新 Release；
标准库包与其他资产同处一个 Release 页面，方便重定向、也方便被找到。

### 安装脚本（零硬编码）

- `tools/install-dial.sh` 删除 `DEFAULT_VERSION="1.3.0"`、`STDLIB_RELEASE` 默认
  `stdlib-v1.3.0`；`tools/install-dial.ps1` 删除 `$DialVersion = "0.99.1"`（旧残留）
  与 `$StdlibRelease` 默认值。版本 / 标签 / 资产名全部运行时解析；
- 新增资产解析：调 `api.github.com/repos/<repo>/releases/latest`（钉版本时改调
  `releases/tags/<tag>`）拿到 `tag_name` 与 `assets[].name`，按平台 slug 选 zip、
  按扩展名选 `.vsix`，`stdlib.dialpkg` 名字固定；
- 下载地址统一为 `releases/latest/download/<资产名>`，钉版本时退化为
  `releases/download/<tag>/<资产名>`。GitHub 的 latest 下载**要求精确资产名**，
  而本项目资产名带版本号，故必须由清单解析而非推导——顺带修掉
  `dialc-v1.3-linux-x86_64.zip`（CI 实际命名）与旧脚本推导的
  `dialc-v1.3.0-…` 不一致造成的 404；
- `DIAL_VERSION` / `STDLIB_RELEASE` 降级为**可选钉版本**覆盖；`STDLIB_URL` /
  `DIAL_VSIX_URL` 覆盖、镜像优先 + 官方回退、断点续传、失败可见原因、卸载与
  `.dialpkg` 扫描均不变。API 不可达时给出明确指引，不静默猜资产名。

### 发布流程（同一页面）

- `.github/workflows/ci.yml` 的 `release` 作业新增「组装 stdlib.dialpkg」步骤，
  并把 `dist-stdlib/stdlib.dialpkg` 加入上传资产列表：每个 `v*` Release 现在同时含
  三平台 zip、`.vsix` 与 `stdlib.dialpkg`，**即使 stdlib 源码一行未改也照常重新组装**
  （ABI 与编译器版本绑定）；Release 说明里的安装命令也改为零版本号的一键脚本；
- 旧 `publish-stdlib` 独立通道仅作向后兼容保留，并标记 `prerelease: true`，
  避免它抢走 `releases/latest` 的指向；历史 `stdlib-v1.0.0` Release 及旧链接仍可用；
- README 的安装说明、§25.10、§32.5.1、§32.6 同步。历史分版条目里「独立发版 /
  两个标签」的描述系当时做法，不再代表当前约定。

### 验证

- `bash -n` 语法通过；`--help` / `--version` 输出正常；
- 实测 GitHub API 解析出当前 latest 的 `tag_name` 与 4 个资产名，
  `releases/latest/download/<zip>` 与 `<vsix>` 均返回 **200** 并正确重定向到真实资产；
- `stdlib.dialpkg` 在当前 latest（`v0.98`，旧 Release 未附带该资产）返回 404，
  脚本会给出「该版本发布时未附带 stdlib.dialpkg」的明确告警，符合预期。

> **后续修复（同日）**：上述两个测试作业已由上方「CI 修复与 v1.0 标签重打」
> 章节修好；`v1.0` 标签删除后重打并推送，新的 `v*` Release 已附带
> `stdlib.dialpkg`，`releases/latest` 随之指向该页面，安装脚本无需再改动。
> 本节记录的是修复前的发布流程设计与当时的已知前置。

---

## [v1.03] — 2026-11-05

**回调与可变参数**：一等函数指针 `fn(A, B): R`、同类型变参 `...xs: T`、
`extern fn` 的 C 风格异构变参（FFI 特例），以及接受回调的 stdlib 高阶数组方法
`map` / `filter` / `sort_by`。

三件事互为因果：回调要用函数地址，于是先有函数指针；变参要接住「一批同类型实参」，
于是脱糖成数组；而高阶方法正好是「回调 + 一批元素」的组合，用来验证前两者在真实
调用链里不会引入隐藏开销。

本版继续守住项目铁律：**不引入 GC**；**不引入闭包**（绝不隐式捕获环境）；
**零隐式转换**（函数签名严格匹配、变参元素类型严格一致）；
**零隐藏开销**（变参脱糖成普通 `T[]`，没有类型标签 / 装箱 / 运行期分派）。

### Added

**一等函数指针 `fn(A, B): R`**
- `Type::FnPtr` 底层就是 LLVM `ptr`（纯代码地址）：函数名可直接作右值、可赋值 /
  传参 / 返回 / 存变量；`cb(x)` 走**精确签名**的间接调用（LLVM 18 的
  `build_indirect_call`），参数个数与类型编译期校验；
- **没有闭包**：无环境对象、无 `Fn` / `FnMut` / `FnOnce` 分类、无匿名函数语法；
  回调要带状态必须显式传 `rawptr<void>`，裸指针生命周期由用户自己负责；
- 不受 ARC 管理、无堆分配、**无需释放**：`drop(cb)`、`print(cb)`、
  `with cb as p { }` 都在编译期拦下并给出明确指引；
- `spawn` 首参仍要求顶层具名函数（线程 thunk 需按目标函数静态生成），但函数指针
  可以作为普通回调参数传递；相关诊断与注释里「Dial 没有一等函数」的旧表述已同步。

**同类型变参 `...xs: T`**
- 在 `semantic.rs` 的 `analyze` 开头**做 AST 脱糖**：形参变成 `xs: T[]`，调用点尾部
  实参打包成带元素类型标注的数组字面量；之后的类型检查 / LSP / codegen 只看得见
  一个普通数组；
- **严禁隐藏类型标签 / 装箱 / 运行期分派**；`...xs` 必须是参数列表最后一项，
  不能与默认实参共存，元素类型须能作为数组元素；
- **拒绝异构**：`sum(1, "a")` 编译期报「元素类型不一致」并引导「显式定义结构体或
  自定义类型包」；
- 变参数组是**普通 ARC 数组**：函数形参进入时 retain、退出时 release；调用点的临时
  数组由编译器在调用结束后插入 release（20 万次调用峰值 RSS **1348 KB**，无泄漏）。
  **绝不能**把裸指针的「手动释放」规则套到变参数组上。

**`extern fn` 的 C 风格异构变参（FFI 特例）**
- `.diali` 声明支持末尾 `...`：`extern fn demo_vprintf(fmt: string, ...): int;`，
  LLVM 侧声明为 varargs 函数；
- 固定参数仍做严格类型检查，尾部参数按 C ABI 直传（`bool` 的 i1 会 zext 成 i64，
  `int` / `char` / `byte` 本就是 i64、`float` 本就是 f64）；
- 只属于包 FFI，Dial 自身函数永远走同类型脱糖；无隐藏类型标签 / 装箱。

**stdlib 高阶数组方法（接受回调）**
- `arr.map(f[, state])` / `arr.filter(f[, state])` / `arr.sort_by(cmp[, state])`；
- 回调签名固定带 `rawptr<void>` 状态参数，状态由 C 运行时透传回回调（无闭包）；
- C 侧新增 `dial_array_hof`，只做遍历 / 建数组 / 插入排序；回调经编译器生成的
  **精确签名 thunk** 间接调用（8 字节位模式 ↔ 类型值适配），C 完全不知道回调类型；
- 仅支持标量元素（int / float / bool / byte / char）；`sort_by` 用插入排序而非
  `qsort`，因为后者的比较器拿不到 `fn_raw` / `state`，无法透传状态。

### Fixed

- 修正 `len` 内建的错误提示：数组 / 字符串长度应写 `.length()`，此前误写 `.len()`；
- `ArrayLit` 带期望元素类型时原本跳过元素一致性校验（旧行为对声明标注的空数组无害），
  会让异构变参实参静默塞进数组——改为**无条件逐元素校验**，这是「严禁异构变参」的落点。

### Changed / 发布协同

- `STDLIB_ABI` **3 → 4**（新增 `dial_array_hof`），官方标准库包版本 `1.2.0 → 1.3.0`；
  `make_stdlib_pkg.py`、CI、一键安装脚本（sh / ps1）同步；
- 4 个正向示例（`fnptr_demo` / `varargs_demo` / `callback_state_demo` / `hof_demo`）、
  14 个负向用例；H 组新增 C 变参 `extern fn` 的正向与负向回归；
- `tests/run_tests.sh` A–O 全套通过；版本 / 链接 / 官网检查通过；零编译警告。
- **版本层级**：`[v1.00]` ~ `[v1.03]` 统一归入上方「v1.0 里程碑」大标题下作为分版记录；
  收尾特性至此全部落地，`v1.0` 里程碑标签已打在本次提交上，并随 `main` 一起推送。

---

## [v1.02] — 2026-11-05

**底层控制三件套**：`goto` + 标签、`with` 资源作用域块、裸指针 `rawptr<T>`。

三样互为因果：`goto` 是底层清理的利器，但无限制乱跳会把程序打崩，因此配三条**死锁**；
死锁之一是「不得跳出 with 块」，所以要先有 `with`；而 `with` 要能自动释放裸指针，
于是裸指针必须先落地。

本版继续守住项目铁律：**不引入 GC**；ARC 所有权语义不退化；**零隐式转换**
（裸指针与 ARC 容器之间、裸指针与裸指针之间都**没有**隐式转换）。

### Added

**`with` 资源作用域块**
- 两种等价写法：`with 资源 as 名 { ... }` 与 `with 名 = 资源 { ... }`；
- 块末自动释放：`file` → close、`thread` → join、裸指针 → raw_free；
- 覆盖**全部**出口（正常落穿 / return / break / continue / throw），且互斥地各执行一次
  ——它直接复用既有的 `defer` 机制，而不是另发明一条出口路径；
- 块内已显式 close / join / free 的，块末**跳过**（二次 `fclose` 是 UB）；
- 只管「需要显式释放」的资源：ARC 句柄用 `with` 是多余的，编译期直接拒绝。

**`goto` 与标签**
- `label 名字:` + `goto 名字;`，标签函数内唯一，允许向前 / 向后跳；
- **三条死锁**（编译期硬拦）：
  ① 禁止跨过变量初始化（Zig 式严格，两个方向都禁；向前跳会读到未初始化的变量，
  向后跳会重复初始化并泄漏前一份）。判定用**全局深度优先语句序号**而非同块下标，
  因此「从内层块跳到外层标签、跳过外层声明」也被拦下（实测那是真实的 UB：`print(x)`
  会打印栈上垃圾值）；② 禁止跳出 / 跳进 `with` 块（否则自动释放被绕过
  或重复）；③ 禁止跨越 `try` 边界（try 依赖 setjmp/longjmp 保存栈帧，跨界跳转会毁掉
  jmp_buf，不同 try 块之间互跳同样禁止）；
- 另三条边界一并拦下：目标标签未定义、标签重复定义、**跳进嵌套块**（那里面的变量尚未初始化）；
- 跳到外层块的标签时，被跨越的若干层作用域**照常清理**（defer 按 LIFO 触发、
  ARC 句柄逐个 release）——这正是 goto 作为「底层清理利器」的价值。

**裸指针 `rawptr<T>`**
- 核心红线：**绝对不受 ARC 管理**——编译器不 retain、不在作用域结束时释放；
  `drop(p)` 对裸指针的唯一作用就是 `free`；
- 与 ARC 容器、裸指针之间**都没有隐式转换**：取 `string` 的字节缓冲区必须显式
  `ptr_cast<rawptr<byte>>(s)`，换视图必须显式 `ptr_cast<rawptr<int>>(p)`；
- `ptr_cast` 从 `string` / `bytes` 取到的是**数据缓冲区**的借用（原对象仍受 ARC 管理，
  原对象释放后该指针即悬垂——使用者的责任）；
- 指针算术**显式且带类型**：只有 `p.offset(n)`，步长由元素类型决定；
  `rawptr<void>` 无元素大小，**禁止**算术（须先 `ptr_cast`）；
- 解引用宽度按元素：`byte` / `bool` 1 字节、`char` 4 字节、`int` / `float` / 句柄 8 字节；
- `&x` 只允许对**可变的栈上值类型**变量取址：对 `let` 取址会绕过不可变约束 → 拒绝；
  对堆对象取址拿到的只是槽位地址 → 拒绝；
- 配套运行时：`dial_raw_alloc`（分配失败 Fail-Fast，绝不返回 NULL）/ `dial_raw_free`。

**包的不透明句柄（`Type::Opaque`）补齐文档与诊断**
- 沿用既有 `type Db;`（`.diali` 声明），**不新增语法、不新增运行时**；
- 明确契约：与裸指针同属「不受 ARC 管束」的一类——编译器不 retain / release、
  作用域结束不释放，必须由包自己的 `close` / `free` 手动释放；与 ARC 容器之间
  无隐式转换；不可打印；与 `rawptr<T>` 的区别写清（具名无布局 / 带元素类型可解引用）；
- **`drop(db)` 由「静默 no-op + 误标变量已转移」改为编译期报错并给出指引**
  （此前 `drop` 对不透明句柄什么都不做，却会禁用该变量，容易让人误以为资源已归还）；
- 作为数组 / 通道元素时按 RC 类别 retain / release，文档明确「包作者须保证句柄
  布局与 `dial_rc_*` 兼容」这一责任边界。

### ⚠ 破坏性变更（标准库 ABI 递增）

- **`stdlib` 包 ABI 由 2 递增到 3**（`STDLIB_ABI`）。原因：标准库新增
  `stdlib/dial_raw.c`（`dial_raw_alloc` / `dial_raw_free`），供裸指针的
  `raw_alloc` / `raw_free` / `drop(p)` / `with` 自动释放使用。与 v1.01 不同，
  这两个符号是**条件引用**（只有用户代码用到裸指针时才产生调用），但装着 abi=2
  旧包的用户只要写了裸指针程序，仍会撞上难懂的
  `undefined symbol: dial_raw_alloc`。判据是「**编译器是否可能要求旧包没有的符号**」，
  因此同样递增——ABI 校验这道安全网不能因为引用是条件性的就放过旧包。
  `build_stdlib.sh` 与 LTO 编译路径都用 `*.c` / `read_dir` 收集源文件，
  `dial_raw.c` 会**自动**进入静态库与官方包，无需手工登记清单。
- 配套同步：官方标准库包版本 `1.1.0 → 1.2.0`，打包默认 `--abi 3`，
  CI 发布作业 `--abi 3`，一键安装脚本的 `STDLIB_RELEASE` 默认标签
  `stdlib-v1.1.0 → stdlib-v1.2.0`（标准库仍**独立发版**，tag `stdlib-v*`）。
- 发布协同项：v1.01 ~ v1.03 的提交已随 `main` 一并推送；按版本号的发布标签
  （v1.03 起为 `v1.3.0` 编译器与 `stdlib-v1.3.0` 标准库包，见上方 v1.03 条目）
  由维护者另行决定何时打；`v1.0` 里程碑标签已在 `[v1.03]` 之后打上，见文件开头说明。

### Fixed

- **`break` / `continue` 未释放被跳出的作用域**（既有缺陷）：外层臂的 `release_scope_locals`
  位于落穿路径上，而 break/continue 恰恰走不到那条路径——循环体的局部因此永远不释放。
  实测 `continue` 循环 20 万轮（每轮一个 `string`）峰值 **13892 KB**，修正后 **1620 KB**。
  修法与 goto 同源：跳转前补发一份「被跳出作用域」的释放代码（两条路径互斥，安全）。

### 测试

- 3 个新增正向示例 + 同名 `.expected` 黄金文件：`goto_demo`、`with_demo`、`rawptr_demo`；
- 12 个新增负向用例：跨初始化（同块 + 跨块）/ 跳出 with / 跨 try / 跳进嵌套块 /
  未定义标签 / 对 `let` 取址 / 解引用非指针 / `rawptr<void>` 算术 / `free` 非指针 /
  `with` 非资源 / 裸指针隐式转换；
- `bash tests/run_tests.sh` A–O 全套通过；H 组另加一条回归：`drop(不透明句柄)`
  必须报错并给出「不受 ARC 管理、请手动释放」的指引。

---

## [v1.01] — 2026-11-05

**盘点驱动版本**：v1.00 补齐语言特性后，没有继续「凭想象加特性」，而是**照着程序员
写真实程序的方式实测编译器**：写 30 多个真实小程序，逐个看哪些能直接写、哪些要绕道、
哪些只能模拟、哪些根本写不出来。盘出的缺口在这一版一次性补齐。

本版继续守住项目铁律：不引入 GC；不提供裸指针 / 地址取值 / 指针算术；
不支持运算符重载；不支持一等函数 / 闭包；ARC 所有权语义不退化；
**零隐式转换铁律保持**（`1 + 2.0` 仍编译失败）。
（后续边界变更：「不提供裸指针」已在 **v1.02** 解除，「不支持一等函数」中的
**函数指针**已在 **v1.03** 解除；**闭包至今仍不提供**，见文件开头里程碑说明。）

### Added

**数组元素类型扩展**
- `float[]` / `byte[]` / `bool[]` / `char[]` 现在都是合法的动态数组元素；
- 底层沿用 `int64` 槽位：`float` 存 **IEEE754 位模式**（bitcast，绝不 `fptosi`
  截断小数——否则 `3.25` 会被写成 `3`）、`bool` 走 `i1 ↔ i64`、
  `byte` / `char` 与 `int` 同为 `i64`；句柄元素仍按元素 ARC 管理；
- 零隐式转换不松动：`float[]` 里 `push(3)`（int 字面量）仍然编译失败。

**空数组字面量 `[]`**
- `var a: int[] = [];` 合法：元素类型由变量标注提供；
- 裸写 `var a = [];` 仍编译失败——编译器不替你猜。

**数组删除元素**
- `a.remove(i)`：删除下标处元素并返回它；越界当场 Fail-Fast；
- `a.pop()`：弹出末元素并返回它；空数组当场 Fail-Fast；
- 数组自身的变化（COW 可能更换指针）由编译器写回接收变量。

**`drop()`：显式提前释放**
- 句柄类（`string` / `bytes` / `array` / `map` / `map<K,V>` / `set` / `error` /
  `channel`）立即调用对应 `*_release`；`file` 走 `dial_file_close`；
  `thread` 走 `dial_thread_join`；标量与结构体是 **no-op**（值类型无所有权）；
- 句柄被 `drop` 后进入「已转移」状态：此后任何使用都在**编译期**被 use-after-move
  诊断拦下，杜绝 use-after-free；标量 drop **不**标记 moved（值类型，drop 后仍可用）。

**字符串方法**
- `s.split(sep) -> string[]`：按分隔符切分（空分隔符整串为唯一元素）；
- `s.replace(from, to) -> string`：替换全部出现，返回新串（原串不变）；
- `s.trim() -> string`：去首尾空白；
- `s.index_of(sub) -> int`：首次出现的字节偏移，未找到返回 `-1`。

**环境 / 时间 / 随机内建**
- `args() -> string[]`：命令行参数（`argv[1..]`），每次调用返回独立副本；
- `now() -> float`：Unix 纪元起的秒数（含小数）；
- `sleep(ms)`：休眠毫秒；`random() -> int`：`[0, 2^63)` 伪随机整数；
- 为拿到 `argv`，编译器生成的 `main` 入口改为 `int main(int, char**)` 并在入口
  调用 `dial_env_init(argc, argv)`——对用户代码完全透明。

**泛型 `map<K,V>` 支持 `for (k, v in m)`**
- v1.00 时泛型 map 只能 `keys()` / `values()` 拿快照再绕路迭代；
- 现在直接可写，且**键 / 值按真实类型绑定**（`map<string,int>` 的 `k` 是 `string`）；
- 所有权口径：循环里的 `k` / `v` 均为**借来**的（键借自键快照、值借自表本身，
  都未 `retain`），故循环末尾与函数出口**不释放**它们，避免重复释放表内元素。

**数组排序 `sort()`**
- `a.sort()`：原地升序排序，遵守 COW 契约（被共享时先克隆，不影响其他持有者）；
- 只支持可比较的标量元素（`int` / `float` / `byte` / `char` / `bool`），
  `string[]` 等句柄元素编译期拒绝；
- `float` 按 **IEEE754 数值序**比较（负数正确排在最前），绝非位模式比较。

### Changed

- `main` 的 LLVM 签名由 `i32 main()` 改为 `i32 main(i64, i8*)`（接收 `argc` / `argv`）。
  这是 C ABI 层面透明的变化，既有程序行为不变。

### ⚠ 破坏性变更（标准库 ABI 递增）

- **`stdlib` 包 ABI 由 1 递增到 2**（`STDLIB_ABI`）。原因：标准库新增
  `dial_env.c`（`dial_env_init` / `dial_args` / `dial_now` / `dial_random` /
  `dial_sleep`）与 `dial_array_sort` 等导出符号，且编译器生成的 `main` 入口会
  **调用** `dial_env_init`——旧标准库包（abi=1）链接新编译器必然报出难懂的
  `undefined symbol: dial_env_init`。递增 ABI 后，旧包在**链接期**就被明确拦下，
  并给出「卸载旧包 → 重装匹配版本」的重装指引，而不是让用户去追一个符号错误。
- 配套同步：官方标准库包版本 `1.0.0 → 1.1.0`，一键安装脚本的
  `STDLIB_RELEASE` 默认标签 `stdlib-v1.0.0 → stdlib-v1.1.0`
  （标准库仍**独立发版**，tag `stdlib-v*`）。
- 发布协同项：本次发布需要**两个标签**——`v1.1.0`（编译器）与
  `stdlib-v1.1.0`（标准库包）。

### 测试

- 6 个新增正向示例 + 同名 `.expected` 黄金文件：`array_demo`、`drop_demo`、
  `string_methods_demo`、`env_demo`、`kvmap_foreach_demo`、`sort_demo`；
- 5 个新增负向用例：`array_empty_no_annot`、`array_push_wrong_elem`、
  `drop_use_after_move`、`string_split_bad_arg`、`array_sort_handle_elem`；
- 移除 2 个已过时的负向用例（`array_bool_elem` / `array_char_elem`：这两种元素类型
  在本版变为合法，原用例的前提已不成立）；
- `bash tests/run_tests.sh` A–O 全套通过。

### 盘点数据

- 提前释放大数组（三阶段、每阶段 200 万元素）：同一作用域全存活时峰值
  **57160 KB**；改用 `drop()` 显式释放后峰值 **33344 KB（-41.7%）**，
  与「嵌套块绕道」的 33460 KB 持平，却不必再套块、也不影响阶段间传数据。

---

## [v1.00] — 2026-11-04

**里程碑版本**：从 `v0.99.1` 直接跳到 `v1.00`（用户明确要求跳版，见 §23 第 3 条）。
本版是**语言特性补齐**发布：把「正常程序需要、此前只能模拟或绕路」的能力一次性补齐，共五组。
所有改动都守住项目铁律（v1.00 当时）：**不引入 GC**、不提供裸指针 / 地址取值 / 指针算术、
不支持运算符重载、不支持一等函数 / 闭包 / 函数指针、ARC 所有权语义不退化、
**零隐式转换铁律保持**（`1 + 2.0` 仍编译失败）。

> 后续边界变更：「不提供裸指针」已在 **v1.02** 解除，「不支持函数指针」已在
> **v1.03** 解除；**闭包至今仍不提供**。见文件开头「v1.0 里程碑」说明。

### Added（五组语言特性）

**① 运算符与字面量**
- 复合赋值 `+= -= *= /= %=`、自增自减 `++` / `--`（编译期脱糖为普通赋值）；
- 位运算 `& | ^ ~ << >>`（仅 `int`）；
- 进制字面量 `0x` / `0b` / `0o`、数字分隔符 `_`；
- 三元表达式 `cond ? a : b`（`cond` 必须是 `bool`，两分支类型必须相同）。
- 正向示例：`examples/op_demo.dial`。

**② 迭代与控制流**
- `for (i in a..b step s)` 步长循环（`step` 必须是 `int` 且为正）；
- `for (v in arr)` 数组遍历、`for (k, v in m)` 哈希表遍历、`for (v in s)` 集合遍历
  （集合遍历编译期自动插入键集快照并纳入函数出口释放）；
- `match` 模式匹配（int / bool / char / string 模式，必须有 `_` 通配分支）；
- `defer` 延迟语句（LIFO，覆盖正常落穿 / `return` / `break` / `continue` / `throw`）；
- `do { ... } while (cond);`。
- 正向示例：`examples/control_demo.dial`、`defer_demo.dial`、`control_edge_demo.dial`。

**③ 函数与内建**
- 默认参数（带默认值的形参必须连续位于末尾；在调用点补齐，不改变 LLVM 函数签名）；
- `print` / `print_raw` 改为 variadic；
- 内建 `assert` / `panic` / `exit` / `min` / `max` / `abs` / `sqrt` / `pow` /
  `floor` / `ceil` / `round`（浮点数学函数最终链接加入 `-lm`）。
- 正向示例：`examples/builtin_demo.dial`。

**④ 表达力增强**
- 字符串插值 `"n=${n}"`（逐段转字符串 + concat；插值表达式只允许可转字符串的类型）；
- 文件级常量 `const NAME: T = 字面量;`（编译期内联，禁止遮蔽与重新赋值）。
- 正向示例：`examples/interp_const_demo.dial`。

**⑤ 重型类型与容器**
- `enum` int-backed 名义类型（变体 `Name.Variant` 编译期内联为整数字面量，默认值 0 起递增，可显式指定）；
- 数组切片 `arr[from..to]`（半开区间；句柄元素逐个 retain，与原数组 ARC 独立）；
- `set<int>` 集合（`new_set()` / `add` / `contains` / `remove` / `size` / `values`）；
- **泛型 `map<K,V>`**（独立 C 运行时 `stdlib/dial_kvmap.c`）：
  - 键支持 `int` / `float` / `bool` / `char` / `byte` / `string`（string 键按条目 ARC 管理）；
  - 值支持标量 `int` / `float` / `bool` / `char` / `byte`（float 以 IEEE754 位模式存储，绝不丢小数）；
  - 方法 `put` / `get` / `contains` / `remove` / `size` / `keys` / `values`；
  - COW 克隆、扩容重哈希、墓碑法删除，全部逐条目 retain / release。
- 正向示例：`examples/container_demo.dial`、`set_demo.dial`、`kvmap_demo.dial`。

**已知边界**：泛型 map 的 `V` 暂不开放 `string`（`get` 未命中时的默认对象会带来所有权移交歧义）；
`map.values()` 当前只支持 `V = int`。两点计划在后续版本补齐。

### Fixed
- **开放寻址哈希表删除采用「左移紧凑」导致键不可达**：`dial_map_remove` 与新
  `dial_kvmap_remove` 改为**墓碑标记 + 负载重建**。2 万键压测里删除 1 万个后 `size`
  曾只剩 13082（应为 10000），修复后正确；`set<int>` 与 `for (k, v in m)` 同时受益。
- `panic` 前 stdout 缓冲内容丢失：`dial_panic()` 增加 `fflush(NULL)`。

### Changed
- `print` 中间参数改为原样输出、最后一个参数补换行；`print_raw` 全部参数原样输出。
- trap 接口补齐 `dial_assert_fail` / `dial_exit` / `dial_panic`；`dial_map_keys()` 供迭代使用。
- LSP 语义 token / 固定文本 / 符号索引补齐所有新增关键字、运算符与语句变体。

### Testing
- 新增 8 个正向示例 + 黄金文件、30+ 个负向用例（`tests/bad/`）锁定语义边界；
- `bash tests/run_tests.sh` A–O 全套通过；
- 2 万键 ARC 压测（泛型 map / set）峰值内存约 4.4 MB / 2.4 MB，无泄漏、无 Fail-Fast。

---

## [v0.99.1] — 2026-11-03

本版是 **v0.99 的修复发布（补丁位）**：只修缺陷，不含任何新特性，
所有基于 v0.99 的程序**无需改动即可运行**（行为完全一致）。

### Fixed（修复）

- **`a[i] = v` 下标赋值在 COW 守卫上误判，导致每次赋值都深拷贝整个数组**：
  数组运行时在「独占」路径（`refcount == 1`）上**多做了 `retain`**，又在「共享」路径
  （`refcount > 1`）上**漏掉了对旧 buffer 的 `release`**——与 `dial_map_put` 已有的
  正确写法（独占时原样移交所有权、共享时克隆并释放旧引用）相悖。
  于是每写一次下标赋值引用计数就 +1，COW 守卫 `refcount > 1` 从此**恒真**：
  **每一次 `a[i] = v` 都深拷贝整个数组**。
  - 后果：`a[i] = v` 循环 N 次即需 **O(N²)** 内存。实测 100 万次写需约 **8 TB**，
    进程被 **OOM Killed**（`sys` 时间高达 3 秒）；同一程序修复后峰值约 **10 MB**、
    耗时 0.01 秒。
  - 根因定位手段：在 `dial_array_set` 入口用 gdb 读 `((int64_t*)a)[-1]`，
    可见引用计数在 `2 → 1 → 2 → 1 → 2` 之间震荡（数组明明由槽位独占持有）。
  - 修复：`dial_array_set` 与 `grow`（`push` 路径）改为与 `dial_map_put` 完全同款的
    所有权交接——独占时原样返回、**不 retain 不 release**；共享时克隆并 `release` 旧 buffer。
  - 附带修复：覆盖槽位时改为**先 `retain` 新元素、再 `release` 旧元素**。
    原顺序在「旧元素 == 新元素」（如 `a[0] = a[0]`）时会把计数打到 0（`free`），
    紧接着又对已释放内存 `retain` —— use-after-free。
- **`dial_array_clone` 在独占数组上泄漏元素的引用计数**：旧实现对「与原数组同一块
  buffer 的别名」也调用 `retain_elems`，等于把同一批元素的引用计数凭空 +1，
  而数组释放时只释放一次——元素引用计数净多 1，永久泄漏。
  现 `clone()` 一律产出**独立副本**（先 `retain` 迫使深拷贝，再抵消该次 `retain`）。
- **嵌套下标赋值 `m[i][j] = v` 的所有权结算错位**：内层数组是从外层数组元素槽
  **借来**的指针，其那一份引用属于外层数组；若内层 `set` 按「消耗引用」的语义
  处理，会与外层的元素替换重复释放同一份引用。现新增 `dial_array_set_borrowed`
  （不消耗被借的引用，改为交出「一份新拥有」的引用），并在 codegen 中按
  「消费者释放、变量槽转移」的规则结算。
- **`dial_rc_clone` 恢复纯粹语义**：唯一持有者时原样返回、**不再 +1**；
  多持有者时深拷贝。过去那个 `+1` 让 `clone()` 只能靠「别名 + 多一个计数」实现，
  是元素引用计数泄漏与 COW 守卫误判的共同源头。
  配套地，`dial_string_clone` / `dial_bytes_clone` 改为强制深拷贝产出独立副本
  （否则纯语义下 `b = s.clone()` 会让 `b` 悬垂）。

- **误拒合法程序 + 三处堆悬垂 + `bytes[]` 被拒 + 两处安全缺陷**（本轮一并修复）：
  - **① `select` 分支里 `move_to` 外层变量被误报「已转移」**：`check_moved_stmt` 的
    `Stmt::Select` 分支既不做「只查 `arm.chan`」的纪律，反而又对 `arm.body` /
    `default_body` 递归了一遍 `check_moved_stmt`，与 `check_stmt` 的递归重复——
    同一条 `move_to` 被检查两次，第二次必然报「变量已转移」。现删掉分支内的递归，
    只保留对 `arm.chan` 的 `check_moved_expr`（分支体仍由 `check_stmt` 递归各自检查，
    恰好一次）。
  - **③ 结构体字面量 / 字段读取 / 字段赋值的堆字段未 `retain`**：`Expr::StructLit`、
    字段读取（`t = p.name`）、字段赋值（`p.name = s`）在写堆类型字段时只 `store` 不
    `retain`，字段于是成了来源的**别名**；来源出作用域被 `release` 后字段即悬垂
    （use-after-free，读到的长度字段可能是垃圾，打印出成片乱码）。现统一引入
    `retain_if_borrowed`：借来的值（变量 / 字段 / 数组元素 / `get`·`at`）`retain` 一份，
    自带所有权的值（字面量 / `clone` 等）直接接管。
  - **③ 的同族泄漏**：`a[i] = <字面量>`、`a.push(<字面量>)`、`a.set(i, <字面量>)`、
    数组字面量元素在循环里逐次泄漏一份引用（容器 `retain` 了元素，字面量自带的
    那一份所有权却没移交）。现引入 `settle_fresh_value`：自带所有权的值在容器接管后
    归还其临时所有权，借来的值则不释放——嵌套下标赋值 `m[i][j] = v` 也按同一规则结算。
  - **④ `bytes[]` 被 `is_array_elem_type` 拒绝**：语义层漏收 `Type::Bytes`，但 codegen
    的 `array_elem_kind` 早已把 `Bytes` 归为 kind=1（RC），两层不一致。现补上
    `Type::Bytes`（`dial_bytes_retain` / `release` 内部即 `dial_rc_*`，完全兼容）。
  - **⑤ IR 验证失败不中止编译**：`module.verify()` 失败只 `eprintln` 后继续
    `emit_object`，可能产出无效 IR 或带未定义行为的机器码。现改为直接以易读文字
    `return Err(...)` 中止。
  - **⑥ `curl --proto '=https,http'` 与「只允许 https」声明矛盾**：https URL 可被
    302 重定向到 http，中间人攻击窗口重新打开。现 `--proto` 仅放行 https（本机地址
    经 `http_allowed` 判定才加 http），并新增 `--proto-redir` 限制重定向协议。

### Added（新增）

- **测试组 N：内存上界 + 所有权语义回归**（`tests/mem/`）。
  - `array_cow_bench.dial`：100 万次下标赋值的基准，在 **128 MB 虚拟内存上限**下运行——
    一旦 COW 误判复发（需 TB 级内存）立刻被上限打爆。
  - `array_cow_semantics.dial`：逐条钉死 `clone` 独立 / COW 隔离 / 嵌套赋值 /
    句柄元素数组四条所有权语义。
- **测试组 O：所有权 / 语义回归**（`tests/sem/`）：钉死「编译器应接受并正确运行」的合法
  程序——`select` 里 `move_to` 外层变量（`select_move_to`）、`StructLit` / 字段读取 /
  字段赋值的堆字段 retain（`structlit_heap_field` / `field_and_assignfield`）、
  `bytes[]` 不被拒（`bytes_array`）、借来值写入容器不重复释放（`borrowed_assign`）。
- **一键安装脚本彻底重写**（`tools/install-dial.sh` / `install-dial.ps1`）：
  - 版本**不再硬编码**：优先从已装好的 `dialc` 读出真实版本号，bootstrap 才回退到内置
    默认；配套件（标准库 / 插件）一律按该版本推导——**消灭版本不同步**。
  - VS Code 插件按 **Release 资产清单**解析真实文件名，不再依赖插件自身 semver 与
    编译器版本号对齐（旧版写死 `dial-0.98.1.vsix`，每次都 404）。
  - 下载失败**可见真实原因**（旧版 `2>/dev/null` 把 curl 报错吞了）。
  - `install-dial.sh` 支持 bash / zsh / fish / profile 四种 shell 的 PATH 持久化，并提示
    当前会话用法；新增 `--help` / `--version` / `--uninstall`，解压前校验压缩包完整性，
    重跑幂等。

### Changed（变更）

- README §11.2 / §11.3：补齐「独占路径必须原样移交所有权、一个引用计数都不能动」
  这条不变量（含做错的后果对照表），并把 `clone()` 的描述从「COW 感知克隆」
  更正为「独立副本（深拷贝）」。

---

## [v0.99] — 2026-11-03

本版是 **v0.98 线的功能发布（`+0.01`，非补丁）**：数组从「只能装 `int`」扩展到
「`int` 或任意 8 字节句柄」，并支持多维数组与嵌套下标赋值。所有基于 v0.98 的程序
**无需改动即可运行**；用到新能力的程序获得了更大的表达力。

> 说明：本版同时正式收纳此前以「v0.98.2（规划）」叙述的连续输出三件套
>（`print_raw` / `write_char` / `str(char)`）、`byte` 单字节值类型与 `bytes` 二进制缓冲区、
> 裸块 `{ ... }` 词法作用域——它们随本版一并发布，文档锚点已统一为 v0.99。

### Added（新增）
- **多维数组 `int[][]`**：内层 `int[]` 作为 8 字节句柄直接存储，支持嵌套读 / 写 / 遍历 / `push`。
- **句柄元素数组** `string[]` / `map[]` / `error[]` / `channel[]` / 不透明指针 `[]`：
  数组头部新增 `kind` 字段（`INT` / `RC` / `ATOMIC`），按元素 ARC 复用
  `dial_rc_*` 与 `dial_channel_*`，正确 `retain` / `release` 元素所有权。
- **嵌套下标赋值 `a[i][j] = v`**：编译器新增递归左值解析 `gen_assign_lvalue`，
  逐层 `gen_expr` 取出内层数组句柄 → `dial_array_set` 写入 → 写回可能 COW 更新的外层句柄，
  直到最外层变量槽。单层 `a[i] = v` 路径不受影响。
- 新增示例 `examples/multi_dim_demo.dial`、`examples/string_array_demo.dial`、
  `examples/channel_array.dial` 及对应黄金文件（纳入 A 组回归）。
- 新增负向用例 `tests/bad/array_bool_elem.dial` / `array_byte_elem.dial` / `array_char_elem.dial`，
  锁定 `float[]` / `bool[]` / `byte[]` / `char[]` 等标量非 int 元素仍被编译期拒绝。

### Changed（变更）
- README §5.1/§5.2/§11/§11.6/§17.2 与官网 `spec.html` 同步：数组元素类型扩展到
  `int` 与 8 字节句柄，多维数组与嵌套下标赋值正式从「未实现」移除。

---

## [v0.98.1] — 2026-11-02

本版是 **v0.98 的修复发布（补丁位）**：只修缺陷与文档矛盾，**不含任何新特性**，
因此所有基于 v0.98 的程序**无需改动即可运行**（行为完全一致）。

### Fixed（修复）
- **裸块作用域 `{ ... }` 从「语法能过、语义/索引崩溃」修到完整可用**：
  `Stmt::Block` 的 AST 变体在定义与模式匹配两侧不一致（元组 vs 具名结构体），
  导致**整个编译器无法编译**。现已统一为具名变体 `{ body, line, col }`，
  词法/语法/语义/索引/代码生成五处同步打通，裸块可正常引入词法作用域。
- **README §17.2 与 §28 对 LSP 能力的结论自相矛盾**：§17.2 仍写「重命名 / 引用查找 / 格式化
  **未实现**」，而 §28 记载这三项已在 v0.98 轮 2 / 轮 3 落地。现按实现统一口径，
  §17.2 改为「已实现」并指向 §28 对应小节。
- **README §3.2 的交叉引用指错节**：注释里的「见 §3.1.2 / §3.1.3」实际指向
  Windows x64 与 Termux aarch64，而 Linux 的 LLVM 依赖在 **§3.1.1**、
  动态库要求（`LD_LIBRARY_PATH`）在 **§3.1.5**。现改为正确节号。
- **README §2 与官网 spec.html §2 的目录结构里 `install-dial.ps1` 重复两行**
  （第二行还少了「支持 irm | iex 一键命令」的说明）。现删去重复行。
- **README §28 标题停在 v0.95**：正文已含 v0.98 的四轮工业化与三编辑器接入，标题与内容脱节。
  现改为覆盖到 v0.98。
- **spec.html §28.2 与 §28.7.1 对 `textDocumentSync` 的说法直接冲突**：
  一处写「声明 `textDocumentSync = 1`（全量同步）」，另一处写「必须用选项形式
  `{openClose, change: 1, save}`，不能用纯数字 1」。后者才是对的
  （纯数字形式没有 `save` 字段，客户端不会发 `didSave`，诊断刷新会静默失效）。
  现按后者统一。
- **CHANGELOG 缺 v0.98.1 条目**：v0.98.1 已实际发布（tag `v0.98.1`），
  但本文件顶部仍是 `[v0.98]`。现补上本条目。
- **README §23 的版本号规则与 v0.98.1 冲突**：原规则写死「最多两位小数、
  不存在三位小数的版本」，而 v0.98.1 是既成事实。现补充第 6 条，
  明确补丁位**仅**用于修复发布（只修缺陷、不加新特性），并规定其边界条件。
- **`cve.html` 的 CVE 编号年份穿越**：项目日期是 2026，编号却写成 `CVE-2024-DIAL-001`。
  现统一为 2026 年，并说明这是**项目自建的跟踪编号**（非 MITRE 分配的正式 CVE 编号）。
- **README §18 的 CI 关卡表缺 `cve_check` 一关**，而 `cve.html` 声称
  「已在 CI 工作流中接入，作为发布前的强制门禁」。现已核对 CI 工作流确实包含该步骤，
  §18 的表格补上这一行——文档与实际流水线对齐。

### Added（新增）
- **`print_raw(s)`**：原样输出字符串、**不追加换行**。此前 `print(s)` 永远在末尾补 `\n`，
  导致**任何需要连续输出的程序都写不出来**（拼行表格、进度条、逐字动画、协议帧）。
- **`write_char(c)`**：输出单个字符、**不追加换行**。与 `print(char)` 的唯一差别就是这个换行符。
- **`str(char)`**：`char` 纳入 `str()` 的支持类型，得到 1 字符的 UTF-8 字符串，
  可直接参与拼接（`str(c) + "!"`）。
- **`dialc version` / `dialc --version` / `dialc -V`**：显示版本号。
  版本取自 `Cargo.toml`（编译期注入），与全站唯一真相一致。

### Changed（变更）
- **`print(char)` 的换行语义被写进文档**：明确 `print(char)` **输出字符本身并追加一个 `\n`**
  （行式输出），而 `write_char(char)` 不追加。此前二者的差别只存在于实现里、没写进文档，
  读者无法确定该用哪个。

---

## [v0.98] — 2026-11-02

本版把 `dialc lsp` 从「骨架 + 基础增强」打磨到**工业级可用**，并补齐三套主流编辑器的接入与安装包。

### Changed（变更）
- **`--fast`（ReleaseFast）不再是「只关检查」**：在关闭运行期检查之外，现在开启三层编译器终极优化，
  让 `--fast` 真正名副其实（此前它只关掉 Fail-Fast 检查，机器码仍是普通 O2）。
  - **① 自动向量化 (SIMD) + 循环展开**：LLVM 切到 `OptimizationLevel::Aggressive`（O3），
    并按宿主实测指令集挑对应的 x86-64 微架构级别（`x86-64-v2/v3/v4` → SSE4.2 / AVX2 / AVX-512，
    ARM64 显式 `+neon`），一次直接吃 4~8 个 int64；循环展开器同时压低分支预测开销。
  - **② LTO 链接时优化**：`--fast` 现在把 inkwell 产出的 LLVM bitcode 经 `clang -flto -O3` 编成
    含 LTO bitcode 的「胖对象」，C 运行时也逐文件以 `-flto` 编译，链接时由 clang 整体重排——
    `dial_array_get`/`push` 等热函数被直接内联进主程序，热循环零 `call` 开销（反汇编可证）。
  - **③ PGO 剖析引导优化（新增 `--pgo`）**：先插桩跑一遍训练样本、合并 profile，再据热点重排指令
    布局（热路径顶到 I-Cache 最前）。`--pgo` 必须配合 `--fast`（需本机 `clang` 支持）。
  - 三者都需要本机 `clang`；若 `DIAL_C_COMPILER` 指向非 clang，则优雅降级为「仅 O3 + SIMD」，
    保留 SIMD 收益、放弃跨模块 LTO/PGO。生成的二进制按本机 CPU 调优，仅供本机运行。
- 新增性能基准示例 [`examples/bench_array.dial`](examples/bench_array.dial)（1000 万次 push + 遍历求和），
  作为上述三层优化的「标准考题」。

### Added（新增）
- **编辑器插件安装包（三主流）**：编译型语言的编辑器支持是必做的苦活，本版落地三套。
  - `editors/vscode/`：**手写 LSP 客户端**（无 npm 依赖），`build.sh` 打包成 `dial-*.vsix`
    （`code --install-extension` 侧载；CI 在 `v*` 发版时现构建并附到 Release）。
  - `editors/neovim/`：零依赖，只用 Neovim 内置 LSP 客户端（`ftdetect/` + `plugin/`，拷进 `runtimepath`）。
  - `editors/emacs/`：**不开发插件**——用 Emacs 内置 `eglot` 原生连 LSP，仅提供 `init.el` 片段。
  - 其余编辑器（JetBrains / Zed / Sublime 等）只要能启动 `dialc lsp` 即可接入，不再单独维护插件。
  - `editors/README.md`：三编辑器总览与能力矩阵。

### Changed（变更）
- **LSP 能力协商**：`initialize` 读取客户端 `capabilities`，按声明动态广播能力（老客户端不报能力也不报错）；
  并新增 `window/logMessage` 自诊断通道（不污染协议流、不进 stdout）。
- **一键安装脚本自动补齐配套件**：除编译器、`stdlib/`+`runtime/`、`.dialpkg` 外，
  现在还会**自动下载并安装 VS Code 插件**（从 Release 取 `dial-<版本>.vsix`，
  再用 `code --install-extension` 装上），不必再自己去 Release 页翻资产、手动侧载。
  - 版本只从 `DIAL_VERSION` 一处推导（Release 标签 `v0.98.1` 与资产名 `dial-0.98.1.vsix`
    都由它算出），不会出现三处各说各话、下次发版又忘了改其中一个。
  - 没装 VS Code / 离线时**只告警、不阻断**安装（编译器与包安装不受影响）。
  - **不加 `--force`**：用户若已装了更新版插件，不该在「一键安装」里被悄悄降级。
  - 下载 URL 可用 `DIAL_VSIX_URL` 覆盖（自建镜像 / 网络受限时用）。
  - Neovim / Emacs 侧无需安装动作（零依赖配置），故不动。
- **一键安装脚本不再「一键卡住」**：下载标准库与插件的 curl 补上
  `--connect-timeout 15 --max-time`。此前没有超时——GitHub 不可达时 curl 会一直挂着，
  用户看到的是脚本**不动**，而不是一句清晰的失败提示。一键安装卡死比装不上更让人手足无措。
- **VS Code 插件接通轮 2~4 的全部能力**：文档大纲、引用查找、重命名（含 prepareRename）、签名帮助、
  保守格式化、参数 inlay 提示、代码动作、折叠范围、选择范围、文档链接——插件不再只转发三个 provider。
- **Neovim 插件补上新能力的键位**：`<leader>f` 格式化、`<leader>a` 代码动作、`<leader>ds` 文档大纲，
  并启用 Neovim 0.10+ 的 `foldingRange` 客户端（0.9 用 `pcall` 兜住）。
- **补全增强**：补全项带 `textEdit` 精确替换范围（修正旧实现「名字重复拼在后面」的 bug）+ `sortText`
  排序（函数/外部函数最前、结构体/不透明类型次之、变量/形参最后）+ `.` 触发字符。
- **VS Code 插件接通 hover / 跳转定义 / 补全**：原扩展仅接了诊断与语义高亮，现补齐三个 provider 转发到
  `dialc lsp`；`initialize` 发送真实 `capabilities`；`package.json` 升 `0.98.0`。
- 文档同步：`README §28.7`（编辑器插件安装包与 LSP 能力清单）、`ARCHITECTURE`、`CHANGELOG` 更新；
  各编辑器 README 与 `web/` 重新生成。

### 轮 2（导航，已完成）
- **LSP 导航全家桶**：`src/index.rs` 新增**引用边**（`Ref`），`dialc lsp` 落地文档符号大纲、工作区符号、引用查找、重命名（含 `prepareRename`）、签名帮助。
  - 文档符号：顶层 fn/struct 为根，函数体形参与局部变量递归为子节点（`DocumentSymbol`）。
  - 工作区符号：跨所有已打开文件 + 已加载包，按名字子串模糊搜索。
  - 引用 / 重命名：基于引用边，同名遮蔽天然正确（内层同名变量不会被误并入外层引用）；包符号（`.diali`）只读不重命名。
  - 签名帮助：`(` / `,` 触发，解析光标前函数名返回完整签名与各参数标签、活动参数序号。
- 冒烟测试 `tools/lsp_full_smoke.py` 扩展到轮 2 全部能力断言（能力协商 / 大纲 / 工作区符号 / 引用 / 重命名 / 签名）。

### 轮 3（编写辅助，已完成）
- **保守格式化**（`textDocument/formatting`）：只做两件事——去除每行行尾空白、确保文件末尾恰有一行换行；
  **绝不**做重排/换行的激进格式化（避免误伤对齐敏感代码，且 Dial 无官定代码风格）。返回覆盖全文的单个 `TextEdit`。
- **参数 inlay 提示**（`textDocument/inlayHint`）：在函数调用实参前标注形参名（如 `a: ` / `b: `）。
  按括号深度配对调用、记录实参起始位置，再按已索引的签名给出形参名；`fn name(` 的声明参数列表不误判为调用。
  索引独立于语义、半成品代码拿不到类型，故只做形参名、不做类型 inlay。
- 冒烟测试 `tools/lsp_full_smoke.py` 扩展到轮 3（格式化 / inlay 断言均已通过）。

### 轮 4（高级，已完成）
- **代码动作**（`textDocument/codeAction`）：按诊断给出**安全的 quickfix**——缺失包 → 「安装包 'x'（dialc install）」，
  其余诊断 → 「在终端运行 dialc build 查看完整错误」。只发 `Command`、**不擅自改代码**，避免误伤用户代码。
- **折叠范围**（`textDocument/foldingRange`）：按作用域包围盒给出可折叠区间（顶层作用域与单行区间不报）。
- **选择范围**（`textDocument/selectionRange`）：从光标处逐级向外构造嵌套 `SelectionRange`（表达式 → 语句 → 块 → 函数）。
- **文档链接**（`textDocument/documentLink`）：`import "pkg";` 的字符串字面量指向该包的 `.diali` 接口文件，可点击跳转。
- 冒烟测试 `tools/lsp_full_smoke.py` 扩展到轮 4（折叠 / 选择 / 代码动作 / 文档链接断言均已通过），并在
  `tests/run_tests.sh` 新增 **M 组**（LSP 全能力往返 + 协议流纯净性）。

### LSP 规范符合性：只用标准形状，不自定义扩展
按 LSP 3.17 规范逐条核对实现，**查出并修掉两处形状偏离**（此前它们是「按自己的理解」写的）：
- **`textDocument/formatting` 的结果**：规范规定是 **`TextEdit[]` 本身**，此前返回的是
  `{ "edits": [...] }` —— 那是私自定义包装，客户端按规范解析会拿到对象而不是数组。现改回数组。
- **`WorkspaceEdit` 的形状**：规范原文「若客户端既不支持 `documentChanges` 也不支持
  `workspace.workspaceEdit.resourceOperations`，那么**只有 `changes` 里的纯 TextEdit 是被
  支持的**」——所以用哪个形状**由客户端能力决定**，不能我们单方面决定。
  现按 `workspace.workspaceEdit.documentChanges` 能力**二选一**：声明了就发 `documentChanges`
  （并如实填客户端给的文档版本号，未打开的文件按规范填 `null`），没声明就回退 `changes`。

**诊断错误码不是「私自扩展」**：规范对 `Diagnostic.code` 的定义是 `integer | string`，
注释为 *"The diagnostic's code, which usually appear in the user interface."*——
**码值由语言服务器自定，规范不设中央码表**（故 `dialc.` 前缀命名空间化即可）；
`Diagnostic.data` 的定义更是直接点名用途：*"A data entry field that is preserved between a
`textDocument/publishDiagnostics` notification and `textDocument/codeAction` request."*——
我们用 `data.package` 把包名从诊断带到 quickfix，正是这个字段的设计用途。

**符合性从此是自动检查的**：冒烟测试新增一张**返回形状对照表**，逐个方法核对响应结构与
规范一致（`Hover` 要 `contents`、`CompletionList` 要 `isIncomplete`+`items`、`formatting`
必须是 `TextEdit[]`、`SelectionRange[]` 只能有 `range`/`parent` 两个键……），
并额外验证「客户端没声明 `documentChanges` 能力时正确回退到 `changes`」。
已做负向验证：把 formatting 改回违规形状，三处断言同时报错。

另修正文档残留：README §18 的「十二组 / A→L」「十一组 / A～K」全部更正为
「十三组 / A～M」并补上 M 组与两道新检查；§2 目录结构同步。

### 诊断：错误码 + 定位 + 缺包不再静默
- **诊断带稳定错误码**（LSP 的 `code` 字段）：`dialc.lex` / `dialc.parse` / `dialc.sema` /
  `dialc.package.missing`。**消息是给人看的、码是给工具看的**——工具按码处理，
  不必去匹配消息文本（措辞会随版本调整，改一次就废掉一个工具）。
- **缺包的诊断终于报出来了**（此前被**静默吞掉**）：`import` 了一个未安装的包，
  编辑器里**毫无任何提示**——用户只能去命令行才知道少了什么。现在会明确报出
  `dialc.package.missing`，定位到那句 `import`，并附带**结构化**的 `data.package`。
  连带好处：轮 4 的「安装这个包」quickfix 终于**真的会触发**（此前它只在冒烟测试里
  被合成诊断触发过，真实场景下从不出现）。
- **语义错误的定位落到出错的那条语句上**（旧行为是落在文件开头）。表达式 `Expr`
  目前不带位置，故精确度到语句级；但「哪条语句错了」这个信息我们本来就有，
  不该浪费。顺带统一了两个位置助手（原先所有权检查那条路径漏了
  `if`/`while`/`for` 等语句变体，会退回文件开头）。
- **代码动作改按错误码匹配**（不再靠匹配消息文本），并优先从 `data.package`
  取包名——结构化数据不必再从自然语言里抠。

### Fixed（修复）
- **能力协商把「没声明」误判成「不支持」**（轮 1 引入，本版才被自家测试抓到）：
  `negotiate_capabilities` 原按「客户端有没有**逐项点名**」判断能力，于是发 `capabilities: {}` 的
  客户端（不少最小客户端都这么发）会**一个能力都拿不到**——仓库自带的 `tests/lsp_features.py`（K 组）当场失败。
  现改为按「客户端有没有**提到这一段**」判断：压根没提 `textDocument` / `workspace` 段 = 没表达偏好 → 全开；
  提到了该段却漏掉某项 = 明确不支持 → 不广播。
  （多广播一个用不上的能力零代价；少广播则让功能**静悄悄消失**，用户查不出原因。）
- **selectionRange 不认规范形状**：LSP 规定 `textDocument/selectionRange` 用 `positions`（**数组**，可多选），
  真实客户端（VS Code）发的就是这个形状；旧实现只认单数 `position`，于是真实客户端**根本拿不到结果**。
  现按规范收数组、返数组，同时保留单数形式（老客户端不至于拿到数组而崩）。
- **LSP 协议流被裸文本污染 → 编辑器卡死**（本版最严重的一个）：
  `resolve_imports()` 是 CLI 构建与 LSP 诊断**共用**的函数，其中两处 `println!`
  （「包 'x' v…」「链接 x -> …」）会把纯文本写进 **stdout**——而 stdout 跑的正是 LSP 的
  `Content-Length` 分帧协议。客户端一旦读到裸字节，分帧立刻错位：症状**不是报错，而是卡死**
  （编辑器在等一条永远对不上的消息）。触发条件却极常见：打开一个 `import` 了**已安装**包的文档。
  - **修复**：`resolve_imports()` 改为**日志出口由调用方注入**（`log: &mut dyn FnMut(&str)`），
    函数本身不再有资格写 stdout；CLI 构建传 stdout，LSP 传 `window/logMessage` 通知（编辑器可见、协议合法）。
  - **加固一（动态）**：冒烟测试新增**协议流纯净性**断言——stdout 上出现任何非协议字节即判失败；
    并给每次请求加**超时看门狗**，服务器卡死会**快速失败并指名道姓**，而不是把 CI 挂死。
  - **加固二（静态）**：新增 `tools/check_lsp_stdout_purity.py`——LSP 用到的库模块与被 LSP 调用的函数里
    出现 `println!` 即失败（CI 独立步骤 + `tests/run_tests.sh` M 组双重把关）。

### 后续轮次（路线）
- 轮 5（待定）：语义化重构（rename 时区分局部/全局）、诊断码（diagnostic code）、工作区符号的模糊匹配排序优化。

---

## [v0.97] — 2026-10-31

本版把 Dial 从「仅 Linux x86_64」扩展到**三平台原生构建**——各平台均为 CI 在对应宿主上
直接编译（编译期目标即运行期平台，无交叉编译 ABI 陷阱）：

| 平台 | 包 slug | 编译器运行依赖 | 用户程序 |
| --- | --- | --- | --- |
| **Linux x86_64** | `linux-x86_64` | 系统 `libLLVM-18.so`（动态 glibc） | 仅依赖 libc |
| **Windows x64（MinGW）** | `windows-x86_64` | 同目录 `LLVM-C.dll`（随包分发） | 单文件 `.exe`，无外部依赖 |
| **Termux aarch64（musl）** | `termux-aarch64` | **无**（musl 全静态） | 单文件，无外部依赖 |

### Added（新增）
- **三平台 CI 构建与发布流水线**（`ci.yml`）：`build-linux` / `build-windows` / `build-termux`
  三个作业各自原生构建，打 `v*` 标签时由 `release` 作业把三个平台的 `dialc-v<版本>-<slug>.zip`
  一并上传到 GitHub Release。
- **Windows（MinGW / `x86_64-pc-windows-gnu`）**：使用官方 MSVC 构建的 LLVM 18 全量包，
  但只取 **C 接口**（`LLVM-C.lib` 导入库 + `LLVM-C.dll` 运行期）。纯 C ABI 跨 MSVC / MinGW
  无名字改写冲突，因此 `dialc.exe` 直接用 MinGW 目标链接它，无需自编译 LLVM。
  - 配套 `tools/llvm-config-win-shim.c`：由 MinGW `gcc` 编译成**真正的 `llvm-config.exe`**，
    把 LLVM 当 C 接口暴露给链接器（`--libnames` 只返回 `LLVM-C.lib`），屏蔽官方 MSVC
    `llvm-config` 的怪癖（它运行期依赖包内并未提供的 `LLVM-18.dll`）。
    **必须是 `.exe` 而不是 `.cmd`/`.bat`**：llvm-sys 先尝试 `llvm-config.exe`、再尝试裸名，
    而 Windows `CreateProcess` 对不带扩展名的程序名只补 `.exe`，批处理垫片永远不会被找到。
  - 用户程序由 **MinGW `gcc` + winpthreads** 编译，stdlib 的 `pthread` 代码**零改动**即用
    （winpthreads 提供兼容层），产物 `-Wl,-Bstatic -lpthread` 静态链入 winpthreads → 单文件 `.exe`。
- **Termux aarch64（musl 全静态）**：在 GitHub **原生 ARM64** runner（`ubuntu-24.04-arm`）上的
  Alpine aarch64 容器内构建（**不用 QEMU**——QEMU 用户态模拟在新 runner 上会报
  `Could not open '/lib/ld-linux-aarch64.so.1'`）。`rustup` 装 `aarch64-unknown-linux-musl`
  宿主工具链，`llvm18-static` 提供静态 LLVM，配合
  `RUSTFLAGS="-C target-feature=+crt-static -C linker=gcc -C link-arg=-static -C link-arg=-lgcc -C link-arg=/usr/lib/libffi.a"`
  → **完全静态**、零外部依赖的 `dialc`（实测：无 `INTERP` 段、无 `NEEDED`，约 118 MB）。
  - 配套 `tools/llvm-config-static-linux.sh`（**只静态**垫片）：inkwell 启用的是
    `llvm18-1-prefer-dynamic`，llvm-sys 会**主动优先** `--libnames --link-shared`、把 LLVM
    动态链成 `libLLVM.so`；垫片对 `--libnames --link-shared` 返回非零码，迫使 llvm-sys 回退到
    `--link-static`。（llvm-sys 并没有 `LLVM_SYS_<ver>_STATIC` 之类的环境变量可覆盖该偏好。）
  - `-lgcc` 与 `libffi.a` 都是补链接缺口：静态 LLVM 组件引用的 `__clear_cache` 在 `libgcc.a`；
    系统库种类取自 build script **自身**的 `cfg!(target_feature="crt-static")`（宿主而非目标），
    若不处理会残留 `NEEDED libffi.so.8`，而 Termux 上没有它 → 执行即 `not found`。

### Changed（变更）
- `src/codegen.rs`：`emit_object` 的 CPU 不再写死 `"x86-64"`，改为按宿主架构推导
  （x86_64→`x86-64`、aarch64→`generic`），aarch64 也能出正确机器码。
- `src/package.rs`：`host_triple()` 改用编译期 `cfg!` 宏推导（Windows→`x86_64-pc-windows-gnu`、
  aarch64+musl→`aarch64-unknown-linux-musl`、aarch64→`aarch64-linux-gnu`、x86_64-linux→`x86_64-linux-gnu`），
  保持与既有测试 / 包清单键兼容。
- `src/main.rs`：`link()` / `output_paths()` / `cc_command()` 平台化——Windows 产物补 `.exe`、
  C 编译器候选按平台排序、链接时静态链入 winpthreads；其余平台行为不变。
- `tools/package_release.sh`：新增精简的**单平台**发布打包（替代旧的全量源码包），产出
  `dialc-v<版本>-<slug>.zip`（含 `dialc[.exe]` + `stdlib/` + `runtime/` + 许可 + 安装脚本；
  Windows 包额外附带 `LLVM-C.dll`）。
- `tools/install-dial.sh` / `install-dial.ps1`：支持 `dialc.exe`、Windows 下随附复制 `LLVM-C.dll`；
  `install-dial.ps1` 可从 GitHub 最新 Release 拉取本平台资产一键安装。
- ARCHITECTURE.md §0.1 / §0.2 / §8、README §3.1 / §3.2.1 / §3.4：同步三平台支持说明。

### Notes（说明）
- **LLVM 18 必须保留**：Alpine 3.22 恰好只提供 llvm18，升版会断 Termux 路线；故 Windows 走官方
  MSVC 包 + C 接口，而非 MSYS2（其 mingw64-llvm 现已是 23.x，18.x 已下架）。
- 路线图 7 阶段已完成 1 / 2 / 3 / 4 / 5 / 7；仅剩阶段 6（**纯 MSVC** 线程后端）在 MinGW 路线下已非阻塞。
  阶段 7（官方标准库 `.dialpkg` 化）随独立 Release **`stdlib-v1.0.0`** 落地，详见下条目。
- **实跑结果**：`v0.97` 三平台作业全部通过并已发布（run `41253324073`，`release` 作业 success），
  三个资产 `dialc-v0.97-{linux-x86_64,windows-x86_64,termux-aarch64}.zip` 均已上传。
  下载后反向验证：Linux 包编译 `hello.dial` 输出 `Hello, dialc!`（产物仅 `NEEDED libc.so.6`）；
  Termux 包 `readelf` 确认 **AArch64 / 无 `INTERP` / 无 `NEEDED`**（真全静态）；
  Windows 包含 `dialc.exe` + `LLVM-C.dll` + `install-dial.ps1`，CI 冒烟通过。

---

## [stdlib v1.0.0] — 2026-10-31（独立 Release，tag `stdlib-v1.0.0`）

官方标准库（由 `stdlib/` + `runtime/` 编译归档而成的 `libdial_std.a`，被**每一个** Dial 程序静态链入）
改造为 `.dialpkg` 形态，**独立发版、与 `dialc` 编译器解耦**，走 GitHub Releases 分发。

### Added（新增）
- **`stdlib.dialpkg` 包**：单包含三平台预编译静态库
  （`lib/x86_64-linux-gnu/libdial_std.a` · `lib/x86_64-pc-windows-gnu/libdial_std.a` ·
  `lib/aarch64-unknown-linux-musl/libdial_std.a`），`manifest.txt` 记录
  `name=stdlib` / `version=1.0.0` / `abi=1` / `license=MIT OR Apache-2.0` 与各平台机器码 SHA256。
- **`stdlib/stdlib.diali`**：与 `declare_stdlib()` 逐条对齐的接口签名文件，供 `.dialpkg` 自描述与校验。
- **`tools/build_stdlib.sh`**：统一选 `zig cc`/`clang`/`gcc` + `ar`，把 `runtime/*.c` + `stdlib/*.c`
  编译归档为单份 `libdial_std.a`（默认 `-DDIAL_SAFETY=1` ReleaseSafe 预编译）。
- **`tools/make_stdlib_pkg.py`**：收三平台 `.a` 与 `.diali`，算 SHA256、生成 `manifest.txt` 并打包 `stdlib.dialpkg`。
- **`dialc` 链接期自动链入**：`src/main.rs::link()` 优先 `package::find_installed("stdlib")` 取本平台 `.a`
  链入，日志打印「使用已安装的标准库包: stdlib v1.0.0（ABI 1，目标 …）」；
  未安装则回退实时编译 `stdlib/` + `runtime/` 源码（默认 Debug），**任何环境都能编出程序**。
- **ABI 兜底**：已装包 `manifest.abi ≠ package::STDLIB_ABI`（新增常量，锁定为 `1`）即**当场报错并给出重装方案**
  （`rm -rf ~/.dial/packages/stdlib` 后重装），绝不静默崩溃。
- **一键脚本自动安装**：`tools/install-dial.sh` / `install-dial.ps1` 装好 `dialc` 后静默从 Release
  `stdlib-v1.0.0` 拉取 `stdlib.dialpkg` 并执行 `dialc install`（`STDLIB_RELEASE` 可钉版本，离线/失败仅告警）。
- **CI 自动发布**：`.github/workflows/ci.yml` 在 `stdlib-v*` 标签触发 `publish-stdlib` 作业，
  由 `build-linux` / `build-windows` / `build-termux` 各自产出本平台 `.a` 后打包发布。

### Changed（变更）
- 预编译安全等级统一为 **ReleaseSafe（`-DDIAL_SAFETY=1`）**：C Runtime 层越界/空指针等 UB 由 `ud2`
  Fail-Fast 拦下，兼顾性能与安全；与用户程序自身 `--release`/`--fast` 等级是两回事（标准库等级发布时已钉死）。

### Docs（文档）
- ARCHITECTURE.md §8 阶段 7 标 ✅；README §25 新增 §25.10「官方标准库 `stdlib.dialpkg`（独立发布）」；
  §25 顶部状态注释同步（URL 安装 / 一键脚本 / stdlib 化均已落地）。

---

## [v0.96] — 2026-10-31

本版补齐 **ARCHITECTURE.md §8 实现路线** 的**阶段 2 剩余部分**与**阶段 5**：
包的来源不再只能是本地文件，**也可以是 URL**；并新增**一键安装脚本**，
把「装好 Dial 环境 + 把该装的包装上」变成一条命令。

至此路线图 7 个阶段已完成 **1 / 2 / 3 / 4 / 5**，
剩余阶段 6（Windows 线程后端）与阶段 7（官方标准库 `.dialpkg` 化）。

### Added（新增）
- **`dialc install <URL>`**（阶段 2）：包可以直接从 URL 装，**不用先手动下载**。
  - **只允许 `https://`**（本机 `localhost` / `127.0.0.1` / `::1` 例外，供本地镜像与自测）。
    明文 http 能被中间人改写，而 SHA256 挡不住「换了内容还带着正确哈希」的伪造，
    因此非本机地址一律拒绝——**宁可装不上，也不降级到明文**。
  - **`--sha256 <哈希>`**：钉死**整个下载物**的哈希，用来证明「这就是我要的那一份包」。
    比对发生在**解压之前**，不符即失败；下载物随即删除。
  - **没给 `--sha256` 也能装**，但会明确提示这次只验了「包内文件与 manifest 是否一致」
    （能发现残缺与篡改），**没验整包来源**——不静默降级。
  - **下载用外部 `curl` 而非引入 Rust 的 HTTP 库**：编译器**本来就依赖外部工具链**
    （`zig cc` / `clang` / `ar`），下载器属同一类依赖，不新增「运行期还要装个东西」的负担；
    引入 TLS 栈（rustls 等）会显著拉长编译时间与依赖树，与 §21「不引入无用依赖」相悖。
    **TLS 绝不手写**——证书校验、主机名校验、重定向全部交给 curl 背后的成熟实现。
  - 临时文件**成功失败都删**，`~/.dial/packages/` 里不留残骸。
- **解压前整包校验**（阶段 2 的安全底座，`src/package.rs::verify_archive`）：
  按包内 `manifest.txt` 登记的 SHA256，把包里**每个文件**验一遍，
  **一个字节都还没落到磁盘**——这是 ARCHITECTURE.md §5.1「校验先于一切 / 每个文件都要校验」
  的字面实现。此前 `dialc install`（本地文件）**完全不做校验**，
  哈希只在**编译时**对用到的那两个文件验；现在安装这一步就拦下了。
  **未在 `manifest.txt` 登记哈希的文件一律拒绝**——无法校验的文件不允许进入缓存。
- **一键安装脚本**（阶段 5）：`tools/install-dial.sh`（Linux / Termux）与
  `tools/install-dial.ps1`（Windows / PowerShell）。按 §3.2 承担两件事（顺序不能反）：
  1. **初始化 Dial 环境**：建 `~/.dial/` 目录树（`bin` / `packages` / `cache/downloads`）、
     把 `dialc` 装进 `~/.dial/bin/` 并挂进 `PATH`、**把 `stdlib/` 与 `runtime/` 一并装到
     `~/.dial/`**、写一份 `~/.dial/config.txt` 工具链配置；
  2. **自动安装已下载的包**：初始化完成后**重新扫描**找到的 `.dialpkg`
     （当前目录及其子目录，或参数指定的目录），逐个执行 `dialc install`。
  > **为什么脚本要「自动重新扫描」而不是让用户逐个点名**：一键安装的语义是
  > 「我把这个目录交给你，你把该装的都装上」。用户不该需要记住包名、也不该需要
  > 为每个包敲一条命令——包就在那里，脚本看得见。

### Fixed（修复）
- **一键安装后编译器找不到 `stdlib/` 与 `runtime/`**：编译器按**可执行文件所在位置**
  向上找这两个目录（`target/release/dialc` → 工程根）。装到 `~/.dial/bin/dialc` 后，
  那个位置正好是 `~/.dial/`——所以它们必须**跟着一起装**。
  少了这一步，用户在自己项目里编译会报「找不到 stdlib/ 与 runtime/ 目录」，
  一键安装等于没装。脚本现已一并安装这两个目录。
- **`install` 时的 `manifest.txt` 校验时机**：此前 `install()` 先解压再校验（且当时压根没校验），
  现在**先校验后解压**。

### Added（测试）
- 测试组 **L**（`tests/pkg_install_url.py`，26 项断言）：真起一个**本机 HTTP 服务器**
  真实提供 `.dialpkg`，用真正的 `dialc install <URL>` 去装，验证：
  - 正常安装成功；`--sha256` 与实际一致时也成功；未钉死哈希时给出明确提示；
  - **坏包一律装不进去**：内容被篡改（改 `.diali` 但保留原哈希）、
    `--sha256` 不符、404、非本机的明文 http——四种情形全部失败；
  - **失败后 `~/.dial/packages/` 无残骸**（半截的包绝不能留在缓存里）；
  - 装完能 `import` 并编译运行，输出正确；
  - 一键脚本：初始化出编译器 + `stdlib/` + `runtime/` + 配置，
    自动扫描并装上 `.dialpkg`，最后在**与 Dial 源码树无关的独立项目目录**里
    用装好的编译器编译 import 程序并运行——这是「一键安装」的意义所在。
  全部用例在**隔离的 HOME** 下运行，不碰用户真实缓存。

### Docs（文档）
- README §25.9 补 URL 安装用法（https 强制、`--sha256`、三步流程）与一键脚本；
  新增 §3.2.1「一键安装」；§2 目录结构补两个脚本与 L 组测试；§17.2 更新 import 行状态；
  §18 新增 L 组（测试组数 十一 → 十二）。
- ARCHITECTURE.md §8 路线表更新：阶段 2 / 5 标记完成，阶段 3 的「LSP 侧待落地」补注
  已由 v0.95 的跨文件符号表兑现；文首「仍待实现」清单同步收窄。
- 官网重新生成，新增「包 URL 安装 + 一键安装」特性卡。

### 验证
- **A～L 十二组测试全绿**（新增 L 组 26 项断言）；
  端到端实测：URL 安装 → `import "demo"` → 编译 → 运行输出正确；
  篡改包 / 哈希不符 / 404 / 明文 http 四种坏包全部装不进去且不留残骸；
  一键脚本装完后，在独立项目目录里编译 `import` 程序成功（输出 3 = `demo_add(1,2)`）。

---

## [v0.95] — 2026-10-31

本版是 **`dialc lsp` 的第一次增强**（见 README [§28.6](README.md#286-lsp-增强hover--跳转定义--补全v095)）：
把 v0.94 骨架里「刻意没做」的三件符号表能力——**悬停（Hover）/ 跳转定义（Go to Definition）/ 基础补全（Completion）**
——一次性补齐。底层是一座**与类型检查解耦、可容错的跨文件符号表**（`src/index.rs`），
它复用 `parse_diali` / `resolve_type_name`，但**不调用 `semantic::analyze`**，
所以半句没写完的代码也照样能索引、照样能给 Hover 与补全。

### Added（新增）
- **GitHub Actions 流水线** `.github/workflows/ci.yml`（**编译 + 发布流程**）：
  - `build` 作业（每次 push / PR）：装 LLVM 18 + Rust → `cargo build --release --locked`
    → 跑**全部十一组**端到端测试（A～K）→ 校验内部锚点 → 校验 `web/` 与文档同步；
    四道关卡任何一道不过，PR 即不可合并。
  - `release` 作业（打 `v*` 标签时）：复用 build 的二进制，
    打成 `dialc-v<版本>.zip`（源码 + `.git` + 四份许可文件 + `web/` + VS Code 扩展 + `bin/dialc`），
    上传为 **GitHub Release** 产物——即官网首页的下载链接。
  - 环境与 README §3.1 一致：Ubuntu 24.04 LTS（近两年内发布的发行版）、Linux x86_64 · 动态 glibc。
- **新增模块 `src/index.rs`**：作用域树（Scope，parent 链）+ 符号表（Symbol），独立于类型检查。
  - **作用域链「由内向外」查找**，与 `semantic::lookup_type` 语义一致，因此 `if` 块里 `let x: int` 遮蔽外层
    `let x: string` 在 Hover / 定义 / 补全三处都查的是**内层那个 `x`**。
  - **跨文件符号**：通过 `import` 自动扫描 `~/.dial/packages/<name>/`，复用现有
    `find_installed` / `verify_iface` / `parse_diali` 拿到包的导出（不透明类型 + `extern fn`），
    并记下来自 `.diali` 的**声明行号**与 `///` 文档——这两样是「跳进 `.diali`」与「Hover 显示包文档」的素材。
- **Hover**（`textDocument/hover`）：
  - 文件内符号（函数 / 结构体 / 局部变量 / 形参）显示名称、**推断类型**与声明处片段；
  - 包符号显示函数签名 + `.diali` 里的 `///` 文档，并标注 **「外部函数」**。
- **跳转定义**（`textDocument/definition`）：
  - 文件内符号 → 返回**本文件**的精确 `Location`（UTF-16 列换算，精确到标识符起止）；
  - 包符号 → 返回 `.diali` 文件的 `Location`，落点正是该 `extern fn` 的**声明行**。
- **基础补全**（`textDocument/completion`，触发字符 `.`）：
  - 收集光标**可见作用域**内的全部符号（本文件局部 + 全局函数/结构体 + 已 import 的包符号）；
  - 按光标处**已输入前缀**过滤（`demo_` 前缀只留 `demo_add`，不会把 `total` / `DemoHandle` 混进来）；
  - 即使光标落在空白（前缀为空）也返回当前可见的全部候选。
- **AST / 解析器 / 包接口补上位置字段**（稀疏、小改）：`VarDecl` / `For` / `Catch` / `SelectArm` 的变量名位置，
  `Func` 的名字与每个形参位置，`StructDef` 的名字位置；`.diali` 的 `type X;` 与 `extern fn f` 也记下声明行号。

### Changed（变更）
- `initialize` 能力协商新增 `hoverProvider: true` / `definitionProvider: true` /
  `completionProvider: { triggerCharacters: ["."] }`（v0.94 仅有语义高亮与诊断）。
- `didOpen` / `didChange` / `didSave` 在发诊断之外，额外把文件喂给符号表索引；
  `didClose` 时从工作区索引里移除该文件，避免残留。

### Added（测试）
- 测试组 **K**（`tests/lsp_features.py`）：真起 `dialc lsp` 子进程按 LSP 协议往返，覆盖：
  1. Hover 区分同名变量遮蔽（内层 `x: int` ≠ 外层 `x: string`）；
  2. Hover 包函数的 `.diali` `///` 文档与「外部函数」标注（跨文件符号）；
  3. 跳转定义对文件内符号回本文件、对包符号跳进 `.diali`；
  4. 补全收集可见符号 + 前缀过滤（含空白光标全量）。

### 刻意未做（与 v0.94 规划一致，本版仍不做）
- **重命名（rename）** 与 **跨文件引用查找**：需要全工作区重索引 + 引用图，当前索引器只建符号、不建引用边。
- **格式化**：Dial 没有官定代码风格，格式化交给编辑器/用户。
- **诊断下放到 `didChange`**：打字到一半的代码必然有错，边打边报只会让红线在半句话上闪；诊断仍留给保存时。
- **LSP 框架（tower-lsp 等）**：本服务仍是手写的「读 JSON / 写 JSON」，框架的异步运行时与依赖树不划算。

### Docs（文档）
本版一并修掉此前遗留的**文档自相矛盾**（均为「文档说的」与「实际做的」对不上，读者会被误导）：

- **标题版本号与内容对不上**：README 首行标题与官网 `spec.html` 的 `<h1>` 仍写 `v0.94`，
  而正文已含 §28.6（v0.95）。现已统一为 **v0.95**，§17.2 标题「截至 v0.94」同步改为「截至 v0.95」，
  并修正了三处指向 §17.2 的**失效锚点**（原先分别写作 `v091` / `v092`，与标题都对不上）。
- **`import` / `.dialpkg` 的版本号写错**：§25 状态、§25.9 标题、§17.2 表格均写 `v0.92`，
  但 CHANGELOG 里 **v0.92 是 `char` 类型、`import` / `.dialpkg` 是 v0.93** 才落地。三处已改为 **v0.93**。
- **`.diali` 的 `type` / `extern fn` / `///` 在语言层没有定义**：§4.3 关键字表与 §9 函数定义里都没有它们，
  而 §25.2 却在用这套语法写示例。现已在 §25.2 明确标注**这是包分发层的接口文件专用语法，不属于 Dial 语言**，
  并在 §4.1（`///` 仅存在于 `.diali`）、§4.3、§9.1 三处各加一句交叉指引。
- **§22「版本变更摘要」名不副实**：内容停在 v0.3（v0.4~v0.95 一条没有），已改名为
  **「早期版本摘要（v0.3 及以前）」**并指向 CHANGELOG（v0.4 及之后的完整记录在那里）。
- **§12.3 交叉引用错误**：写「见 §13.5」，但 §13 没有 13.5（实为 **§14.5**）。链接锚点本就指向 14.5，
  只有显示文字写错，已改正。
- **测试组顺序混乱**：长期是 `A/B/D/E/C/F/G/H/I/J`——C（产物契约）夹在 E 与 F 之间，既非字母序也非逻辑序。
  现已统一为 **A→K**（C 移到 B 之后），`tests/run_tests.sh` 的执行顺序与 README §18 的表格同步调整；
  并修正了脚本里「I 组是 v0.93 新增」的错误标注（实为 v0.94）。
- **P2 遗留项补齐**：① §19.2 补上 **ARC 不解决循环引用**的讨论（泄漏 vs 悬垂的取舍，及四条处置办法）；
  ② §17.2 的 `select` 超时行改写，明确「未实现的是 select 语句级超时，`recv_timeout` 早已实现」，
  消除误读；③ §24.4 的文件 IO 示例**补上 `try`**（§24.2 规定 `open`/`read`/`write` 失败即抛 `Error`，
  原示例不包 try 属于错误示范）；④ §27.10 补上 **`--fast` 下 select 的行为**——
  三档安全等级完全一致（协议语义），唯一随等级变化的是「向已关闭通道 send」的 Fail-Fast 断言。
- **§28.4 表格时空错乱**：该节标题是「v0.94 刻意没做的部分」，表格首行却写「已在 v0.95 落地」。
  已在节首加说明：本节记录的是 v0.94 当时的决策，前三项已在 v0.95 推翻（并链接 §28.6）。
- **锚点体系统一（顺带修掉一批「点不动的链接」）**：README.md 的手写锚点一直是按 GitHub 的规则写的，
  而官网生成器 `tools/build_web.py` 的 `dial_slugify` 与之**不一致**（多保留反引号、把连续空格折叠成一个连字符），
  于是同一份链接「在 GitHub 上能跳、在官网上跳不动」。
  现已让 `dial_slugify` **严格复刻 GitHub（github-slugger）语义**，两者同源；
  并新增 `tools/check_links.py` 做**锚点一致性校验**（可 `--fix` 自动修正）。
  实测：README 的 6 处失效锚点、CHANGELOG 的 25 处失效锚点全部归零，
  生成后的 `web/spec.html`（232 条内部链接）与 `web/changelog.html`（77 条）**零失效**。
  另修掉 CHANGELOG 里 25 处 `](#…)` 跨文件链接——它们在 CHANGELOG 内部**根本解析不到**，
  已改写为 `](README.md#…)`（链接文字里的 §20→§21、§22 旧标题等一并更正）。
- **清理过时产物**：删除 10 份逐版本报告（`REPORT_v0.3.md` … `REPORT_v0.91.md`）——
  它们的内容早已被 CHANGELOG + README + 官网取代，且从未被链接（只在 CHANGELOG 正文里作为历史叙述被提及）。
  同时清理编译遗留物（`examples/*.o`、`examples/hello`、`examples/flow`、`.dialc_build/`、`TEST_RUN_*.log`）。
  `.gitignore` 补上 `REPORT_*.md`，避免这类一次性报告再次堆回仓库。

### 验证
- **A/B/C/D/E/F/G/H/I/J 十组原有测试全绿**（J 组「包安装三条硬规则」于 v0.94 收尾时锁死，v0.95 未改动其用例，只随套件一并执行），
  新增 **K 组 25 项断言全通过**；
  端到端实测：Hover 内层 `x` → `int`、Hover `demo_add` → 显示 `/// 两数相加`；
  定义内层 `x` → 本文件 line=6、定义 `demo_add` → `.diali`；补全前缀 `demo_` 只留 `demo_add`。

#### 流水线实跑记录（GitHub Actions 已验证）
本版的 CI 不是「写完就交」——**已实跑并全绿**：

| 运行 | 提交 | 结果 |
| --- | --- | --- |
| 首次 CI | `02cf561` | ❌ 官网同步校验失败——runner 缺 python-markdown（`ModuleNotFoundError`）。**编译与十一组测试当时已通过**，只有这一步挂。已修：CI 改用 venv 装 `markdown==3.10.1`（版本必须钉死，否则逐字节 diff 会误报） |
| 修复后 CI | `348f916` | ✅ **全绿**：编译 → 十一组测试 → 锚点校验 → 官网同步校验 → 上传二进制，五步全过 |
| 发布流程 | tag `v0.95` | ✅ `build` + `release` 两个作业全过，自动产出 [GitHub Release v0.95](https://github.com/shichencike/Dial/releases/tag/v0.95)，资产 `dialc-v0.95.zip`（1.7 MB） |

发布包已做**反向验证**（不是只看它上传成功）：下载 → `unzip -t` 完整性通过 → 解压 →
本地重新 `cargo build --release` 成功 → 在解压出来的源码树上再跑一遍**十一组测试，全绿**；
并抽查包内 `bin/dialc`（CI 构建的二进制，ELF x86-64）、`BUILD_INFO.txt`（记录构建提交与时间）、
四份许可文件、`.git/`、`web/`、`editors/vscode/` 齐备，而**已废弃的 `REPORT_v0.x.md`、
`TEST_RUN_*.log`、`target/` 确实不在包里**。

> 一处环境差异记录在案：CI 产出的二进制链接的是 apt.llvm.org 的 `libLLVM-18.so.18.1`，
> 与本机自带的 `libLLVM.so.18.1rc` **符号版本不同**（`LLVM_18.1` vs `LLVM_18.1rc`），
> 因此不能直接拿本机的 libLLVM 跑它——这正是 `BUILD_INFO.txt` 里写明
> 「需 `LD_LIBRARY_PATH=/usr/lib/llvm-18/lib`」的原因。**发布包里的源码不受影响**：
> 它在本机重新构建后一切正常（上面的反向验证即是如此）。

---

## [v0.94] — 2026-10-31

本版落地 **`dialc lsp` 语言服务器骨架**（见 README [§28](README.md#28-语言服务器-dialc-lspv094-骨架--v095-增强)），
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

### 收尾补丁（包安装三条硬规则）
进入 v0.95（LSP 增强）前的收尾，全部落在 `src/package.rs` 与 [§25.6.1](README.md#2561-三条硬规则已落地)，
并由新增的 **J 组**测试锁死：

1. **重复安装**：`~/.dial/packages/<name>/` 下已存在同名包时，先读旧 `manifest.txt` 比对 `abi`。
   - **ABI 相同** → 整目录删除后重新解压（覆盖，避免残留旧机器码 / 旧 `.diali`）；
   - **ABI 不同** → **报错并拒绝**，提示先 `rm -rf ~/.dial/packages/<name>/` 再装，
     防止不兼容二进制悄悄链入。拒绝「同名不同 ABI 共存」——`import` 只凭名字引用，共存会重新打开版本冲突的口子。
2. **强制静态库**：`targets` 映射的库文件后缀**必须为 `.a`（Linux/Termux）或 `.lib`（Windows）**，
   形如 `.so` / `.dll` 的动态库在解析 `manifest.txt` 时**当场拒绝**。
   理由：Dial 产物契约是「只链 libc，绝不引入运行时动态依赖」（[§3.4 产物依赖契约](README.md#34-产物依赖契约)），
   动态库会破坏这条契约并让 `ldd` 检查（C 组）失效。
3. **卸载**：缓存是纯派生数据，没有 `dialc uninstall` 命令——直接 `rm -rf ~/.dial/packages/<name>/` 即可，
   随时可重装。这条与「ABI 冲突报错提示」自洽。

### Docs（文档）
- **[§3.1 运行环境要求](README.md#31-运行环境要求linux--动态-glibc)**：明确 Dial **只支持 Linux（x86_64 · 动态 glibc）**，
  并只在**近两年内发布的发行版**上验证（Ubuntu 24.04 LTS / Fedora 42 / Debian 13 等）；
  **明确排除 musl 系统（Alpine 等）**——编译器本体与产物都依赖 glibc，跨 ABI 用不了。
  列出工具链版本（Rust ≥ 1.80、LLVM 18 开发库、`zig cc` 或 `clang`）与运行 `dialc` 本体所需的
  `LD_LIBRARY_PATH`（仅影响编译器，不影响产物）。
- **[§3.2 从源码构建](README.md#32-从源码构建克隆--进入--编译)**：把流程钉死为
  **克隆 → 进入（`cd Dial`，仓库根即编译器本体）→ 编译（`cargo build --release`）**，
  并说明仓库根就是 `Cargo.toml` 与 `src/` 所在之处（没有嵌套的 `dialc/` 子目录），避免读者多进一层目录。
- 官网「快速开始」同步加了 Linux/glibc 提示、`LD_LIBRARY_PATH` 步骤与十组自测命令。

## [v0.93] — 2026-10-31

本版落地 **`import` 与 `.dialpkg` 包分发**（见 README [§25.9](README.md#259-用法import-与-dialc-installv093-已实现v096-支持-url)），
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

本版落地 **`char` 单字符类型**（见 README [§5.4](README.md#54-char单字符类型v092)），
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
（见 README [§27.8](README.md#278-判空多消费者安全与超时v09)）。

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
（见 README [§27.7](README.md#277-通道关闭与-eofv081)）。

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
（见 README [§27](README.md#27-所有权转移与多线程并发move_to--thread--channel)）。
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

本版落地**可恢复异常（try / catch / throw）**（见 README [§26](README.md#26-可恢复异常trycatchthrow)），
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
- **生态规划草案**：README 新增 [§25 包分发与依赖管理](README.md#25-包分发与依赖管理生态规划--最终规范)
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

本版落地**文件 IO（file 句柄）**（见 README [§24](README.md#24-文件-iofile-句柄)），并正式确立**版本号规则**（最多两位小数、禁止跳版、严格按时间递增，见 README [§23](README.md#23-版本号规则)）。

### Added（新增）
- **`file` 不透明指针类型**：新增 `file` 类型，IR 中为 `ptr`，封装 C 的 `FILE*`，**不受 ARC 管理**；用户必须显式 `open` / `close`（与内存宪法显式指针回收契约一致）。
- **`open` / `read` / `write` / `close`**：仅封装 C 标准库 `fopen` / `fread` / `fwrite` / `fclose`，Rust 前端不重写任何文件逻辑；只提供最基础的 C 风格 API，无链式流、无额外语法糖。
- **C 侧实现**：`stdlib/dial_io.c` 新增 `dial_file_open` / `dial_file_read` / `dial_file_write` / `dial_file_close`；`stdlib/dial_string.c` 新增 `dial_string_cstr` / `dial_string_from_bytes`（供 `read` 从字节缓冲区构造字符串）。
- **示例与负向用例**：`examples/file_demo.dial`（写后读回并打印，黄金输出 `dialc-file-io`）；`tests/bad/io_bad_arg.dial`（open 参数类型错误须在编译期拒绝）。

### Changed（变更）
- 遵循版本号规则：本版属「小功能 / 新语法」，版本号 `v0.6 → v0.61`（Cargo `version` 字段按三段式写作 `0.61.0`，语义等同 `v0.61`）。
- README 同步：§4.3 关键字、§5.1 基本类型、§15 FFI 表与 C 导出函数、§17.2（从「未实现」移除文件 IO）、新增 [§24 文件 IO](README.md#24-文件-iofile-句柄) 与 [§22 早期版本摘要](README.md#22-早期版本摘要v03-及以前)。

### 设计约束（用户拍板）
- **绝不引入可变全局状态**：文件句柄显式传递（v1.04 §37.1 统一措辞，原写作「绝不引入全局变量」）。
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
- README 新增 [§21 代码编写纪律（防臃肿与允许冗余原则）](README.md#21-代码编写纪律防臃肿与允许冗余原则)：
  ① 绝对拒绝过度抽象（宁可 WET，绝不 DRY 过头）② 不引入无用依赖 ③ 允许安全与性能的冗余
  ④ 保持代码直白（能用一个循环和一个 if 解决的，绝不套设计模式）。
- README §8.1 语句一览、§8.4 `break` / `continue`、§17.2 未实现清单（移除已落地的 break/continue）同步更新。
- 新增 `REPORT_v0.5.md`（`objdump` 真实反汇编证据）与 `TEST_RUN_v0.5.log`。
- 官网 `web/` 重新生成，首页版本同步 v0.5。
- 版本号升级至 `v0.5`。

---

## [v0.4] — 2026-10-29

本版落地**部分标准库**与**控制流语法糖**，并正式写入[内存宪法](README.md#19-内存宪法防-gc-铁律)（见 README §19）。

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
  这消除了旧实现与[内存宪法](README.md#19-内存宪法防-gc-铁律)（防 GC 铁律）的矛盾。
- Map 与 `read_line` 从「未实现」清单移除（v0.4 已暴露）。

### Fixed（修复）
- 修正 `0..5` 这类范围字面量此前被数字词法器误识别为浮点 `0.5` 的问题：仅当小数点后紧跟数字才视为浮点，
  `..` 现被正确识别为范围运算符。

### Docs（文档）
- README 新增 [§12 哈希表 `map`](README.md#12-哈希表-mapint--int)、[§8.2 `else if`](README.md#82-if--else-if--while)、[§8.3 范围 `for`](README.md#83-范围-for-循环)、
  [§19 内存宪法](README.md#19-内存宪法防-gc-铁律)；并将 FFI 表与 C 导出函数清单补齐 `dial_map_*` / `dial_read_line`。
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
- 指针、地址取值、指针算术（永不提供）；运算符重载（永不提供）；可变全局状态（`var` 全局变量，v1.04 §37.1 统一措辞）。
