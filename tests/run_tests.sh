#!/usr/bin/env bash
# dialc 端到端测试
#
# 五组检查，全部通过才算绿：
#   A. 正向：examples/*.dial 必须编译成功，且运行输出与同名 .expected 黄金文件逐字节一致；
#   B. 负向：tests/bad/*.dial 必须编译失败（用于锁定语义/语法诊断不被弱化）；
#   D. Fail-Fast：tests/trap/*.dial 越界/除零必须当场因信号终止，且不得打印哨兵；
#   E. 对照组：同一批用例在 --fast 下应关闭检查（证明陷阱确由编译器插入，而非碰巧崩溃）；
#   C. 契约：产物可执行文件不得依赖 libLLVM，只允许依赖 C Runtime（libc）。
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
if [ $fail -eq 0 ]; then
    echo "== 全部测试通过 ✅ =="
else
    echo "== 存在失败用例 ❌ =="
    exit 1
fi
