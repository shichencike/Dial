#!/usr/bin/env bash
# dialc 端到端测试
#
# 十一组检查，全部通过才算绿（顺序即 A→K，C 组「产物契约」排在 B 之后、逻辑序而非字母序）：
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
        runtime/dial_rt_trap.c -o "$mt_bin" -lpthread >"${TMP}/mtbuild.log" 2>&1; then
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
if [ $fail -eq 0 ]; then
    echo "== 全部测试通过 ✅ =="
else
    echo "== 存在失败用例 ❌ =="
    exit 1
fi
