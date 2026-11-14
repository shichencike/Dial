#!/usr/bin/env bash
# dialc 端到端测试
#
# 三十组检查，全部通过才算绿（顺序为 A→Z 再 AA→AB→AC→AD，C 组「产物契约」排在 B 之后、逻辑序而非字母序）：
#   A. 正向：examples/*.dial 必须编译成功，且运行输出与同名 .expected 黄金文件逐字节一致；
#   B. 负向：tests/bad/*.dial 必须编译失败（用于锁定语义/语法诊断不被弱化）；
#   C. 契约：产物可执行文件不得依赖 libLLVM，只允许依赖 C Runtime（libc）；
#   D. Fail-Fast：tests/trap/*.dial 越界/除零必须当场因信号终止，且不得打印哨兵；
#   E. 对照组：同一批用例在 --fast 下应关闭检查（证明陷阱确由编译器插入，而非碰巧崩溃）；
#   F. 并发压测：C Runtime 多消费者抢同一通道（v0.9 新增，绕开编译器直压 stdlib）。
#   G. 未捕获异常：内建函数的隐式抛出允许写在 try 外，运行期必须 panic 且打印真实消息
#      （v0.92 新增；不参与 E 组对照——异常不是安全等级检查，--fast 不该关掉它）。
#   H. 包分发：示例 .dialpkg 的安装 + import 后编译运行 + SHA256 篡改必须被拦截（v0.93 新增）。
#   I. LSP：`dialc lsp` 按真实 JSON-RPC 协议跑一遍——初始化、诊断定位、语义 token 的
#      列偏移（含 UTF-16 换算）、以及 shutdown 后的干净退出（v0.94 新增）。
#   J. 包安装三条硬规则：覆盖 / 静态库限制 / ABI 冲突（v0.94 收尾新增；复用 H 组装好的 demo 包）。
#   K. LSP 增强：Hover（变量遮蔽 + 跨文件 .diali 文档）、跳转定义（文件内 / 包符号跳
#      .diali）、基础补全（可见符号收集 + 前缀过滤）（v0.95 新增）。
#   L. 包 URL 安装：真起本机 HTTP 服务器提供 .dialpkg，用 `dialc install <URL>` 装——
#      坏包（篡改 / 哈希不符 / 404 / 非 https）一律装不进去，且失败后缓存无残骸（v0.96 新增）。
#   M. LSP 全能力 + 协议流纯净性：轮2~4 的导航/编写/高级能力做一遍黑盒往返，
#      并静态+动态两路守住「stdout 只有协议字节」这条铁律（v0.98 新增）。
#   N. 内存上界 + 所有权语义：a[i]=v 不得触发 COW 深拷贝（虚拟内存上限钉死 O(N²) 退化），
#      并逐条核对 clone 独立 / COW 隔离 / 嵌套赋值 / 句柄元素数组（v0.99 新增）。
#   O. 所有权 / 语义回归：误拒合法程序、堆字段悬垂、bytes[] 被拒（v0.99.1 修复）。
#   P. FFI：语言层 extern fn + link 静态/动态库 + extern struct（C ABI sizeof/offsetof 与 C 比对）
#      + Dial↔C 双向字段读写 + C 回调状态透传 + 动态链接 RUNPATH 端到端
#      + 按魔数拒绝伪装的 .a / .so / .dll / .dylib（v1.04 新增）。
#   Q. 工程工具链：新增 CLI 子命令、本地源码模块合并、test / fmt / check、
#      new→pack→install→import→uninstall 闭环、A3 Fail-Fast、A7 位置诊断（v1.04 增补）。
#   R. DWARF 调试信息：-g 生成 CompileUnit / 函数 / 行号 / 变量名，GDB 按源码行
#      断点并 print 变量名，无 -g 时不产 Dial 调试符号（v1.04 增补）。
#   S. 性能基准套件：4 语言（C/Go/Python/Dial）28 项校验和一致（抽验）、
#      bench/data.json 数据完整性、web/bench.html 与数据配套及六维雷达图（v1.04 增补）。
#      完整计时采集由 CI 独立步骤负责，本组只做秒级快速正确性校验。
#   T. B/C 组缺口第一批：块注释 B9 / ! 逻辑非 B10 / .slice() B11 /
#      get_or C6 / eprint C8，含宪法否决项（函数重载 B5）的负向锁定（v1.04 增补）。
#   U. B/C 组缺口第二批：字符串/Unicode C1/C4、数值解析与格式化 C2/C3、
#      数组 API C5、文件/环境/时间 C7/C9/C10 的标准库全量（v1.04 增补）。
#   V. B/C 组缺口第三批：语言机制——B7 文件级只读 const（禁止可变全局状态）、
#      B8 结构体字段编译期常量默认值、B6 结构体进容器（Point[] / map<string,Struct>）（v1.04 增补）。
#   W. C11 独立包：http.dialpkg 端到端——三平台构建 / 打包安装 / import 编译运行 /
#      两层 API（高层 get-post + 低层流式）/ gzip 解压 / chunked（含畸形长度行
#      Fail-Fast 与合法 chunk-ext）/ 错误路径 /
#      TLS 1.3（证书校验 fail-closed + keep-alive 连接池 + Termux $PREFIX CA 路径）/
#      HTTP/2（h2c 明文 + h2 ALPN、自研 HPACK、单连接真·并发多路复用、
#      超长额外头不截断）（v1.04 增补，v1.10 开发中增补回归）。
#   X. C11 独立包：text.dialpkg 端到端——自研 Thompson NFA 正则（语法子集 /
#      捕获组 / $n 替换 / 错误路径），三平台零警告构建（v1.04 增补）。
#   Y. C11 独立包：json.dialpkg 端到端——自研递归下降解析 + 紧凑序列化（严格模式 /
#      独立拥有句柄 / 深拷贝构造 / 大整数精度 / \u 与代理对 / 错误码），三平台零警告构建
#      （v1.04 增补）。
#   Z. C11 独立包：crypto.dialpkg 端到端——复用 vendor mbedTLS 的五种摘要
#      （MD5/SHA-1/SHA-256/SHA-512/SHA3-256，标准向量核对）+ Base64 编解码 +
#      HMAC-SHA256，string 与 bytes 双入口、含内嵌 NUL 的二进制数据、严格错误路径，
#      三平台零警告构建（v1.04 增补）。
#   AA. 手搓 GUI 六层：地基 + 控件框架 + 成品控件外观 + 输入层 +
#      主题层/控件扩展 + 信号槽/控件事件订阅，gui.dialpkg 端到端——三平台真实现
#      （X11+XShm+XRender / Win32 GDI）/ 零链接期依赖（dlopen / LoadLibraryA）/
#      纯内存光栅化器像素级 CRC 回归 / 覆盖面积 AA / 内置点阵与 TrueType
#      双字体 / Widget 树 + 绝对·VBox·HBox 布局 + 命中测试 + packed 事件纯值块
#      （sizeof/offsetof 双向锁死 / string_from_cstr 复制成 ARC）/
#      Label·Button·CheckBox 默认主题四态绘制 + 纯值事件推进（use_gui_look.dial）/
#      TextBox 单行编辑·选区·UTF-8 边界·焦点环·max_len·统一功能键码
#      （use_gui_input.dial）/ 14 角色可覆写主题色 set·get·reset 幂等 +
#      Radio 同组互斥 / Slider 按 x 设值 / ProgressBar 只读 / 值域 Fail-Fast
#      （use_gui_theme.dial）/ 6 类可订阅纯值信号 + 每树环形队列（72 字节纯值块
#      sizeof/offsetof 双向锁死 / 零订阅零开销 / 订阅掩码 Fail-Fast / 溢出
#      dropped 可观测 / Dial 主动 pending→data→pop 分派）(use_gui_signal.dial)/
#      Xvfb 实窗上屏 + **XTest 真实鼠标/键盘注入**
#      （xtest_inject.sh：点击聚焦 / 落字 / Tab / Shift+Tab / 按钮激活）/ 无显示
#      环境自动降级为「窗口路径明确跳过」并如实标注（v1.10 增补）。
#      另含第 9 ~ 15 层覆盖层 demo（5h ~ 5o）：滚动视图 / 右键上下文菜单 /
#      悬浮提示 / 状态栏 / 通知气泡 / 窗口级自动集成 / 快捷键·命令系统，
#      纯内存对象 + 离屏 CRC 锁定，第 14 层另在 Xvfb 实窗验证固定 z-order
#      自动绘制与借用语义，第 15 层零 DISPLAY 依赖（166 条断言）。
#      第 16 层 demo（5p ~ 5s）：网格布局 GRID / 组合框 ComboBox / 列表框
#      ListBox / 右键·滚轮交互，纯内存对象 + 离屏 CRC 锁定，set_kind 上界扩到 11
#      （8/9 仍留给信号 kind 标记），零 DISPLAY 依赖。
#      第 17 层 demo（5t ~ 5w）：TextBox 撤销/重做快照栈 / Slider 拖动 /
#      每控件主题覆写（未覆写逐像素回落全局表）/ Grid 自动换行，纯内存对象
#      + 离屏 CRC 锁定，未启用时旧路径逐像素等价（历史 CRC 一字不漂移），
#      零 DISPLAY 依赖。
#      第 18 层 demo（5x ~ 5z）：高 DPI 整数放大（painter_scaled 最近邻）/
#      ListBox 虚拟化 lazy load（100 万逻辑行只持有 <=64 个可视槽，零 C→Dial
#      反向回调）/ IME 预编辑纯值事件块（不进信号位体系）/ GPU+原生平台纯值
#      契约层（headless 确定性降级 -2），纯内存对象 + 离屏 CRC 锁定，默认态
#      未启用时旧路径逐像素等价（历史 CRC 一字不漂移），零 DISPLAY 依赖。
#   AB. int32 / float32 一等类型：零隐式转换 + 显式转换内建 + 同类算术/比较/打印，
#       FFI 精确 32 位宽度（C 侧按 int32_t/float 读参数·返回值·多参数·
#       extern struct sizeof/offsetof 与双向字段读写）/ 越界 Fail-Fast /
#       负向锁定零隐式转换（v1.10 增补）。
#   AC. B1「impl 方法块」语言特性：impl T { fn m(self,...) } 编译期脱糖为普通
#       函数（不新增关键字、ABI 不变、旧签名一字不动），obj.m(args) 降级为静态
#       调用——自定义 struct（self 字段读写 / 方法间调用 / 返回结构体 / 表达式位置 /
#       值语义边界 / impl 作普通标识符）/ 包不透明句柄 GuiMenu（win.draw() 等价
#       dial_gui_window_draw(win)）/ 默认实参 / 容器 push 同名方法隔离 /
#       6 个 Fail-Fast 负向 + 诊断面向用户（不暴露 mangle 名、参数不含 self）
#       （v1.10 开发中增量）。
#   AD. GUI 第 8 层 Dial 侧 OO 封装：packages/gui/src/gui_oo.dial 用 343 个 impl
#       方法包住 344 个 C 风格 extern fn（入口 app_new 除外，纯 Dial 源码、
#       随包 src/ 分发、ABI 不变）；契约审计（方法数 / 全覆盖 / 零 extern 声明）、
#       无 DISPLAY 纯内存路径 + C 风格与 OO 风格绘制 CRC32 逐字节相等、
#       随包 src/ 装包即用（不复制源码，输出与本地副本逐字节一致）、
#       Xvfb 实窗覆盖层与信号队列、3 个 Fail-Fast 透传负向 +
#       诊断面向用户（v1.10 开发中增量）。
#   AE. 编译器产物形态（第 18 层，工具链层非 gui 契约）：--tiny 体积模式
#       （-Os + 分段 + --gc-sections + strip，体积小于默认、无 .symtab /
#       .debug_info）/ --single-file 便携模式（-static 全静态，file 显示
#       statically linked、ldd 显示 not a dynamic executable）/ 两者叠加 /
#       冲突矩阵 3/3 Fail-Fast（--tiny 与 --fast / --pgo(配合 fast) / -g）/
#       --single-file 与 --fast 正交叠加，均用 examples/hello.dial 独立验证
#       （零已安装包依赖，v1.10 增补）。
#   AF. 自研后端（--backend=asm）：llvm / asm 双后端对同一份 .dial 的运行输出
#      逐字节一致 / 确实产出 .s / 默认后端仍为 llvm / 未覆盖构造 Fail-Fast 明确报错 /
#      AArch64 汇编语法经交叉汇编器验证（tools/run_asm_golden.sh，v1.10 增补）。
set -u
set +m   # 关闭作业控制提示（避免 "Trace/breakpoint trap" 噪音）

cd "$(dirname "$0")/.." || exit 1

export LLVM_SYS_181_PREFIX="${LLVM_SYS_181_PREFIX:-/usr/lib/llvm-18}"

