#!/usr/bin/env bash
# dialc 端到端测试
#
# 二十三组检查，全部通过才算绿（顺序为 A→W，C 组「产物契约」排在 B 之后、逻辑序而非字母序）：
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
#      两层 API（高层 get-post + 低层流式）/ gzip 解压 / chunked / 错误路径（v1.04 增补）。
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
rm -f "$FFI_DIR/libffi_demo.a" "$FFI_DIR"/*.o


# =====================================================================
echo
echo "== Q) 工程工具链：CLI / 本地模块 / test / fmt / pack 闭环（v1.04 增补） =="

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
echo "== W) C11 独立包：http.dialpkg 端到端（自研 HTTP/1.1 + gzip 解压，v1.04 增补） =="
wpass() { echo "  ✅ $1"; }
wbad()  { echo "  ❌ $1"; fail=1; }

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

# 5) 错误路径：https 未接线 / URL 缺主机名 / 连不上 —— 都必须**明确失败**而非静默成功。
if [ -n "$PORT" ]; then
    out=$(DIAL_HTTP_TEST_BASE="http://127.0.0.1:$PORT" "${TMP}/use_http" 2>&1)
    if printf '%s\n' "$out" | grep -q 'tls_is_null = true' \
       && printf '%s\n' "$out" | grep -q 'nourl_is_null = true' \
       && printf '%s\n' "$out" | grep -q 'bad_is_null = true' ; then
        wpass "错误路径明确失败（https 未接线 / URL 非法 / 连接失败）"
    else wbad "W 组：错误路径未按预期失败"; printf '%s\n' "$out" | head -20; fi
fi

# 6) 清理：停服务器、卸包（不污染后续测试与用户环境）。
if [ -n "${SRV_PID:-}" ]; then kill "$SRV_PID" >/dev/null 2>&1 || true; fi
"$DIALC" uninstall http >/dev/null 2>&1 || true
wpass "清理完成（服务器已停、http 包已卸载）"

echo
if [ $fail -eq 0 ]; then
    echo "== 全部测试通过 ✅ =="
else
    echo "== 存在失败用例 ❌ =="
    exit 1
fi
