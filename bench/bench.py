#!/usr/bin/env python3
"""Dial 性能基准 —— Python (CPython 3) 实现。

28 个跨语言等价的纯整数微基准。每个任务由第一个命令行参数指定，程序只在
stdout 打印一行十进制校验和；计时完全交给外部采集器（tools/bench_harness.py），
用进程墙钟时间并扣除 noop 启动基线，避免各语言内部时钟 API 差异造成不公。

一致性铁律（4 语言必须算出**完全相同**的校验和）：
  - 所有中间值保持 [0, 2^63)，模数 MOD/MOD2 恒定且被除数恒为非负；
  - 整数除法只对正数做（C/Go/Dial 向零截断 == Python // 向下取整）；
  - 位运算只在非负且 < 2^62 的值上做，符号位不参与。
编译：python3 -m py_compile bench.py（采集器记录该耗时作为「编译耗时」维度）。
"""
import sys

sys.setrecursionlimit(100000)

LOOP = 1_000_000       # 大循环
MID = 100_000          # 中循环
ARR = 2048             # 数组规模（2 的幂，便于 & 掩码）
SORTN = 512            # 冒泡排序规模
MOD = 1_000_000_007
MOD2 = 1_000_003
S = "abcdefghijklmnopqrstuvwxyz0123456789"  # 36 个 ASCII 字符


def b01():  # 算术：依赖链累加
    s = 0
    for i in range(LOOP):
        s = (s + i) % MOD
    return s


def b02():  # 算术：乘加取模
    s = 1
    for i in range(MID):
        s = (s * 31 + i) % MOD
    return s