echo "== 0) 构建 dialc 编译器 =="
cargo build --release 2>&1 | tail -3
DIALC=./target/release/dialc
[ -x "$DIALC" ] || { echo "FAIL: 编译器未构建成功"; exit 1; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail=0

echo
echo "== A) 正向：编译 + 运行 + 输出比对 =="
for src in examples/*.dial; do
    name=$(basename "$src" .dial)
    exp="examples/${name}.expected"
    bin="${TMP}/${name}"

    if ! [ -f "$exp" ]; then
        echo "  ⚠️  跳过 $name（缺少黄金文件 $exp）"
        continue
    fi

    if ! "$DIALC" build "$src" -o "$bin" >"${TMP}/build.log" 2>&1; then
        echo "  ❌ $name 编译失败"
        sed 's/^/       /' "${TMP}/build.log"
        fail=1
        continue
    fi

    "$bin" >"${TMP}/out.txt" 2>&1
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "  ❌ $name 运行退出码非 0（$rc）"
        fail=1
        continue
    fi

    if diff -u "$exp" "${TMP}/out.txt" >"${TMP}/diff.txt" 2>&1; then
        echo "  ✅ $name 输出一致"
    else
        echo "  ❌ $name 输出与黄金文件不一致："
        sed 's/^/       /' "${TMP}/diff.txt"
        fail=1
    fi
done

echo
echo "== B) 负向：下列用例必须编译失败 =="
for f in tests/bad/*.dial; do
    if "$DIALC" build "$f" -o "${TMP}/bad" >"${TMP}/bad.log" 2>&1; then
        echo "  ❌ 应当报错却编译成功: $(basename "$f")"
        fail=1
    else
        msg=$(head -1 "${TMP}/bad.log")
        echo "  ✅ $(basename "$f") -> ${msg}"
    fi
done

echo
echo "== C) 契约：产物不得依赖 libLLVM =="
bin="${TMP}/contract_hello"
"$DIALC" build examples/hello.dial -o "$bin" >/dev/null 2>&1
if ldd "$bin" 2>/dev/null | grep -qi "LLVM"; then
    echo "  ❌ 用户程序依赖了 libLLVM，违反契约"
    ldd "$bin" | sed 's/^/       /'
    fail=1
else
    echo "  ✅ 用户程序仅依赖 C Runtime：$(ldd "$bin" | tr -s ' \t' ' ' | tr '\n' '|')"
fi

echo
echo "== D) Fail-Fast：越界/除零必须当场终止（默认 Debug 等级）=="
# 判定标准两条，缺一不可：
#   1) 进程因信号而死（退出码 >= 128：SIGILL=4 / SIGTRAP=5 / SIGSEGV=11）
#   2) 输出中不含哨兵 9999 —— 证明它在越界那一行就停了，而不是带病跑完
for src in tests/trap/*.dial; do
    name=$(basename "$src" .dial)
    bin="${TMP}/trap_${name}"

    if ! "$DIALC" build "$src" -o "$bin" >"${TMP}/tbuild.log" 2>&1; then
        echo "  ❌ $name 编译失败"
        sed 's/^/       /' "${TMP}/tbuild.log"
        fail=1
        continue
    fi

    "$bin" >"${TMP}/tout.txt" 2>"${TMP}/terr.txt"
    rc=$?
    sig=$((rc - 128))
    if [ $rc -lt 128 ]; then
        echo "  ❌ $name 未终止（退出码 $rc），Fail-Fast 失效"
        sed 's/^/       /' "${TMP}/tout.txt"
        fail=1
        continue
    fi
    if grep -q "9999" "${TMP}/tout.txt"; then
        echo "  ❌ $name 越界后仍继续执行（哨兵 9999 被打印）"
        fail=1
        continue
    fi
    diag=$(head -1 "${TMP}/terr.txt")
    echo "  ✅ $name 信号 $sig 终止${diag:+（诊断: ${diag}）}"
done

echo
echo "== E) 对照组：--fast 关闭检查后，同一份代码应恢复 C 的“静默”行为 =="
# 这一组存在的意义：证明上面的陷阱**确实由我们插入**，而不是碰巧段错误。
# 关闭检查后程序应当跑完并打印哨兵 9999（即 UB，后果由使用者承担）。
for src in tests/trap/*.dial; do
    name=$(basename "$src" .dial)
    bin="${TMP}/fast_${name}"
    if ! "$DIALC" build "$src" -o "$bin" --fast >"${TMP}/fbuild.log" 2>&1; then
        echo "  ❌ $name (--fast) 编译失败"
        sed 's/^/       /' "${TMP}/fbuild.log"
        fail=1
        continue
    fi
    "$bin" >"${TMP}/fout.txt" 2>/dev/null
    rc=$?
    if [ $rc -eq 0 ] && grep -q "9999" "${TMP}/fout.txt"; then
        echo "  ✅ $name (--fast) 检查已关闭，产生未定义行为（符合预期）"
    else
        echo "  ⚠️  $name (--fast) 仍以退出码 $rc 终止（该平台下越界恰好命中不可访问内存）"
    fi
done

echo
echo "== F) C Runtime 并发压测：多消费者抢同一通道（v0.9 新增）=="
# 这一组绕开编译器，直接压 stdlib/dial_channel.c，验证三件 Dial 层看不见、
# 但多消费者下会致命的事：
#   1) 不丢唤醒：8 个消费者 + 100 万元素。只要有一个消费者被 signal 吞掉，
#      join 就永远挂住 -> timeout 杀掉 -> 判失败。
#   2) 每个元素恰好被取走一次（位图 + 原子 fetch_or）：重复领取 = 必然 double-free。
#   3) 不丢失：收到的总数必须恰好等于发出的总数。
# 注意：这一组**不经过 dialc**，因此不能用 --fast（它直接编译 C 源文件）。
mt_bin="${TMP}/channel_mt"
# 编译器探测与 dialc 内部保持一致（main.rs 的 cc_command）：优先 zig cc，其次 clang。
if command -v zig >/dev/null 2>&1; then
    CC_CMD=(zig cc)
elif command -v clang >/dev/null 2>&1; then
    CC_CMD=(clang)
else
    CC_CMD=(cc)
fi
if ! "${CC_CMD[@]}" -O2 -g -DDIAL_SAFETY=2 -Iruntime -Istdlib \
        tests/stress/channel_mt.c stdlib/dial_channel.c \
        runtime/dial_rt_trap.c runtime/dial_rt_mem.c -o "$mt_bin" -lpthread >"${TMP}/mtbuild.log" 2>&1; then
    echo "  ❌ 并发压测用例编译失败"
    sed 's/^/       /' "${TMP}/mtbuild.log"
    fail=1
else
    if ! timeout 120 "$mt_bin" >"${TMP}/mt.log" 2>&1; then
        echo "  ❌ 多消费者并发压测失败（超时挂死 = 丢唤醒；或断言失败）"
        sed 's/^/       /' "${TMP}/mt.log"
        fail=1
    else
        echo "  ✅ 多消费者并发压测通过： $(grep -m1 received= "${TMP}/mt.log")"
    fi
fi

echo
echo "== G) 未捕获异常：隐式抛出可写在 try 外，运行期 panic 且打印真实消息 =="
# 与 D 组（Fail-Fast 陷阱）不同，这一组**不**参与 E 组的 --fast 对照：
# 未捕获异常不是「安全等级检查」，--fast 不会（也不该）把它关掉。
for src in tests/uncaught/*.dial; do
    name=$(basename "$src" .dial)
    bin="${TMP}/uncaught_${name}"

    # 1) 必须能编译：内建函数的隐式抛出允许出现在 try 块之外（§26.1.1）
    if ! "$DIALC" build "$src" -o "$bin" >"${TMP}/gbuild.log" 2>&1; then
        echo "  ❌ $name 编译失败（内建函数的隐式抛出必须允许写在 try 块外）"
        sed 's/^/       /' "${TMP}/gbuild.log"
        fail=1
        continue
    fi

    "$bin" >"${TMP}/gout.txt" 2>"${TMP}/gerr.txt"
    rc=$?
    if [ $rc -lt 128 ]; then
        echo "  ❌ $name 未捕获异常却没有终止进程（退出码 $rc）"
        fail=1
        continue
    fi
    if grep -q "9999" "${TMP}/gout.txt"; then
        echo "  ❌ $name panic 之后仍继续执行（哨兵 9999 被打印）"
        fail=1
        continue
    fi
    # 2) 关键回归点：必须是**真实的错误消息**，而不是把 DialString 句柄
    #    当 const char* 打印出来的乱码或单个数字（缺 dial_string_cstr 的症状）
    if ! grep -q "无法打开文件" "${TMP}/gerr.txt"; then
        echo "  ❌ $name panic 未打印真实错误消息（诊断: $(head -1 "${TMP}/gerr.txt")）"
        fail=1
        continue
    fi
    echo "  ✅ $name 信号 $((rc - 128)) 终止且消息正确（诊断: $(head -1 "${TMP}/gerr.txt")）"
done

echo
echo "== H) 包分发：.dialpkg 安装 + import + SHA256 篡改拦截（v0.93） =="
pkgdir="${TMP}/samplepkg"
if ! bash tools/make_sample_pkg.sh "$pkgdir" >"${TMP}/hbuild.log" 2>&1; then
    echo "  ❌ 示例包构建失败"; sed 's/^/       /' "${TMP}/hbuild.log"; fail=1
else
    if ! "$DIALC" install "$pkgdir/demo.dialpkg" >"${TMP}/hinstall.log" 2>&1; then
        echo "  ❌ 包安装失败"; sed 's/^/       /' "${TMP}/hinstall.log"; fail=1
    else
        echo "  ✅ 示例包构建并安装成功"
        # 1) import 之后能编译、能运行，且输出与黄金文件一致
        if ! "$DIALC" build tests/pkg/use_demo.dial -o "${TMP}/use_demo" >"${TMP}/hcomp.log" 2>&1; then
            echo "  ❌ use_demo 编译失败"; sed 's/^/       /' "${TMP}/hcomp.log"; fail=1
        elif ! diff -u tests/pkg/use_demo.expected <("${TMP}/use_demo" 2>&1) >"${TMP}/hdiff.txt" 2>&1; then
            echo "  ❌ use_demo 输出不一致："; sed 's/^/       /' "${TMP}/hdiff.txt"; fail=1
        else
            echo "  ✅ use_demo 输出一致（import 的外部函数与不透明句柄均可用）"
        fi

        # 2) 篡改机器码必须被 SHA256 当场拦截——绝不让它流到链接器
        dlib=$(find "${HOME}/.dial/packages/demo/lib" -name '*.a' 2>/dev/null | head -1)
        if [ -z "$dlib" ]; then
            echo "  ❌ 找不到已安装包的机器码，无法校验篡改拦截"; fail=1
        else
            cp "$dlib" "${TMP}/dlib.bak"
            printf '\x00' >> "$dlib"
            if "$DIALC" build tests/pkg/use_demo.dial -o "${TMP}/use_demo2" >"${TMP}/htamper.log" 2>&1; then
                echo "  ❌ 篡改后的机器码竟然通过了 SHA256 校验"; fail=1
            elif ! grep -q "SHA256" "${TMP}/htamper.log"; then
                echo "  ❌ 篡改被拦截，但报错里没有说明是 SHA256 问题"; fail=1
            else
                echo "  ✅ 篡改机器码被 SHA256 拦截（$(grep -o 'SHA256 校验失败' "${TMP}/htamper.log" | head -1)）"
            fi
            cp "${TMP}/dlib.bak" "$dlib"   # 还原，避免污染后续用例
        fi

        # 3) drop(不透明句柄) 必须编译期报错并给出「不受 ARC 管理」的指引（v1.02）。
        #    此前它是静默 no-op，还会把变量误标为「已转移」——容易让人以为资源已归还。
        cat > "${TMP}/drop_opaque.dial" <<'EOF'
import "demo";
fn main(): int {
    var h: DemoHandle = demo_new(42);
    drop(h);
    return 0;
}
EOF
        if "$DIALC" build "${TMP}/drop_opaque.dial" -o "${TMP}/drop_opaque" >"${TMP}/hdrop.log" 2>&1; then
            echo "  ❌ drop(不透明句柄) 竟通过编译（应报错并指引手动释放）"; fail=1
        elif ! grep -q "不受 ARC 管理" "${TMP}/hdrop.log"; then
            echo "  ❌ drop 被拦下，但报错未说明「不受 ARC 管理」："; sed 's/^/       /' "${TMP}/hdrop.log"; fail=1
        else
            echo "  ✅ drop(不透明句柄) 被拦下并给出手动释放指引"
        fi

        # 4) C 风格异构变参 extern fn（v1.03）：这是 FFI 特例，固定参数严格检查，
        #    尾部参数按 C varargs ABI 直传（i64 / double / bool 混传）。
        cat > "${TMP}/ffi_varargs.dial" <<'EOF'
import "demo";
fn main(): int {
    demo_vprintf("i=%ld f=%.1f b=%ld\n", 42, 3.5, true);
    return 0;
}
EOF
        if "$DIALC" build "${TMP}/ffi_varargs.dial" -o "${TMP}/ffi_varargs" >"${TMP}/hffi.log" 2>&1; then
            if ! diff -u <(printf 'i=42 f=3.5 b=1\n') <("${TMP}/ffi_varargs" 2>&1) >"${TMP}/hffidiff.txt" 2>&1; then
                echo "  ❌ C 变参 extern fn 输出不一致"; sed 's/^/       /' "${TMP}/hffidiff.txt"; fail=1
            else
                echo "  ✅ C 变参 extern fn 可用（int64 / double / bool 混传）"
            fi
        else
            echo "  ❌ C 变参 extern fn 编译失败"; sed 's/^/       /' "${TMP}/hffi.log"; fail=1
        fi

        # 5) FFI 特例不等于放弃类型检查：固定参数类型不匹配仍要当场拦下。
        cat > "${TMP}/ffi_varargs_bad.dial" <<'EOF'
import "demo";
fn main(): int {
    demo_vprintf(123, 42);
    return 0;
}
EOF
        if "$DIALC" build "${TMP}/ffi_varargs_bad.dial" -o "${TMP}/ffi_varargs_bad" >"${TMP}/hffibad.log" 2>&1; then
            echo "  ❌ C 变参的固定参数类型错误竟通过编译"; fail=1
        else
            echo "  ✅ C 变参固定参数仍做严格类型检查"
        fi
    fi
fi

echo
echo "== I) LSP：dialc lsp 端到端协议往返（v0.94） =="
# 单独一个 Python 脚本驱动：LSP 的语义 token 是差分编码的整数流，
# 要在 shell 里解码 5 元组并反查源码子串根本不现实，
# 而「反查子串」恰恰是能证明列偏移没算错的唯一手段。
if command -v python3 >/dev/null 2>&1; then
    if python3 tests/lsp_smoke.py; then
        :
    else
        fail=1
    fi
else
    echo "  ⚠️  跳过（未找到 python3）"
fi

echo
echo "== J) 包安装三条硬规则（v0.94.1）：覆盖 / 静态库限制 / ABI 冲突 =="
# 复用 H 组已构建并安装的 demo（abi=1）。若没装，先装一次。
if [ ! -f "${HOME}/.dial/packages/demo/manifest.txt" ]; then
    "$DIALC" install "$pkgdir/demo.dialpkg" >/dev/null 2>&1 || true
fi
# 1) 同 ABI 重复安装 → 必须覆盖成功（不退出版本冲突）
if "$DIALC" install "$pkgdir/demo.dialpkg" >"${TMP}/j_reinstall.log" 2>&1; then
    echo "  ✅ 同 ABI 重复安装成功（已覆盖）"
else
    echo "  ❌ 同 ABI 重复安装本应覆盖却失败："; sed 's/^/       /' "${TMP}/j_reinstall.log"; fail=1
fi

# 2) 动态库（.so）包 → 必须当场拒绝
python3 - "$pkgdir" "${TMP}" <<'PY'
import json, os, zipfile, sys
pkgdir, tmp = sys.argv[1], sys.argv[2]
os.makedirs(f"{tmp}/sobad/lib", exist_ok=True)
open(f"{tmp}/sobad/lib/x86_64-linux-gnu.so","wb").write(b"\x7fELF")  # 假 .so
open(f"{tmp}/sobad/demo.diali","w").write("type X;\n")
m={"name":"sobad","version":"1.0.0","abi":1,"iface":"demo.diali","license":"MIT",
   "targets":{"x86_64-linux-gnu":{"lib":"lib/x86_64-linux-gnu.so"}},
   "sha256":{"demo.diali":"00","lib/x86_64-linux-gnu.so":"00"}}
open(f"{tmp}/sobad/manifest.txt","w").write(json.dumps(m))
with zipfile.ZipFile(f"{tmp}/sobad/sobad.dialpkg","w") as z:
    z.write(f"{tmp}/sobad/manifest.txt","manifest.txt")
    z.write(f"{tmp}/sobad/demo.diali","demo.diali")
PY
if "$DIALC" install "${TMP}/sobad/sobad.dialpkg" >"${TMP}/j_so.log" 2>&1; then
    echo "  ❌ 含 .so 动态库的包竟然被安装了（应拒绝）"; fail=1
elif ! grep -q "静态库" "${TMP}/j_so.log"; then
    echo "  ❌ .so 包被拒绝，但报错没说明是静态库限制："; sed 's/^/       /' "${TMP}/j_so.log"; fail=1
else
    echo "  ✅ 含 .so 动态库的包被拒绝（静态库限制生效）"
fi

# 3) 不同 ABI 的同名包 → 必须报错并提示先卸载
python3 - "$pkgdir" "${TMP}" <<'PY'
import json, zipfile, sys
pkgdir, tmp = sys.argv[1], sys.argv[2]
import shutil, os
d=f"{tmp}/abidiff"; shutil.rmtree(d, ignore_errors=True); shutil.copytree(pkgdir, d)
m=json.load(open(f"{d}/manifest.txt")); m["abi"]=2; m["version"]="2.0.0"
open(f"{d}/manifest.txt","w").write(json.dumps(m))
with zipfile.ZipFile(f"{d}/demo.dialpkg","w",zipfile.ZIP_DEFLATED) as z:
    z.write(f"{d}/manifest.txt","manifest.txt")
    z.write(f"{d}/demo.diali","demo.diali")
    z.write(f"{d}/lib/x86_64-linux-gnu.a","lib/x86_64-linux-gnu.a")
PY
if "$DIALC" install "${TMP}/abidiff/demo.dialpkg" >"${TMP}/j_abi.log" 2>&1; then
    echo "  ❌ 不同 ABI 的同名包竟然被安装了（应报错）"; fail=1
elif ! grep -q "ABI" "${TMP}/j_abi.log" || ! grep -q "rm -rf" "${TMP}/j_abi.log"; then
    echo "  ❌ ABI 冲突被拦截，但报错未说明冲突 / 未给出卸载提示："; sed 's/^/       /' "${TMP}/j_abi.log"; fail=1
else
    echo "  ✅ 不同 ABI 的同名包被拒绝并提示先卸载"
fi

echo
echo "== K) LSP 增强特性：Hover / 跳转定义 / 补全（v0.95） =="
# 真起子进程、按 LSP 协议往返，聚焦三件事：
#   1) Hover 区分同名变量遮蔽、并从 import 的包 .diali 拿文档；
#   2) 跳转定义对文件内符号回本文件 Location、对包符号跳进 .diali；
#   3) 补全收集光标可见符号并按已输入前缀过滤。
if command -v python3 >/dev/null 2>&1; then
    if python3 tests/lsp_features.py; then
        :
    else
        fail=1
    fi
else
    echo "  ⚠️  跳过（未找到 python3）"
fi

echo
echo "== L) 包 URL 安装：下载 → 校验 → 解压（v0.96） =="
# 真起一个本机 HTTP 服务器提供 .dialpkg，用真正的 `dialc install <URL>` 去装。
# 重点不在「能装上」，而在**坏包一律装不进去**：
#   · 内容被篡改 -> 解压前逐文件 SHA256 拦下，且不落盘；
#   · --sha256 不符 / 404 / 非 https -> 直接失败；
#   · 失败后缓存目录不留残骸（半截的包绝不能留在 ~/.dial 里）。
# 全部用例在隔离的 HOME 下跑，不碰用户真实缓存。
if command -v python3 >/dev/null 2>&1; then
    if python3 tests/pkg_install_url.py; then
        :
    else
        fail=1
    fi
else
    echo "  ⚠️  跳过（未找到 python3）"
fi

echo
echo "== M) LSP 全能力往返 + 协议流纯净性（v0.98 轮2~4） =="
# 1) 黑盒往返：文档符号 / 工作区符号 / 引用 / 重命名 / 签名帮助 / 格式化 /
#    inlay 提示 / codeAction / folding / selection / documentLink 等能力，
#    外加「stdout 上不得出现任何非协议字节」的动态断言。
# 2) 静态守卫：LSP 运行时用到的模块与函数里不得有 stdout 打印。
#
# 第 2 条绝不是"锦上添花"：LSP 的 stdout 跑的是 Content-Length 分帧协议，
# 混入一行日志就会让客户端分帧错位——症状不是报错，而是**编辑器卡死**。
# 真实事故：`resolve_imports()`（CLI 与 LSP 共用）里有两处 println!，
# 于是"打开一个 import 了已安装包的文档"就必卡（v0.98 修复）。
if command -v python3 >/dev/null 2>&1; then
    if python3 tools/lsp_full_smoke.py; then
        :
    else
        fail=1
    fi
    if python3 tools/check_lsp_stdout_purity.py; then
        :
    else
        fail=1
    fi
else
    echo "  ⚠️  跳过（未找到 python3）"
fi

echo
echo "== N) 内存上界 + 所有权语义回归（v0.99 修复：COW 误判致 O(N²) 内存） =="
# 1) 下标赋值的内存上界。
#    缺陷背景：数组运行时在「独占」路径上多做了 retain、共享路径上没释放旧引用，
#    于是每写一次 a[i] = v 引用计数就 +1，COW 守卫 `refcount > 1` 恒真——
#    每次赋值都深拷贝整个数组，100 万次写需要约 8 TB（进程被 OOM Killed）。
#    这里用虚拟内存上限把那种退化钉死：修复后峰值约 10 MB，一旦复发立刻被上限打爆。
MEM_CAP_KB=131072
if ! "$DIALC" build tests/mem/array_cow_bench.dial -o "${TMP}/cow_bench" >"${TMP}/cowbuild.log" 2>&1; then
    echo "  ❌ 内存上界用例编译失败"; sed 's/^/       /' "${TMP}/cowbuild.log"; fail=1
else
    if ( ulimit -v $MEM_CAP_KB 2>/dev/null; "${TMP}/cow_bench" >"${TMP}/cowout.txt" 2>&1 ); then
        if diff -u tests/mem/array_cow_bench.expected "${TMP}/cowout.txt" >/dev/null 2>&1; then
            echo "  ✅ 下标赋值在 ${MEM_CAP_KB} KB 虚拟内存上限内跑完且结果正确（未触发 COW 深拷贝）"
        else
            echo "  ❌ 内存上界用例输出不一致"; diff -u tests/mem/array_cow_bench.expected "${TMP}/cowout.txt"; fail=1
        fi
    else
        echo "  ❌ 下标赋值用例超出 ${MEM_CAP_KB} KB 虚拟内存上限（COW 误判复发：每次赋值都深拷贝整个数组）"; fail=1
    fi
fi
# 2) 所有权语义：clone 独立 / COW 隔离 / 嵌套赋值 / 句柄元素数组。
#    这几条正是修引用计数时动过的语义，必须逐条钉死。
if ! "$DIALC" build tests/mem/array_cow_semantics.dial -o "${TMP}/cow_sem" >"${TMP}/cowsem.log" 2>&1; then
    echo "  ❌ 所有权语义用例编译失败"; sed 's/^/       /' "${TMP}/cowsem.log"; fail=1
else
    "${TMP}/cow_sem" >"${TMP}/cowsemout.txt" 2>&1
    if diff -u tests/mem/array_cow_semantics.expected "${TMP}/cowsemout.txt" >/dev/null 2>&1; then
        echo "  ✅ 所有权语义正确（clone 独立 / COW 隔离 / 嵌套赋值 / 句柄元素数组）"
    else
        echo "  ❌ 所有权语义不一致"; diff -u tests/mem/array_cow_semantics.expected "${TMP}/cowsemout.txt"; fail=1
    fi
fi

echo
echo "== O) 所有权 / 语义回归（v0.99.1 修复：误拒合法程序 + 堆字段悬垂 + bytes[] 拒绝） =="
# 这批用例钉死「编译器应接受并正确运行」的合法程序，正好覆盖此前被改坏的几条语义：
#   ① select 分支里 move_to 外层变量被误报「已转移」（重复下探）；
#   ③ StructLit / 字段读取 / 字段赋值的堆字段未 retain → 来源出作用域后悬垂（use-after-free）；
#   ④ is_array_elem_type 漏掉 Type::Bytes → bytes[] 被拒绝；
#   借来的值（变量 / 数组元素）在写入容器时不得被重复释放或泄漏。
for src in tests/sem/*.dial; do
    name=$(basename "$src" .dial)
    exp="tests/sem/${name}.expected"
    bin="${TMP}/sem_${name}"
    if ! [ -f "$exp" ]; then echo "  ⚠️  跳过 $name（缺少黄金文件 $exp）"; continue; fi
    if ! "$DIALC" build "$src" -o "$bin" >"${TMP}/sembuild.log" 2>&1; then
        echo "  ❌ $name 编译失败"; sed 's/^/       /' "${TMP}/sembuild.log"; fail=1; continue
    fi
    # 运行时同样不能崩（堆悬垂会以 segfault / 乱码 / 非零退出码暴露）
    if ! timeout 30 "$bin" >"${TMP}/semout.txt" 2>&1; then
        echo "  ❌ $name 运行失败（rc=$?，可能为堆悬垂）"; fail=1; continue
    fi
    if diff -u "$exp" "${TMP}/semout.txt" >/dev/null 2>&1; then
        echo "  ✅ $name 编译 + 运行 + 输出一致"
    else
        echo "  ❌ $name 输出不一致"; diff -u "$exp" "${TMP}/semout.txt"; fail=1
    fi
done

echo
echo "== P) FFI：语言层 extern / link 静态+动态库 / C ABI 结构体 / C 回调 / 魔数校验（v1.04） =="
FFI_DIR="$PWD/tests/ffi"
# 1) 把 C 侧专项库编成静态归档（仍验 ar 魔数）。
#    系统库（libc / libm）由 C 编译器默认动态解析，不进这条链路；
#    .dialpkg 包分发仍强制静态（J 组铁律），与下面的语言层动态 link 是两条路。
if ! ( cd "$FFI_DIR" && cc -O2 -c ffi_demo.c -o ffi_demo.o && ar rcs libffi_demo.a ffi_demo.o ) >"${TMP}/ffibuild.log" 2>&1; then
    echo "  ❌ FFI 专项静态库编译失败"; sed 's/^/       /' "${TMP}/ffibuild.log"; fail=1
else
    # 2) 端到端：link + extern struct 的 sizeof/offsetof 与 C 逐项比对、
    #    Dial↔C 双向字段读写、C 回调（含 rawptr<void> 状态透传）。
    if ! "$DIALC" build "$FFI_DIR/ffi_link_demo.dial" -o "${TMP}/ffi_link_demo" >"${TMP}/ffibuild2.log" 2>&1; then
        echo "  ❌ FFI 专项程序编译失败"; sed 's/^/       /' "${TMP}/ffibuild2.log"; fail=1
    elif ! timeout 30 "${TMP}/ffi_link_demo" >"${TMP}/ffiout.txt" 2>&1; then
        echo "  ❌ FFI 专项程序运行失败"; fail=1
    elif ! diff -u "$FFI_DIR/ffi_link_demo.expected" "${TMP}/ffiout.txt" >"${TMP}/ffidiff.txt" 2>&1; then
        echo "  ❌ FFI 专项程序输出不一致"; sed 's/^/       /' "${TMP}/ffidiff.txt"; fail=1
    else
        echo "  ✅ sizeof/offsetof 与 C 一致，Dial↔C 双向字段读写、C 回调均正确"
    fi

    # 2b) 编译期类型内建（v1.04 增补）：sizeof<T>() / alignof<T>() / offsetof<T>(field)
    #     直接吐编译期常量，与同结构体的字段读写路径、与 C 的 offsetof 必须逐字节一致。
    #     这一步专门锁住「新内建」不被 layout 漂移带偏（FFI 绑定核对内存布局的刚需）。
    cat > "${TMP}/ffi_layout_builtins.dial" <<'EOF'
extern struct Point { x: int, y: byte, flag: bool }
fn main(): int {
    print(sizeof<Point>());
    print(alignof<Point>());
    print(offsetof<Point>(x));
    print(offsetof<Point>(y));
    print(offsetof<Point>(flag));
    print(sizeof<int>());
    print(alignof<int>());
    print(sizeof<byte>());
    print(sizeof<bool>());
    print(sizeof<float>());
    print(sizeof<string>());
    print(sizeof<rawptr<byte>>());
    return 0;
}
EOF
    if ! "$DIALC" build "${TMP}/ffi_layout_builtins.dial" -o "${TMP}/ffi_layout_builtins" >"${TMP}/ffilb.log" 2>&1; then
        echo "  ❌ 编译期类型内建编译失败"; sed 's/^/       /' "${TMP}/ffilb.log"; fail=1
    elif ! diff -u <(printf '16\n8\n0\n8\n9\n8\n8\n8\n1\n8\n8\n8\n') \
                      <("${TMP}/ffi_layout_builtins" 2>&1) >"${TMP}/ffilbdiff.txt" 2>&1; then
        echo "  ❌ 编译期类型内建与 C 布局不一致"; sed 's/^/       /' "${TMP}/ffilbdiff.txt"; fail=1
    else
        echo "  ✅ sizeof/alignof/offsetof 编译期内建与 C ABI 逐字节一致"
    fi
fi

# 2c) FFI 导出四件套（v1.04 增补）：@export 导出 / packed struct / volatile 指针 / bitcast。
#     这四项同属「把 Dial 接到系统底层」这一件事：
#       @export       —— Dial 函数以裸 C 符号名导出，C 侧能反过来调它；
#       packed struct —— 网络协议头的逐字节对齐（无 padding），与 C 的
#                       __attribute__((packed)) 完全一致；
#       volatile     —— 内存映射 IO / 硬件寄存器：每次读写必须真实落总线；
#       bitcast       —— 地址 ↔ 整数的位模式强转（底层内存强转）。
if ! ( cd "$FFI_DIR" && cc -O2 -c ffi_export_bridge.c -o ffi_export_bridge.o && \
        ar rcs libffi_export_bridge.a ffi_export_bridge.o ) >"${TMP}/ffiexpbuild.log" 2>&1; then
    echo "  ❌ FFI 导出专项 C 桥编译失败"; sed 's/^/       /' "${TMP}/ffiexpbuild.log"; fail=1
else
    if ! "$DIALC" build "$FFI_DIR/ffi_export_demo.dial" -o "${TMP}/ffi_export_demo" >"${TMP}/ffiexp.log" 2>&1; then
        echo "  ❌ FFI 导出专项程序编译失败"; sed 's/^/       /' "${TMP}/ffiexp.log"; fail=1
    elif ! timeout 30 "${TMP}/ffi_export_demo" >"${TMP}/ffiexpout.txt" 2>&1; then
        echo "  ❌ FFI 导出专项程序运行失败"; sed 's/^/       /' "${TMP}/ffiexpout.txt"; fail=1
    else
        # 布局等值性：前 16 行是 8 组「Dial 侧 / C 侧」对照，必须逐项相等。
        # 这里**刻意不写死期望值**——布局随平台变，「两边相等」才是真正要锁住的东西；
        # 若有人把 packed 悄悄改成不 packed，packed 组与普通组的数字会立刻分叉。
        if ! head -16 "${TMP}/ffiexpout.txt" | awk 'NR % 2 == 1 { dial = $1; next }
                 { if ($1 != dial) { printf "第 %d 行布局不一致: Dial=%s C=%s\n", NR-1, dial, $1; bad = 1 } }
                 END { exit bad }' >"${TMP}/ffiexpawk.txt" 2>&1; then
            echo "  ❌ packed struct / extern struct 布局与 C 不一致"; sed 's/^/       /' "${TMP}/ffiexpawk.txt"; fail=1
        else
            echo "  ✅ packed struct / extern struct 布局与 C 逐项一致（sizeof/alignof/offsetof）"
        fi
        # 后 4 行：@export 双向调用（C→Dial）+ volatile 往返 + bitcast 往返。
        if [ "$(tail -4 "${TMP}/ffiexpout.txt" | tr '\n' ' ')" = "42 42 42 9 " ]; then
            echo "  ✅ @export 双向调用（C→Dial）、volatile 往返、bitcast 往返全部正确"
        else
            echo "  ❌ @export / volatile / bitcast 的往返值不正确（期望 42 42 42 9）"
            sed 's/^/       /' "${TMP}/ffiexpout.txt"; fail=1
        fi
    fi

    # 2c-1) 符号可见性：@export 的函数必须是**全局**符号（能被 C 侧链接到），
    #        未标注 @export 的内部函数必须**不导出**（不得污染 C 侧的符号空间）。
    #        注：--no-link 的目标文件固定落在源文件同目录（-o 只作用于可执行文件）。
    if command -v nm >/dev/null 2>&1; then
        if ! "$DIALC" build "$FFI_DIR/ffi_export_demo.dial" --no-link >"${TMP}/ffiexpobj.log" 2>&1; then
            echo "  ❌ FFI 导出专项 --no-link 失败"; sed 's/^/       /' "${TMP}/ffiexpobj.log"; fail=1
        else
            EXPORTED=$(nm -g "$FFI_DIR/ffi_export_demo.o" 2>/dev/null | grep -c " T dial_add$")
            INTERNAL=$(nm "$FFI_DIR/ffi_export_demo.o" 2>/dev/null | grep -c "dial_internal_only")
            if [ "$EXPORTED" -ge 1 ] && [ "$INTERNAL" -eq 0 ]; then
                echo "  ✅ @export 函数以全局 C 符号暴露，未标注的内部函数不导出"
            else
                echo "  ❌ @export 符号可见性不对（dial_add 全局数=$EXPORTED，内部函数出现数=$INTERNAL）"; fail=1
            fi
        fi
    fi

    # 2c-2) volatile 的 IR 形态：易失访问必须真的是 volatile load/store。
    #        运行期看不出来（编译器没开优化），但 IR 里必须带 volatile 标记——
    #        这才是「每次访问都落总线」的真正保证。
    if DIALC_DUMP_IR=1 "$DIALC" build "$FFI_DIR/ffi_export_demo.dial" -o "${TMP}/ffi_export_demo2" >"${TMP}/ffiexpout2.log" 2>"${TMP}/ffiexp_ir.txt"; then
        if grep -q "load volatile" "${TMP}/ffiexp_ir.txt" && grep -q "store volatile" "${TMP}/ffiexp_ir.txt"; then
            echo "  ✅ volatile 指针的读写在 IR 里确实是 volatile load/store"
        else
            echo "  ❌ volatile 指针未生成 volatile load/store"; fail=1
        fi
    else
        echo "  ❌ volatile IR 检查用的编译失败"; sed 's/^/       /' "${TMP}/ffiexpout2.log"; fail=1
    fi
fi

# 2d) 底层原语（v1.04 增补）：@asm 内嵌汇编 与 simd<T, N> 原生向量。
#     运行期逐值断言只证明「结果对」；IR 形态断言才证明「真的是 LLVM inline asm
#     与原生向量指令」，而不是被编译器降级成标量模拟。两者缺一不可。
LOW_EXPECTED=$(cat <<'EOF'
42
42
77
200
11
14
32
16
16
5.000000
true
200
true
false
EOF
)
if ! "$DIALC" build "$FFI_DIR/ffi_lowlevel_demo.dial" -o "${TMP}/ffi_lowlevel_demo" >"${TMP}/ffilow.log" 2>&1; then
    echo "  ❌ 底层原语专项编译失败"; sed 's/^/       /' "${TMP}/ffilow.log"; fail=1
elif ! "${TMP}/ffi_lowlevel_demo" >"${TMP}/ffilowout.txt" 2>&1; then
    echo "  ❌ 底层原语程序运行失败"; sed 's/^/       /' "${TMP}/ffilowout.txt"; fail=1
elif ! printf '%s\n' "$LOW_EXPECTED" | diff -u - "${TMP}/ffilowout.txt" >"${TMP}/ffilowdiff.txt" 2>&1; then
    echo "  ❌ 底层原语输出不一致"; sed 's/^/       /' "${TMP}/ffilowdiff.txt"; fail=1
else
    echo "  ✅ @asm（AT&T / 匹配约束 / Intel 方言 / C ABI 宽度）与 simd 运算逐值正确"
fi

# IR 形态：必须是真正的 asm 调用 + 向量类型/指令。
if DIALC_DUMP_IR=1 "$DIALC" build "$FFI_DIR/ffi_lowlevel_demo.dial" -o "${TMP}/ffi_lowlevel_demo2" >"${TMP}/ffilow_ir.txt" 2>&1; then
    IR_OK=1
    grep -q "asm sideeffect"        "${TMP}/ffilow_ir.txt" || IR_OK=0
    grep -q "inteldialect"          "${TMP}/ffilow_ir.txt" || IR_OK=0
    grep -qE "add <4 x i64>"        "${TMP}/ffilow_ir.txt" || IR_OK=0
    grep -q "<4 x i8>"              "${TMP}/ffilow_ir.txt" || IR_OK=0
    grep -q "<2 x double>"          "${TMP}/ffilow_ir.txt" || IR_OK=0
    if [ "$IR_OK" -eq 1 ]; then
        echo "  ✅ IR 里是真正的 inline asm + 原生向量指令（非标量模拟）"
    else
        echo "  ❌ IR 形态不对（缺 asm / 向量指令）"; fail=1
    fi
else
    echo "  ❌ 底层原语 IR 检查用编译失败"; sed 's/^/       /' "${TMP}/ffilow_ir.txt"; fail=1
fi

# 3) 动态链接（v1.04 增补）：真实共享库必须能显式 link + 运行，且产物自带查找路径。
#    按平台选后缀与编译参数；PE（Windows）没有 rpath 概念，只验链接与运行。
case "$(uname -s)" in
    Linux*)  DYN_EXT=so;    DYN_FLAGS="-shared -fPIC" ;;
    Darwin*) DYN_EXT=dylib; DYN_FLAGS="-dynamiclib" ;;
    MINGW*|MSYS*|CYGWIN*) DYN_EXT=dll; DYN_FLAGS="-shared -Wl,--export-all-symbols" ;;
    *)       DYN_EXT=so;    DYN_FLAGS="-shared -fPIC" ;;
esac
DYN_LIB="${TMP}/libffi_dyn.${DYN_EXT}"
if ! cc -O2 $DYN_FLAGS "$FFI_DIR/ffi_dyn_demo.c" -o "$DYN_LIB" >"${TMP}/ffidynbuild.log" 2>&1; then
    echo "  ❌ FFI 动态库编译失败"; sed 's/^/       /' "${TMP}/ffidynbuild.log"; fail=1
else
    # link 用绝对路径（库编在 TMP 里）；extern fn 与静态库路径是同一套声明。
    cat > "${TMP}/ffi_dyn_demo.dial" <<EOF
extern fn dyn_add(a: int, b: int): int;
extern fn dyn_scale_five(v: int): int;
link "${DYN_LIB}";
fn main(): int {
    print(dyn_add(20, 22));
    print(dyn_scale_five(9));
    return 0;
}
EOF
    if ! "$DIALC" build "${TMP}/ffi_dyn_demo.dial" -o "${TMP}/ffi_dyn_demo" >"${TMP}/ffidyn.log" 2>&1; then
        echo "  ❌ 动态库 link 编译失败"; sed 's/^/       /' "${TMP}/ffidyn.log"; fail=1
    elif ! timeout 30 env -u LD_LIBRARY_PATH -u DYLD_LIBRARY_PATH "${TMP}/ffi_dyn_demo" >"${TMP}/ffidynout.txt" 2>&1; then
        echo "  ❌ 动态库程序运行失败（多半是 rpath 未生效）"; sed 's/^/       /' "${TMP}/ffidynout.txt"; fail=1
    elif ! diff -u "$FFI_DIR/ffi_dyn_demo.expected" "${TMP}/ffidynout.txt" >"${TMP}/ffidyndiff.txt" 2>&1; then
        echo "  ❌ 动态库程序输出不一致"; sed 's/^/       /' "${TMP}/ffidyndiff.txt"; fail=1
    else
        echo "  ✅ .${DYN_EXT} 动态链接端到端通过（explicit link + extern fn，无需 LD_LIBRARY_PATH）"
    fi
    # ELF：断言 RUNPATH 真的写进产物，而不是碰巧靠 DT_NEEDED 里的绝对路径跑起来。
    if [ "$(uname -s)" = "Linux" ] && command -v readelf >/dev/null 2>&1; then
        if readelf -d "${TMP}/ffi_dyn_demo" 2>/dev/null | grep -q "RUNPATH"; then
            echo "  ✅ ELF RUNPATH 已注入（显式动态库目录随产物走）"
        else
            echo "  ❌ ELF 产物缺少 RUNPATH，动态库查找路径未被写入"; fail=1
        fi
    fi
fi

# 3b) 伪装动态库：三种平台后缀各一份，一律按魔数拒绝（不是靠文件不存在蒙混过关）。
for ext in so dll dylib; do
    printf 'not a real shared object at all' > "${TMP}/libfake_${ext}.${ext}"
    cat > "${TMP}/ffi_link_fake_${ext}.dial" <<EOF
link "${TMP}/libfake_${ext}.${ext}";
fn main(): int { return 0; }
EOF
    if "$DIALC" build "${TMP}/ffi_link_fake_${ext}.dial" -o "${TMP}/ffi_fake_${ext}" >"${TMP}/ffifake_${ext}.log" 2>&1; then
        echo "  ❌ 伪装的 .${ext} 竟然被接受"; fail=1
    elif ! grep -q "魔数" "${TMP}/ffifake_${ext}.log"; then
        echo "  ❌ 伪装的 .${ext} 被拒绝，但诊断未说明魔数校验"; sed 's/^/       /' "${TMP}/ffifake_${ext}.log"; fail=1
    else
        echo "  ✅ 伪装的 .${ext} 按魔数拒绝"
    fi
done

# 4) ar 归档魔数：后缀对但内容不是静态库，也当场拒绝。
printf 'this is not an ar archive at all' > "${TMP}/fake.a"
cat > "${TMP}/ffi_link_fakea.dial" <<EOF
link "${TMP}/fake.a";
fn main(): int { return 0; }
EOF
if "$DIALC" build "${TMP}/ffi_link_fakea.dial" -o "${TMP}/ffi_fakea" >"${TMP}/ffifakea.log" 2>&1; then
    echo "  ❌ link 竟然接受了伪装的 .a"; fail=1
elif ! grep -q "ar 归档魔数" "${TMP}/ffifakea.log"; then
    echo "  ❌ 伪静态库拒绝的诊断信息缺失"; fail=1
else
    echo "  ✅ 伪装的 .a（缺 ar 魔数）被编译期拒绝"
fi

# 5) 清理构建产物，保持工作树干净（*.o 已被 .gitignore 忽略，但物理文件不该堆积）。
#    dialc build 默认也会在源码同目录留下一份 .o，一并清掉。
rm -f "$FFI_DIR/libffi_demo.a" "$FFI_DIR/libffi_export_bridge.a" "$FFI_DIR"/*.o


# =====================================================================
echo
echo "== Q) 工程工具链：CLI / 本地模块 / test / fmt / pack 闭环 / dialpkg src/ 随包源码模块 / doctor·deps·watch·clean·completion / 统一诊断（v1.04 增补，v1.10 扩展） =="

QDIR="${TMP}/qtool"
QHOME="${TMP}/qhome"
mkdir -p "$QDIR" "$QHOME"
# 包管理相关一律用隔离 HOME，绝不污染开发者真实的 ~/.dial/packages。
qrun() { env HOME="$QHOME" "$DIALC" "$@"; }
qpass() { echo "  ✅ $1"; }
qbad()  { echo "  ❌ $1"; fail=1; }
CV=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')

# 1) help / version / list / 未知命令
#    逐项核对每个子命令都在帮助里出现（用子命令名做词边界匹配，
#    不要依赖 "dialc pack" 这类并未出现在帮助文案里的字面串）。
qhelp=$(qrun help 2>&1)
qmissing=""
for sub in build run check test fmt new pack install list uninstall version lsp help; do
    echo "$qhelp" | grep -qE "(^|[[:space:]])${sub}([[:space:]]|$)" || qmissing="${qmissing} ${sub}"
done
if [ -z "$qmissing" ]; then qpass "help 列出全部子命令"; else qbad "help 缺子命令:${qmissing}"; fi
if [ "$(qrun --version 2>&1)" = "dialc $CV" ]; then qpass "version 取自 Cargo.toml（$CV）"; else qbad "version 与 Cargo.toml 不一致"; fi
if qrun list >/dev/null 2>&1; then qpass "list 在空缓存下正常返回"; else qbad "list 失败"; fi
if qrun frobnicate >/dev/null 2>&1; then qbad "未知命令竟然成功"; else qpass "未知命令非零退出"; fi

# 2) check：只做前端，不产出可执行文件
qout=$(qrun check examples/hello.dial 2>&1)
if echo "$qout" | grep -q "类型检查通过" && ! echo "$qout" | grep -q "已生成可执行文件"; then
    qpass "check 通过且不产出文件"
else qbad "check 输出异常"; fi
badone=$(ls tests/bad/*.dial | head -1)
if qrun check "$badone" >/dev/null 2>&1; then qbad "check 竟然接受了负向用例"; else qpass "check 拒绝负向用例"; fi
if qrun check examples/hello.dial --bogus >/dev/null 2>&1; then qbad "check 静默吞掉未知参数"; else qpass "check 拒绝未知参数"; fi

# 3) A3：多文件输入必须非零退出且绝不产出可执行文件
rm -f "${TMP}/q_a3"
if qrun build examples/hello.dial examples/flow.dial -o "${TMP}/q_a3" >"${TMP}/q_a3.log" 2>&1; then
    qbad "A3：多余 .dial 输入竟然编译通过"
elif [ -x "${TMP}/q_a3" ]; then
    qbad "A3：失败后仍产出可执行文件"
elif ! grep -q 'import "./xxx.dial"' "${TMP}/q_a3.log"; then
    qbad "A3：诊断缺本地模块指引"
else
    qpass "A3：多文件输入非零退出、不产物、附 import 指引"
fi

# 4) fmt：--check / 幂等 / 格式化后编译运行一致
cat > "${TMP}/q_messy.dial" <<'EOF'
fn main(): int {
var x: int = 1;
if (x > 0) {
print("yes");   
}
return 0;
}
EOF
if qrun fmt --check "${TMP}/q_messy.dial" >/dev/null 2>&1; then
    qbad "fmt --check 未识别未格式化文件"
else qpass "fmt --check 报告未格式化并非零退出"; fi
if qrun fmt "${TMP}/q_messy.dial" >/dev/null 2>&1 \
   && qrun fmt --check "${TMP}/q_messy.dial" >/dev/null 2>&1; then
    qpass "fmt 写入后幂等"
else qbad "fmt 写入或幂等自检失败"; fi
if qrun build "${TMP}/q_messy.dial" -o "${TMP}/q_messy" >/dev/null 2>&1 \
   && [ "$("${TMP}/q_messy" 2>/dev/null)" = "yes" ]; then
    qpass "fmt 后仍能编译运行且输出一致"
else qbad "fmt 改变了程序语义"; fi

# 4b) fmt 强化（v1.10）：间距规范化 + 原文保护 + --diff + 幂等
cat > "${TMP}/q_fmt2.dial" <<'EOF'
// 注释内  双空格必须原样保留
fn qadd2(a:int,b:int):int{return a+b;}
fn main(): int {
var s:string="a  b // not comment";
if(qadd2(1,2)>0){print("ok");}
return 0;
}
EOF
if qrun fmt --diff "${TMP}/q_fmt2.dial" >"${TMP}/q_fmt2.diff" 2>/dev/null; then
    qbad "fmt --diff 未对未格式化文件报非零"
else qpass "fmt --diff 逐行报告差异并非零退出"; fi
if grep -q '^+fn qadd2(a: int, b: int): int { return a + b; }' "${TMP}/q_fmt2.diff"; then
    qpass "fmt --diff 展示规范间距（运算符 / 逗号 / 冒号 / 大括号）"
else qbad "fmt --diff 未展示规范间距"; sed -n '1,12p' "${TMP}/q_fmt2.diff"; fi
if qrun fmt "${TMP}/q_fmt2.dial" >/dev/null 2>&1 \
   && grep -qF 'fn qadd2(a: int, b: int): int { return a + b; }' "${TMP}/q_fmt2.dial" \
   && grep -qF 'if (qadd2(1, 2) > 0) { print("ok"); }' "${TMP}/q_fmt2.dial" \
   && grep -qF '// 注释内  双空格必须原样保留' "${TMP}/q_fmt2.dial" \
   && grep -qF '"a  b // not comment"' "${TMP}/q_fmt2.dial" \
   && qrun fmt --check "${TMP}/q_fmt2.dial" >/dev/null 2>&1; then
    qpass "fmt 规范化间距且字面量 / 注释原文不变、幂等"
else qbad "fmt 强化断言失败"; cat "${TMP}/q_fmt2.dial"; fi
if qrun build "${TMP}/q_fmt2.dial" -o "${TMP}/q_fmt2" >/dev/null 2>&1 \
   && [ "$("${TMP}/q_fmt2" 2>/dev/null)" = "ok" ]; then
    qpass "fmt 强化后仍能编译运行且输出一致"
else qbad "fmt 强化改变了程序语义"; fi

# 5) test：通过 / 失败 / 过滤 / 失败带位置
cat > "${TMP}/q_tests.dial" <<'EOF'
fn qadd(a: int, b: int): int { return a + b; }
test "addition" { assert(qadd(1, 2) == 3, "add"); }
test "failure" { assert(1 == 2, "must fail"); }
fn main(): int { return 0; }
EOF
qrun test "${TMP}/q_tests.dial" > "${TMP}/q_test.out" 2>&1; qtrc=$?
if [ "$qtrc" -ne 0 ] && grep -q "✅ addition" "${TMP}/q_test.out" \
   && grep -q "❌ failure" "${TMP}/q_test.out" \
   && grep -Eq "q_tests\.dial:[0-9]+:[0-9]+" "${TMP}/q_test.out"; then
    qpass "test 逐块隔离、失败非零退出且带文件:行:列"
else qbad "test 汇总/隔离/位置诊断异常"; sed 's/^/       /' "${TMP}/q_test.out" | head -12; fi
if qrun test "${TMP}/q_tests.dial" --filter addition > "${TMP}/q_filter.out" 2>&1 \
   && grep -q "通过 1 / 失败 0" "${TMP}/q_filter.out"; then
    qpass "test --filter 只跑匹配的块"
else qbad "test --filter 异常"; fi

# 6) 本地源码模块：裸名 + 显式路径 + 嵌套依赖
cat > "${TMP}/q_util.dial" <<'EOF'
fn qhelper(): int { return 7; }
EOF
cat > "${TMP}/q_inner.dial" <<'EOF'
fn qinner(): int { return qhelper() * 2; }
EOF
cat > "${TMP}/q_main.dial" <<'EOF'
import "q_util";
import "./q_inner.dial";
fn main(): int { print(qinner()); return 0; }
EOF
if qrun build "${TMP}/q_main.dial" -o "${TMP}/q_main" >"${TMP}/q_main.log" 2>&1 \
   && [ "$("${TMP}/q_main" 2>/dev/null)" = "14" ]; then
    qpass "本地源码模块合并编译（裸名 + 显式路径 + 跨模块调用）"
else qbad "本地源码模块合并失败"; sed 's/^/       /' "${TMP}/q_main.log" | tail -6; fi
cat > "${TMP}/q_cyc_a.dial" <<'EOF'
import "./q_cyc_b.dial";
fn main(): int { return 0; }
EOF
cat > "${TMP}/q_cyc_b.dial" <<'EOF'
import "./q_cyc_a.dial";
fn qb(): int { return 0; }
EOF
if qrun check "${TMP}/q_cyc_a.dial" 2>&1 | grep -q "循环引用"; then qpass "循环引用被拒绝"; else qbad "循环引用未检出"; fi
cat > "${TMP}/q_dup.dial" <<'EOF'
fn qhelper(): int { return 1; }
EOF
cat > "${TMP}/q_main_dup.dial" <<'EOF'
import "./q_util.dial";
import "./q_dup.dial";
fn main(): int { return 0; }
EOF
if qrun check "${TMP}/q_main_dup.dial" 2>&1 | grep -q "重复定义"; then qpass "同名函数冲突被拒绝"; else qbad "同名函数冲突未检出"; fi
cat > "${TMP}/q_miss.dial" <<'EOF'
import "./nope.dial";
fn main(): int { return 0; }
EOF
if qrun check "${TMP}/q_miss.dial" 2>&1 | grep -q "指向的文件不存在"; then qpass "本地模块缺失给出明确诊断"; else qbad "缺失诊断不明确"; fi

# 7) new → pack → install → import → uninstall 全链路闭环
QPROJ="${TMP}/qproj"
if ! qrun new "$QPROJ" qdemo >"${TMP}/q_new.log" 2>&1; then
    qbad "new 生成骨架失败"; sed 's/^/       /' "${TMP}/q_new.log"
else
    qpass "new 生成 dial.toml / main.dial / .diali / lib/ 骨架"
fi
QTRIPLE=$(sed -n '/\[targets\]/,$p' "$QPROJ/dial.toml" 2>/dev/null | grep -E '^[A-Za-z0-9_-]+ = ' | head -1 | cut -d' ' -f1)
if [ -z "$QTRIPLE" ]; then qbad "dial.toml 未解析出 target triple"; fi
# abi 期望值从唯一真相 src/package.rs 动态读取，避免 ABI 递增时测试漂移。
QABI=$(sed -n 's/.*STDLIB_ABI: u64 = \([0-9]*\);.*/\1/p' src/package.rs | head -1)
if [ -z "$QABI" ]; then qbad "未能从 package.rs 读出 STDLIB_ABI"; fi
cat > "$QPROJ/qdemo.c" <<'EOF'
#include <stdint.h>
int64_t qdemo_add(int64_t a, int64_t b) { return a + b; }
EOF
mkdir -p "$QPROJ/lib/$QTRIPLE"
cc -O2 -c "$QPROJ/qdemo.c" -o "$QPROJ/qdemo.o" >/dev/null 2>&1 \
  && ar rcs "$QPROJ/lib/$QTRIPLE/libqdemo.a" "$QPROJ/qdemo.o"
if qrun pack "$QPROJ" -o "${TMP}/qdemo.dialpkg" >"${TMP}/q_pack.log" 2>&1 \
   && python3 -c 'import json,sys,zipfile; z=zipfile.ZipFile(sys.argv[1]); m=json.loads(z.read("manifest.txt")); assert m["name"]=="qdemo" and m["abi"]==int(sys.argv[2]), "abi=%r 期望 %s"%(m["abi"],sys.argv[2]); assert m["sha256"]; assert set(z.namelist())=={"manifest.txt","qdemo.diali"}|set(m["sha256"].keys())' "${TMP}/qdemo.dialpkg" "$QABI" >/dev/null 2>&1; then
    qpass "pack 生成合规 manifest + 逐文件 SHA256（自检通过）"
else qbad "pack 失败或产物不合规"; sed 's/^/       /' "${TMP}/q_pack.log" | tail -8; fi
if qrun install "${TMP}/qdemo.dialpkg" >/dev/null 2>&1; then qpass "install 吃下 pack 产物"; else qbad "install 拒绝了自己打的包"; fi
if qrun list 2>&1 | grep -q "qdemo"; then qpass "list 显示新装包"; else qbad "list 未显示新装包"; fi
cat > "${TMP}/q_use.dial" <<'EOF'
import "qdemo";
fn main(): int { print(qdemo_add(20, 22)); return 0; }
EOF
if qrun build "${TMP}/q_use.dial" -o "${TMP}/q_use" >"${TMP}/q_use.log" 2>&1 \
   && [ "$("${TMP}/q_use" 2>/dev/null)" = "42" ]; then
    qpass "import 调用包函数端到端（42）"
else qbad "import 闭环失败"; sed 's/^/       /' "${TMP}/q_use.log" | tail -8; fi
if qrun uninstall qdemo >/dev/null 2>&1 && ! qrun list 2>&1 | grep -q "qdemo"; then
    qpass "uninstall 删除包且 list 不再显示"
else qbad "uninstall 异常"; fi

# 8) pack 的 Fail-Fast：动态库 / 缺字段一律拒绝
QBAD="${TMP}/qbad"; mkdir -p "$QBAD/lib"
cp "$QPROJ/qdemo.diali" "$QBAD/" 2>/dev/null || true
printf 'not a shared object' > "$QBAD/lib/libfake.so"
cat > "$QBAD/dial.toml" <<EOF
name = "qbad"
version = "1.0.0"
abi = ${QABI}
iface = "qdemo.diali"
license = "MIT"
[targets]
$QTRIPLE = "lib/libfake.so"
EOF
if qrun pack "$QBAD" -o "${TMP}/qbad.dialpkg" >"${TMP}/qbad.log" 2>&1; then
    qbad "pack 竟然接受了动态库"
elif grep -q "静态" "${TMP}/qbad.log"; then
    qpass "pack 拒绝动态库（J 组铁律）"
else qbad "pack 拒绝动态库但诊断不明确"; fi
QMISS="${TMP}/qmiss"; mkdir -p "$QMISS"
cat > "$QMISS/dial.toml" <<'EOF'
name = "qmiss"
EOF
if qrun pack "$QMISS" >"${TMP}/qmiss.log" 2>&1; then
    qbad "pack 竟然接受了缺字段清单"
elif grep -q "缺少必填字段" "${TMP}/qmiss.log"; then
    qpass "pack 对缺字段清单显式报错"
else qbad "pack 缺字段诊断不明确"; fi

# 8b) pack 强化（v1.10）：--init / --dry-run / --json / --verify / 未登记提示 / 篡改拦截
QINIT="${TMP}/qinit"
rm -rf "$QINIT"; cp -r "$QPROJ" "$QINIT"
rm -f "$QINIT/dial.toml" "$QINIT/qdemo.dialpkg"
mkdir -p "$QINIT/src"
printf 'fn qi(): int { return 7; }\n' > "$QINIT/src/qi_oo.dial"
printf '// 故意不登记\nfn qx(): int { return 0; }\n' > "$QINIT/src/qx_extra.dial"
if qrun pack --init "$QINIT" >"${TMP}/q_init.log" 2>&1 \
   && grep -q 'name = "qinit"' "$QINIT/dial.toml" \
   && grep -q "abi = ${QABI}" "$QINIT/dial.toml" \
   && grep -qF 'qi_oo = "src/qi_oo.dial"' "$QINIT/dial.toml" \
   && grep -qF "lib/${QTRIPLE}/libqdemo.a" "$QINIT/dial.toml"; then
    qpass "pack --init 自动扫描生成清单草稿（iface / targets / src 全为显式声明）"
else qbad "pack --init 失败或草稿不完整"; cat "${TMP}/q_init.log"; fi
if qrun pack --init "$QINIT" >/dev/null 2>&1; then
    qbad "pack --init 竟然覆盖了已有清单"
else qpass "pack --init 拒绝覆盖已有 dial.toml"; fi
if qrun pack --dry-run "$QINIT" >"${TMP}/q_dry.log" 2>&1 \
   && grep -q '将入库' "${TMP}/q_dry.log" \
   && grep -qF 'src/qi_oo.dial' "${TMP}/q_dry.log" \
   && ! grep -q '已生成包' "${TMP}/q_dry.log"; then
    qpass "pack --dry-run 预览入库文件与 SHA256 且不产出包"
else qbad "pack --dry-run 预览失败"; cat "${TMP}/q_dry.log"; fi
if qrun pack --json "$QINIT" >"${TMP}/q_pjson.log" 2>&1 \
   && python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["written"] is False and len(d["files"])>=3 and d["name"]=="qinit"' "${TMP}/q_pjson.log" >/dev/null 2>&1; then
    qpass "pack --json 输出机器可读清单（written=false / 逐文件 SHA256）"
else qbad "pack --json 输出不合规"; head -10 "${TMP}/q_pjson.log"; fi
# 未登记提示：从清单里删掉一条 src 声明，dry-run 应提示「磁盘有但不进包」。
sed -i '/^qx_extra = /d' "$QINIT/dial.toml"
if qrun pack --dry-run "$QINIT" >"${TMP}/q_unlisted.log" 2>&1 \
   && grep -q '未登记' "${TMP}/q_unlisted.log" \
   && grep -qF 'src/qx_extra.dial' "${TMP}/q_unlisted.log"; then
    qpass "pack --dry-run 提示磁盘有但清单未登记的文件（绝不自动塞进包）"
else qbad "未登记提示缺失"; cat "${TMP}/q_unlisted.log"; fi
if qrun pack "$QINIT" -o "${TMP}/qinit.dialpkg" >"${TMP}/q_initpack.log" 2>&1 \
   && qrun pack --verify "${TMP}/qinit.dialpkg" >"${TMP}/q_verify.log" 2>&1 \
   && grep -q '逐文件 SHA256 全部通过' "${TMP}/q_verify.log"; then
    qpass "pack → --verify 用安装期同一套规则校验通过"
else qbad "pack --verify 校验失败"; cat "${TMP}/q_initpack.log" "${TMP}/q_verify.log"; fi
python3 - "${TMP}/qinit.dialpkg" "${TMP}/qinit_bad.dialpkg" <<'PY'
import zipfile, sys
src, dst = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(src) as z:
    files = {n: z.read(n) for n in z.namelist()}
files["src/qi_oo.dial"] = b"fn tampered(): int { return 9; }\n"
with zipfile.ZipFile(dst, "w") as z:
    for n, d in files.items():
        z.writestr(n, d)
PY
if qrun pack --verify "${TMP}/qinit_bad.dialpkg" >"${TMP}/q_verify_bad.log" 2>&1; then
    qbad "pack --verify 竟然放行了被篡改的包"
elif grep -q 'SHA256' "${TMP}/q_verify_bad.log"; then
    qpass "pack --verify 拦截篡改包且诊断含 SHA256"
else qbad "篡改包被拒但诊断不明确"; cat "${TMP}/q_verify_bad.log"; fi

# 9) A7：panic 运行时诊断带文件:行:列
cat > "${TMP}/q_panic.dial" <<'EOF'
fn main(): int {
    panic("boom");
    return 0;
}
EOF
if qrun build "${TMP}/q_panic.dial" -o "${TMP}/q_panic" >/dev/null 2>&1; then
    # 交给 sh 间接执行并让 sh 正常退出：bash 只有在直接等待被信号终止的
    # 子进程时才会打印 "Aborted" 作业提示；程序 stderr 仍完整写入 q_panic.err。
    sh -c '"$1" >/dev/null 2>"$2"; exit 0' _ "${TMP}/q_panic" "${TMP}/q_panic.err" || true
    if grep -q "位置:" "${TMP}/q_panic.err" \
       && grep -Eq "q_panic\.dial:[0-9]+:[0-9]+" "${TMP}/q_panic.err"; then
        qpass "panic 诊断含 文件:行:列"
    else qbad "panic 诊断缺位置"; sed 's/^/       /' "${TMP}/q_panic.err"; fi
else qbad "panic 用例编译失败"; fi

# 10) dialpkg src/：随包 Dial 源码模块（[src] 段显式声明 + 装包即用）
#     验证三级 import 的第二级：模块名直接从包缓存命中，无需复制源码到项目目录；
#     同时验证 src 文件与机器码同级 SHA256 保护、本地同名文件优先、Fail-Fast。
QSPROJ="${TMP}/qsrcproj"
if ! qrun new "$QSPROJ" qsrc >"${TMP}/qsrc_new.log" 2>&1; then
    qbad "new 生成 qsrc 骨架失败"; sed 's/^/       /' "${TMP}/qsrc_new.log" | tail -5
fi
if [ -d "$QSPROJ/src" ]; then
    qpass "dialc new 骨架预建空 src/ 目录"
else qbad "dialc new 骨架缺 src/ 目录"; fi
cat > "$QSPROJ/qsrc.c" <<'EOF'
#include <stdint.h>
int64_t qsrc_add(int64_t a, int64_t b) { return a + b; }
EOF
mkdir -p "$QSPROJ/lib/$QTRIPLE"
cc -O2 -c "$QSPROJ/qsrc.c" -o "$QSPROJ/qsrc.o" >/dev/null 2>&1 \
  && ar rcs "$QSPROJ/lib/$QTRIPLE/libqsrc.a" "$QSPROJ/qsrc.o"
# 显式声明（清单声明方式）：键 = import 裸名，值 = src/ 下的 .dial。
cat >> "$QSPROJ/dial.toml" <<'EOF'

[src]
qsrc_oo = "src/qsrc_oo.dial"
EOF
cat > "$QSPROJ/src/qsrc_oo.dial" <<'EOF'
// 随包分发的 Dial 源码模块：内部 import "qsrc" 会被前端汇总到第三级，
// 于是本模块既能用包里的 extern fn，又不必复制任何文件到用户项目。
import "qsrc";
fn qsrc_oo_twice(a: int): int { return qsrc_add(a, a); }
EOF
if qrun pack "$QSPROJ" -o "${TMP}/qsrc.dialpkg" >"${TMP}/qsrc_pack.log" 2>&1 \
   && python3 -c '
import json,sys,zipfile
z=zipfile.ZipFile(sys.argv[1]); m=json.loads(z.read("manifest.txt"))
assert m["src"]["qsrc_oo"]=="src/qsrc_oo.dial", m.get("src")
assert "src/qsrc_oo.dial" in z.namelist(), z.namelist()
assert "src/qsrc_oo.dial" in m["sha256"], m["sha256"]
assert set(z.namelist())=={"manifest.txt","qsrc.diali"}|set(m["sha256"].keys())
' "${TMP}/qsrc.dialpkg" >/dev/null 2>&1; then
    qpass "pack 把 [src] 源码写进包 + manifest.src 映射 + 逐文件 SHA256"
else qbad "pack 的 src/ 产物不合规"; sed 's/^/       /' "${TMP}/qsrc_pack.log" | tail -8; fi
if qrun install "${TMP}/qsrc.dialpkg" >/dev/null 2>&1 \
   && [ -f "$QHOME/.dial/packages/qsrc/src/qsrc_oo.dial" ]; then
    qpass "install 把 src/*.dial 落到包缓存（与 manifest/iface/lib 同级）"
else qbad "install 未落地 src/*.dial"; fi

mkdir -p "${TMP}/qsrclocal"
cat > "${TMP}/qsrclocal/main.dial" <<'EOF'
import "qsrc_oo";
fn main(): int { print(qsrc_oo_twice(21)); return 0; }
EOF
if qrun build "${TMP}/qsrclocal/main.dial" -o "${TMP}/qsrclocal/run" >"${TMP}/qsrc_use.log" 2>&1 \
   && [ "$("${TMP}/qsrclocal/run" 2>/dev/null)" = "42" ]; then
    qpass "裸名 import 自动命中包内 src 模块（不复制源码，内部 import 包名联动 = 42）"
else qbad "包内 src 模块自动 import 失败"; sed 's/^/       /' "${TMP}/qsrc_use.log" | tail -8; fi

# 篡改包内 Dial 源码：与机器码同级，必须被 SHA256 当场拦下。
QSRC_INSTALLED="$QHOME/.dial/packages/qsrc/src/qsrc_oo.dial"
cp "$QSRC_INSTALLED" "${TMP}/qsrc_oo.orig"
printf '\n// tampered\n' >> "$QSRC_INSTALLED"
if qrun build "${TMP}/qsrclocal/main.dial" -o "${TMP}/qsrclocal/run2" >"${TMP}/qsrc_tamper.log" 2>&1; then
    qbad "篡改包内 src 源码竟然编译通过"
elif grep -q "SHA256 校验失败" "${TMP}/qsrc_tamper.log"; then
    qpass "篡改包内 src 源码被 SHA256 拦截（与机器码同级保护）"
else qbad "篡改被拦截但诊断未指明 SHA256"; sed 's/^/       /' "${TMP}/qsrc_tamper.log" | tail -5; fi
cp "${TMP}/qsrc_oo.orig" "$QSRC_INSTALLED"

# 本地同名文件优先于包内副本（第一级 > 第二级）。
cat > "${TMP}/qsrclocal/qsrc_oo.dial" <<'EOF'
fn qsrc_oo_twice(a: int): int { return 999; }
EOF
if qrun build "${TMP}/qsrclocal/main.dial" -o "${TMP}/qsrclocal/run3" >"${TMP}/qsrc_local.log" 2>&1 \
   && [ "$("${TMP}/qsrclocal/run3" 2>/dev/null)" = "999" ]; then
    qpass "本地同名 .dial 优先于包内 src 副本（三级优先不变）"
else qbad "本地优先失效"; sed 's/^/       /' "${TMP}/qsrc_local.log" | tail -6; fi
rm -f "${TMP}/qsrclocal/qsrc_oo.dial"

# [src] 段非法声明一律 Fail-Fast（打包期，不拖到安装 / 编译期）。
qsrc_ff() {
    qsrc_line="$1"; qsrc_want="$2"
    qsrc_d="${TMP}/qsrc_ff"
    rm -rf "$qsrc_d"; cp -r "$QSPROJ" "$qsrc_d"
    sed -i "s|^qsrc_oo = .*|${qsrc_line}|" "$qsrc_d/dial.toml"
    if qrun pack "$qsrc_d" -o "${TMP}/qsrc_ff.dialpkg" >"${TMP}/qsrc_ff.log" 2>&1; then
        qbad "[src] 竟然接受非法声明: ${qsrc_line}"
    elif grep -q "$qsrc_want" "${TMP}/qsrc_ff.log"; then
        qpass "[src] Fail-Fast: ${qsrc_want}"
    else
        qbad "[src] 拒绝非法声明但诊断不明确: ${qsrc_line}"
    fi
}
qsrc_ff 'qsrc_oo = "qsrc_oo.dial"' '必须位于 src/ 目录下'
qsrc_ff 'qsrc_oo = "src/qsrc_oo.txt"' '必须以 .dial 结尾'
qsrc_ff 'qsrc_oo = "src/missing.dial"' '源码文件不存在'
qsrc_ff 'bad/name = "src/qsrc_oo.dial"' '必须是裸名'
# 向后兼容：完全没有 [src] 段的清单照常可打包 / 安装（第 7 步的 qdemo 已端到端验证）。
qrun uninstall qsrc >/dev/null 2>&1 || true



# 11) v1.10 工具链增补：doctor / deps / 统一诊断 / watch / clean / completion
#     共用 src/toolchain.rs 的命令元数据与本地源码递归（help / completion /
#     doctor 一份真相）；src/diagnostic.rs 统一前端错误渲染：人读模式原消息
#     一字不动只追加定位，DIALC_DIAG_FORMAT=json 输出一行 JSON 供机读，
#     stdout 始终不被诊断污染。

# 11.1 help 列出五个新命令
qnew_missing=""
for sub in doctor deps watch clean completion; do
    echo "$qhelp" | grep -qE "(^|[[:space:]])${sub}([[:space:]]|$)" || qnew_missing="${qnew_missing} ${sub}"
done
if [ -z "$qnew_missing" ]; then qpass "help 列出工具链新命令（doctor/deps/watch/clean/completion）"; else qbad "help 缺新命令:${qnew_missing}"; fi

# 11.2 doctor 人读：健康结论 + 七项检查
if qrun doctor >"${TMP}/q_doctor.log" 2>&1 \
   && grep -q '结论：健康' "${TMP}/q_doctor.log" \
   && grep -q '包内 src 模块解析' "${TMP}/q_doctor.log" \
   && grep -q '已安装包完整性' "${TMP}/q_doctor.log"; then
    qpass "doctor 人读自检（健康结论 + 编译器/C 编译器/缓存/包完整性/清单/src 解析）"
else qbad "doctor 人读输出异常"; sed 's/^/       /' "${TMP}/q_doctor.log" | tail -10; fi

# 11.3 doctor --json：机器可读、healthy 且 0 失败
if qrun doctor --json >"${TMP}/q_doctor.json" 2>&1 \
   && python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
assert d["healthy"] is True and d["failures"]==0, d
assert len(d["checks"])>=7, d["checks"]
assert all(set(c)=={"name","status","detail"} for c in d["checks"])
' "${TMP}/q_doctor.json" >/dev/null 2>&1; then
    qpass "doctor --json 可解析（healthy / failures / 7 项检查结构）"
else qbad "doctor --json 结构不合规"; sed 's/^/       /' "${TMP}/q_doctor.json" | tail -8; fi

# 11.4 deps：三级 import 全部分类（本地源码 / 包内 src / 包名）
#      qsrc 包（带 [src] 段）在 10) 末尾已卸载，这里重装一次专供 deps 命中第二级。
QDEPS="${TMP}/qdeps"
mkdir -p "$QDEPS"
qrun install "${TMP}/qsrc.dialpkg" >/dev/null 2>&1
cat > "$QDEPS/main.dial" <<'EOF'
import "./util.dial";
import "qsrc_oo";
import "gui";
fn main(): int { return 0; }
EOF
cat > "$QDEPS/util.dial" <<'EOF'
fn util(): int { return 1; }
EOF
if qrun deps "$QDEPS/main.dial" >"${TMP}/q_deps.log" 2>&1 \
   && grep -q 'ROOT' "${TMP}/q_deps.log" \
   && grep -q '本地' "${TMP}/q_deps.log" \
   && grep -q '包内src' "${TMP}/q_deps.log" \
   && grep -q '包名' "${TMP}/q_deps.log"; then
    qpass "deps 三级 import 分类齐全（本地源码 / 包内 src / 包名）"
else qbad "deps 三级分类缺失"; sed 's/^/       /' "${TMP}/q_deps.log" | tail -12; fi

# 11.5 deps --json：节点结构 + 深度 + cycles 字段
if qrun deps "$QDEPS/main.dial" --json >"${TMP}/q_deps.json" 2>&1 \
   && python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
assert d["root"].endswith("main.dial") and d["cycles"] is False, d
kinds={n["kind"] for n in d["nodes"]}
assert {"root","local","pkg-src","package"}<=kinds, kinds
assert all(set(n)=={"kind","spec","path","depth","cyclic"} for n in d["nodes"])
' "${TMP}/q_deps.json" >/dev/null 2>&1; then
    qpass "deps --json 节点结构 / 三级 kind / 深度 / cycles"
else qbad "deps --json 结构不合规"; sed 's/^/       /' "${TMP}/q_deps.json" | tail -8; fi

# 11.6 deps 循环引用检测（本地模块互相 import）
cat > "$QDEPS/cyc_a.dial" <<'EOF'
import "./cyc_b.dial";
fn ca(): int { return 0; }
EOF
cat > "$QDEPS/cyc_b.dial" <<'EOF'
import "./cyc_a.dial";
fn cb(): int { return 0; }
EOF
if qrun deps "$QDEPS/cyc_a.dial" --json >"${TMP}/q_deps_cyc.json" 2>&1 \
   && python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
assert d["cycles"] is True, d
assert any(n["cyclic"] for n in d["nodes"]), d["nodes"]
' "${TMP}/q_deps_cyc.json" >/dev/null 2>&1; then
    qpass "deps 检出本地模块循环引用并停止递归"
else qbad "deps 循环引用未检出"; sed 's/^/       /' "${TMP}/q_deps_cyc.json" | tail -8; fi

# 11.7 统一诊断（人读）：原消息一字不动 + 定位/稳定错误码/源码行/caret
cat > "${TMP}/q_diag_bad.dial" <<'EOF'
fn main(): int {
    var x: int = "字符串";
    return x;
}
EOF
if qrun check "${TMP}/q_diag_bad.dial" >"${TMP}/q_diag_h.out" 2>"${TMP}/q_diag_h.err"; then
    qbad "诊断负向用例竟然通过检查"
elif grep -q '语义错误' "${TMP}/q_diag_h.err" \
     && grep -q -- '--> .*q_diag_bad.dial:2:5' "${TMP}/q_diag_h.err" \
     && grep -q '\[dialc.sema\]' "${TMP}/q_diag_h.err" \
     && grep -q '| .*\^' "${TMP}/q_diag_h.err"; then
    qpass "诊断人读：原消息不变 + 文件:行:列 + 稳定错误码 + 源码行 + caret"
else qbad "诊断人读缺定位信息"; sed 's/^/       /' "${TMP}/q_diag_h.err" | tail -10; fi

# 11.8 诊断绝不污染 stdout（stdout 是协议/数据通道）
if [ ! -s "${TMP}/q_diag_h.out" ]; then qpass "诊断失败时 stdout 保持干净（诊断只走 stderr）"; else qbad "诊断写进了 stdout"; fi

# 11.9 统一诊断（机读）：一行合法 JSON + 结构化字段
if env HOME="$QHOME" DIALC_DIAG_FORMAT=json "$DIALC" check "${TMP}/q_diag_bad.dial" \
        >"${TMP}/q_diag_j.out" 2>"${TMP}/q_diag_j.err"; then
    qbad "JSON 诊断模式下负向用例竟然通过"
elif [ "$(wc -l < "${TMP}/q_diag_j.err")" -eq 1 ] \
     && python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
assert d["tool"]=="dialc" and d["severity"]=="error", d
assert d["code"]=="dialc.sema" and d["line"]==2 and d["col"]==5, d
assert "字符串" in d["snippet"] and d["caret"].endswith("^"), d
assert d["message"] and d["file"].endswith("q_diag_bad.dial"), d
' "${TMP}/q_diag_j.err" >/dev/null 2>&1 \
     && [ ! -s "${TMP}/q_diag_j.out" ]; then
    qpass "DIALC_DIAG_FORMAT=json 输出一行 JSON（code/line/col/snippet/caret）且 stdout 干净"
else qbad "JSON 诊断不合规"; sed 's/^/       /' "${TMP}/q_diag_j.err" | tail -10; fi

# 11.10 watch：首次执行 + 可限拍正常退出（不引入无限循环）
if qrun watch examples/hello.dial --action check --interval 60 --max-ticks 2 \
        >"${TMP}/q_watch.log" 2>&1 \
   && grep -q '首次执行 check' "${TMP}/q_watch.log" \
   && grep -q '类型检查通过' "${TMP}/q_watch.log" \
   && grep -q '达到 --max-ticks 2' "${TMP}/q_watch.log"; then
    qpass "watch 首次执行 + --max-ticks 限拍正常退出"
else qbad "watch 限拍执行异常"; sed 's/^/       /' "${TMP}/q_watch.log" | tail -10; fi
if qrun watch examples/hello.dial --action frobnicate --max-ticks 1 \
        >"${TMP}/q_watch_bad.log" 2>&1; then
    qbad "watch 竟然接受未知动作"
else qpass "watch 拒绝未知动作（只认 check / build / test）"; fi

# 11.11 clean：默认 dry-run / --json / --apply 只删产物不动源码
QCLEAN="${TMP}/qclean"
mkdir -p "$QCLEAN"
for f in a.o b.bc c.ll keep.dial p.dialpkg; do : > "$QCLEAN/$f"; done
if qrun clean "$QCLEAN" >"${TMP}/q_clean_dry.log" 2>&1 \
   && grep -q 'dry-run' "${TMP}/q_clean_dry.log" \
   && [ -f "$QCLEAN/a.o" ] && [ -f "$QCLEAN/keep.dial" ]; then
    qpass "clean 默认 dry-run（列出产物但一个都不删）"
else qbad "clean dry-run 误删或输出异常"; sed 's/^/       /' "${TMP}/q_clean_dry.log" | tail -8; fi
if qrun clean "$QCLEAN" --json >"${TMP}/q_clean.json" 2>&1 \
   && python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
assert d["applied"] is False and len(d["targets"])==4, d
assert d["removed"]==0
' "${TMP}/q_clean.json" >/dev/null 2>&1; then
    qpass "clean --json 列出 4 个产物且 applied=false"
else qbad "clean --json 结构不合规"; sed 's/^/       /' "${TMP}/q_clean.json" | tail -8; fi
if qrun clean "$QCLEAN" --apply >"${TMP}/q_clean_apply.log" 2>&1 \
   && [ ! -f "$QCLEAN/a.o" ] && [ ! -f "$QCLEAN/p.dialpkg" ] \
   && [ -f "$QCLEAN/keep.dial" ] && grep -q '已删除 4 个' "${TMP}/q_clean_apply.log"; then
    qpass "clean --apply 只删 .o/.bc/.ll/.dialpkg，保留 .dial 源码"
else qbad "clean --apply 删除范围不对"; sed 's/^/       /' "${TMP}/q_clean_apply.log" | tail -8; fi

# 11.12 completion：三种 shell + 拒绝未知 shell + 管道截断不 panic
for sh in bash zsh fish; do
    if qrun completion "$sh" >"${TMP}/q_comp_${sh}" 2>&1 \
       && grep -q 'doctor' "${TMP}/q_comp_${sh}" \
       && grep -q 'completion' "${TMP}/q_comp_${sh}"; then
        qpass "completion ${sh} 生成脚本且含全部命令名"
    else qbad "completion ${sh} 输出异常"; fi
done
if qrun completion powershell >/dev/null 2>&1; then
    qbad "completion 竟然接受未知 shell"
else qpass "completion 拒绝未知 shell（只认 bash / zsh / fish）"; fi
if qrun completion bash 2>"${TMP}/q_comp_pipe.err" | head -1 >/dev/null \
   && ! grep -q 'panicked' "${TMP}/q_comp_pipe.err"; then
    qpass "completion 输出被 head 截断不触发 Broken pipe panic"
else qbad "completion 管道截断仍 panic"; cat "${TMP}/q_comp_pipe.err"; fi
qrun uninstall qsrc >/dev/null 2>&1 || true

# =====================================================================
echo
echo "== R) DWARF 调试信息：-g / GDB 源码行断点 / print 变量名（v1.04 增补） =="

RDIR="${TMP}/rdwarf"
mkdir -p "$RDIR"
cat > "${RDIR}/dbg.dial" <<'EOF'
fn add(a: int, b: int): int {
    let sum: int = a + b;
    return sum;
}

fn main(): int {
    let counter: int = 42;
    let note: string = "dial";
    print(add(counter, 1));
    return 0;
}
EOF
rpass() { echo "  ✅ $1"; }
rbad()  { echo "  ❌ $1"; fail=1; }
rskip() { echo "  ⏭️  $1"; }

# 探测 dwarfdump：不同环境的 LLVM 工具带不同版本后缀，缺工具时明确跳过而非误报失败。
DDUMP=""
for cand in llvm-dwarfdump-18 llvm-dwarfdump llvm-dwarfdump-17 llvm-dwarfdump-16; do
    if command -v "$cand" >/dev/null 2>&1; then DDUMP="$cand"; break; fi
done

# 1) -g 编译 + 运行输出不变（不依赖任何调试工具，必测）
if "$DIALC" build "${RDIR}/dbg.dial" -g -o "${RDIR}/dbg" >"${RDIR}/build.log" 2>&1 \
   && [ "$("${RDIR}/dbg")" = "43" ]; then
    rpass "-g 编译且运行输出不变（43）"
else
    rbad "-g 编译或运行失败"; sed 's/^/       /' "${RDIR}/build.log"
fi
if "$DIALC" help 2>&1 | grep -q -- '-g / --debug-info'; then
    rpass "help 列出 -g 选项"
else rbad "help 缺 -g 选项"; fi

if [ -n "$DDUMP" ]; then
    # 2) DWARF 段
    if readelf -S "${RDIR}/dbg" 2>/dev/null | grep -q '\.debug_info' \
       && readelf -S "${RDIR}/dbg" 2>/dev/null | grep -q '\.debug_line'; then
        rpass "产物带 .debug_info / .debug_line 段"
    else rbad "产物缺 DWARF 段"; fi

    # 3) Dial 函数名 / 形参名 / 局部变量名都写进了 DWARF
    "$DDUMP" --debug-info "${RDIR}/dbg" > "${RDIR}/di.log" 2>&1
    dmiss=""
    for sym in add main a b sum counter note; do
        grep -q "DW_AT_name.*\"${sym}\"" "${RDIR}/di.log" || dmiss="${dmiss} ${sym}"
    done
    if [ -z "$dmiss" ]; then rpass "DWARF 含 Dial 函数名 / 形参名 / 局部变量名"
    else rbad "DWARF 缺 Dial 符号:${dmiss}"; fi

    # 4) 行表里有 Dial 源文件（gdb break file:N 的前提）
    if "$DDUMP" --debug-line "${RDIR}/dbg" 2>/dev/null | grep -q 'dbg.dial'; then
        rpass "DWARF 行表含 Dial 源文件"
    else rbad "DWARF 行表缺 Dial 源文件"; fi

    # 6) 负向：不带 -g 时不产 Dial 调试符号。
    #    注：C Runtime 由 zig cc 编译，本身就带 .debug_* 段，所以不能拿「有无段」
    #    当判据，只能精确看有没有 Dial 的源码名 / 变量名。
    if "$DIALC" build "${RDIR}/dbg.dial" -o "${RDIR}/plain" >/dev/null 2>&1; then
        "$DDUMP" --debug-info "${RDIR}/plain" > "${RDIR}/plain.di" 2>&1
        if grep -q 'DW_AT_name.*"counter"' "${RDIR}/plain.di" \
           || "$DDUMP" --debug-line "${RDIR}/plain" 2>/dev/null | grep -q 'dbg.dial'; then
            rbad "无 -g 也混入了 Dial 调试符号"
        else
            rpass "无 -g 时不产 Dial 调试符号（零开销）"
        fi
    else rbad "无 -g 基线编译失败"; fi
else
    rskip "静态 DWARF 检查（环境无 llvm-dwarfdump）"
fi

if command -v gdb >/dev/null 2>&1; then
    # 5) GDB 按 Dial 源码行断点，并用 Dial 变量名 print（本组核心）
    gdb -q -batch -ex "set pagination off" \
        -ex "break ${RDIR}/dbg.dial:9" -ex "run" \
        -ex "print counter" -ex "bt" \
        "${RDIR}/dbg" > "${RDIR}/gdb.log" 2>&1
    if grep -qF '$1 = 42' "${RDIR}/gdb.log" \
       && grep -qF 'Breakpoint 1, main () at' "${RDIR}/gdb.log"; then
        rpass "GDB 按源码行断点命中且 print counter = 42"
    else
        rbad "GDB 断点/print 失败"; sed 's/^/       /' "${RDIR}/gdb.log" | tail -20
    fi
    if grep -q 'dbg.dial:9' "${RDIR}/gdb.log"; then
        rpass "GDB backtrace 显示 Dial 源码行"
    else rbad "GDB backtrace 缺源码行"; fi
else
    rskip "GDB 交互检查（环境无 gdb）"
fi

# 7) -g 与 --fast 同用不应阻断构建，且要提示 O3 下变量可能被优化掉
if "$DIALC" build "${RDIR}/dbg.dial" -g --fast -o "${RDIR}/fastg" >"${RDIR}/fast.log" 2>&1 \
   && [ "$("${RDIR}/fastg")" = "43" ] \
   && grep -q '<optimized out>' "${RDIR}/fast.log"; then
    rpass "-g 与 --fast 同用可构建且给出 O3 提示"
else
    rbad "-g 与 --fast 同用异常"; sed 's/^/       /' "${RDIR}/fast.log" | tail -12
fi


# ===========================================================================
# S) 性能基准套件（v1.04 增补）：秒级快速正确性校验
#    完整 2 分钟计时采集交给 CI 独立步骤；这里只验证「4 语言等价实现 + 交付
#    数据/网页配套」，避免拖慢常规回归。缺 gcc / go 时对应子项明确跳过。
# ===========================================================================
echo
echo "== S) 性能基准：4 语言 × 28 维校验和与交付数据（v1.04 增补） =="
spass() { echo "  ✅ $1"; }
sbad()  { echo "  ❌ $1"; fail=1; }
sskip() { echo "  ⏭️  $1"; }

BDIR="bench"
SDIR="${TMP}/sbench"
mkdir -p "$SDIR"

# 1) 交付数据完整性 + 网页/数据同步（纯 Python，无外部工具依赖）
if python3 tools/check_bench_data.py \
     --data "${BDIR}/data.json" --html web/bench.html >"${SDIR}/check.log" 2>&1; then
    spass "data.json 28×4 / 6 维 / 6 雷达轴 / 校验和齐全，且网页与数据同步"
else
    sbad "交付数据或网页校验失败"; sed 's/^/       /' "${SDIR}/check.log"
fi

# 2) 网页含六维雷达图（SVG）、4 语言多边形、C=1.00 基准
if grep -q '<svg' web/bench.html \
   && [ "$(grep -c '<polygon' web/bench.html)" -ge 8 ] \
   && grep -q 'C = 1.00' web/bench.html \
   && grep -q '六维工程能力雷达图' web/bench.html; then
    spass "网页含六维 SVG 雷达图（4 语言多边形 + 归一化基准）"
else
    sbad "网页缺六维雷达图或比值基准"
fi

# 3) 4 语言基准程序编译 + 代表性任务校验和一致（抽 6 项覆盖五类）
if command -v gcc >/dev/null 2>&1 && command -v go >/dev/null 2>&1; then
    C_OK=1
    gcc -O2 -o "${SDIR}/bench_c" "${BDIR}/bench.c" 2>"${SDIR}/c.log" || C_OK=0
    (cd "$BDIR" && GO111MODULE=off GOCACHE="${SDIR}/gocache" \
        go build -o "${SDIR}/bench_go" bench.go) 2>"${SDIR}/go.log" || C_OK=0
    "$DIALC" build "${BDIR}/bench.dial" --fast -o "${SDIR}/bench_dial" \
        >"${SDIR}/dial.log" 2>&1 || C_OK=0
    if [ "$C_OK" -eq 1 ]; then
        spass "C / Go / Dial 三份基准源码编译成功（含 Dial --fast）"
        # 抽验 b01/b09/b14/b19/b24/b28 六项（算术/递归/数组/字符串/位运算/综合）
        mismatch=0
        for t in b01 b09 b14 b19 b24 b28; do
            vc=$("${SDIR}/bench_c" "$t" 2>/dev/null | tail -1)
            vg=$("${SDIR}/bench_go" "$t" 2>/dev/null | tail -1)
            vd=$("${SDIR}/bench_dial" "$t" 2>/dev/null | tail -1)
            vp=$(python3 "${BDIR}/bench.py" "$t" 2>/dev/null | tail -1)
            if [ "$vc" != "$vg" ] || [ "$vc" != "$vd" ] || [ "$vc" != "$vp" ] \
               || [ -z "$vc" ]; then
                echo "       $t: C=$vc Go=$vg Dial=$vd Py=$vp"
                mismatch=$((mismatch+1))
            fi
        done
        if [ "$mismatch" -eq 0 ]; then
            spass "抽验 6 项 × 4 语言校验和完全一致（等价计算锚点）"
        else
            sbad "校验和不一致（$mismatch 项）"
        fi
        # repeat 自校准入口：rep=3 校验和不变（纯函数式任务）
        vr_c=$("${SDIR}/bench_c" b14 3 2>/dev/null | tail -1)
        vr_d=$("${SDIR}/bench_dial" b14 3 2>/dev/null | tail -1)
        v1_c=$("${SDIR}/bench_c" b14 2>/dev/null | tail -1)
        if [ "$vr_c" = "$v1_c" ] && [ "$vr_d" = "$v1_c" ]; then
            spass "repeat 自校准入口可用且校验和不漂移"
        else
            sbad "repeat 参数破坏校验和一致性"
        fi
    else
        sbad "基准源码编译失败"; tail -6 "${SDIR}/c.log" "${SDIR}/go.log" "${SDIR}/dial.log"
    fi
else
    sskip "4 语言编译抽验（环境缺 gcc 或 go）"
fi

echo "== T) B/C 组缺口第一批：块注释 / ! / .slice() / get_or / eprint（v1.04 增补） =="
tpass() { echo "  ✅ $1"; }
tbad()  { echo "  ❌ $1"; fail=1; }
TDIR="${TMP}/tbatch1"
mkdir -p "$TDIR"

# 1) 冒烟：5 项一次性编译 + 运行（stdout / stderr 分流核对）
cp tests/batch1_smoke.dial "${TDIR}/smoke.dial"
if "$DIALC" build "${TDIR}/smoke.dial" -o "${TDIR}/smoke" >"${TDIR}/build.log" 2>&1 \
   && "${TDIR}/smoke" >"${TDIR}/out" 2>"${TDIR}/err"; then
    tpass "第一批冒烟程序编译运行成功"
else
    tbad "第一批冒烟程序编译或运行失败"; tail -8 "${TDIR}/build.log"
fi
if grep -q "^B10 !false = true / not false = true$" "${TDIR}/out" \
   && grep -q "^B10 !true = false / 3 != 4 = true$" "${TDIR}/out"; then
    tpass "B10：! 作一元逻辑非，!= 仍是不等号（零语义改动）"
else tbad "B10 输出不符"; fi
if grep -q "^B11 slice len = 3 / bracket len = 3$" "${TDIR}/out" \
   && grep -q "^B11 slice = 20,30,40$" "${TDIR}/out"; then
    tpass "B11：.slice(1,4) 与 a[1..4] 等价"
else tbad "B11 .slice 结果不符"; fi
if grep -q "^C6 map hit = 100$" "${TDIR}/out" \
   && grep -q "^C6 map real -1 = -1$" "${TDIR}/out" \
   && grep -q "^C6 map miss = 999$" "${TDIR}/out"; then
    tpass "C6：map.get_or 区分「未命中」与「真实值 == -1」"
else tbad "C6 map.get_or 结果不符"; fi
if grep -q "^C6 kv hit = 7$" "${TDIR}/out" \
   && grep -q "^C6 kv miss = 0$" "${TDIR}/out"; then
    tpass "C6：泛型 map<string,int>.get_or 命中/未命中正确"
else tbad "C6 kvmap.get_or 结果不符"; fi
if grep -q "^C8 stdout still clean$" "${TDIR}/out" \
   && grep -q "^C8 eprint line$" "${TDIR}/err" \
   && grep -q "^C8 42 3.500000 true$" "${TDIR}/err" \
   && grep -q "^C8 raw joined$" "${TDIR}/err"; then
    tpass "C8：eprint/eprint_raw 全部写 stderr，stdout 保持干净"
else tbad "C8 stdout/stderr 分流不符"; fi
# stdout 绝不能混入 stderr 内容（分流铁律）
if grep -q "eprint line\|raw joined" "${TDIR}/out"; then
    tbad "C8：stderr 内容泄漏到 stdout"
else tpass "C8：stderr 内容零泄漏到 stdout"; fi

# 2) B9 负向：未闭合块注释必须 Fail-Fast 报错，绝不静默吞掉源码
cat > "${TDIR}/unclosed.dial" <<'EOF'
/* 这个块注释故意不闭合
fn main(): int { print("should not compile"); return 0; }
EOF
if "$DIALC" build "${TDIR}/unclosed.dial" -o "${TDIR}/unclosed" >"${TDIR}/unclosed.log" 2>&1; then
    tbad "B9：未闭合块注释竟被接受（静默吞源码）"
elif grep -q "未闭合" "${TDIR}/unclosed.log"; then
    tpass "B9：未闭合块注释 Fail-Fast 报错且不产可执行文件"
else tbad "B9：未闭合块注释报错但诊断不含「未闭合」"; fi

# 3) B9：块注释内有 `*/` 边界，正常闭合且不影响整除/乘法
cat > "${TDIR}/block.dial" <<'EOF'
/* 块注释内含 // 与 * 号，均不产生 token */
fn main(): int {
    var a: int = 6 / 2 * 3;   /* 9 */
    print(a);
    return 0;
}
EOF
if "$DIALC" build "${TDIR}/block.dial" -o "${TDIR}/block" >"${TDIR}/block.log" 2>&1 \
   && [ "$("${TDIR}/block" 2>/dev/null)" = "9" ]; then
    tpass "B9：块注释跨行且不影响 / 与 * 运算符"
else tbad "B9：块注释程序结果不符"; tail -6 "${TDIR}/block.log"; fi

# 4) C6 负向：泛型 map 的 get_or 参数类型必须严格匹配（零隐式转换铁律）
cat > "${TDIR}/getor_bad.dial" <<'EOF'
fn main(): int {
    var kv: map<string, int> = new_kv_map();
    kv.put("a", 7);
    print(kv.get_or(3, 0));
    return 0;
}
EOF
if "$DIALC" build "${TDIR}/getor_bad.dial" -o "${TDIR}/getor_bad" >"${TDIR}/getor_bad.log" 2>&1; then
    tbad "C6：get_or 键类型不匹配竟被接受（违反零隐式转换）"
elif grep -q "get_or" "${TDIR}/getor_bad.log"; then
    tpass "C6：get_or 键类型严格校验"
else tbad "C6：get_or 类型报错但诊断未提到 get_or"; fi

# 5) C6 负向：get_or 的默认值类型必须与 V 完全一致（零隐式转换铁律）。
#    注：句柄值 V（map<string,string>）本就被 §29.5 的容器类型系统拒绝（B6 缺口），
#    因此 get_or 的「只支持标量 V」分支是**不可达的防御代码**——这里改测真正会
#    触发的边界：默认值传错类型。
cat > "${TDIR}/getor_deftype.dial" <<'EOF'
fn main(): int {
    var kv: map<string, int> = new_kv_map();
    kv.put("a", 7);
    print(kv.get_or("a", "wrong"));
    return 0;
}
EOF
if "$DIALC" build "${TDIR}/getor_deftype.dial" -o "${TDIR}/getor_deftype" >"${TDIR}/getor_deftype.log" 2>&1; then
    tbad "C6：get_or 默认值类型不匹配竟被接受（违反零隐式转换）"
elif grep -q "get_or" "${TDIR}/getor_deftype.log"; then
    tpass "C6：get_or 默认值类型严格校验（零隐式转换铁律）"
else tbad "C6：get_or 默认值类型报错但诊断未提到 get_or"; fi

# 6) B4/B5 宪法否决：闭包与函数重载仍不可用（永久否决项不入语言）
cat > "${TDIR}/no_overload.dial" <<'EOF'
fn f(x: int): int { return x; }
fn f(x: string): string { return x; }
fn main(): int { return 0; }
EOF
if "$DIALC" build "${TDIR}/no_overload.dial" -o "${TDIR}/no_overload" >"${TDIR}/no_overload.log" 2>&1; then
    tbad "宪法：函数名重载竟被接受（B5 永久否决）"
elif grep -q "重复定义\|重复" "${TDIR}/no_overload.log"; then
    tpass "宪法：函数重载仍被拒绝（B5 永久否决，ABI 稳定）"
else tbad "宪法：函数重载报错文本异常"; fi

# 7) C8 配套铁律：输出族内建按名字硬编码分派，用户不得定义同名函数。
#    否则 `fn eprint(...)` 能编译却在调用点被内建分支静默绕过（§20 Fail-Fast）。
cat > "${TDIR}/no_shadow.dial" <<'EOF'
fn eprint(x: int): int { return x; }
fn main(): int { print(eprint(3)); return 0; }
EOF
if "$DIALC" build "${TDIR}/no_shadow.dial" -o "${TDIR}/no_shadow" >"${TDIR}/no_shadow.log" 2>&1; then
    tbad "C8：输出族内建可被同名函数定义静默绕过（违反 Fail-Fast）"
elif grep -q "保留名" "${TDIR}/no_shadow.log"; then
    tpass "C8：输出族内建进入保留名清单（同名定义编译期拒绝）"
else tbad "C8：同名定义被拒但诊断未提到保留名"; fi

echo
echo "== U) B/C 组缺口第二批：标准库 API 全量（C1/C4/C2/C3/C5/C7/C9/C10，v1.04 增补） =="
upass() { echo "  ✅ $1"; }
ubad()  { echo "  ❌ $1"; fail=1; }

# 1) 正向：复用 batch2_smoke.dial，断言代表性输出（覆盖 8 组 API 的关键语义）。
if "$DIALC" build tests/batch2_smoke.dial -o "${TMP}/u_smoke" >"${TMP}/u_smoke.log" 2>&1; then
    out=$("${TMP}/u_smoke" 2>/dev/null)
    # 关键断言：UTF-8 码点语义 / 类型安全解析 / 数组 COW 写回 / 文件抛出路径 / 环境快照
    if printf '%s\n' "$out" | grep -q 'C1 upper = HELLO WORLD' \
       && printf '%s\n' "$out" | grep -q 'C4 char_count = 3' \
       && printf '%s\n' "$out" | grep -q 'C4 char_at(1) = 20013' \
       && printf '%s\n' "$out" | grep -q 'C4 char_slice(1,2) = 中' \
       && printf '%s\n' "$out" | grep -q 'C2 parse_int = -12345' \
       && printf '%s\n' "$out" | grep -q 'C2 is_float(nan) = false' \
       && printf '%s\n' "$out" | grep -q 'C3 fmt2 = 3.14' \
       && printf '%s\n' "$out" | grep -q 'C5 reduce = 75' \
       && printf '%s\n' "$out" | grep -q 'C5 join = apple,banana,cherry' \
       && printf '%s\n' "$out" | grep -q 'C7 exists = true' \
       && printf '%s\n' "$out" | grep -q 'C9 has_env(PATH) = true' \
       && printf '%s\n' "$out" | grep -q 'C10 parts len = 8' ; then
        upass "C1/C4/C2/C3/C5/C7/C9/C10：8 组标准库 API 端到端通过"
    else ubad "U 组：batch2_smoke 输出缺失关键断言"; printf '%s\n' "$out" | head -40; fi
else ubad "U 组：batch2_smoke 编译失败"; tail -8 "${TMP}/u_smoke.log"; fi

# 2) 负向：parse_int 非法输入必须 Fail-Fast（绝不静默返回 0），进程非 0 退出。
cat > "${TMP}/u_parse_bad.dial" <<'EOF'
fn main(): int { print(parse_int("abc")); return 0; }
EOF
if "$DIALC" build "${TMP}/u_parse_bad.dial" -o "${TMP}/u_parse_bad" >/dev/null 2>&1 \
   && ! sh -c "${TMP}/u_parse_bad" >/dev/null 2>&1; then
    upass "C2：parse_int 非法输入 Fail-Fast（进程非 0，无静默 0）"
else ubad "C2：parse_int 非法输入未 Fail-Fast"; fi

# 3) 负向：char_at 越界必须 Fail-Fast（绝不返回哨兵/截断）。
cat > "${TMP}/u_charat_bad.dial" <<'EOF'
fn main(): int { print("abc".char_at(9)); return 0; }
EOF
if "$DIALC" build "${TMP}/u_charat_bad.dial" -o "${TMP}/u_charat_bad" >/dev/null 2>&1 \
   && ! sh -c "${TMP}/u_charat_bad" >/dev/null 2>&1; then
    upass "C4：char_at 越界 Fail-Fast"
else ubad "C4：char_at 越界未 Fail-Fast"; fi

# 4) 负向：string[] 不得有 index_of（按位模式比较句柄会错），编译期拒绝。
cat > "${TMP}/u_idx_bad.dial" <<'EOF'
fn main(): int { var a: string[] = ["a"]; print(a.index_of("a")); return 0; }
EOF
if "$DIALC" build "${TMP}/u_idx_bad.dial" -o "${TMP}/u_idx_bad" >"${TMP}/u_idx_bad.log" 2>&1; then
    ubad "C5：string[].index_of 竟被接受（位模式歧义）"
elif grep -q "index_of\|无方法" "${TMP}/u_idx_bad.log"; then
    upass "C5：string[].index_of 编译期拒绝（位模式歧义）"
else ubad "C5：string[].index_of 被拒但诊断未提及"; fi

# 5) 负向：format_float 第一个参数必须是 float（零隐式转换铁律），int 被拒。
cat > "${TMP}/u_fmt_bad.dial" <<'EOF'
fn main(): int { print(format_float(3, 2)); return 0; }
EOF
if "$DIALC" build "${TMP}/u_fmt_bad.dial" -o "${TMP}/u_fmt_bad" >"${TMP}/u_fmt_bad.log" 2>&1; then
    ubad "C3：format_float(int,..) 竟被接受（违反零隐式转换）"
elif grep -q "format_float" "${TMP}/u_fmt_bad.log"; then
    upass "C3：format_float 严格 float 参数（零隐式转换铁律）"
else ubad "C3：format_float 类型报错但诊断未提及"; fi

# 6) 名称铁律：第 2 批新增内建（read_all/file_read_line/getenv/has_env/...）进入保留名清单，
#    用户不得定义同名函数（延续第 1 批 Fail-Fast 加固，杜绝内建分支静默绕过）。
cat > "${TMP}/u_shadow.dial" <<'EOF'
fn getenv(x: int): int { return x; }
fn main(): int { return 0; }
EOF
if "$DIALC" build "${TMP}/u_shadow.dial" -o "${TMP}/u_shadow" >"${TMP}/u_shadow.log" 2>&1; then
    ubad "U：第 2 批内建可被同名函数定义静默绕过（违反 Fail-Fast）"
elif grep -q "保留名" "${TMP}/u_shadow.log"; then
    upass "U：第 2 批内建进入保留名清单（同名定义编译期拒绝）"
else ubad "U：同名定义被拒但诊断未提到保留名"; fi

echo
echo "== V) B/C 组缺口第三批：语言机制（B7 文件级 const / B8 结构体默认值 / B6 结构体进容器，v1.04 增补） =="
vpass() { echo "  ✅ $1"; }
vbad()  { echo "  ❌ $1"; fail=1; }

# 1) B8 正向：结构体字段默认值（省略用默认值 / 显式覆盖 / 常量表达式默认值）。
if "$DIALC" build tests/batch3_b8_smoke.dial -o "${TMP}/v_b8" >"${TMP}/v_b8.log" 2>&1; then
    out=$("${TMP}/v_b8" 2>/dev/null)
    if printf '%s\n' "$out" | grep -q 'B8 p0.x = 0' \
       && printf '%s\n' "$out" | grep -q 'B8 p1.x = 7' \
       && printf '%s\n' "$out" | grep -q 'B8 p1.y = 0' \
       && printf '%s\n' "$out" | grep -q 'B8 r0.h = 2' \
       && printf '%s\n' "$out" | grep -q 'B8 r0.tag = box' \
       && printf '%s\n' "$out" | grep -q 'B8 r1.w = 4' ; then
        vpass "B8：结构体字段编译期常量默认值（省略取默认 / 显式覆盖 / 常量表达式）"
    else vbad "V 组：batch3_b8_smoke 输出缺失关键断言"; printf '%s\n' "$out" | head -30; fi
else vbad "V 组：batch3_b8_smoke 编译失败"; tail -8 "${TMP}/v_b8.log"; fi

# 2) B7 正向：文件级只读 const（标量 / 字符串 / 数组 / 嵌套数组 / 结构体 / 常量表达式）。
if "$DIALC" build tests/batch3_b7_smoke.dial -o "${TMP}/v_b7" >"${TMP}/v_b7.log" 2>&1; then
    out=$("${TMP}/v_b7" 2>/dev/null)
    if printf '%s\n' "$out" | grep -q 'B7 N = 100' \
       && printf '%s\n' "$out" | grep -q 'B7 FLAGS\[3\] = 100' \
       && printf '%s\n' "$out" | grep -q 'B7 MAT\[1\]\[0\] = 3' \
       && printf '%s\n' "$out" | grep -q 'B7 TAGS\[2\] = c' \
       && printf '%s\n' "$out" | grep -q 'B7 ORIGIN.x = 0' \
       && printf '%s\n' "$out" | grep -q 'B7 UNIT.y = 1' \
       && printf '%s\n' "$out" | grep -q 'B7 STEP = 15' \
       && printf '%s\n' "$out" | grep -q 'B7 SCALE = 6.000000' ; then
        vpass "B7：文件级只读 const（标量/字符串/数组/结构体/常量表达式）内联正确"
    else vbad "V 组：batch3_b7_smoke 输出缺失关键断言"; printf '%s\n' "$out" | head -30; fi
else vbad "V 组：batch3_b7_smoke 编译失败"; tail -8 "${TMP}/v_b7.log"; fi

# 3) B6 正向：结构体进容器（Point[] 与 map<string, Point>）。
if "$DIALC" build tests/batch3_b6_smoke.dial -o "${TMP}/v_b6" >"${TMP}/v_b6.log" 2>&1; then
    out=$("${TMP}/v_b6" 2>/dev/null)
    if printf '%s\n' "$out" | grep -q 'B6 len = 2' \
       && printf '%s\n' "$out" | grep -q 'B6 p0.x = 1' \
       && printf '%s\n' "$out" | grep -q 'B6 after write p0.x = 9' \
       && printf '%s\n' "$out" | grep -q 'B6 pts2\[0\].x = 7' \
       && printf '%s\n' "$out" | grep -q 'B6 map size = 2' \
       && printf '%s\n' "$out" | grep -q 'B6 map hit.x = 1' \
       && printf '%s\n' "$out" | grep -q 'B6 map miss.x = 0' \
       && printf '%s\n' "$out" | grep -q 'B6 map get_or default.x = 9' \
       && printf '%s\n' "$out" | grep -q 'B6 map removed.x = 1' ; then
        vpass "B6：结构体进容器（Point[] / map<string,Point>）端到端通过"
    else vbad "V 组：batch3_b6_smoke 输出缺失关键断言"; printf '%s\n' "$out" | head -40; fi
else vbad "V 组：batch3_b6_smoke 编译失败"; tail -8 "${TMP}/v_b6.log"; fi

# 4) B7 负向：顶层 var 全局变量必须被拒（红线：禁止可变全局状态）。
if "$DIALC" build tests/bad/const_top_level_var.dial -o "${TMP}/v_var" >"${TMP}/v_var.log" 2>&1; then
    vbad "B7：顶层 var 全局变量竟被接受（违反禁止可变全局状态红线）"
elif grep -q "禁止可变全局状态" "${TMP}/v_var.log"; then
    vpass "B7：顶层 var 全局变量编译期拒绝（禁止可变全局状态红线）"
else vbad "B7：顶层 var 被拒但诊断未提到可变全局状态"; fi

# 5) B8 负向：省略无默认值的字段必须被拒（不得静默零初始化）。
if "$DIALC" build tests/bad/struct_default_missing.dial -o "${TMP}/v_defmiss" >"${TMP}/v_defmiss.log" 2>&1; then
    vbad "B8：省略无默认值字段竟被接受（静默零初始化）"
elif grep -q "没有默认值" "${TMP}/v_defmiss.log"; then
    vpass "B8：省略无默认值字段编译期拒绝（不得静默零初始化）"
else vbad "B8：字段缺失被拒但诊断未说明原因"; fi

# 6) B8 负向：默认值类型不匹配必须被拒（零隐式转换铁律）。
if "$DIALC" build tests/bad/struct_default_type_mismatch.dial -o "${TMP}/v_defmm" >"${TMP}/v_defmm.log" 2>&1; then
    vbad "B8：默认值类型不匹配竟被接受（违反零隐式转换）"
elif grep -q "不匹配" "${TMP}/v_defmm.log"; then
    vpass "B8：默认值类型不匹配编译期拒绝（零隐式转换铁律）"
else vbad "B8：类型不匹配被拒但诊断未说明原因"; fi

# 7) B6 负向：含句柄字段的结构体不得进容器（装箱的字段级 ARC 未落地，宁可拒绝）。
if "$DIALC" build tests/bad/struct_array_handle_field.dial -o "${TMP}/v_hf1" >"${TMP}/v_hf1.log" 2>&1; then
    vbad "B6：含句柄字段的结构体竟可作数组元素（字段级 ARC 未落地）"
elif grep -q "数组元素" "${TMP}/v_hf1.log"; then
    vpass "B6：含句柄字段的结构体作数组元素编译期拒绝"
else vbad "B6：数组元素被拒但诊断未说明原因"; fi

if "$DIALC" build tests/bad/struct_map_handle_field.dial -o "${TMP}/v_hf2" >"${TMP}/v_hf2.log" 2>&1; then
    vbad "B6：含句柄字段的结构体竟可作 map 值（字段级 ARC 未落地）"
elif grep -q "map<" "${TMP}/v_hf2.log"; then
    vpass "B6：含句柄字段的结构体作 map 值编译期拒绝"
else vbad "B6：map 值被拒但诊断未说明原因"; fi

echo
echo "== W) C11 独立包：http.dialpkg 端到端（HTTP/1.1 + TLS 1.3 + keep-alive + HTTP/2 h2c/h2 真并发多路复用，v1.04 增补） =="
wpass() { echo "  ✅ $1"; }
wbad()  { echo "  ❌ $1"; fail=1; }
wskip() { echo "  ⚠️  $1"; }

# 1) 构建三平台静态库（自研 HTTP 层 + zlib 核心 + mbedTLS）。
if bash packages/http/build.sh >"${TMP}/w_build.log" 2>&1; then
    wpass "http 包三平台静态库构建完成（linux-gnu / musl / windows-gnu）"
else wbad "http 包静态库构建失败"; tail -8 "${TMP}/w_build.log"; fi

# 2) 打包 + 安装（走 §25 包分发链路：manifest / 逐文件 SHA256 自检）。
if "$DIALC" pack packages/http -o "${TMP}/http.dialpkg" >"${TMP}/w_pack.log" 2>&1 \
   && "$DIALC" install "${TMP}/http.dialpkg" >"${TMP}/w_install.log" 2>&1; then
    wpass "http.dialpkg 打包 + 安装成功（manifest / SHA256 自检通过）"
else wbad "http.dialpkg 打包或安装失败"; tail -8 "${TMP}/w_pack.log" "${TMP}/w_install.log"; fi

# 3) 起本地 HTTP 服务器（覆盖 200 / 404 / POST 回显 / chunked / gzip / 多响应头）。
rm -f "${TMP}/w_port"
python3 tests/pkg/http_test_server.py "${TMP}/w_port" >"${TMP}/w_srv.log" 2>&1 &
SRV_PID=$!
PORT=""
for _ in $(seq 1 50); do
    if [ -s "${TMP}/w_port" ]; then PORT="$(cat "${TMP}/w_port")"; break; fi
    sleep 0.1
done
if [ -n "$PORT" ]; then
    wpass "本地 HTTP 服务器就绪（端口 $PORT）"
else
    wbad "本地 HTTP 服务器未就绪"
fi

# 4) import \"http\" 编译 + 运行：两层 API 与各协议要点。
if [ -n "$PORT" ] \
   && DIAL_HTTP_TEST_BASE="http://127.0.0.1:$PORT" \
      "$DIALC" build tests/pkg/use_http.dial -o "${TMP}/use_http" >"${TMP}/w_comp.log" 2>&1; then
    out=$(DIAL_HTTP_TEST_BASE="http://127.0.0.1:$PORT" "${TMP}/use_http" 2>&1)
    if printf '%s\n' "$out" | grep -q 'status = 200' \
       && printf '%s\n' "$out" | grep -q 'body = hello dial http' \
       && printf '%s\n' "$out" | grep -q 'x_dial_test = hello' \
       && printf '%s\n' "$out" | grep -q 'ctype = text/plain' \
       && printf '%s\n' "$out" | grep -q 'nf_status = 404' \
       && printf '%s\n' "$out" | grep -q 'post_body = dial-body-123' \
       && printf '%s\n' "$out" | grep -q 'chunked_body = chunk-one|chunk-two|chunk-three' \
       && printf '%s\n' "$out" | grep -q 'gzip_body = gzip-compressed-payload-gzip-compressed-payload-' \
       && printf '%s\n' "$out" | grep -q 'multi_count = 7' \
       && printf '%s\n' "$out" | grep -q 'raw_status_line = HTTP/1.1 200 OK' ; then
        wpass "高层 API（GET/POST/响应头/404）+ gzip 解压 + chunked + 低层流式 端到端通过"
    else wbad "W 组：use_http 输出缺失关键断言"; printf '%s\n' "$out" | head -40; fi
else wbad "use_http（import \"http\"）编译失败"; tail -8 "${TMP}/w_comp.log"; fi

# 4b) chunked 长度行 Fail-Fast 回归（§20）：畸形长度行 "xyz" 必须明确报协议错，
#     绝不静默当成「结束块」；合法 chunk-ext（";ext=1"）必须照常解出完整 body。
if [ -n "$PORT" ]; then
    out=$(DIAL_HTTP_TEST_BASE="http://127.0.0.1:$PORT" "${TMP}/use_http" 2>&1)
    if printf '%s\n' "$out" | grep -qF 'chunked_bad_is_null = true' \
       && printf '%s\n' "$out" | grep -qF 'chunked_bad_err = -5' \
       && printf '%s\n' "$out" | grep -q 'chunked_bad_msg = .*chunked' \
       && printf '%s\n' "$out" | grep -qF 'chunked_ext_status = 200' \
       && printf '%s\n' "$out" | grep -qF 'chunked_ext_body = ext-one|ext-two|ext-three' ; then
        wpass "chunked 畸形长度行明确失败（§20）+ 合法 chunk-ext 不误伤"
    else wbad "W 组：chunked 长度行回归断言失败"; printf '%s\n' "$out" | grep -i 'chunked' | head -20; fi
else wbad "W 组：chunked 长度行回归跳过（服务器未就绪）"; fi

# 5) 错误路径：https 未接线 / URL 缺主机名 / 连不上 —— 都必须**明确失败**而非静默成功。
if [ -n "$PORT" ]; then
    out=$(DIAL_HTTP_TEST_BASE="http://127.0.0.1:$PORT" "${TMP}/use_http" 2>&1)
    if printf '%s\n' "$out" | grep -q 'tls_bad_is_null = true' \
       && printf '%s\n' "$out" | grep -q 'nourl_is_null = true' \
       && printf '%s\n' "$out" | grep -q 'bad_is_null = true' ; then
        wpass "错误路径明确失败（https 连不上 / URL 非法 / 连接失败）"
    else wbad "W 组：错误路径未按预期失败"; printf '%s\n' "$out" | head -20; fi
fi

# 11) 浏览器一致性画像：set_profile + 诊断 getter 回读一致，且 chrome 画像真实发出的
#     请求头被服务端收到（UA 含 Chrome、含 sec-ch-ua / sec-fetch-*）；切回 default 后
#     UA 回落 dial-http/1.1 且不再发 Sec-CH-UA。复用步骤 3 的服务器与已安装包。
#     （离线指纹比对 tests/pkg/fingerprint 负责逐字段对齐基准；本步骤锁「画像在主回归里
#      生效」这一底线，让它随 CI 一起跑。）
if [ -n "$PORT" ] \
   && DIAL_HTTP_TEST_BASE="http://127.0.0.1:$PORT" \
      "$DIALC" build tests/pkg/use_http_profile.dial -o "${TMP}/use_http_profile" >"${TMP}/w_prof.log" 2>&1; then
    out=$(DIAL_HTTP_TEST_BASE="http://127.0.0.1:$PORT" "${TMP}/use_http_profile" 2>&1)
    ok=1
    printf '%s\n' "$out" | grep -qF 'profile_chrome = chrome'       || ok=0
    printf '%s\n' "$out" | grep -qF 'profile_default = default'     || ok=0
    printf '%s\n' "$out" | grep -qF 'chrome_ua_has_chrome = true'   || ok=0
    printf '%s\n' "$out" | grep -qF 'chrome_has_sec_ch_ua = true'   || ok=0
    printf '%s\n' "$out" | grep -qF 'chrome_has_sec_fetch = true'   || ok=0
    printf '%s\n' "$out" | grep -qF 'default_ua_is_dialhttp = true' || ok=0
    printf '%s\n' "$out" | grep -qF 'default_no_sec_ch_ua = true'   || ok=0
    if [ "$ok" -eq 1 ]; then
        wpass "浏览器一致性画像：set/getter 回读一致 + chrome UA/Sec-CH-UA 真实上 wire + default 回落"
    else wbad "W 组：use_http_profile 输出缺失关键断言"; printf '%s\n' "$out" | head -40; fi
else wbad "use_http_profile（import \"http\" 画像）编译失败"; tail -8 "${TMP}/w_prof.log"; fi

# 6) 清理：停服务器、卸包（不污染后续测试与用户环境）。
if [ -n "${SRV_PID:-}" ]; then kill "$SRV_PID" >/dev/null 2>&1 || true; fi
"$DIALC" uninstall http >/dev/null 2>&1 || true
wpass "清理完成（服务器已停、http 包已卸载）"

# 7) TLS 1.3 子组：自签 CA + 本地 TLS 服务器，验证 TLS 1.3 协商 / 证书校验 fail-closed /
#    keep-alive 连接池复用 / 404-500 错误路径 / 自定义 CA 与系统根两条信任链。
#    复用步骤 2 已打包安装好的 http 包（含 mbedTLS 与 SIGPIPE 防护）。
#    注意：步骤 6 已把明文子组安装的 http 包卸载，这里须重新安装后再编译 use_https.dial。
if [ -f "${TMP}/http.dialpkg" ] \
   && "$DIALC" install "${TMP}/http.dialpkg" >"${TMP}/w_install_tls.log" 2>&1; then
    wpass "TLS 子组复用 http 包重新安装成功"
else wbad "TLS 子组 http 包重新安装失败"; tail -8 "${TMP}/w_install_tls.log"; fi

TLSDIR="${TMP}/tlstest"
mkdir -p "$TLSDIR"
if command -v openssl >/dev/null 2>&1; then
    # 生成自签 CA + 带 IP SAN(127.0.0.1) 的服务器证书（内网 / 私有 CA 场景的缩影）
    openssl genrsa -out "${TLSDIR}/ca.key" 2048 2>/dev/null
    openssl req -x509 -new -nodes -key "${TLSDIR}/ca.key" -sha256 -days 3650 \
        -subj "/CN=dial-test-ca" -out "${TLSDIR}/ca.crt" 2>/dev/null
    openssl genrsa -out "${TLSDIR}/srv.key" 2048 2>/dev/null
    openssl req -new -key "${TLSDIR}/srv.key" -subj "/CN=127.0.0.1" \
        -out "${TLSDIR}/srv.csr" 2>/dev/null
    printf "subjectAltName=IP:127.0.0.1\n" > "${TLSDIR}/san.ext"
    openssl x509 -req -in "${TLSDIR}/srv.csr" -CA "${TLSDIR}/ca.crt" \
        -CAkey "${TLSDIR}/ca.key" -CAcreateserial -out "${TLSDIR}/srv.crt" \
        -days 3650 -sha256 -extfile "${TLSDIR}/san.ext" 2>/dev/null
    rm -f "${TMP}/w_tlsport"
    # 强制 TLS1.3 的本地服务器（同一 TLS 连接上 keep-alive 复用多个请求）。
    # 端口号用随机高端口，端口文件路径作为第二参数传入（脚本 argv: PORT PORTFILE）。
    TLS_PORT_NUM=$(shuf -i 40000-49999 -n1)
    DIAL_TLS_CERT="${TLSDIR}/srv.crt" DIAL_TLS_KEY="${TLSDIR}/srv.key" \
        python3 tests/pkg/tls_test_server.py "$TLS_PORT_NUM" "${TMP}/w_tlsport" >"${TMP}/w_tlssrv.log" 2>&1 &
    TLS_PID=$!
    TLS_PORT=""
    for _ in $(seq 1 50); do
        if [ -s "${TMP}/w_tlsport" ]; then TLS_PORT="$(cat "${TMP}/w_tlsport")"; break; fi
        sleep 0.1
    done
    if [ -n "$TLS_PORT" ]; then
        wpass "本地 TLS 1.3 服务器就绪（端口 $TLS_PORT）"
    else
        wbad "本地 TLS 1.3 服务器未就绪"; tail -8 "${TMP}/w_tlssrv.log"
    fi
    if [ -n "$TLS_PORT" ] \
       && DIAL_HTTPS_TEST_BASE="https://127.0.0.1:$TLS_PORT" \
          DIAL_HTTPS_TEST_CA="${TLSDIR}/ca.crt" \
          "$DIALC" build tests/pkg/use_https.dial -o "${TMP}/use_https" >"${TMP}/w_tlscomp.log" 2>&1; then
        out=$(DIAL_HTTPS_TEST_BASE="https://127.0.0.1:$TLS_PORT" \
              DIAL_HTTPS_TEST_CA="${TLSDIR}/ca.crt" "${TMP}/use_https" 2>&1)
        if printf '%s\n' "$out" | grep -q 'TLS status = 200' \
           && printf '%s\n' "$out" | grep -q 'TLS version = TLSv1.3' \
           && printf '%s\n' "$out" | grep -q 'TLS verify = 0' \
           && printf '%s\n' "$out" | grep -q 'TLS x-test = dialtls' \
           && printf '%s\n' "$out" | grep -Eq 'pool hits = [1-9]' \
           && printf '%s\n' "$out" | grep -q '404 status = 404' \
           && printf '%s\n' "$out" | grep -q '500 status = 500' \
           && printf '%s\n' "$out" | grep -q 'no-ca rejected = true' ; then
            wpass "TLS 1.3：协商 + 证书校验 + keep-alive 复用 + 404/500 + fail-closed 全通过"
        else wbad "W 组：use_https 输出缺失关键断言"; printf '%s\n' "$out" | head -40; fi
    else wbad "use_https（import \"http\" TLS）编译失败"; tail -8 "${TMP}/w_tlscomp.log"; fi

    # 7b) Termux 系统根证书库可用性回归：Android/Termux 上没有 /etc/ssl/certs，
    #     证书在 $PREFIX/etc/tls/cert.pem。这里把步骤 7 的自签 CA 装成该文件，
    #     不设自定义 CA，只靠 load_linux_ca 的 Termux 分支完成 TLS 1.3 握手；
    #     PREFIX 指向空目录时必须 fail-closed 明确拒绝自签证书。两组都是真实
    #     握手（非 mock），复用步骤 7 的服务器与已编译的 use_https 二进制。
    TERMUX_ROOT="${TMP}/termux-root"
    TERMUX_EMPTY_ROOT="${TMP}/termux-root-empty"
    mkdir -p "${TERMUX_ROOT}/etc/tls" "${TERMUX_EMPTY_ROOT}/etc/tls"
    if [ -n "$TLS_PORT" ] && [ -f "${TLSDIR}/ca.crt" ] && [ -x "${TMP}/use_https" ]; then
        cp "${TLSDIR}/ca.crt" "${TERMUX_ROOT}/etc/tls/cert.pem"
        ok_out=$(PREFIX="$TERMUX_ROOT" DIAL_HTTPS_TERMUX_MODE=1 \
                 DIAL_HTTPS_TEST_BASE="https://127.0.0.1:$TLS_PORT" \
                 "${TMP}/use_https" 2>&1)
        if printf '%s\n' "$ok_out" | grep -qF 'termux status = 200' \
           && printf '%s\n' "$ok_out" | grep -qF 'termux verify = 0' ; then
            wpass "Termux 系统根库（\$PREFIX/etc/tls/cert.pem）真实握手成功，证书校验通过"
        else wbad "W 组：Termux 系统根库路径未生效"; printf '%s\n' "$ok_out" | head -20; fi

        bad_out=$(PREFIX="$TERMUX_EMPTY_ROOT" DIAL_HTTPS_TERMUX_MODE=1 \
                  DIAL_HTTPS_TEST_BASE="https://127.0.0.1:$TLS_PORT" \
                  "${TMP}/use_https" 2>&1)
        if printf '%s\n' "$bad_out" | grep -qF 'termux status = -1' ; then
            wpass "Termux 空 CA 目录仍 fail-closed（自签证书被明确拒绝，无静默降级）"
        else wbad "W 组：Termux 空 CA 目录未按预期拒绝"; printf '%s\n' "$bad_out" | head -20; fi
    else
        wskip "Termux 系统根库回归跳过（TLS 服务器 / 证书 / 二进制未就绪）"
    fi

    if [ -n "${TLS_PID:-}" ]; then kill "$TLS_PID" >/dev/null 2>&1 || true; fi
else
    wskip "TLS 1.3 子组跳过（环境无 openssl）"
fi

# 8) 清理：停 TLS 服务器、卸包（不污染后续测试与用户环境）。
if [ -n "${TLS_PID:-}" ]; then kill "$TLS_PID" >/dev/null 2>&1 || true; fi
"$DIALC" uninstall http >/dev/null 2>&1 || true
wpass "TLS 子组清理完成（TLS 服务器已停、http 包已卸载）"

# 9) HTTP/2 子组：明文 h2c prior-knowledge 单连接 6 stream 真并发多路复用 +
#    https 的 h2（TLS 1.3 + ALPN 协商）+ fail-closed，自研 HPACK 响应解码。
#    依赖 Python h2/hpack/hyperframe 起本地 HTTP/2 服务器；TLS 部分复用步骤 7 的自签 CA。
#    步骤 8 已卸包，这里须重新安装（包内已含 dial_http2.o / dial_hpack.o）。
if [ -f "${TMP}/http.dialpkg" ] \
   && "$DIALC" install "${TMP}/http.dialpkg" >"${TMP}/w_install_h2.log" 2>&1; then
    wpass "HTTP/2 子组复用 http 包重新安装成功"
else wbad "HTTP/2 子组 http 包重新安装失败"; tail -8 "${TMP}/w_install_h2.log"; fi

H2_DEPS=0
if python3 -c "import h2, hpack, hyperframe" >/dev/null 2>&1; then H2_DEPS=1; fi
H2C_PID=""
H2_PID=""
H2_BASE=""
if [ "$H2_DEPS" -eq 1 ]; then
    # 9a) 明文 h2c 服务器（PORT=0 由 OS 分配，实际端口写入端口文件）。
    rm -f "${TMP}/w_h2cport"
    python3 tests/pkg/http2_test_server.py h2c 0 "${TMP}/w_h2cport" \
        >"${TMP}/w_h2csrv.log" 2>&1 &
    H2C_PID=$!
    H2C_PORT=""
    for _ in $(seq 1 50); do
        if [ -s "${TMP}/w_h2cport" ]; then H2C_PORT="$(cat "${TMP}/w_h2cport")"; break; fi
        sleep 0.1
    done
    if [ -n "$H2C_PORT" ]; then
        wpass "本地 h2c 明文服务器就绪（端口 $H2C_PORT）"
    else
        wbad "本地 h2c 明文服务器未就绪"; tail -8 "${TMP}/w_h2csrv.log"
    fi

    # 9b) TLS h2 服务器（ALPN 固定 h2，复用步骤 7 的 IP SAN 证书）。证书缺失则只跑 h2c。
    if [ -f "${TLSDIR}/ca.crt" ] && [ -f "${TLSDIR}/srv.crt" ] && [ -f "${TLSDIR}/srv.key" ]; then
        rm -f "${TMP}/w_h2port"
        DIAL_TLS_CERT="${TLSDIR}/srv.crt" DIAL_TLS_KEY="${TLSDIR}/srv.key" \
            python3 tests/pkg/http2_test_server.py h2 0 "${TMP}/w_h2port" \
            >"${TMP}/w_h2srv.log" 2>&1 &
        H2_PID=$!
        H2_PORT=""
        for _ in $(seq 1 50); do
            if [ -s "${TMP}/w_h2port" ]; then H2_PORT="$(cat "${TMP}/w_h2port")"; break; fi
            sleep 0.1
        done
        if [ -n "$H2_PORT" ]; then
            H2_BASE="https://127.0.0.1:$H2_PORT"
            wpass "本地 h2 TLS 服务器就绪（端口 $H2_PORT，ALPN=h2）"
        else
            wbad "本地 h2 TLS 服务器未就绪"; tail -8 "${TMP}/w_h2srv.log"
        fi
    else
        wskip "HTTP/2 TLS h2 子步跳过（步骤 7 未生成自签证书）"
    fi

    # 9c) 编译运行 use_http2.dial：6 个 spawn worker 共享一个 Http2Session，
    #     服务端故意乱序响应，逐条断言 path == X-Stream（不串台）。
    if [ -n "$H2C_PORT" ] \
       && "$DIALC" build tests/pkg/use_http2.dial -o "${TMP}/use_http2" >"${TMP}/w_h2comp.log" 2>&1; then
        out=$(DIAL_HTTP2_H2C_BASE="http://127.0.0.1:$H2C_PORT" \
              DIAL_HTTP2_H2_BASE="$H2_BASE" \
              DIAL_HTTP2_CA="${TLSDIR}/ca.crt" \
              timeout 60 "${TMP}/use_http2" 2>&1)
        ok=1
        printf '%s\n' "$out" | grep -qF 'h2c connected = true' || ok=0
        printf '%s\n' "$out" | grep -qF 'h2c protocol = h2c' || ok=0
        for p in /slow0 /slow1 /slow2 /slow3 /fast0 /ok; do
            printf '%s\n' "$out" | grep -qF "h2c resp = ${p}|200|${p}|dialh2|" || ok=0
        done
        [ "$(printf '%s\n' "$out" | grep -c '^h2c resp = ')" -eq 6 ] || ok=0
        printf '%s\n' "$out" | grep -qF 'h2c conns = 1' || ok=0
        printf '%s\n' "$out" | grep -qF 'h2c streams = 6' || ok=0
        printf '%s\n' "$out" | grep -qF 'h2 404 status = 404' || ok=0
        printf '%s\n' "$out" | grep -qF 'h2 500 status = 500' || ok=0
        if [ -n "$H2_BASE" ]; then
            printf '%s\n' "$out" | grep -qF 'h2 tls connected = true' || ok=0
            printf '%s\n' "$out" | grep -qF 'h2 tls protocol = h2' || ok=0
            printf '%s\n' "$out" | grep -qF 'h2 tls alpn = h2' || ok=0
            printf '%s\n' "$out" | grep -qF 'h2 tls status = 200' || ok=0
            printf '%s\n' "$out" | grep -qF 'h2 tls x-test = dialh2' || ok=0
            printf '%s\n' "$out" | grep -qF 'h2 no-ca rejected = true' || ok=0
        fi
        if [ "$ok" -eq 1 ]; then
            wpass "HTTP/2：h2c 真并发多路复用（单连接 6 stream 不串台）+ h2 ALPN + 404/500 + fail-closed 全通过"
        else wbad "W 组：use_http2 输出缺失关键断言"; printf '%s\n' "$out" | head -40; fi

        # 9d) 超长额外头不再被 512 字节静默截断：1200 字节的头必须原样送达，
        #     服务端按实际收到的长度回显 X-Long-Len（§20 Fail-Fast）。
        if printf '%s\n' "$out" | grep -qF 'h2 long header status = 200' \
           && printf '%s\n' "$out" | grep -qF 'h2 long header echoed len = 1200' ; then
            wpass "HTTP/2 超长额外请求头原样送达 1200 字节（不再 512 静默截断）"
        else wbad "W 组：超长额外头回显长度不符"; printf '%s\n' "$out" | grep -i 'long header' | head -10; fi
    else wbad "use_http2（import \"http\" HTTP/2）编译失败"; tail -8 "${TMP}/w_h2comp.log"; fi
else
    wskip "HTTP/2 子组跳过（环境缺 python h2/hpack/hyperframe）"
fi

# 10) 清理：停两台 HTTP/2 服务器、卸包（不污染后续测试与用户环境）。
if [ -n "${H2C_PID:-}" ]; then kill "$H2C_PID" >/dev/null 2>&1 || true; fi
if [ -n "${H2_PID:-}" ]; then kill "$H2_PID" >/dev/null 2>&1 || true; fi
"$DIALC" uninstall http >/dev/null 2>&1 || true
wpass "HTTP/2 子组清理完成（服务器已停、http 包已卸载）"

# ---------------------------------------------------------------------------
# X) C11 独立包：text.dialpkg（自研 Thompson NFA 正则，零第三方依赖）
# ---------------------------------------------------------------------------
echo "== X) C11 独立包：text.dialpkg 端到端（自研 Thompson NFA 正则，v1.04 增补） =="
xpass() { echo "  ✅ $1"; }
xbad()  { echo "  ❌ $1"; fail=1; }

# 1) 构建三平台静态库（纯 C，无 vendor 依赖）。
if bash packages/text/build.sh >"${TMP}/x_build.log" 2>&1; then
    xpass "text 包三平台静态库构建完成（linux-gnu / musl / windows-gnu）"
else xbad "text 包静态库构建失败"; tail -8 "${TMP}/x_build.log"; fi

# 2) 零警告硬约束（与 http 包同口径）。
if grep -qiE 'warning:|error:' "${TMP}/x_build.log"; then
    xbad "text 包构建存在 warning / error"
    grep -iE 'warning:|error:' "${TMP}/x_build.log" | head -8
else xpass "text 包三平台构建零警告零错误"; fi

# 3) 打包 + 安装（manifest / 逐文件 SHA256 自检）。
"$DIALC" uninstall text >/dev/null 2>&1 || true
if "$DIALC" pack packages/text -o "${TMP}/text.dialpkg" >"${TMP}/x_pack.log" 2>&1 \
   && "$DIALC" install "${TMP}/text.dialpkg" >"${TMP}/x_install.log" 2>&1; then
    xpass "text.dialpkg 打包 + 安装成功（manifest / SHA256 自检通过）"
else xbad "text.dialpkg 打包或安装失败"; tail -8 "${TMP}/x_pack.log" "${TMP}/x_install.log"; fi

# 4) 编译运行：42 项断言覆盖语法子集 / 捕获组 / 替换 / 错误路径。
if "$DIALC" build tests/pkg/use_text.dial -o "${TMP}/use_text" >"${TMP}/x_comp.log" 2>&1; then
    xpass "use_text.dial（import \"text\"）编译成功"
    xout="$("${TMP}/use_text" 2>&1)"
    xok=1
    printf '%s\n' "$xout" | grep -qF 'text_done = true' || xok=0
    for key in compile_ok group1 group2 digit_group0 plus_group0 optional_colour \
               anchor_hit word_nohit noncapture_group0 replace_all cn_group0 \
               bad_err_code oob_err_code; do
        printf '%s\n' "$xout" | grep -qF "$key = ok" || xok=0
    done
    nbad="$(printf '%s\n' "$xout" | grep -c ' = bad' || true)"
    nok="$(printf '%s\n' "$xout" | grep -c ' = ok$' || true)"
    if [ "$xok" -eq 1 ] && [ "$nbad" -eq 0 ] && [ "$nok" -ge 40 ]; then
        xpass "text 包端到端全通过（${nok} 项断言，零 = bad）"
    else
        xbad "text 包端到端断言失败（ok=${nok} bad=${nbad}）"
        printf '%s\n' "$xout" | head -50
    fi
else xbad "use_text.dial 编译失败"; tail -8 "${TMP}/x_comp.log"; fi

# 5) 清理：卸包（不污染后续测试与用户环境）。
"$DIALC" uninstall text >/dev/null 2>&1 || true
xpass "text 包已卸载"

# ---------------------------------------------------------------------------
# Y) C11 独立包：json.dialpkg（自研递归下降解析 + 紧凑序列化，零第三方依赖）
# ---------------------------------------------------------------------------
echo "== Y) C11 独立包：json.dialpkg 端到端（自研 JSON 解析/序列化，v1.04 增补） =="
ypass() { echo "  ✅ $1"; }
ybad()  { echo "  ❌ $1"; fail=1; }

# 1) 构建三平台静态库（纯 C，无 vendor 依赖）。
if bash packages/json/build.sh >"${TMP}/y_build.log" 2>&1; then
    ypass "json 包三平台静态库构建完成（linux-gnu / musl / windows-gnu）"
else ybad "json 包静态库构建失败"; tail -8 "${TMP}/y_build.log"; fi

# 2) 零警告硬约束（与 text / http 包同口径）。
if grep -qiE 'warning:|error:' "${TMP}/y_build.log"; then
    ybad "json 包构建存在 warning / error"
    grep -iE 'warning:|error:' "${TMP}/y_build.log" | head -8
else ypass "json 包三平台构建零警告零错误"; fi

# 3) 打包 + 安装（manifest / 逐文件 SHA256 自检）。
"$DIALC" uninstall json >/dev/null 2>&1 || true
if "$DIALC" pack packages/json -o "${TMP}/json.dialpkg" >"${TMP}/y_pack.log" 2>&1 \
   && "$DIALC" install "${TMP}/json.dialpkg" >"${TMP}/y_install.log" 2>&1; then
    ypass "json.dialpkg 打包 + 安装成功（manifest / SHA256 自检通过）"
else ybad "json.dialpkg 打包或安装失败"; tail -8 "${TMP}/y_pack.log" "${TMP}/y_install.log"; fi

# 4) 编译运行：45 项断言覆盖解析 / 类型与访问 / 构造与深拷贝 /
#    大整数精度 / Unicode 与代理对 / 严格错误路径 / 错误码。
if "$DIALC" build tests/pkg/use_json.dial -o "${TMP}/use_json" >"${TMP}/y_comp.log" 2>&1; then
    ypass "use_json.dial（import \"json\"）编译成功"
    yout="$("${TMP}/use_json" 2>&1)"
    yok=1
    printf '%s\n' "$yout" | grep -qF 'json_done = true' || yok=0
    for key in parse_ok root_type root_len key0 name_value n_value ok_value arr_item1 \
               stringify_root build_stringify deepcopy_survives big_int big_roundtrip \
               unicode_value emoji_roundtrip trailing_comma_rejected nul_escape_rejected \
               obj_trailing_rejected get_miss_err item_not_array_err type_mismatch_err \
               len_on_scalar; do
        printf '%s\n' "$yout" | grep -qF "$key = ok" || yok=0
    done
    ynbad="$(printf '%s\n' "$yout" | grep -c ' = bad' || true)"
    ynok="$(printf '%s\n' "$yout" | grep -c ' = ok$' || true)"
    if [ "$yok" -eq 1 ] && [ "$ynbad" -eq 0 ] && [ "$ynok" -ge 40 ]; then
        ypass "json 包端到端全通过（${ynok} 项断言，零 = bad）"
    else
        ybad "json 包端到端断言失败（ok=${ynok} bad=${ynbad}）"
        printf '%s\n' "$yout" | head -50
    fi
else ybad "use_json.dial 编译失败"; tail -8 "${TMP}/y_comp.log"; fi

# 5) 清理：卸包（不污染后续测试与用户环境）。
"$DIALC" uninstall json >/dev/null 2>&1 || true
ypass "json 包已卸载"

# ---------------------------------------------------------------------------
# Z) C11 独立包：crypto.dialpkg（mbedTLS 摘要 / Base64 / HMAC，密码学绝不手搓）
# ---------------------------------------------------------------------------
echo "== Z) C11 独立包：crypto.dialpkg 端到端（mbedTLS 摘要 + Base64 + HMAC，v1.04 增补） =="
zpass() { echo "  ✅ $1"; }
zbad()  { echo "  ❌ $1"; fail=1; }

# 1) 构建三平台静态库（dial_crypto.o + mbedTLS 全量，链接期按需抽取）。
if bash packages/crypto/build.sh >"${TMP}/z_build.log" 2>&1; then
    zpass "crypto 包三平台静态库构建完成（linux-gnu / musl / windows-gnu）"
else zbad "crypto 包静态库构建失败"; tail -8 "${TMP}/z_build.log"; fi

# 2) 零警告硬约束（与 text / json / http 包同口径）。
if grep -qiE 'warning:|error:' "${TMP}/z_build.log"; then
    zbad "crypto 包构建存在 warning / error"
    grep -iE 'warning:|error:' "${TMP}/z_build.log" | head -8
else zpass "crypto 包三平台构建零警告零错误"; fi

# 3) 打包 + 安装（manifest / 逐文件 SHA256 自检）。
"$DIALC" uninstall crypto >/dev/null 2>&1 || true
if "$DIALC" pack packages/crypto -o "${TMP}/crypto.dialpkg" >"${TMP}/z_pack.log" 2>&1 \
   && "$DIALC" install "${TMP}/crypto.dialpkg" >"${TMP}/z_install.log" 2>&1; then
    zpass "crypto.dialpkg 打包 + 安装成功（manifest / SHA256 自检通过）"
else zbad "crypto.dialpkg 打包或安装失败"; tail -8 "${TMP}/z_pack.log" "${TMP}/z_install.log"; fi

# 4) 编译运行：29 项断言覆盖五种摘要标准向量 / string-bytes 双入口 /
#    含 NUL 的二进制 / 原始摘要字节 / Base64 往返 / HMAC / 严格错误路径。
if "$DIALC" build tests/pkg/use_crypto.dial -o "${TMP}/use_crypto" >"${TMP}/z_comp.log" 2>&1; then
    zpass "use_crypto.dial（import \"crypto\"）编译成功"
    zout="$("${TMP}/use_crypto" 2>&1)"
    zok=1
    printf '%s\n' "$zout" | grep -qF 'crypto_done = true' || zok=0
    for key in md5_abc sha1_abc sha256_abc sha512_abc sha3_256_abc md5_empty \
               sha256_utf8_cn bytes_string_agree md5_bytes_agree b64_decode_bin \
               b64_bin_b0 b64_bin_b2 b64_bin_roundtrip md5_bytes_bin sha256_bytes_bin \
               b64_hello b64_dial b64_empty b64_decode_hello digest_sha256_len \
               digest_md5_len hmac_fox hmac_bytes_agree b64_bad_rejected b64_bad_err \
               b64_bad_msg unknown_algo_rejected unknown_algo_err ok_clears_error; do
        printf '%s\n' "$zout" | grep -qF "$key = ok" || zok=0
    done
    znbad="$(printf '%s\n' "$zout" | grep -c ' = bad' || true)"
    znok="$(printf '%s\n' "$zout" | grep -c ' = ok$' || true)"
    if [ "$zok" -eq 1 ] && [ "$znbad" -eq 0 ] && [ "$znok" -ge 25 ]; then
        zpass "crypto 包端到端全通过（${znok} 项断言，零 = bad）"
    else
        zbad "crypto 包端到端断言失败（ok=${znok} bad=${znbad}）"
        printf '%s\n' "$zout" | head -50
    fi
else zbad "use_crypto.dial 编译失败"; tail -8 "${TMP}/z_comp.log"; fi

# 5) 清理：卸包（不污染后续测试与用户环境）。
"$DIALC" uninstall crypto >/dev/null 2>&1 || true
zpass "crypto 包已卸载"

# ---------------------------------------------------------------------------
# AA) 手搓 GUI 六层：地基 + 控件框架 + 成品外观 + 输入层 + 主题层/控件扩展 + 信号槽
#     （gui.dialpkg 端到端，v1.10 增补）
# ---------------------------------------------------------------------------
echo "== AA) 手搓 GUI 六层 + 第 9~15 层覆盖层：gui.dialpkg 端到端（三平台真实现 / XShm+XRender / 像素级 CRC / Widget 树·布局·命中·packed 事件块 / 默认主题四态绘制 / TextBox 输入与焦点链 / 主题色可覆写 + Radio·Slider·Progress / 6 类可订阅纯值信号 · 环形队列 / XTest 实窗注入 / 覆盖层纯内存离屏 CRC / 窗口级自动集成固定 z-order / 快捷键·命令系统，v1.10 增补） =="
aapass() { echo "  ✅ $1"; }
aabad()  { echo "  ❌ $1"; fail=1; }

# 1) 三平台静态库构建：x11 / x11-musl / win32 三份都是真实现，不是 stub。
if bash packages/gui/build.sh >"${TMP}/aa_build.log" 2>&1; then
    aapass "gui 包三平台静态库构建完成（x11 / x11-musl / win32 三份真实现）"
else aabad "gui 包静态库构建失败"; tail -10 "${TMP}/aa_build.log"; fi

if grep -qiE 'warning:|error:' "${TMP}/aa_build.log"; then
    aabad "gui 包构建存在 warning / error"
    grep -iE 'warning:|error:' "${TMP}/aa_build.log" | head -8
else aapass "gui 包三平台构建零警告零错误（含 vendor stb_truetype 基线校验）"; fi

# 2) 零链接期依赖：三份 .a 都不能出现平台 GUI 函数的未定义符号，否则 dlopen
#    策略就被破坏，dialc 的 final_link 会要求注入 -lX11 / -lgdi32。
aa_sym=0
for t in x86_64-linux-gnu aarch64-unknown-linux-musl x86_64-pc-windows-gnu; do
    lib="packages/gui/lib/$t/libgui.a"
    if [ ! -f "$lib" ]; then aabad "缺少 $lib"; aa_sym=1; continue; fi
    if nm -u "$lib" 2>/dev/null | grep -qiE 'XOpenDisplay|XShmCreateImage|XShmPutImage|XRenderComposite|CreateDIBSection|CreateWindowExA'; then
        aabad "$t 的 .a 出现平台 GUI 未定义符号（dlopen 动态装载策略被破坏）"
        aa_sym=1
    fi
done
if [ "$aa_sym" -eq 0 ]; then
    aapass "三份 .a 均无 X11/XShm/XRender/GDI 链接期依赖（POSIX dlopen / Win32 LoadLibraryA 成立）"
fi

# 3) 打包 + 安装（manifest / 逐文件 SHA256 自检）。
"$DIALC" uninstall gui >/dev/null 2>&1 || true
if "$DIALC" pack packages/gui -o "${TMP}/gui.dialpkg" >"${TMP}/aa_pack.log" 2>&1 \
   && "$DIALC" install "${TMP}/gui.dialpkg" >"${TMP}/aa_install.log" 2>&1; then
    aapass "gui.dialpkg 打包 + 安装成功（manifest / SHA256 自检通过）"
else aabad "gui.dialpkg 打包或安装失败"; tail -8 "${TMP}/aa_pack.log" "${TMP}/aa_install.log"; fi

# 4) 编译 + 无 DISPLAY 运行：纯内存光栅化器 / 字体 / 错误槽必须全绿，
#    窗口路径必须**明确跳过**（Fail-Fast 口径：不静默、不伪装通过）。
if "$DIALC" build tests/pkg/use_gui.dial -o "${TMP}/use_gui" >"${TMP}/aa_comp.log" 2>&1; then
    aapass "use_gui.dial（import \"gui\"）编译成功"
    env -u DISPLAY "${TMP}/use_gui" >"${TMP}/aa_nodisplay.log" 2>&1
    aa_no_exit=$?
    aa_bad=$(grep -c ' = bad' "${TMP}/aa_nodisplay.log" || true)
    if [ "$aa_no_exit" -eq 0 ] && [ "$aa_bad" -eq 0 ] \
       && grep -qF 'window = skipped [' "${TMP}/aa_nodisplay.log" ] \
       && grep -qF 'crc_offscreen = 4007435570' "${TMP}/aa_nodisplay.log" \
       && grep -qF 'trapezoid_aa_partial = ok' "${TMP}/aa_nodisplay.log" \
       && grep -qF 'bad_font_rejected = ok' "${TMP}/aa_nodisplay.log" \
       && grep -qF 'gui_done = true' "${TMP}/aa_nodisplay.log"; then
        aapass "无 DISPLAY：离屏光栅化器 / 覆盖面积 AA / 双字体 / 错误槽全绿，窗口明确跳过，CRC 稳定"
    else
        aabad "无 DISPLAY 路径断言失败（exit=$aa_no_exit bad=$aa_bad）"
        tail -30 "${TMP}/aa_nodisplay.log"
    fi
else aabad "use_gui.dial 编译失败"; tail -12 "${TMP}/aa_comp.log"; fi

# 4b) 控件框架层 demo：独立纯内存树不连显示服务器，因此无 DISPLAY 也能把
#     布局 / 命中 / packed 事件块布局 / string_from_cstr 全部跑完。
#     刻意不创建 GuiPainter，绝不触碰 use_gui.dial 的 crc_offscreen 基准。
if "$DIALC" build tests/pkg/use_gui_widget.dial -o "${TMP}/use_gui_widget" >"${TMP}/aa_wcomp.log" 2>&1; then
    aapass "use_gui_widget.dial（控件框架层）编译成功"
    env -u DISPLAY "${TMP}/use_gui_widget" >"${TMP}/aa_wnodisplay.log" 2>&1
    aa_w_no_exit=$?
    aa_w_bad=$(grep -c ' = bad' "${TMP}/aa_wnodisplay.log" || true)
    if [ "$aa_w_no_exit" -eq 0 ] && [ "$aa_w_bad" -eq 0 ] \
       && grep -qF 'event_size_104 = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'widget_id_arc_copy = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'vbox_idempotent = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'hit_last_child_first = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'widget_window = skipped [' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'widget_done = true' "${TMP}/aa_wnodisplay.log"; then
        aapass "无 DISPLAY：控件树 / 三种布局 / 命中 / packed 布局双向断言 / ARC 字符串复制全绿"
    else
        aabad "控件框架层无 DISPLAY 路径断言失败（exit=$aa_w_no_exit bad=$aa_w_bad）"
        tail -30 "${TMP}/aa_wnodisplay.log"
    fi
else aabad "use_gui_widget.dial 编译失败"; tail -12 "${TMP}/aa_wcomp.log"; fi

# 4c) 成品控件外观层 demo：独立 offscreen 画布（关 AA）逐像素可断言，四态 /
#     CheckBox 翻转 / 禁用配色 / 文本 ARC 复制全部在无 DISPLAY 下跑完；同样
#     绝不触碰 use_gui.dial 的 crc_offscreen 基准。
if "$DIALC" build tests/pkg/use_gui_look.dial -o "${TMP}/use_gui_look" >"${TMP}/aa_lcomp.log" 2>&1; then
    aapass "use_gui_look.dial（成品控件外观层）编译成功"
    env -u DISPLAY "${TMP}/use_gui_look" >"${TMP}/aa_lnodisplay.log" 2>&1
    aa_l_no_exit=$?
    aa_l_bad=$(grep -c ' = bad' "${TMP}/aa_lnodisplay.log" || true)
    if [ "$aa_l_no_exit" -eq 0 ] && [ "$aa_l_bad" -eq 0 ] \
       && grep -qF 'look_crc_pinned = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_four_states_distinct = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_state_reset_idempotent = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_btn_bg_normal = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_btn_bg_hover = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_btn_bg_pressed = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_btn_bg_disabled = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_checkbox_checked_on = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_checkbox_checked_off = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_text_arc_copy = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_window = skipped [' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'look_done = true' "${TMP}/aa_lnodisplay.log"; then
        aapass "无 DISPLAY：成品控件外观层四态互异 / 复位幂等 / 禁用配色 / CheckBox 翻转 / 文本 ARC 复制全绿"
    else
        aabad "成品控件外观层无 DISPLAY 路径断言失败（exit=$aa_l_no_exit bad=$aa_l_bad）"
        tail -30 "${TMP}/aa_lnodisplay.log"
    fi
else aabad "use_gui_look.dial 编译失败"; tail -12 "${TMP}/aa_lcomp.log"; fi

# 4d) 输入层 demo：独立纯内存树 + 独立 offscreen 画布，无 DISPLAY 也能把
#     TextBox 编辑 / 选区 / UTF-8 边界 / 焦点环 / max_len / 统一键码 / 焦点态
#     绘制逐像素断言跑完；绝不触碰前三个 demo 的 CRC 基准。
if "$DIALC" build tests/pkg/use_gui_input.dial -o "${TMP}/use_gui_input" >"${TMP}/aa_icomp.log" 2>&1; then
    aapass "use_gui_input.dial（输入层 TextBox / 焦点链）编译成功"
    env -u DISPLAY "${TMP}/use_gui_input" >"${TMP}/aa_inodisplay.log" 2>&1
    aa_i_no_exit=$?
    aa_i_bad=$(grep -c ' = bad' "${TMP}/aa_inodisplay.log" || true)
    if [ "$aa_i_no_exit" -eq 0 ] && [ "$aa_i_bad" -eq 0 ] \
       && grep -qF 'input_kind_textbox = ok' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_text_typed = ok' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_text_after_replace = ok' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_utf8_boundary = ok' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_max_len_reject = ok' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_key_tab_result = ok' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_focus_crc_differs = ok' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_focus_cleared_on_remove = ok' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_window = skipped [' "${TMP}/aa_inodisplay.log" \
       && grep -qF 'input_done = true' "${TMP}/aa_inodisplay.log"; then
        aapass "无 DISPLAY：TextBox 编辑 / 选区 / UTF-8 边界 / 焦点环 / max_len / 统一键码 / 焦点态绘制全绿"
    else
        aabad "输入层无 DISPLAY 路径断言失败（exit=$aa_i_no_exit bad=$aa_i_bad）"
        tail -40 "${TMP}/aa_inodisplay.log"
    fi
else aabad "use_gui_input.dial 编译失败"; tail -16 "${TMP}/aa_icomp.log"; fi

# 4e) 第 5 层 demo：14 角色可覆写主题色（set / get / reset 幂等 / 非法值
#     Fail-Fast）+ 八种控件种类 + Radio 同组互斥 / Slider 按局部 x 设值 /
#     ProgressBar 只读 / 值域与值 Fail-Fast / 主题改色逐像素生效；独立
#     offscreen 画布，绝不触碰前四个 demo 的 CRC 基准。
if "$DIALC" build tests/pkg/use_gui_theme.dial -o "${TMP}/use_gui_theme" >"${TMP}/aa_tcomp.log" 2>&1; then
    aapass "use_gui_theme.dial（主题层 + 控件扩展）编译成功"
    env -u DISPLAY "${TMP}/use_gui_theme" >"${TMP}/aa_tnodisplay.log" 2>&1
    aa_t_no_exit=$?
    aa_t_bad=$(grep -c ' = bad' "${TMP}/aa_tnodisplay.log" || true)
    if [ "$aa_t_no_exit" -eq 0 ] && [ "$aa_t_bad" -eq 0 ] \
       && grep -qF 'theme_role_count = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_reset_idempotent = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_widgets_crc_stable = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_recolor_crc_differs = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_radio_group_exclusive = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_slider_monotonic = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_progress_readonly = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_value_clamped_into_new_range = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_bad_kind_rejected = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_window = skipped [' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'theme_done = true' "${TMP}/aa_tnodisplay.log"; then
        aapass "无 DISPLAY：14 角色主题 set/get/reset 幂等 / 八种控件 / Radio 互斥 / Slider 设值 / Progress 只读 / 值域 Fail-Fast 全绿"
    else
        aabad "主题层无 DISPLAY 路径断言失败（exit=$aa_t_no_exit bad=$aa_t_bad）"
        tail -40 "${TMP}/aa_tnodisplay.log"
    fi
else aabad "use_gui_theme.dial 编译失败"; tail -16 "${TMP}/aa_tcomp.log"; fi

# 4f) 第 6 层 demo：信号槽 / 控件事件订阅——6 类可订阅纯值信号 + 每树环形
#     队列（72 字节纯值块 sizeof/offsetof 双向锁死 / 零订阅零开销 / 订阅掩码
#     Fail-Fast / 点击·勾选·文本·值·焦点·提交六类信号 / Dial 主动
#     pending→data→pop 分派 / seq 单调 / 溢出 dropped 可观测 / 取消订阅）。
#     独立纯内存树，绝不触碰前五个 demo 的 CRC 基准。给 AA 组 +3 条 aapass。
if "$DIALC" build tests/pkg/use_gui_signal.dial -o "${TMP}/use_gui_signal" >"${TMP}/aa_scomp.log" 2>&1; then
    aapass "use_gui_signal.dial（第 6 层信号槽 / 控件事件订阅）编译成功"
    env -u DISPLAY "${TMP}/use_gui_signal" >"${TMP}/aa_snodisplay.log" 2>&1
    aa_s_no_exit=$?
    aa_s_bad=$(grep -c ' = bad' "${TMP}/aa_snodisplay.log" || true)
    if [ "$aa_s_no_exit" -eq 0 ] && [ "$aa_s_bad" -eq 0 ] \
       && grep -qF 'signal_size_72 = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_kind_count_7 = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_zero_pending_empty = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_click_code = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_subscribe_bit8_rejected = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_toggle_sig = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_setvalue_sig = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_text_sig = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_focus_gain_sig = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_submit_sig = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_seq_monotonic = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_overflow_dropped_observable = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_window = skipped [' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'signal_done = true' "${TMP}/aa_snodisplay.log"; then
        aapass "无 DISPLAY：信号槽纯值块布局 / 零订阅零开销 / 六类信号 / 订阅 Fail-Fast / 溢出可观测 / 主动分派全绿"
    else
        aabad "信号槽层无 DISPLAY 路径断言失败（exit=$aa_s_no_exit bad=$aa_s_bad）"
        tail -45 "${TMP}/aa_snodisplay.log"
    fi
else aabad "use_gui_signal.dial 编译失败"; tail -20 "${TMP}/aa_scomp.log"; fi

# 4g) 第 7 层 demo：菜单 / 对话框——纯内存对象（菜单项增删/排序/越界/禁用 +
#     tag 键 + 对话框标题/消息/按钮位/结果码）在无 DISPLAY 也能全跑；需窗口的
#     顶栏/弹窗/对话框覆盖层路径在 env -u DISPLAY 下**明确跳过**（Fail-Fast 口径）。
#     独立纯内存 + 独立 offscreen 画布，绝不触碰前六个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_menu.dial -o "${TMP}/use_gui_menu" >"${TMP}/aa_mcomp.log" 2>&1; then
    aapass "use_gui_menu.dial（第 7 层菜单 / 对话框）编译成功"
    env -u DISPLAY "${TMP}/use_gui_menu" >"${TMP}/aa_mnodisplay.log" 2>&1
    aa_m_no_exit=$?
    aa_m_bad=$(grep -c ' = bad' "${TMP}/aa_mnodisplay.log" || true)
    if [ "$aa_m_no_exit" -eq 0 ] && [ "$aa_m_bad" -eq 0 ] \
       && grep -qF 'menu_new = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'menu_count_4 = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'menu_item_oob_rejected = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'menu_set_tag = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'menu_tag_readback = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'menu_disable_quit = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'dialog_new = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'dialog_set_buttons_okcancel = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'dialog_bad_buttons_rejected = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'dialog_result_unclicked = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'dialog_open_false = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'menu_free = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'dialog_free = ok' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'gui_menu_window = skipped [' "${TMP}/aa_mnodisplay.log" \
       && grep -qF 'gui_menu_done = true' "${TMP}/aa_mnodisplay.log"; then
        aapass "无 DISPLAY：菜单对象增删/排序/越界/禁用/tag + 对话框按钮位/结果码全绿，覆盖层窗口明确跳过"
    else
        aabad "第 7 层无 DISPLAY 路径断言失败（exit=$aa_m_no_exit bad=$aa_m_bad）"
        tail -40 "${TMP}/aa_mnodisplay.log"
    fi
else aabad "use_gui_menu.dial 编译失败"; tail -20 "${TMP}/aa_mcomp.log"; fi

# 5h) 第 9 层滚动视图：纯内存对象 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存 + 独立 offscreen 画布，绝不触碰前七个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_scroll.dial -o "${TMP}/use_gui_scroll" >"${TMP}/aa_scomp.log" 2>&1; then
    aapass "use_gui_scroll.dial（第 9 层滚动视图）编译成功"
    env -u DISPLAY "${TMP}/use_gui_scroll" >"${TMP}/aa_snodisplay.log" 2>&1
    aa_s_no_exit=$?
    aa_s_bad=$(grep -c ' = bad' "${TMP}/aa_snodisplay.log" || true)
    if [ "$aa_s_no_exit" -eq 0 ] && [ "$aa_s_bad" -eq 0 ] \
       && grep -qF 'scroll_new = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_enable_both = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_clamp_x_max = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_clamp_y_max = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_wheel = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_hit_track = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_hit_thumb = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_hit_none = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_drag_clamped = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_tag_roundtrip = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_bar_size_small_rejected = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_crc_nonzero = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_crc_stable = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_window = ok' "${TMP}/aa_snodisplay.log" \
       && grep -qF 'scroll_done = true' "${TMP}/aa_snodisplay.log"; then
        aapass "无 DISPLAY：滚动视图夹紧/滚轮/命中(轨道·滑块·无)/拖拽/tag 全宽 / bar_size Fail-Fast / 离屏绘制 CRC 稳定全绿"
    else
        aabad "第 9 层无 DISPLAY 路径断言失败（exit=$aa_s_no_exit bad=$aa_s_bad）"
        tail -40 "${TMP}/aa_snodisplay.log"
    fi
else aabad "use_gui_scroll.dial 编译失败"; tail -20 "${TMP}/aa_scomp.log"; fi

# 5i) 第 10 层右键上下文菜单：纯内存对象 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存 + 独立 offscreen 画布，绝不触碰前九个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_context.dial -o "${TMP}/use_gui_context" >"${TMP}/aa_ccomp.log" 2>&1; then
    aapass "use_gui_context.dial（第 10 层右键上下文菜单）编译成功"
    env -u DISPLAY "${TMP}/use_gui_context" >"${TMP}/aa_cnodisplay.log" 2>&1
    aa_c_no_exit=$?
    aa_c_bad=$(grep -c ' = bad' "${TMP}/aa_cnodisplay.log" || true)
    if [ "$aa_c_no_exit" -eq 0 ] && [ "$aa_c_bad" -eq 0 ] \
       && grep -qF 'context_new = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_open_without_menu_rejected = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_set_null_menu_rejected = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_menu_same_object = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_hit_row0 = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_hit_sep_rejected = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_hit_outside = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_hover_hit = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_select_row0_id = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_closed_after_select = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_stays_open_after_miss = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_hit_when_closed = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_tag_roundtrip = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_menu_survives_free = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_crc_nonzero = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_crc_stable = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_crc_changed_on_hover = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_window = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'context_done = true' "${TMP}/aa_cnodisplay.log"; then
        aapass "无 DISPLAY：上下文菜单借用菜单/锚点/命中(行序·分隔符·区域外)/悬停/选中收起 / tag 全宽 / 借菜单不随 free 释放 / 离屏绘制 CRC 稳定全绿"
    else
        aabad "第 10 层无 DISPLAY 路径断言失败（exit=$aa_c_no_exit bad=$aa_c_bad）"
        tail -40 "${TMP}/aa_cnodisplay.log"
    fi
else aabad "use_gui_context.dial 编译失败"; tail -20 "${TMP}/aa_ccomp.log"; fi

# 5j) 第 11 层悬浮提示 Tooltip：纯内存对象 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存 + 独立 offscreen 画布，绝不触碰前十个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_tooltip.dial -o "${TMP}/use_gui_tooltip" >"${TMP}/aa_tcomp.log" 2>&1; then
    aapass "use_gui_tooltip.dial（第 11 层悬浮提示）编译成功"
    env -u DISPLAY "${TMP}/use_gui_tooltip" >"${TMP}/aa_tnodisplay.log" 2>&1
    aa_t_no_exit=$?
    aa_t_bad=$(grep -c ' = bad' "${TMP}/aa_tnodisplay.log" || true)
    if [ "$aa_t_no_exit" -eq 0 ] && [ "$aa_t_bad" -eq 0 ] \
       && grep -qF 'tooltip_new = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_default_delay = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_tick_without_text = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_text_readback = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_tick_below_delay = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_hover_ms_accumulated = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_shown_at_delay = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_hidden_after_reset = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_reaccum_at_delay = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_negative_delay_rejected = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_hover_ms_cleared_on_new_text = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_empty_text_never_shown = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_zero_width_rejected = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_tag_roundtrip = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_crc_nonzero = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_crc_stable = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_crc_unchanged_after_reset = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_window = ok' "${TMP}/aa_tnodisplay.log" \
       && grep -qF 'tooltip_done = true' "${TMP}/aa_tnodisplay.log"; then
        aapass "无 DISPLAY：提示文本拷贝/延迟计时(纯值推进·不读时钟)/达延迟才显示/reset 归零/换文本归零/空文本永不显示 / 位置·尺寸 Fail-Fast / tag 全宽 / 离屏绘制 CRC 稳定全绿"
    else
        aabad "第 11 层无 DISPLAY 路径断言失败（exit=$aa_t_no_exit bad=$aa_t_bad）"
        tail -40 "${TMP}/aa_tnodisplay.log"
    fi
else aabad "use_gui_tooltip.dial 编译失败"; tail -20 "${TMP}/aa_tcomp.log"; fi

# 5k) 第 12 层状态栏 StatusBar：纯内存对象 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存 + 独立 offscreen 画布，绝不触碰前十一个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_statusbar.dial -o "${TMP}/use_gui_statusbar" >"${TMP}/aa_bcomp.log" 2>&1; then
    aapass "use_gui_statusbar.dial（第 12 层状态栏）编译成功"
    env -u DISPLAY "${TMP}/use_gui_statusbar" >"${TMP}/aa_bnodisplay.log" 2>&1
    aa_b_no_exit=$?
    aa_b_bad=$(grep -c ' = bad' "${TMP}/aa_bnodisplay.log" || true)
    if [ "$aa_b_no_exit" -eq 0 ] && [ "$aa_b_bad" -eq 0 ] \
       && grep -qF 'statusbar_new = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_default_height = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_default_vmax = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_left_readback = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_right_readback = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_left_cleared = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_small_height_rejected = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_value_clamped_max = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_value_clamped_min = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_value_clamped_on_narrow = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_bad_range_rejected = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_tag_roundtrip = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_crc_nonzero = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_crc_stable = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_crc_changed_on_progress = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_crc_changed_on_value = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_crc_changed_on_right_text = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_window = ok' "${TMP}/aa_bnodisplay.log" \
       && grep -qF 'statusbar_done = true' "${TMP}/aa_bnodisplay.log"; then
        aapass "无 DISPLAY：状态栏左右文本/高度·分隔线·进度槽位开关/进度值越界夹紧/值域收窄同步夹紧/非法值域 Fail-Fast / tag 全宽 / 离屏绘制 CRC 稳定全绿"
    else
        aabad "第 12 层无 DISPLAY 路径断言失败（exit=$aa_b_no_exit bad=$aa_b_bad）"
        tail -40 "${TMP}/aa_bnodisplay.log"
    fi
else aabad "use_gui_statusbar.dial 编译失败"; tail -20 "${TMP}/aa_bcomp.log"; fi

# 5l) 第 13 层通知气泡 Toast：纯内存队列对象 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存 + 独立 offscreen 画布，绝不触碰前十二个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_toast.dial -o "${TMP}/use_gui_toast" >"${TMP}/aa_ocomp.log" 2>&1; then
    aapass "use_gui_toast.dial（第 13 层通知气泡）编译成功"
    env -u DISPLAY "${TMP}/use_gui_toast" >"${TMP}/aa_onodisplay.log" 2>&1
    aa_o_no_exit=$?
    aa_o_bad=$(grep -c ' = bad' "${TMP}/aa_onodisplay.log" || true)
    if [ "$aa_o_no_exit" -eq 0 ] && [ "$aa_o_bad" -eq 0 ] \
       && grep -qF 'toast_new = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_empty = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_push_info = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_count_3 = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_item0_text = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_item2_kind = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_bad_kind_rejected = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_item_tag_roundtrip = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_tick_expired_count = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_fifo_shift_after_expire = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_never_expire_stays = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_clear = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_full_count = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_newest_at_tail = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_zero_width_rejected = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_negative_spacing_rejected = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_tag_roundtrip = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_crc_nonzero = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_crc_stable = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_crc_changed_on_second = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_crc_changed_on_kind = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_crc_unchanged_after_expire = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_window = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'toast_done = true' "${TMP}/aa_onodisplay.log"; then
        aapass "无 DISPLAY：通知队列 FIFO/满时挤最旧/三种 kind/非法 kind Fail-Fast / 生命周期纯值推进·到期消隐·不过期永驻 / 单条与整体 tag 全宽 / 离屏绘制 CRC 稳定全绿"
    else
        aabad "第 13 层无 DISPLAY 路径断言失败（exit=$aa_o_no_exit bad=$aa_o_bad）"
        tail -40 "${TMP}/aa_onodisplay.log"
    fi
else aabad "use_gui_toast.dial 编译失败"; tail -20 "${TMP}/aa_ocomp.log"; fi

# 5m) 第 14 层窗口级覆盖层自动集成：无 DISPLAY 路径——5 类对象纯内存可建，
#     15 个新 API 对空窗口一律 Fail-Fast，应用未创建时如实 skipped，对象仍可独立 free。
#     给 AA 组 +2 条 aapass（编译 / 无 DISPLAY 断言）。
if "$DIALC" build tests/pkg/use_gui_overlays.dial -o "${TMP}/use_gui_overlays" >"${TMP}/aa_vcomp.log" 2>&1; then
    aapass "use_gui_overlays.dial（第 14 层窗口级覆盖层自动集成）编译成功"
    env -u DISPLAY "${TMP}/use_gui_overlays" >"${TMP}/aa_vnodisplay.log" 2>&1
    aa_v_no_exit=$?
    aa_v_bad=$(grep -c ' = bad' "${TMP}/aa_vnodisplay.log" || true)
    if [ "$aa_v_no_exit" -eq 0 ] && [ "$aa_v_bad" -eq 0 ] \
       && grep -qF 'overlays_scroll_new = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_toast_new = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_set_scroll_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_set_context_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_set_tooltip_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_set_statusbar_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_set_toast_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_scroll_get_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_toast_get_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_clear_scroll_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_clear_toast_null = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_app = skipped' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_window = skipped' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_app_free = skipped' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_scroll_free = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_toast_free = ok' "${TMP}/aa_vnodisplay.log" \
       && grep -qF 'overlays_done = true' "${TMP}/aa_vnodisplay.log"; then
        aapass "无 DISPLAY：15 个窗口覆盖层 API 空窗口 Fail-Fast / 5 类对象纯内存可建并独立 free / 应用未创建如实 skipped 全绿"
    else
        aabad "第 14 层无 DISPLAY 路径断言失败（exit=$aa_v_no_exit bad=$aa_v_bad）"
        tail -40 "${TMP}/aa_vnodisplay.log"
    fi
else aabad "use_gui_overlays.dial 编译失败"; tail -20 "${TMP}/aa_vcomp.log"; fi

# 5) 有 Xvfb 时验证真实窗口（XShm 离屏 + XRender 合成 + 上屏 / 过滤 / 事件循环）；
#    无 Xvfb 的环境如实打印跳过，绝不把「未验证」写成「通过」。
if command -v xvfb-run >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
        "${TMP}/use_gui" >"${TMP}/aa_xvfb.log" 2>&1; then
        aa_bad2=$(grep -c ' = bad' "${TMP}/aa_xvfb.log" || true)
        if [ "$aa_bad2" -eq 0 ] \
           && grep -qF 'window = ok' "${TMP}/aa_xvfb.log" \
           && grep -qF 'window_backend = x11/xshm/xrender' "${TMP}/aa_xvfb.log" \
           && grep -qF 'window_flush = ok' "${TMP}/aa_xvfb.log" \
           && grep -qF 'window_filter = ok' "${TMP}/aa_xvfb.log" \
           && grep -qF 'quit_event = ok' "${TMP}/aa_xvfb.log"; then
            aapass "Xvfb 实窗：XShm 离屏 + XRender filter 合成 + 上屏 / 过滤 / 事件循环全通过"
        else
            aabad "Xvfb 实窗断言失败（bad=$aa_bad2）"; tail -30 "${TMP}/aa_xvfb.log"
        fi
    else
        aabad "Xvfb 下 use_gui 运行失败"; tail -20 "${TMP}/aa_xvfb.log"
    fi
else
    echo "  ⚠️  本环境无 xvfb-run：真实窗口路径未实机验证（如实标注，不伪装通过）"
fi

# 5b) 控件框架层实窗：挂树 / QUIT 主动分发 / packed 纯值块 /
#     string_from_cstr 对 NULL 借用指针安全（返回空串）。
if command -v xvfb-run >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
        "${TMP}/use_gui_widget" >"${TMP}/aa_wxvfb.log" 2>&1; then
        aa_w_bad2=$(grep -c ' = bad' "${TMP}/aa_wxvfb.log" || true)
        if [ "$aa_w_bad2" -eq 0 ] \
           && grep -qF 'widget_window = ok' "${TMP}/aa_wxvfb.log" \
           && grep -qF 'window_set_root = ok' "${TMP}/aa_wxvfb.log" \
           && grep -qF 'event_kind_quit = ok' "${TMP}/aa_wxvfb.log" \
           && grep -qF 'event_win_title_null_borrow = ok' "${TMP}/aa_wxvfb.log" \
           && grep -qF 'widget_done = true' "${TMP}/aa_wxvfb.log"; then
            aapass "Xvfb 实窗：window_set_root / QUIT 主动分发 / packed 纯值块 / 借用指针空串全通过"
        else
            aabad "控件框架层 Xvfb 实窗断言失败（bad=$aa_w_bad2）"; tail -30 "${TMP}/aa_wxvfb.log"
        fi
    else
        aabad "Xvfb 下 use_gui_widget 运行失败"; tail -20 "${TMP}/aa_wxvfb.log"
    fi
else
    echo "  ⚠️  本环境无 xvfb-run：控件框架层实窗路径未实机验证（如实标注，不伪装通过）"
fi

# 5c) 成品控件外观层实窗：把整棵树挂到窗口、画进窗口画布并 flush 上屏。
if command -v xvfb-run >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
        "${TMP}/use_gui_look" >"${TMP}/aa_lxvfb.log" 2>&1; then
        aa_l_bad2=$(grep -c ' = bad' "${TMP}/aa_lxvfb.log" || true)
        if [ "$aa_l_bad2" -eq 0 ] \
           && grep -qF 'look_window = ok' "${TMP}/aa_lxvfb.log" \
           && grep -qF 'look_window_set_root = ok' "${TMP}/aa_lxvfb.log" \
           && grep -qF 'look_window_draw = ok' "${TMP}/aa_lxvfb.log" \
           && grep -qF 'look_window_flush = ok' "${TMP}/aa_lxvfb.log" \
           && grep -qF 'look_done = true' "${TMP}/aa_lxvfb.log"; then
            aapass "Xvfb 实窗：window_set_root 挂树 + 整树绘制 + flush 上屏全通过"
        else
            aabad "成品控件外观层 Xvfb 实窗断言失败（bad=$aa_l_bad2）"; tail -30 "${TMP}/aa_lxvfb.log"
        fi
    else
        aabad "Xvfb 下 use_gui_look 运行失败"; tail -20 "${TMP}/aa_lxvfb.log"
    fi
else
    echo "  ⚠️  本环境无 xvfb-run：成品控件外观层实窗路径未实机验证（如实标注，不伪装通过）"
fi

# 5d) XTest 真实事件注入：在 Xvfb 实窗里用 xdotool 真注入鼠标点击 / 键盘 "hi"
#     / Tab / Shift+Tab，Dial 侧主动 poll/wait 读走事件再喂进纯 C 逻辑。
#     libXtst 只在测试侧用，包本体三份 .a 仍零平台链接期依赖（第 2 项已断言）。
if command -v xvfb-run >/dev/null 2>&1 && command -v xdotool >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
        bash tests/pkg/xtest_inject.sh "${TMP}/use_gui_input" "${TMP}/aa_xtest.log" \
        >"${TMP}/aa_xtest_runner.log" 2>&1; then
        if grep -qF 'input_xtest_injection = skipped [' "${TMP}/aa_xtest.log"; then
            echo "  ⚠️  XTest 注入未实机验证：$(grep -F 'input_xtest_injection = skipped [' "${TMP}/aa_xtest.log" | head -1)"
            echo "      （如实标注，不伪装通过）"
        else
            aa_x_bad=$(grep -c ' = bad' "${TMP}/aa_xtest.log" || true)
            if [ "$aa_x_bad" -eq 0 ] \
               && grep -qF 'input_xtest_injection = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_window = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_window_flush = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_xtest_keys = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_xtest_tabs = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_xtest_shift_mods = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_xtest_mouse_events = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_xtest_mouse_activation = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_xtest_typed_text = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_xtest_focus_back = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_xtest_flush = ok' "${TMP}/aa_xtest.log" \
               && grep -qF 'input_done = true' "${TMP}/aa_xtest.log"; then
                aapass "Xvfb + XTest 实机：真实鼠标点击 / 键盘落字 / Tab·Shift+Tab 焦点环 / 按钮激活 / 统一键码 + mods 全通过"
            else
                aabad "XTest 实窗注入断言失败（bad=$aa_x_bad）"
                tail -45 "${TMP}/aa_xtest.log"
                tail -15 "${TMP}/aa_xtest_runner.log"
            fi
        fi
    else
        aabad "Xvfb 下 xtest_inject.sh 运行失败"
        tail -25 "${TMP}/aa_xtest_runner.log"
        tail -25 "${TMP}/aa_xtest.log"
    fi
else
    echo "  ⚠️  本环境无 xvfb-run / xdotool：XTest 真实事件注入未实机验证（如实标注，不伪装通过）"
fi

# 5e) 第 5 层实窗：把含八种控件的树挂到窗口、画进窗口画布并 flush 上屏。
if command -v xvfb-run >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
        "${TMP}/use_gui_theme" >"${TMP}/aa_txvfb.log" 2>&1; then
        aa_t_bad2=$(grep -c ' = bad' "${TMP}/aa_txvfb.log" || true)
        if [ "$aa_t_bad2" -eq 0 ] \
           && grep -qF 'theme_window = ok' "${TMP}/aa_txvfb.log" \
           && grep -qF 'theme_window_set_root = ok' "${TMP}/aa_txvfb.log" \
           && grep -qF 'theme_window_draw = ok' "${TMP}/aa_txvfb.log" \
           && grep -qF 'theme_window_flush = ok' "${TMP}/aa_txvfb.log" \
           && grep -qF 'theme_done = true' "${TMP}/aa_txvfb.log"; then
            aapass "Xvfb 实窗：主题层八种控件整树绘制 + flush 上屏全通过"
        else
            aabad "主题层 Xvfb 实窗断言失败（bad=$aa_t_bad2）"; tail -30 "${TMP}/aa_txvfb.log"
        fi
    else
        aabad "Xvfb 下 use_gui_theme 运行失败"; tail -20 "${TMP}/aa_txvfb.log"
    fi
else
    echo "  ⚠️  本环境无 xvfb-run：主题层实窗路径未实机验证（如实标注，不伪装通过）"
fi

# 5f) 第 6 层实窗：把含按钮的树挂到窗口、订阅 CLICKED、喂纯值事件读信号
#     队列、整树绘制并 flush 上屏。给 AA 组 +1 条 aapass。
if command -v xvfb-run >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
        "${TMP}/use_gui_signal" >"${TMP}/aa_sxvfb.log" 2>&1; then
        aa_s_bad2=$(grep -c ' = bad' "${TMP}/aa_sxvfb.log" || true)
        if [ "$aa_s_bad2" -eq 0 ] \
           && grep -qF 'signal_window = ok' "${TMP}/aa_sxvfb.log" \
           && grep -qF 'signal_window_set_root = ok' "${TMP}/aa_sxvfb.log" \
           && grep -qF 'signal_window_signal_tag = ok' "${TMP}/aa_sxvfb.log" \
           && grep -qF 'signal_window_draw = ok' "${TMP}/aa_sxvfb.log" \
           && grep -qF 'signal_window_flush = ok' "${TMP}/aa_sxvfb.log" \
           && grep -qF 'signal_done = true' "${TMP}/aa_sxvfb.log"; then
            aapass "Xvfb 实窗：信号槽挂树 / 订阅 CLICKED / 纯值信号读取 / 整树绘制 + flush 上屏全通过"
        else
            aabad "信号槽层 Xvfb 实窗断言失败（bad=$aa_s_bad2）"; tail -30 "${TMP}/aa_sxvfb.log"
        fi
    else
        aabad "Xvfb 下 use_gui_signal 运行失败"; tail -20 "${TMP}/aa_sxvfb.log"
    fi
else
    echo "  ⚠️  本环境无 xvfb-run：信号槽层实窗路径未实机验证（如实标注，不伪装通过）"
fi

# 5g) 第 7 层实窗：顶栏菜单点击 → SELECTED 信号（kind=MENU，a=item id，tag 透传）/
#     弹出菜单命中项 → SELECTED（a=Paste）/ 外部点击关闭弹窗 / 模态对话框按钮点击
#     → SUBMITTED 信号（kind=DIALOG，a=结果码）且对话框自动关闭 / 控件树仍可用
#     （按钮 CLICKED）。覆盖层不进控件树，故控件几何 / 绘制 CRC 逐字节不漂移。
#     给 AA 组 +1 条 aapass。
if command -v xvfb-run >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
        "${TMP}/use_gui_menu" >"${TMP}/aa_mxvfb.log" 2>&1; then
        aa_m_bad2=$(grep -c ' = bad' "${TMP}/aa_mxvfb.log" || true)
        if [ "$aa_m_bad2" -eq 0 ] \
           && grep -qF 'gui_menu_window = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_window_set_root = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_set_menubar = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_bar_click_activates = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_bar_sig_selected = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_bar_item_id = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_bar_kind_menu = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_bar_tag = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_popup_click_activates = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_popup_a = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_popup_close = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_dialog_click_activates = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_dialog_result_ok = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_dialog_sig_submitted = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_dialog_kind_dialog = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_dialog_result_code = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_widget_click_activates = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_widget_sig = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'menu_widget_tag = ok' "${TMP}/aa_mxvfb.log" \
           && grep -qF 'gui_menu_done = true' "${TMP}/aa_mxvfb.log"; then
            aapass "Xvfb 实窗：顶栏/弹窗 SELECTED(a=item id) + 模态对话框 SUBMITTED(a=结果码) 信号 + 控件树 CLICKED 全通过"
        else
            aabad "第 7 层 Xvfb 实窗断言失败（bad=$aa_m_bad2）"; tail -30 "${TMP}/aa_mxvfb.log"
        fi
    else
        aabad "Xvfb 下 use_gui_menu 运行失败"; tail -20 "${TMP}/aa_mxvfb.log"
    fi
else
    echo "  ⚠️  本环境无 xvfb-run：第 7 层实窗路径未实机验证（如实标注，不伪装通过）"
fi

# 5n) 第 14 层 Xvfb 实窗：5 类覆盖层挂到窗口槽位后，win.draw() 按固定 z-order
#     自动叠加；空槽位两次 draw CRC 稳定，挂载后 CRC 改变且重绘幂等，清空后 CRC
#     回到基线；clear 只清槽位不释放对象，app_free 之后 5 个对象仍可 free（借用语义）。
#     给 AA 组 +1 条 aapass。
if command -v xvfb-run >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
        "${TMP}/use_gui_overlays" >"${TMP}/aa_vxvfb.log" 2>&1; then
        aa_v_bad2=$(grep -c ' = bad' "${TMP}/aa_vxvfb.log" || true)
        if [ "$aa_v_bad2" -eq 0 ] \
           && grep -qF 'overlays_app = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_window = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_window_painter = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_draw_empty = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_empty_crc_stable = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_set_scroll = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_set_toast = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_scroll_readback = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_toast_readback = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_draw_mounted = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_mounted_crc_changed = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_mounted_crc_stable = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_clear_scroll = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_clear_toast = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_scroll_cleared = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_toast_cleared = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_cleared_crc_restored = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_menu_free = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_app_free = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_scroll_free = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_toast_free = ok' "${TMP}/aa_vxvfb.log" \
           && grep -qF 'overlays_done = true' "${TMP}/aa_vxvfb.log" \
           && ! grep -qF 'skipped' "${TMP}/aa_vxvfb.log"; then
            aapass "Xvfb 实窗：5 类覆盖层自动叠加 / 固定 z-order / 空槽位零像素影响 / 挂载 CRC 改变且重绘幂等 / 清空 CRC 回基线 / 借用语义全通过"
        else
            aabad "第 14 层 Xvfb 实窗断言失败（bad=$aa_v_bad2）"; tail -30 "${TMP}/aa_vxvfb.log"
        fi
    else
        aabad "Xvfb 下 use_gui_overlays 运行失败"; tail -20 "${TMP}/aa_vxvfb.log"
    fi
else
    echo "  ⚠️  本环境无 xvfb-run：第 14 层实窗路径未实机验证（如实标注，不伪装通过）"
fi

# 5o) 第 15 层快捷键 / 命令系统：纯内存 KeyMap + 命令启用态 + 离屏画布
#     （零 DISPLAY 依赖，CRC32 像素级锁定）。独立纯内存对象与独立 offscreen
#     画布，绝不触碰前十四个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_hotkey.dial -o "${TMP}/use_gui_hotkey" >"${TMP}/aa_hkcomp.log" 2>&1; then
    aapass "use_gui_hotkey.dial（第 15 层快捷键/命令系统）编译成功"
    env -u DISPLAY "${TMP}/use_gui_hotkey" >"${TMP}/aa_hknodisplay.log" 2>&1
    aa_hk_no_exit=$?
    aa_hk_bad=$(grep -c ' = bad' "${TMP}/aa_hknodisplay.log" || true)
    if [ "$aa_hk_no_exit" -eq 0 ] && [ "$aa_hk_bad" -eq 0 ] \
       && grep -qF 'hotkey_new = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_capacity_64 = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_lookup_ctrl_s = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_dispatch_ctrl_s = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_lookup_bare_s_miss = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_bind_conflict_rejected = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_original_binding_kept = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_rebind_ctrl_s = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_negative_key_rejected = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_bad_mods_rejected = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_dispatch_blocked_when_disabled = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_lookup_ignores_disabled = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_compacted_key_at1 = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_clear_resets_enabled = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_clear_keeps_tag = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_full_count_64 = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_65th_bind_rejected = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_draw_idempotent = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_draw_changed_on_unbind = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_clear_draw_zero_pixels = ok' "${TMP}/aa_hknodisplay.log" \
       && grep -qF 'hotkey_done = true' "${TMP}/aa_hknodisplay.log"; then
        aapass "第 15 层：绑定/冲突/覆盖/压实/容量上界/命令启用态/离屏 CRC 幂等 166 条断言全绿（零 DISPLAY 依赖）"
    else
        aabad "第 15 层断言失败（exit=$aa_hk_no_exit bad=$aa_hk_bad）"
        tail -40 "${TMP}/aa_hknodisplay.log"
    fi
else aabad "use_gui_hotkey.dial 编译失败"; tail -20 "${TMP}/aa_hkcomp.log"; fi

# 5p) 第 16 层网格布局 GRID：纯内存 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存与独立 offscreen 画布，绝不触碰前十五个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_grid.dial -o "${TMP}/use_gui_grid" >"${TMP}/aa_gcomp.log" 2>&1; then
    aapass "use_gui_grid.dial（第 16 层网格布局 GRID）编译成功"
    env -u DISPLAY "${TMP}/use_gui_grid" >"${TMP}/aa_gnodisplay.log" 2>&1
    aa_g_no_exit=$?
    aa_g_bad=$(grep -c ' = bad' "${TMP}/aa_gnodisplay.log" || true)
    if [ "$aa_g_no_exit" -eq 0 ] && [ "$aa_g_bad" -eq 0 ] \
       && grep -qF 'grid_set_ok = ok' "${TMP}/aa_gnodisplay.log" \
       && grep -qF 'grid_apply_gap0 = ok' "${TMP}/aa_gnodisplay.log" \
       && grep -qF 'grid_apply_gap10 = ok' "${TMP}/aa_gnodisplay.log" \
       && grep -qF 'grid_span_apply = ok' "${TMP}/aa_gnodisplay.log" \
       && grep -qF 'grid_crc_pinned = ok' "${TMP}/aa_gnodisplay.log" \
       && grep -qF 'grid_done = true' "${TMP}/aa_gnodisplay.log"; then
        aapass "第 16 层：set_grid/set_grid_cell 越界 Fail-Fast / 2×2 弹性均分几何 / 跨度夹紧 / 离屏绘制 CRC 稳定全绿（零 DISPLAY 依赖）"
    else
        aabad "第 16 层网格布局断言失败（exit=$aa_g_no_exit bad=$aa_g_bad）"
        tail -40 "${TMP}/aa_gnodisplay.log"
    fi
else aabad "use_gui_grid.dial 编译失败"; tail -20 "${TMP}/aa_gcomp.log"; fi

# 5q) 第 16 层组合框 ComboBox：纯内存 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存与独立 offscreen 画布，绝不触碰前十五个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_combo.dial -o "${TMP}/use_gui_combo" >"${TMP}/aa_ccomp.log" 2>&1; then
    aapass "use_gui_combo.dial（第 16 层组合框 ComboBox）编译成功"
    env -u DISPLAY "${TMP}/use_gui_combo" >"${TMP}/aa_cnodisplay.log" 2>&1
    aa_c_no_exit=$?
    aa_c_bad=$(grep -c ' = bad' "${TMP}/aa_cnodisplay.log" || true)
    if [ "$aa_c_no_exit" -eq 0 ] && [ "$aa_c_bad" -eq 0 ] \
       && grep -qF 'combo_set_kind = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_kind8_menu_marker_rejected = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_kind9_dialog_marker_rejected = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_kind_unchanged_after_rejects = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_count_3 = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_selected_2 = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_dropdown_item2_hit = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_signal_a_itemid = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_crc_pinned = ok' "${TMP}/aa_cnodisplay.log" \
       && grep -qF 'combo_done = true' "${TMP}/aa_cnodisplay.log"; then
        aapass "第 16 层：kind=10 / 项增删·容量上界 / 单选·展开态 / 点击框体切换·下拉项命中选中并发 SELECTED / 离屏 CRC 稳定全绿"
    else
        aabad "第 16 层组合框断言失败（exit=$aa_c_no_exit bad=$aa_c_bad）"
        tail -40 "${TMP}/aa_cnodisplay.log"
    fi
else aabad "use_gui_combo.dial 编译失败"; tail -20 "${TMP}/aa_ccomp.log"; fi

# 5r) 第 16 层列表框 ListBox：纯内存 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存与独立 offscreen 画布，绝不触碰前十五个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_list.dial -o "${TMP}/use_gui_list" >"${TMP}/aa_lcomp.log" 2>&1; then
    aapass "use_gui_list.dial（第 16 层列表框 ListBox）编译成功"
    env -u DISPLAY "${TMP}/use_gui_list" >"${TMP}/aa_lnodisplay.log" 2>&1
    aa_l_no_exit=$?
    aa_l_bad=$(grep -c ' = bad' "${TMP}/aa_lnodisplay.log" || true)
    if [ "$aa_l_no_exit" -eq 0 ] && [ "$aa_l_bad" -eq 0 ] \
       && grep -qF 'list_set_kind = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'list_count_4 = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'list_selected_count_3 = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'list_signal_a_itemid = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'list_crc_pinned = ok' "${TMP}/aa_lnodisplay.log" \
       && grep -qF 'list_done = true' "${TMP}/aa_lnodisplay.log"; then
        aapass "第 16 层：kind=11 / 项增删·容量上界 / 单选·多选 toggle / 点击行选中并发 SELECTED / 离屏 CRC 稳定全绿"
    else
        aabad "第 16 层列表框断言失败（exit=$aa_l_no_exit bad=$aa_l_bad）"
        tail -40 "${TMP}/aa_lnodisplay.log"
    fi
else aabad "use_gui_list.dial 编译失败"; tail -20 "${TMP}/aa_lcomp.log"; fi

# 5s) 第 16 层右键 / 滚轮交互：纯内存 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存与独立 offscreen 画布，绝不触碰前十五个 demo 的 CRC 基准。给 AA 组 +1 aapass。
if "$DIALC" build tests/pkg/use_gui_wheel.dial -o "${TMP}/use_gui_wheel" >"${TMP}/aa_wcomp.log" 2>&1; then
    aapass "use_gui_wheel.dial（第 16 层右键 / 滚轮交互）编译成功"
    env -u DISPLAY "${TMP}/use_gui_wheel" >"${TMP}/aa_wnodisplay.log" 2>&1
    aa_w_no_exit=$?
    aa_w_bad=$(grep -c ' = bad' "${TMP}/aa_wnodisplay.log" || true)
    if [ "$aa_w_no_exit" -eq 0 ] && [ "$aa_w_bad" -eq 0 ] \
       && grep -qF 'wheel_set_kind_slider = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'wheel_value_after_up = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'wheel_v0_stay = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'right_pressed_on = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'right_pressed_off = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'wheel_crc_pinned = ok' "${TMP}/aa_wnodisplay.log" \
       && grep -qF 'wheel_done = true' "${TMP}/aa_wnodisplay.log"; then
        aapass "第 16 层：滚轮上/下滚按步进增减 SLIDER 值·clamp 不越界 / 右键置 right_pressed·RELEASE 清 / 滚轮落非 SLIDER 零影响 / 离屏 CRC 稳定全绿"
    else
        aabad "第 16 层右键·滚轮断言失败（exit=$aa_w_no_exit bad=$aa_w_bad）"
        tail -40 "${TMP}/aa_wnodisplay.log"
    fi
else aabad "use_gui_wheel.dial 编译失败"; tail -20 "${TMP}/aa_wcomp.log"; fi

# 5t) 第 17 层 TextBox 撤销 / 重做：纯内存 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存与独立 offscreen 画布，绝不触碰前十六个 demo 的 CRC 基准。给 AA 组 +2 aapass。
if "$DIALC" build tests/pkg/use_gui_undo.dial -o "${TMP}/use_gui_undo" >"${TMP}/aa_ucomp.log" 2>&1; then
    aapass "use_gui_undo.dial（第 17 层 TextBox 撤销 / 重做）编译成功"
    env -u DISPLAY "${TMP}/use_gui_undo" >"${TMP}/aa_unodisplay.log" 2>&1
    aa_u_no_exit=$?
    aa_u_bad=$(grep -c ' = bad' "${TMP}/aa_unodisplay.log" || true)
    if [ "$aa_u_no_exit" -eq 0 ] && [ "$aa_u_bad" -eq 0 ] \
       && grep -qF 'undo_set_text_not_recorded = ok' "${TMP}/aa_unodisplay.log" \
       && grep -qF 'undo_text_abcd = ok' "${TMP}/aa_unodisplay.log" \
       && grep -qF 'redo_text_abcd = ok' "${TMP}/aa_unodisplay.log" \
       && grep -qF 'redo_invalidated_by_edit = ok' "${TMP}/aa_unodisplay.log" \
       && grep -qF 'undo_cursor_restored = ok' "${TMP}/aa_unodisplay.log" \
       && grep -qF 'undo_full_stack_cap = ok' "${TMP}/aa_unodisplay.log" \
       && grep -qF 'undo_crc_pinned = ok' "${TMP}/aa_unodisplay.log" \
       && grep -qF 'undo_done = true' "${TMP}/aa_unodisplay.log"; then
        aapass "第 17 层：快照式 undo/redo·显式 set_text 不进历史·新编辑使 redo 失效·光标随快照恢复·栈满丢最老 / 离屏 CRC 稳定全绿"
    else
        aabad "第 17 层 TextBox 撤销/重做断言失败（exit=$aa_u_no_exit bad=$aa_u_bad）"
        tail -40 "${TMP}/aa_unodisplay.log"
    fi
else aabad "use_gui_undo.dial 编译失败"; tail -20 "${TMP}/aa_ucomp.log"; fi

# 5u) 第 17 层 Slider 拖动：纯内存 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存与独立 offscreen 画布，绝不触碰前十六个 demo 的 CRC 基准。给 AA 组 +2 aapass。
if "$DIALC" build tests/pkg/use_gui_drag.dial -o "${TMP}/use_gui_drag" >"${TMP}/aa_dcomp.log" 2>&1; then
    aapass "use_gui_drag.dial（第 17 层 Slider 拖动）编译成功"
    env -u DISPLAY "${TMP}/use_gui_drag" >"${TMP}/aa_dnodisplay.log" 2>&1
    aa_d_no_exit=$?
    aa_d_bad=$(grep -c ' = bad' "${TMP}/aa_dnodisplay.log" || true)
    if [ "$aa_d_no_exit" -eq 0 ] && [ "$aa_d_bad" -eq 0 ] \
       && grep -qF 'drag_active_on_press = ok' "${TMP}/aa_dnodisplay.log" \
       && grep -qF 'drag_press_no_value_change = ok' "${TMP}/aa_dnodisplay.log" \
       && grep -qF 'drag_value_mid_50 = ok' "${TMP}/aa_dnodisplay.log" \
       && grep -qF 'drag_value_clamped_far = ok' "${TMP}/aa_dnodisplay.log" \
       && grep -qF 'drag_active_off_release = ok' "${TMP}/aa_dnodisplay.log" \
       && grep -qF 'drag_value_unchanged_without_press = ok' "${TMP}/aa_dnodisplay.log" \
       && grep -qF 'drag_crc_pinned = ok' "${TMP}/aa_dnodisplay.log" \
       && grep -qF 'drag_done = true' "${TMP}/aa_dnodisplay.log"; then
        aapass "第 17 层：PRESS 置拖动态不改 value·MOVE 按绝对 x 设值并 clamp·拖出控件仍生效 / 非 SLIDER 不进拖动 / 离屏 CRC 稳定全绿"
    else
        aabad "第 17 层 Slider 拖动断言失败（exit=$aa_d_no_exit bad=$aa_d_bad）"
        tail -40 "${TMP}/aa_dnodisplay.log"
    fi
else aabad "use_gui_drag.dial 编译失败"; tail -20 "${TMP}/aa_dcomp.log"; fi

# 5v) 第 17 层每控件主题覆写：纯内存 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存与独立 offscreen 画布，绝不触碰前十六个 demo 的 CRC 基准。给 AA 组 +2 aapass。
if "$DIALC" build tests/pkg/use_gui_theme_override.dial -o "${TMP}/use_gui_theme_override" >"${TMP}/aa_ocomp.log" 2>&1; then
    aapass "use_gui_theme_override.dial（第 17 层每控件主题覆写）编译成功"
    env -u DISPLAY "${TMP}/use_gui_theme_override" >"${TMP}/aa_onodisplay.log" 2>&1
    aa_o_no_exit=$?
    aa_o_bad=$(grep -c ' = bad' "${TMP}/aa_onodisplay.log" || true)
    if [ "$aa_o_no_exit" -eq 0 ] && [ "$aa_o_bad" -eq 0 ] \
       && grep -qF 'ovr_color_fallback_global = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'ovr_crc_differs = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'ovr_pixel_b1_red = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'ovr_crc_back_to_base = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'ovr_zero_color_legal = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'ovr_all_cleared_6 = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'ovr_crc_pinned = ok' "${TMP}/aa_onodisplay.log" \
       && grep -qF 'ovr_done = true' "${TMP}/aa_onodisplay.log"; then
        aapass "第 17 层：未覆写逐像素回落全局表·覆写只影响本控件·Fail-Fast 边界·color=0 合法·clear 后 CRC 回历史基准 / 离屏 CRC 稳定全绿"
    else
        aabad "第 17 层每控件主题覆写断言失败（exit=$aa_o_no_exit bad=$aa_o_bad）"
        tail -40 "${TMP}/aa_onodisplay.log"
    fi
else aabad "use_gui_theme_override.dial 编译失败"; tail -20 "${TMP}/aa_ocomp.log"; fi

# 5w) 第 17 层 Grid 自动换行：纯内存 + 离屏画布（零 DISPLAY 依赖，CRC32 像素级锁定）。
#     独立纯内存与独立 offscreen 画布，绝不触碰前十六个 demo 的 CRC 基准。给 AA 组 +2 aapass。
if "$DIALC" build tests/pkg/use_gui_autoflow.dial -o "${TMP}/use_gui_autoflow" >"${TMP}/aa_fcomp.log" 2>&1; then
    aapass "use_gui_autoflow.dial（第 17 层 Grid 自动换行）编译成功"
    env -u DISPLAY "${TMP}/use_gui_autoflow" >"${TMP}/aa_fnodisplay.log" 2>&1
    aa_f_no_exit=$?
    aa_f_bad=$(grep -c ' = bad' "${TMP}/aa_fnodisplay.log" || true)
    if [ "$aa_f_no_exit" -eq 0 ] && [ "$aa_f_bad" -eq 0 ] \
       && grep -qF 'flow_c1_x = ok' "${TMP}/aa_fnodisplay.log" \
       && grep -qF 'flow_c2_y = ok' "${TMP}/aa_fnodisplay.log" \
       && grep -qF 'flow_explicit_col_ignored = ok' "${TMP}/aa_fnodisplay.log" \
       && grep -qF 'flow_cspan_respected = ok' "${TMP}/aa_fnodisplay.log" \
       && grep -qF 'flow_off_c1_default_0 = ok' "${TMP}/aa_fnodisplay.log" \
       && grep -qF 'flow_overflow_clamped_y = ok' "${TMP}/aa_fnodisplay.log" \
       && grep -qF 'flow_crc_pinned = ok' "${TMP}/aa_fnodisplay.log" \
       && grep -qF 'flow_done = true' "${TMP}/aa_fnodisplay.log"; then
        aapass "第 17 层：按加入顺序自动排格·显式 col/row 被忽略·cspan 仍生效·关闭回到第 16 层语义·溢出夹末行 / 离屏 CRC 稳定全绿"
    else
        aabad "第 17 层 Grid 自动换行断言失败（exit=$aa_f_no_exit bad=$aa_f_bad）"
        tail -40 "${TMP}/aa_fnodisplay.log"
    fi
else aabad "use_gui_autoflow.dial 编译失败"; tail -20 "${TMP}/aa_fcomp.log"; fi

# 5x) 第 18 层高 DPI 整数放大 + ListBox 虚拟化 lazy load：纯内存 + 离屏画布
#     （零 DISPLAY 依赖，CRC32 像素级锁定）。两个独立 offscreen 画布 / 独立
#     纯内存树，绝不触碰前十六个 demo 的 CRC 基准。给 AA 组 +4 aapass。
if "$DIALC" build tests/pkg/use_gui_scale.dial -o "${TMP}/use_gui_scale" >"${TMP}/aa_sccomp.log" 2>&1; then
    aapass "use_gui_scale.dial（第 18 层高 DPI 整数放大）编译成功"
    env -u DISPLAY "${TMP}/use_gui_scale" >"${TMP}/aa_scnodisplay.log" 2>&1
    aa_sc_no_exit=$?
    aa_sc_bad=$(grep -c ' = bad' "${TMP}/aa_scnodisplay.log" || true)
    if [ "$aa_sc_no_exit" -eq 0 ] && [ "$aa_sc_bad" -eq 0 ] \
       && grep -qF 'scale_default_1 = ok' "${TMP}/aa_scnodisplay.log" \
       && grep -qF 'scale_meta_no_pixel_change = ok' "${TMP}/aa_scnodisplay.log" \
       && grep -qF 'scale_up1_crc_equals_src = ok' "${TMP}/aa_scnodisplay.log" \
       && grep -qF 'scale_up2_w = ok' "${TMP}/aa_scnodisplay.log" \
       && grep -qF 'scale_up3_h = ok' "${TMP}/aa_scnodisplay.log" \
       && grep -qF 'scale_crc_pinned = ok' "${TMP}/aa_scnodisplay.log" \
       && grep -qF 'scale_done = true' "${TMP}/aa_scnodisplay.log"; then
        aapass "第 18 层高 DPI：scale 默认 1·越界 [1,4] Fail-Fast·factor=1 逐字节等价·2x/3x 最近邻放大尺寸/像素正确 / 离屏 CRC 稳定全绿"
    else
        aabad "第 18 层高 DPI 断言失败（exit=$aa_sc_no_exit bad=$aa_sc_bad）"
        tail -40 "${TMP}/aa_scnodisplay.log"
    fi
else aabad "use_gui_scale.dial 编译失败"; tail -20 "${TMP}/aa_sccomp.log"; fi

if "$DIALC" build tests/pkg/use_gui_virtual.dial -o "${TMP}/use_gui_virtual" >"${TMP}/aa_vtcomp.log" 2>&1; then
    aapass "use_gui_virtual.dial（第 18 层 ListBox 虚拟化 lazy load）编译成功"
    env -u DISPLAY "${TMP}/use_gui_virtual" >"${TMP}/aa_vtnodisplay.log" 2>&1
    aa_vt_no_exit=$?
    aa_vt_bad=$(grep -c ' = bad' "${TMP}/aa_vtnodisplay.log" || true)
    if [ "$aa_vt_no_exit" -eq 0 ] && [ "$aa_vt_bad" -eq 0 ] \
       && grep -qF 'virt_total_1e6 = ok' "${TMP}/aa_vtnodisplay.log" \
       && grep -qF 'virt_off_equals_legacy_path = ok' "${TMP}/aa_vtnodisplay.log" \
       && grep -qF 'virt_window_ok = ok' "${TMP}/aa_vtnodisplay.log" \
       && grep -qF 'virt_slot7 = ok' "${TMP}/aa_vtnodisplay.log" \
       && grep -qF 'virt_signal_a_slot0_id = ok' "${TMP}/aa_vtnodisplay.log" \
       && grep -qF 'virt_signal_kind_count_7 = ok' "${TMP}/aa_vtnodisplay.log" \
       && grep -qF 'virt_crc_pinned = ok' "${TMP}/aa_vtnodisplay.log" \
       && grep -qF 'virt_done = true' "${TMP}/aa_vtnodisplay.log"; then
        aapass "第 18 层虚拟化：100 万逻辑行只持有 8 个可视槽·window/槽越界 Fail-Fast·槽数据真实进入绘制与命中·关闭态逐像素等价旧路径·signal_count 仍 7 / 离屏 CRC 稳定全绿"
    else
        aabad "第 18 层虚拟化断言失败（exit=$aa_vt_no_exit bad=$aa_vt_bad）"
        tail -40 "${TMP}/aa_vtnodisplay.log"
    fi
else aabad "use_gui_virtual.dial 编译失败"; tail -20 "${TMP}/aa_vtcomp.log"; fi

# 5y) 第 18 层 IME 预编辑纯值事件块：纯内存 + 离屏画布（零 DISPLAY 依赖）。
#     独立 offscreen 画布，绝不触碰前十七个 demo 的 CRC 基准。给 AA 组 +2 aapass。
if "$DIALC" build tests/pkg/use_gui_ime.dial -o "${TMP}/use_gui_ime" >"${TMP}/aa_imcomp.log" 2>&1; then
    aapass "use_gui_ime.dial（第 18 层 IME 预编辑纯值事件块）编译成功"
    env -u DISPLAY "${TMP}/use_gui_ime" >"${TMP}/aa_imnodisplay.log" 2>&1
    aa_im_no_exit=$?
    aa_im_bad=$(grep -c ' = bad' "${TMP}/aa_imnodisplay.log" || true)
    if [ "$aa_im_no_exit" -eq 0 ] && [ "$aa_im_bad" -eq 0 ] \
       && grep -qF 'ime_feed_ok = ok' "${TMP}/aa_imnodisplay.log" \
       && grep -qF 'ime_preedit_zhong = ok' "${TMP}/aa_imnodisplay.log" \
       && grep -qF 'ime_preedit_256_rejected = ok' "${TMP}/aa_imnodisplay.log" \
       && grep -qF 'ime_seq_not_reset_by_clear = ok' "${TMP}/aa_imnodisplay.log" \
       && grep -qF 'ime_no_signal_emitted = ok' "${TMP}/aa_imnodisplay.log" \
       && grep -qF 'ime_crc_unchanged_by_feed = ok' "${TMP}/aa_imnodisplay.log" \
       && grep -qF 'ime_crc_pinned = ok' "${TMP}/aa_imnodisplay.log" \
       && grep -qF 'ime_done = true' "${TMP}/aa_imnodisplay.log"; then
        aapass "第 18 层 IME：一次 feed 自洽纯值快照·借用串复制·caret/串长/矩形/候选数越界 Fail-Fast·clear 不重置 seq·不进信号位体系·不碰 framebuffer / 离屏 CRC 稳定全绿"
    else
        aabad "第 18 层 IME 断言失败（exit=$aa_im_no_exit bad=$aa_im_bad）"
        tail -40 "${TMP}/aa_imnodisplay.log"
    fi
else aabad "use_gui_ime.dial 编译失败"; tail -20 "${TMP}/aa_imcomp.log"; fi

# 5z) 第 18 层 GPU + 原生平台纯值契约层：纯内存 + 离屏画布（零 DISPLAY 依赖）。
#     独立 offscreen 画布，绝不触碰前十八个 demo 的 CRC 基准。给 AA 组 +2 aapass。
if "$DIALC" build tests/pkg/use_gui_platform.dial -o "${TMP}/use_gui_platform" >"${TMP}/aa_plcomp.log" 2>&1; then
    aapass "use_gui_platform.dial（第 18 层 GPU + 原生平台纯值契约层）编译成功"
    env -u DISPLAY "${TMP}/use_gui_platform" >"${TMP}/aa_plnodisplay.log" 2>&1
    aa_pl_no_exit=$?
    aa_pl_bad=$(grep -c ' = bad' "${TMP}/aa_plnodisplay.log" || true)
    if [ "$aa_pl_no_exit" -eq 0 ] && [ "$aa_pl_bad" -eq 0 ] \
       && grep -qF 'plat_gpu_available_0 = ok' "${TMP}/aa_plnodisplay.log" \
       && grep -qF 'plat_gpu_backend_none = ok' "${TMP}/aa_plnodisplay.log" \
       && grep -qF 'plat_degrade_code_is_minus2 = ok' "${TMP}/aa_plnodisplay.log" \
       && grep -qF 'plat_try_composite_degraded = ok' "${TMP}/aa_plnodisplay.log" \
       && grep -qF 'plat_framebuffer_untouched = ok' "${TMP}/aa_plnodisplay.log" \
       && grep -qF 'plat_crc_pinned = ok' "${TMP}/aa_plnodisplay.log" \
       && grep -qF 'plat_done = true' "${TMP}/aa_plnodisplay.log"; then
        aapass "第 18 层平台纯值层：headless 能力恒未装载·降级码 -2·try_composite/托盘/文件框/快捷键参数 Fail-Fast + 确定性降级·不碰 framebuffer·能力位不漂移 / 离屏 CRC 稳定全绿"
    else
        aabad "第 18 层平台纯值层断言失败（exit=$aa_pl_no_exit bad=$aa_pl_bad）"
        tail -40 "${TMP}/aa_plnodisplay.log"
    fi
else aabad "use_gui_platform.dial 编译失败"; tail -20 "${TMP}/aa_plcomp.log"; fi

# 6) 清理：卸包，避免污染后续测试与用户环境。
"$DIALC" uninstall gui >/dev/null 2>&1 || true
aapass "gui 包已卸载"

echo
echo "== AB) int32 / float32 一等类型 + FFI 精确 32 位宽度 =="
abpass() { echo "  ✅ $1"; }
abfail() { echo "  ❌ $1"; fail=1; }

AB_DIR=tests/ffi
# 1) C 探针：函数参数 / 返回值 / 结构体字段一律用真实 int32_t 与 float。
#    若 Dial 侧误用 int(i64) / float(f64) 声明，C 按 4 字节读取必得垃圾。
if ! ( cd "$AB_DIR" && cc -O2 -c scalar32_probe.c -o scalar32_probe.o \
       && ar rcs libscalar32_probe.a scalar32_probe.o ) >"${TMP}/ab_c.log" 2>&1; then
    abfail "C 探针静态库构建失败"; tail -20 "${TMP}/ab_c.log"
else
    abpass "C 探针编译：参数 / 返回 / 结构体字段全部使用真实 int32_t 与 float"
fi

# 2) 端到端往返：语言层 + FFI 参数/返回值/多参数 + extern struct 布局与双向读写。
if ! "$DIALC" build "$AB_DIR/scalar32_probe.dial" -o "${TMP}/s32probe" >"${TMP}/ab_build.log" 2>&1; then
    abfail "scalar32_probe.dial 编译失败"; tail -30 "${TMP}/ab_build.log"
elif ! "${TMP}/s32probe" >"${TMP}/ab_out.txt" 2>&1; then
    abfail "scalar32_probe 运行非零退出"; tail -20 "${TMP}/ab_out.txt"
elif ! diff -u "$AB_DIR/scalar32_probe.expected" "${TMP}/ab_out.txt" >"${TMP}/ab_diff.txt" 2>&1; then
    abfail "FFI 32 位往返输出与黄金文件不一致"; sed 's/^/       /' "${TMP}/ab_diff.txt"
else
    abpass "FFI 往返全绿：int32/float32 参数·返回值·多参数·extern struct sizeof/offsetof·双向字段读写"
fi

# 3) int -> int32 越界必须 Fail-Fast，绝不静默截断成另一个「合法」值。
if ! "$DIALC" build "$AB_DIR/scalar32_overflow.dial" -o "${TMP}/s32ovf" >"${TMP}/ab_ovf_build.log" 2>&1; then
    abfail "越界用例编译失败"; tail -20 "${TMP}/ab_ovf_build.log"
elif timeout 30 "${TMP}/s32ovf" >"${TMP}/ab_ovf.log" 2>&1; then
    abfail "int_to_int32(2147483648) 未 Fail-Fast（正常退出 = 静默截断成垃圾）"
elif grep -qF 'dial_int_to_int32: 越界' "${TMP}/ab_ovf.log" \
     && ! grep -qF '9999' "${TMP}/ab_ovf.log"; then
    abpass "越界 Fail-Fast：int→int32 超出范围当场终止，哨兵 9999 未打印"
else
    abfail "越界路径未按预期终止"; tail -20 "${TMP}/ab_ovf.log"
fi

# 4) 负向：零隐式转换铁律（B 组已逐条拒绝，这里汇总再锁一次）。
ab_neg_ok=0
ab_neg_total=0
for f in tests/bad/s32_*.dial; do
    ab_neg_total=$((ab_neg_total + 1))
    if ! "$DIALC" build "$f" -o "${TMP}/ab_bad" >"${TMP}/ab_neg.log" 2>&1; then
        ab_neg_ok=$((ab_neg_ok + 1))
    fi
done
if [ "$ab_neg_total" -ge 5 ] && [ "$ab_neg_ok" -eq "$ab_neg_total" ]; then
    abpass "零隐式转换负向 $ab_neg_ok/$ab_neg_total 全被拒绝（int32/float32 不与 int/float 隐式互通）"
else
    abfail "零隐式转换负向未全部拒绝（$ab_neg_ok/$ab_neg_total）"
fi

# 5) 清理：探针静态库与 .o 是回归现编的派生数据，不入库（与 P 组同口径）。
rm -f "$AB_DIR/libscalar32_probe.a" "$AB_DIR/scalar32_probe.o" "$AB_DIR/scalar32_overflow.o"

# ---------------------------------------------------------------------------
# AC) B1「impl 方法块」语言特性（v1.10 开发中增量）
#     impl T { fn m(self, ...) } 编译期脱糖为普通函数 __impl_T_m，obj.m(args)
#     降级为静态调用（self 即首参），ABI 不变、旧签名一字不动、不新增关键字。
# ---------------------------------------------------------------------------
echo "== AC) B1 impl 方法块：自定义 struct / 包不透明句柄（GUI）/ 默认实参 / Fail-Fast / 值语义 / 容器方法隔离 =="
acpass() { echo "  ✅ $1"; }
acbad()  { echo "  ❌ $1"; fail=1; }

AC_DIR=tests/impl

# 1) 正向：自定义 struct 方法（self 字段读写 / 方法间调用 / 返回结构体再调用 /
#    表达式位置 / impl 作普通标识符 / 容器 push 隔离 / 值语义边界）。
if ! "$DIALC" build "$AC_DIR/impl_struct.dial" -o "${TMP}/impl_struct" >"${TMP}/ac_s_build.log" 2>&1; then
    acbad "impl_struct.dial 编译失败"; tail -20 "${TMP}/ac_s_build.log"
elif ! "${TMP}/impl_struct" >"${TMP}/ac_s_out.txt" 2>&1; then
    acbad "impl_struct 运行非零退出"; tail -20 "${TMP}/ac_s_out.txt"
elif ! diff -u "$AC_DIR/impl_struct.expected" "${TMP}/ac_s_out.txt" >"${TMP}/ac_s_diff.txt" 2>&1; then
    acbad "impl_struct 输出与黄金文件不一致"; sed 's/^/       /' "${TMP}/ac_s_diff.txt"
else
    acpass "自定义 struct impl 全绿：self 字段读写·方法间调用·返回结构体·表达式位置·impl 作标识符·容器 push 隔离·值语义边界"
fi

# 2) 正向：impl 方法默认实参（脱糖后由 Call 链按 FuncInfo.defaults 补齐）。
if ! "$DIALC" build "$AC_DIR/impl_default.dial" -o "${TMP}/impl_default" >"${TMP}/ac_d_build.log" 2>&1; then
    acbad "impl_default.dial 编译失败"; tail -20 "${TMP}/ac_d_build.log"
elif ! "${TMP}/impl_default" >"${TMP}/ac_d_out.txt" 2>&1; then
    acbad "impl_default 运行非零退出"; tail -20 "${TMP}/ac_d_out.txt"
elif ! diff -u "$AC_DIR/impl_default.expected" "${TMP}/ac_d_out.txt" >"${TMP}/ac_d_diff.txt" 2>&1; then
    acbad "impl_default 输出与黄金文件不一致"; sed 's/^/       /' "${TMP}/ac_d_diff.txt"
else
    acpass "impl 默认实参全绿：省略取默认值 / 显式覆盖，self 对用户不可见"
fi

# 3) 正向：包不透明句柄（type GuiMenu;）impl 方法——GUI 场景核心诉求。
#    自行 pack+install gui（与 AA 组同口径），全程无 DISPLAY、纯内存菜单逻辑。
if ! "$DIALC" pack packages/gui -o "${TMP}/ac_gui.dialpkg" >"${TMP}/ac_pack.log" 2>&1 \
   || ! "$DIALC" install "${TMP}/ac_gui.dialpkg" >"${TMP}/ac_install.log" 2>&1; then
    acbad "gui.dialpkg 打包或安装失败（AC 组）"; tail -8 "${TMP}/ac_pack.log" "${TMP}/ac_install.log"
elif ! "$DIALC" build "$AC_DIR/impl_gui_handle.dial" -o "${TMP}/impl_gui" >"${TMP}/ac_g_build.log" 2>&1; then
    acbad "impl_gui_handle.dial 编译失败"; tail -20 "${TMP}/ac_g_build.log"
elif ! env -u DISPLAY "${TMP}/impl_gui" >"${TMP}/ac_g_out.txt" 2>&1; then
    acbad "impl_gui_handle 运行非零退出"; tail -20 "${TMP}/ac_g_out.txt"
elif ! diff -u "$AC_DIR/impl_gui_handle.expected" "${TMP}/ac_g_out.txt" >"${TMP}/ac_g_diff.txt" 2>&1; then
    acbad "impl_gui_handle 输出与黄金文件不一致"; sed 's/^/       /' "${TMP}/ac_g_diff.txt"
else
    acpass "包不透明句柄 impl 全绿：win.draw() 等价 dial_gui_window_draw(win)，self 作 Opaque 首参传给包 extern fn"
fi

# 4) 负向：6 个 Fail-Fast 边界（未知方法 / 参数类型 / 参数个数 / 缺 self /
#    重复方法 / 空块）逐一拒绝，并锁定诊断不暴露内部 mangle 名、不把 self 计入参数个数。
ac_neg_ok=0
ac_neg_total=0
for f in tests/bad/impl_*.dial; do
    ac_neg_total=$((ac_neg_total + 1))
    if ! "$DIALC" build "$f" -o "${TMP}/ac_bad" >"${TMP}/ac_neg.log" 2>&1; then
        ac_neg_ok=$((ac_neg_ok + 1))
    fi
done
if [ "$ac_neg_total" -ge 6 ] && [ "$ac_neg_ok" -eq "$ac_neg_total" ]; then
    acpass "Fail-Fast 负向 $ac_neg_ok/$ac_neg_total 全被拒绝（未知方法·参数类型·参数个数·缺 self·重复方法·空块）"
else
    acbad "Fail-Fast 负向未全部拒绝（$ac_neg_ok/$ac_neg_total）"
fi

# 诊断面向用户：不含内部符号名 __impl_、参数个数不含 self。
if "$DIALC" build tests/bad/impl_arg_count.dial -o "${TMP}/ac_bad" >"${TMP}/ac_cnt.log" 2>&1; then
    acbad "参数个数负向用例竟编译成功"
elif grep -qF "方法 'P.add' 需要 1~1 个参数，但传入了 0 个" "${TMP}/ac_cnt.log" \
     && ! grep -qF "__impl_" "${TMP}/ac_cnt.log"; then
    acpass "诊断面向用户：显示 P.add 且参数个数/序号不含 self、不暴露 mangle 名"
else
    acbad "诊断未还原用户视角"; tail -6 "${TMP}/ac_cnt.log"
fi

# ===========================================================================
echo "== AD) GUI 第 8 层 OO 封装：343 个 impl 方法 / 随包 src/ 装包即用 / C 风格 CRC 逐字节相等 / 窗口覆盖层 / Fail-Fast 透传 =="
adpass() { echo "  ✅ $1"; }
adbad()  { echo "  ❌ $1"; fail=1; }

AD_DIR="${TMP}/ad_oo"
mkdir -p "$AD_DIR"
# 路径 ①（本地源码模块）：模拟用户把包内 src/gui_oo.dial 复制到自己项目目录；
# 源文件是唯一真相，副本为临时派生数据。路径 ②（随包模块）见下方 2.5) 块，
# 那里故意不复制任何文件，验证装包即用。
cp packages/gui/src/gui_oo.dial "$AD_DIR/"
cp tests/oo/use_gui_oo.dial "$AD_DIR/"
"$DIALC" pack packages/gui -o "${TMP}/ad_gui.dialpkg" >"${TMP}/ad_pack.log" 2>&1 \
    && "$DIALC" install "${TMP}/ad_gui.dialpkg" >"${TMP}/ad_install.log" 2>&1 \
    || { adbad "gui.dialpkg 打包或安装失败（AD 组）"; tail -6 "${TMP}/ad_pack.log" "${TMP}/ad_install.log"; }

# 1) 契约防漂移审计：343 个方法恰好覆盖 343 个 extern fn（入口 app_new 除外），
#    封装层不含任何 extern fn 声明（否则与 .diali 撞名），gui.diali 仍是 344 个签名。
ad_diag=$(grep '^extern fn' packages/gui/gui.diali | sed -E 's/^extern fn ([A-Za-z0-9_]+).*/\1/' \
          | grep -v '^dial_gui_app_new$' | sort -u)
ad_refs=$(grep '^    fn ' packages/gui/src/gui_oo.dial | grep -oE 'dial_gui_[A-Za-z0-9_]+' | sort -u)
ad_methods=$(grep -c '^    fn ' packages/gui/src/gui_oo.dial)
ad_diag_n=$(echo "$ad_diag" | wc -l)
ad_iface_n=$(grep -c '^extern fn' packages/gui/gui.diali)
ad_oo_decl=$(grep -c '^extern fn' packages/gui/src/gui_oo.dial || true)
if [ "$ad_methods" -eq 343 ] && [ "$ad_diag_n" -eq 343 ] && [ "$ad_iface_n" -eq 344 ] \
   && [ "$ad_oo_decl" -eq 0 ] && [ "$ad_diag" = "$ad_refs" ]; then
    adpass "契约审计：343 个 impl 方法全覆盖 343 个 extern fn、gui.diali 共 344 个签名、封装层零 extern 声明（ABI 不变）"
else
    adbad "契约审计不通过：methods=$ad_methods diag=$ad_diag_n iface=$ad_iface_n oo_decl=$ad_oo_decl"
    diff <(echo "$ad_diag") <(echo "$ad_refs") | head -20
fi

# 2) 正向：编译 + 无 DISPLAY 纯内存路径（含 C 风格 / OO 风格 CRC32 逐字节相等）。
if ! "$DIALC" build "$AD_DIR/use_gui_oo.dial" -o "${TMP}/ad_oo_run" >"${TMP}/ad_build.log" 2>&1; then
    adbad "use_gui_oo.dial 编译失败"; tail -20 "${TMP}/ad_build.log"
else
    adpass "OO 封装层 + 用例编译成功（343 个 impl 方法签名全部通过）"
    if ! env -u DISPLAY "${TMP}/ad_oo_run" >"${TMP}/ad_nodisp.out" 2>&1; then
        adbad "use_gui_oo 无 DISPLAY 运行非零退出"; tail -20 "${TMP}/ad_nodisp.out"
    elif grep -q '= bad' "${TMP}/ad_nodisp.out" \
         || ! grep -q '^oo_done = true$' "${TMP}/ad_nodisp.out" \
         || ! grep -q '^oo_painter_crc_matches_c = ok$' "${TMP}/ad_nodisp.out"; then
        adbad "无 DISPLAY 路径有 bad / 缺完成标记 / CRC 对照未通过"
        grep '= bad' "${TMP}/ad_nodisp.out" | head -10
    else
        adpass "无 DISPLAY 全绿（$(grep -c '= ok' "${TMP}/ad_nodisp.out") 条）：C 风格与 OO 风格绘制 CRC32 逐字节相等、7 类句柄方法可用、Fail-Fast 透传"
    fi
fi

# 2.5) 随包分发（§25.11 三级优先第 ② 级）：不复制 gui_oo.dial，裸名 import 直接
#      命中包缓存 src/gui_oo.dial；输出必须与「本地副本版」逐字节一致。
AD_PKG_DIR="${TMP}/ad_pkg"
mkdir -p "$AD_PKG_DIR"
cp tests/oo/use_gui_oo.dial "$AD_PKG_DIR/"
if ! "$DIALC" build "$AD_PKG_DIR/use_gui_oo.dial" -o "${TMP}/ad_pkg_run" >"${TMP}/ad_pkg_build.log" 2>&1; then
    adbad "随包 src/ 模块自动 import 编译失败"; tail -14 "${TMP}/ad_pkg_build.log"
elif ! env -u DISPLAY "${TMP}/ad_pkg_run" >"${TMP}/ad_pkg_nodisp.out" 2>&1; then
    adbad "随包 src/ 版本无 DISPLAY 运行非零退出"; tail -14 "${TMP}/ad_pkg_nodisp.out"
elif ! diff -u "${TMP}/ad_nodisp.out" "${TMP}/ad_pkg_nodisp.out" >"${TMP}/ad_pkg_diff.txt" 2>&1; then
    adbad "随包 src/ 版本与本地副本版输出不一致"; sed 's/^/       /' "${TMP}/ad_pkg_diff.txt" | head -14
else
    adpass "装包即用：不复制 gui_oo.dial，裸名 import 命中包内 src/，输出与本地副本逐字节一致（$(grep -c '= ok' "${TMP}/ad_pkg_nodisp.out") 条）"
fi

# 3) Xvfb 真实窗口：顶栏 / 弹窗 / 对话框 / 控件树全走方法，信号队列与 AA 组逐项对齐。
if command -v xvfb-run >/dev/null 2>&1; then
    if xvfb-run -a -s "-screen 0 1024x768x24 +extension MIT-SHM +extension RENDER" \
       "${TMP}/ad_oo_run" >"${TMP}/ad_xvfb.out" 2>&1 \
       && ! grep -q '= bad' "${TMP}/ad_xvfb.out" \
       && ! grep -q 'skipped' "${TMP}/ad_xvfb.out" \
       && grep -q '^oo_done = true$' "${TMP}/ad_xvfb.out" \
       && grep -q '^oo_win_draw_bar = ok$' "${TMP}/ad_xvfb.out" \
       && grep -q '^oo_win_bar_click_activates = ok$' "${TMP}/ad_xvfb.out" \
       && grep -q '^oo_win_dialog_result_ok = ok$' "${TMP}/ad_xvfb.out" \
       && grep -q '^oo_win_widget_click_activates = ok$' "${TMP}/ad_xvfb.out" \
       && grep -q '^oo_win_app_free = ok$' "${TMP}/ad_xvfb.out"; then
        adpass "Xvfb 实窗全绿（$(grep -c '= ok' "${TMP}/ad_xvfb.out") 条）：顶栏·弹窗·对话框·控件树走方法，信号队列与 AA 组逐项对齐"
    else
        adbad "Xvfb 实窗路径未全绿"; grep '= bad\|skipped' "${TMP}/ad_xvfb.out" | head -10
    fi
else
    echo "  ⚠️  本环境无 xvfb-run：OO 层实窗路径未实机验证（如实标注，不伪装通过）"
fi

# 4) Fail-Fast 透传负向：未知方法 / 参数类型零隐式转换 / 参数个数不足。
cat > "$AD_DIR/oo_unknown.dial" <<'EOF'
import "gui_oo";
fn main(): int {
    var app: GuiApp = dial_gui_app_new();
    var m: GuiMenu = app.menu_new();
    return m.nope();
}
EOF
cat > "$AD_DIR/oo_arg_type.dial" <<'EOF'
import "gui_oo";
fn main(): int {
    var app: GuiApp = dial_gui_app_new();
    var m: GuiMenu = app.menu_new();
    return m.add_item("x", "File");
}
EOF
cat > "$AD_DIR/oo_arg_count.dial" <<'EOF'
import "gui_oo";
fn main(): int {
    var app: GuiApp = dial_gui_app_new();
    var m: GuiMenu = app.menu_new();
    return m.add_item(1);
}
EOF
ad_neg_ok=0
ad_neg_total=0
for f in "$AD_DIR"/oo_*.dial; do
    ad_neg_total=$((ad_neg_total + 1))
    if ! "$DIALC" build "$f" -o "${TMP}/ad_bad" >"${TMP}/ad_neg.log" 2>&1; then
        ad_neg_ok=$((ad_neg_ok + 1))
    fi
done
if [ "$ad_neg_total" -eq 3 ] && [ "$ad_neg_ok" -eq 3 ]; then
    adpass "Fail-Fast 透传 3/3 拒绝：未知方法 / 参数类型零隐式转换 / 参数个数不足"
else
    adbad "Fail-Fast 透传未全部拒绝（$ad_neg_ok/$ad_neg_total）"
fi

# 5) 诊断面向用户：显示 GuiMenu.add_item、参数个数不含 self、不暴露 mangle 名。
if "$DIALC" build "$AD_DIR/oo_arg_count.dial" -o "${TMP}/ad_bad" >"${TMP}/ad_cnt.log" 2>&1; then
    adbad "参数个数负向用例竟编译成功"
elif grep -qF "方法 'GuiMenu.add_item' 需要 2~2 个参数，但传入了 1 个" "${TMP}/ad_cnt.log" \
     && ! grep -qF "__impl_" "${TMP}/ad_cnt.log"; then
    adpass "诊断面向用户：显示 GuiMenu.add_item、参数个数不含 self、不暴露 mangle 名"
else
    adbad "诊断未还原用户视角"; tail -6 "${TMP}/ad_cnt.log"
fi

# ---------------------------------------------------------------------------
# AE) 第 18 层编译器产物形态（工具链层，非 gui 契约）
#     --tiny 体积模式 / --single-file 便携模式 / 冲突矩阵 / 正交叠加。
#     刻意用 examples/hello.dial（零已安装包依赖），与 AA / AD 组解耦。
# ---------------------------------------------------------------------------
echo
echo "== AE) 编译器产物形态：--tiny 体积模式 / --single-file 便携模式（第 18 层）=="
aepass() { echo "  ✅ $1"; }
aebad()  { echo "  ❌ $1"; fail=1; }

AE_SRC=examples/hello.dial
AE_DEF="${TMP}/ae_hello_default"
AE_TINY="${TMP}/ae_hello_tiny"
AE_STATIC="${TMP}/ae_hello_static"
AE_BOTH="${TMP}/ae_hello_both"

# 1) 默认产物基线：可运行、输出一致，作为 --tiny 体积比较基准。
if "$DIALC" build "$AE_SRC" -o "$AE_DEF" >"${TMP}/ae_def.log" 2>&1 \
   && "$AE_DEF" >"${TMP}/ae_def.out" 2>&1 \
   && grep -qF 'Hello, dialc!' "${TMP}/ae_def.out"; then
    aepass "默认产物运行正常（Hello, dialc!），作为体积基线"
else
    aebad "默认产物编译/运行/输出不符"; tail -20 "${TMP}/ae_def.log" "${TMP}/ae_def.out"
fi

# 2) --tiny：体积显著小于默认、无 .symtab/.debug_info、仍可运行。
if ! "$DIALC" build "$AE_SRC" --tiny -o "$AE_TINY" >"${TMP}/ae_tiny.log" 2>&1; then
    aebad "--tiny 编译失败"; tail -20 "${TMP}/ae_tiny.log"
else
    ae_def_sz=$(stat -c %s "$AE_DEF" 2>/dev/null || echo 0)
    ae_tiny_sz=$(stat -c %s "$AE_TINY" 2>/dev/null || echo 0)
    if [ "$ae_tiny_sz" -gt 0 ] && [ "$ae_def_sz" -gt 0 ] && [ "$ae_tiny_sz" -lt "$ae_def_sz" ]; then
        aepass "--tiny 产物体积小于默认基线（${ae_tiny_sz} B < ${ae_def_sz} B）"
    else
        aebad "--tiny 产物体积未小于默认（tiny=${ae_tiny_sz} default=${ae_def_sz}）"
    fi
    if readelf -S "$AE_TINY" 2>/dev/null | grep -qE '\.(symtab|debug_info)'; then
        aebad "--tiny 产物仍含 .symtab / .debug_info（strip 未生效）"
    else
        aepass "--tiny 产物已 strip：readelf -S 无 .symtab / .debug_info"
    fi
    if "$AE_TINY" >"${TMP}/ae_tiny.out" 2>&1 && grep -qF 'Hello, dialc!' "${TMP}/ae_tiny.out"; then
        aepass "--tiny 产物运行正常（Hello, dialc!）"
    else
        aebad "--tiny 产物运行/输出不符"; cat "${TMP}/ae_tiny.out"
    fi
fi

# 3) --single-file：全静态链接，file / ldd 双重确认，仍可运行。
if ! "$DIALC" build "$AE_SRC" --single-file -o "$AE_STATIC" >"${TMP}/ae_static.log" 2>&1; then
    aebad "--single-file 编译失败"; tail -20 "${TMP}/ae_static.log"
else
    if file "$AE_STATIC" 2>/dev/null | grep -qF 'statically linked'; then
        aepass "--single-file 产物 file 显示 statically linked"
    else
        aebad "--single-file 产物非静态链接"; file "$AE_STATIC"
    fi
    if ldd "$AE_STATIC" 2>&1 | grep -qF 'not a dynamic executable'; then
        aepass "--single-file 产物 ldd 显示 not a dynamic executable"
    else
        aebad "--single-file 产物仍为动态可执行"; ldd "$AE_STATIC" 2>&1 | head -5
    fi
    if "$AE_STATIC" >"${TMP}/ae_static.out" 2>&1 && grep -qF 'Hello, dialc!' "${TMP}/ae_static.out"; then
        aepass "--single-file 产物运行正常（Hello, dialc!）"
    else
        aebad "--single-file 产物运行/输出不符"; cat "${TMP}/ae_static.out"
    fi
fi

# 4) --tiny + --single-file 可叠加（如实提示诉求不同），静态且可运行。
if ! "$DIALC" build "$AE_SRC" --tiny --single-file -o "$AE_BOTH" >"${TMP}/ae_both.log" 2>&1; then
    aebad "--tiny + --single-file 编译失败"; tail -20 "${TMP}/ae_both.log"
elif ! file "$AE_BOTH" 2>/dev/null | grep -qF 'statically linked'; then
    aebad "--tiny + --single-file 产物非静态链接"; file "$AE_BOTH"
elif ! "$AE_BOTH" >"${TMP}/ae_both.out" 2>&1 || ! grep -qF 'Hello, dialc!' "${TMP}/ae_both.out"; then
    aebad "--tiny + --single-file 产物运行/输出不符"
else
    aepass "--tiny + --single-file 可叠加：静态链接且运行正常"
fi

# 5) 冲突矩阵 Fail-Fast：--tiny 的目标（最小体积）与 --fast / --pgo / -g
#    目标相反，一律当场拒绝（--pgo 须配合 --fast 才生效，故用 --tiny --pgo --fast）。
ae_c1=0; ae_c2=0; ae_c3=0
if ! "$DIALC" build "$AE_SRC" --tiny --fast -o "${TMP}/ae_c1" >"${TMP}/ae_c1.log" 2>&1 \
   && grep -qF '❌ --tiny 与 --fast 冲突' "${TMP}/ae_c1.log"; then ae_c1=1; fi
if ! "$DIALC" build "$AE_SRC" --tiny --pgo --fast -o "${TMP}/ae_c2" >"${TMP}/ae_c2.log" 2>&1 \
   && grep -qF '❌ --tiny' "${TMP}/ae_c2.log"; then ae_c2=1; fi
if ! "$DIALC" build "$AE_SRC" --tiny -g -o "${TMP}/ae_c3" >"${TMP}/ae_c3.log" 2>&1 \
   && grep -qF '❌ --tiny 与 -g 冲突' "${TMP}/ae_c3.log"; then ae_c3=1; fi
if [ "$ae_c1" -eq 1 ] && [ "$ae_c2" -eq 1 ] && [ "$ae_c3" -eq 1 ]; then
    aepass "冲突矩阵 3/3 拒绝：--tiny 与 --fast / --pgo(配合 fast) / -g 目标相反，Fail-Fast"
else
    aebad "冲突矩阵未全部拒绝（fast=$ae_c1 pgo+fast=$ae_c2 g=$ae_c3）"
fi

# 6) --single-file 只影响链接期 -static，与 --fast 正交，允许叠加（不误拒）。
if "$DIALC" build "$AE_SRC" --single-file --fast -o "${TMP}/ae_sf" >"${TMP}/ae_sf.log" 2>&1 \
   && file "${TMP}/ae_sf" 2>/dev/null | grep -qF 'statically linked' \
   && "${TMP}/ae_sf" >"${TMP}/ae_sf.out" 2>&1 \
   && grep -qF 'Hello, dialc!' "${TMP}/ae_sf.out"; then
    aepass "--single-file 与 --fast 正交叠加：静态链接且运行正常"
else
    aebad "--single-file 与 --fast 正交叠加被误拒或不静态"; tail -10 "${TMP}/ae_sf.log"
fi

echo
echo "== AF) 自研后端（--backend=asm）双后端黄金门禁 =="
if bash tools/run_asm_golden.sh "$DIALC"; then
    :   # 脚本自己逐条打印 ✅；失败项会打印 ❌ 并以非 0 退出
else
    fail=1
fi

echo
if [ $fail -eq 0 ]; then
    echo "== 全部测试通过 ✅ =="
else
    echo "== 存在失败用例 ❌ =="
    exit 1
fi