def b03():  # 算术：除法 + 取模混合
    s = 0
    for i in range(1, MID):
        s = (s + (i * 1103515245) // 7 + i % 97) % MOD
    return s


def b04():  # 算术：FMA 依赖链
    s = 0
    for i in range(LOOP):
        s = (s * 3 + 7) % MOD
    return s


def b05():  # 算术：连乘幂（快速幂的乘法内核）
    s = 0
    for a in range(20000):
        base = a % MOD2 + 1
        r = 1
        for _ in range(8):
            r = (r * base) % MOD
        s = (s + r) % MOD
    return s


def _gcd(a, b):
    while b:
        a, b = b, a % b
    return a


def b06():  # 算术：批量欧几里得 gcd
    s = 0
    for i in range(1, 30000):
        s = (s + _gcd(i * 7 + 1, i * 13 + 3)) % MOD
    return s


def b07():  # 算术：迭代斐波那契
    a, b = 0, 1
    for _ in range(LOOP):
        t = (a + b) % MOD
        a = b
        b = t
    return a


def b08():  # 算术：3n+1 总步数
    s = 0
    for n in range(1, 20000):
        x = n
        st = 0
        while x > 1:
            if x % 2 == 0:
                x = x // 2
            else:
                x = 3 * x + 1
            st += 1
        s = (s + st) % MOD
    return s


def _fib(n):
    if n < 2:
        return n
    return _fib(n - 1) + _fib(n - 2)


def b09():  # 递归：二叉斐波那契
    s = 0
    for _ in range(3):
        s = (s + _fib(23)) % MOD
    return s


def _rsum(n):
    if n == 0:
        return 0
    return (n + _rsum(n - 1)) % MOD


def b10():  # 递归：浅深度重复调用（测调用开销）
    s = 0
    for _ in range(50000):
        s = (s + _rsum(25)) % MOD
    return s


def _tree(n):
    if n < 2:
        return 1
    return (_tree(n - 1) + _tree(n - 2) + 1) % MOD


def b11():  # 递归：二叉递归树
    s = 0
    for _ in range(200):
        s = (s + _tree(18)) % MOD
    return s


def _ev(n):
    if n == 0:
        return 1
    return _od(n - 1)


def _od(n):
    if n == 0:
        return 0
    return _ev(n - 1)


def b12():  # 递归：互递归奇偶判定
    s = 0
    for k in range(20000):
        s = (s + _ev(k % 60 + 1)) % MOD
    return s


def b13():  # 数组：构建 + 顺序求和
    a = []
    for i in range(ARR):
        a.append((i * 31 + 7) % MOD2)
    s = 0
    for j in range(ARR):
        s = (s + a[j]) % MOD
    return s


def b14():  # 数组：伪随机索引读写
    a = [(i * 17) % MOD2 for i in range(ARR)]
    idx = 0
    s = 0
    for i in range(LOOP):
        idx = (idx * 1103515245 + 12345) & (ARR - 1)
        s = (s + a[idx]) % MOD
        a[idx] = (a[idx] + 1) % MOD2
    return s


def b15():  # 数组：冒泡排序
    a = [SORTN - i for i in range(SORTN)]
    s = 0
    for i in range(SORTN):
        for j in range(SORTN - 1 - i):
            if a[j] > a[j + 1]:
                t = a[j]
                a[j] = a[j + 1]
                a[j + 1] = t
        s = (s + a[i]) % MOD
    return s


def b16():  # 数组：原地反转 + 归约（重复放大）
    a = [(i * 13 + 5) % MOD2 for i in range(ARR)]
    s = 0
    for _ in range(100):
        for i in range(ARR // 2):
            t = a[i]
            a[i] = a[ARR - 1 - i]
            a[ARR - 1 - i] = t
        for j in range(ARR):
            s = (s + a[j]) % MOD
    return s


def b17():  # 数组：64×64 矩阵乘法（一维展平）
    n = 64
    A = [(i * 7 + 3) % MOD2 for i in range(n * n)]
    B = [(i * 11 + 5) % MOD2 for i in range(n * n)]
    C = [0] * (n * n)
    for i in range(n):
        for k in range(n):
            aik = A[i * n + k]
            for j in range(n):
                C[i * n + j] += aik * B[k * n + j]
    s = 0
    for v in C:
        s = (s + v) % MOD
    return s


def b18():  # 数组：前缀和 + 归约（重复放大）
    s = 0
    for _ in range(200):
        a = [(i * 13 + 5) % MOD2 for i in range(ARR)]
        for i in range(1, ARR):
            a[i] = (a[i] + a[i - 1]) % MOD
        for j in range(ARR):
            s = (s + a[j]) % MOD
    return s


def b19():  # 字符串：逐字符码点扫描
    s = 0
    L = len(S)
    for _ in range(20000):
        for c in range(L):
            s = (s + ord(S[c])) % MOD
    return s


def b20():  # 字符串：循环拼接
    x = "x"
    for _ in range(4000):
        x = x + "y"
    return (len(x) * 31 + ord(x[0]) + ord(x[len(x) - 1])) % MOD


def b21():  # 字符串：index_of / find 批量
    subs = ("mnop", "0123", "xyz", "abcd")
    s = 0
    for k in range(20000):
        s = (s + S.find(subs[k % 4])) % MOD
    return s


def b22():  # 字符串：拼接 + 定长子串取码点
    base = S + "-" + S
    s = 0
    for k in range(10000):
        f = k % 20
        t = base[f:f + 10]
        for c in range(len(t)):
            s = (s + ord(t[c])) % MOD
    return s


def b23():  # 位运算：popcount
    s = 0
    for i in range(MID):
        x = i
        c = 0
        while x:
            c += x & 1
            x >>= 1
        s = (s + c) % MOD
    return s


def b24():  # 位运算：xorshift32 伪随机累加
    x = 1
    s = 0
    for _ in range(LOOP):
        x ^= (x << 13) & 0xFFFFFFFF
        x ^= x >> 7
        x ^= (x << 17) & 0xFFFFFFFF
        x &= 0xFFFFFFFF
        s = (s + x % MOD2) % MOD
    return s


def b25():  # 控制流：三路分支
    s = 0
    for i in range(LOOP):
        r = i % 3
        if r == 0:
            s = (s + i) % MOD
        elif r == 1:
            s = (s + i * 2) % MOD
        else:
            s = (s + i * 3) % MOD
    return s


def b26():  # 控制流：嵌套循环异或
    s = 0
    for i in range(1000):
        for j in range(1000):
            s = (s + (i ^ j)) % MOD
    return s


def b27():  # 控制流：短路布尔求值
    s = 0
    for i in range(LOOP):
        if ((i % 7 == 0) and (i % 3 != 0)) or (i & 1):
            s = (s + 1) % MOD
        else:
            s = (s + 2) % MOD
    return s


def b28():  # 综合：数组读写 + 算术 + 位运算
    a = [(i * 29 + 11) % MOD2 for i in range(ARR)]
    s = 0
    for i in range(MID):
        p = i & (ARR - 1)
        a[p] = (a[p] + i) % MOD2
        s = (s * 31 + a[p] + (i & 255)) % MOD
    return s


TASKS = {f"b{i:02d}": globals()[f"b{i:02d}"] for i in range(1, 29)}
TASKS["noop"] = lambda: 0


def main():
    # 用法：bench.py <任务名> [重复次数]
    # 采集器用「单次进程内重复 N 次」自校准，摊薄进程启动开销；
    # 重复调用的是纯函数式任务（每次重新初始化局部状态），校验和恒定。
    name = sys.argv[1] if len(sys.argv) > 1 else "noop"
    rep = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    r = 0
    for _ in range(rep):
        r = TASKS[name]()
    print(r)


if __name__ == "__main__":
    main()
