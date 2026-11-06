/* Dial 性能基准 —— C (gcc -O2) 实现。
 *
 * 28 个跨语言等价的纯整数微基准。第一个命令行参数指定任务名，程序只在
 * stdout 打印一行十进制校验和；计时由外部采集器负责。
 * 一致性铁律见 bench.py 顶部注释。全程 int64_t，所有被取模/相除的值非负。
 * 编译：gcc -O2 -o bench_c bench.c
 */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>

#define LOOP 1000000
#define MID 100000
#define ARR 2048
#define SORTN 512
#define MOD 1000000007LL
#define MOD2 1000003LL

static const char *S = "abcdefghijklmnopqrstuvwxyz0123456789";

static int64_t b01(void) {
    int64_t s = 0;
    for (int64_t i = 0; i < LOOP; i++) s = (s + i) % MOD;
    return s;
}
static int64_t b02(void) {
    int64_t s = 1;
    for (int64_t i = 0; i < MID; i++) s = (s * 31 + i) % MOD;
    return s;
}
static int64_t b03(void) {
    int64_t s = 0;
    for (int64_t i = 1; i < MID; i++) s = (s + (i * 1103515245LL) / 7 + i % 97) % MOD;
    return s;
}
static int64_t b04(void) {
    int64_t s = 0;
    for (int64_t i = 0; i < LOOP; i++) s = (s * 3 + 7) % MOD;
    return s;
}
static int64_t b05(void) {
    int64_t s = 0;
    for (int64_t a = 0; a < 20000; a++) {
        int64_t base = a % MOD2 + 1, r = 1;
        for (int j = 0; j < 8; j++) r = (r * base) % MOD;
        s = (s + r) % MOD;
    }
    return s;
}
static int64_t gcd_i(int64_t a, int64_t b) {
    while (b) { int64_t t = a % b; a = b; b = t; }
    return a;
}
static int64_t b06(void) {
    int64_t s = 0;
    for (int64_t i = 1; i < 30000; i++) s = (s + gcd_i(i * 7 + 1, i * 13 + 3)) % MOD;
    return s;
}
static int64_t b07(void) {
    int64_t a = 0, b = 1;
    for (int64_t i = 0; i < LOOP; i++) { int64_t t = (a + b) % MOD; a = b; b = t; }
    return a;
}
static int64_t b08(void) {
    int64_t s = 0;
    for (int64_t n = 1; n < 20000; n++) {
        int64_t x = n, st = 0;
        while (x > 1) { if (x % 2 == 0) x = x / 2; else x = 3 * x + 1; st++; }
        s = (s + st) % MOD;
    }
    return s;
}
static int64_t fib_r(int64_t n) {
    if (n < 2) return n;
    return fib_r(n - 1) + fib_r(n - 2);
}
static int64_t b09(void) {
    int64_t s = 0;
    for (int k = 0; k < 3; k++) s = (s + fib_r(23)) % MOD;
    return s;
}
static int64_t rsum_r(int64_t n) {
    if (n == 0) return 0;
    return (n + rsum_r(n - 1)) % MOD;
}
static int64_t b10(void) {
    int64_t s = 0;
    for (int k = 0; k < 50000; k++) s = (s + rsum_r(25)) % MOD;
    return s;
}
static int64_t tree_r(int64_t n) {
    if (n < 2) return 1;
    return (tree_r(n - 1) + tree_r(n - 2) + 1) % MOD;
}
static int64_t b11(void) {
    int64_t s = 0;
    for (int k = 0; k < 200; k++) s = (s + tree_r(18)) % MOD;
    return s;
}
static int64_t od_r(int64_t n);
static int64_t ev_r(int64_t n) {
    if (n == 0) return 1;
    return od_r(n - 1);
}
static int64_t od_r(int64_t n) {
    if (n == 0) return 0;
    return ev_r(n - 1);
}
static int64_t b12(void) {
    int64_t s = 0;
    for (int64_t k = 0; k < 20000; k++) s = (s + ev_r(k % 60 + 1)) % MOD;
    return s;
}
static int64_t b13(void) {
    int64_t a[ARR], s = 0;
    for (int i = 0; i < ARR; i++) a[i] = (i * 31 + 7) % MOD2;
    for (int j = 0; j < ARR; j++) s = (s + a[j]) % MOD;
    return s;
}
static int64_t b14(void) {
    int64_t a[ARR], idx = 0, s = 0;
    for (int i = 0; i < ARR; i++) a[i] = (i * 17) % MOD2;
    for (int64_t i = 0; i < LOOP; i++) {
        idx = (idx * 1103515245LL + 12345) & (ARR - 1);
        s = (s + a[idx]) % MOD;
        a[idx] = (a[idx] + 1) % MOD2;
    }
    return s;
}
static int64_t b15(void) {
    int64_t a[SORTN], s = 0;
    for (int i = 0; i < SORTN; i++) a[i] = SORTN - i;
    for (int i = 0; i < SORTN; i++) {
        for (int j = 0; j < SORTN - 1 - i; j++)
            if (a[j] > a[j + 1]) { int64_t t = a[j]; a[j] = a[j + 1]; a[j + 1] = t; }
        s = (s + a[i]) % MOD;
    }
    return s;
}
static int64_t b16(void) {
    int64_t a[ARR], s = 0;
    for (int i = 0; i < ARR; i++) a[i] = (i * 13 + 5) % MOD2;
    for (int r = 0; r < 100; r++) {
        for (int i = 0; i < ARR / 2; i++) {
            int64_t t = a[i]; a[i] = a[ARR - 1 - i]; a[ARR - 1 - i] = t;
        }
        for (int j = 0; j < ARR; j++) s = (s + a[j]) % MOD;
    }
    return s;
}
static int64_t b17(void) {
    const int n = 64;
    int64_t A[4096], B[4096], C[4096];
    for (int i = 0; i < n * n; i++) { A[i] = (i * 7 + 3) % MOD2; B[i] = (i * 11 + 5) % MOD2; C[i] = 0; }
    for (int i = 0; i < n; i++)
        for (int k = 0; k < n; k++) {
            int64_t aik = A[i * n + k];
            for (int j = 0; j < n; j++) C[i * n + j] += aik * B[k * n + j];
        }
    int64_t s = 0;
    for (int v = 0; v < n * n; v++) s = (s + C[v]) % MOD;
    return s;
}
static int64_t b18(void) {
    int64_t a[ARR], s = 0;
    for (int r = 0; r < 200; r++) {
        for (int i = 0; i < ARR; i++) a[i] = (i * 13 + 5) % MOD2;
        for (int i = 1; i < ARR; i++) a[i] = (a[i] + a[i - 1]) % MOD;
        for (int j = 0; j < ARR; j++) s = (s + a[j]) % MOD;
    }
    return s;
}
static int64_t b19(void) {
    int64_t s = 0; int L = (int)strlen(S);
    for (int r = 0; r < 20000; r++)
        for (int c = 0; c < L; c++) s = (s + (unsigned char)S[c]) % MOD;
    return s;
}
static int64_t b20(void) {
    char x[8192]; x[0] = 'x'; int len = 1;
    for (int i = 0; i < 4000; i++) x[len++] = 'y';
    x[len] = 0;
    return (len * 31 + (unsigned char)x[0] + (unsigned char)x[len - 1]) % MOD;
}
static int64_t find_sub(const char *s, const char *p) {
    for (int64_t i = 0; s[i]; i++) {
        int j = 0;
        while (p[j] && s[i + j] == p[j]) j++;
        if (!p[j]) return i;
    }
    return -1;
}
static int64_t b21(void) {
    static const char *subs[4] = {"mnop", "0123", "xyz", "abcd"};
    int64_t s = 0;
    for (int64_t k = 0; k < 20000; k++) s = (s + find_sub(S, subs[k % 4])) % MOD;
    return s;
}
static int64_t b22(void) {
    char base[128];
    int bl = 0;
    for (int i = 0; S[i]; i++) base[bl++] = S[i];
    base[bl++] = '-';
    for (int i = 0; S[i]; i++) base[bl++] = S[i];
    base[bl] = 0;
    int64_t s = 0;
    for (int64_t k = 0; k < 10000; k++) {
        int f = (int)(k % 20);
        for (int c = f; c < f + 10; c++) s = (s + (unsigned char)base[c]) % MOD;
    }
    return s;
}
static int64_t b23(void) {
    int64_t s = 0;
    for (int64_t i = 0; i < MID; i++) {
        int64_t x = i, c = 0;
        while (x) { c += x & 1; x >>= 1; }
        s = (s + c) % MOD;
    }
    return s;
}
static int64_t b24(void) {
    int64_t x = 1, s = 0;
    for (int64_t i = 0; i < LOOP; i++) {
        x ^= (x << 13) & 0xFFFFFFFFLL;
        x ^= x >> 7;
        x ^= (x << 17) & 0xFFFFFFFFLL;
        x &= 0xFFFFFFFFLL;
        s = (s + x % MOD2) % MOD;
    }
    return s;
}
static int64_t b25(void) {
    int64_t s = 0;
    for (int64_t i = 0; i < LOOP; i++) {
        int64_t r = i % 3;
        if (r == 0) s = (s + i) % MOD;
        else if (r == 1) s = (s + i * 2) % MOD;
        else s = (s + i * 3) % MOD;
    }
    return s;
}
static int64_t b26(void) {
    int64_t s = 0;
    for (int64_t i = 0; i < 1000; i++)
        for (int64_t j = 0; j < 1000; j++) s = (s + (i ^ j)) % MOD;
    return s;
}
static int64_t b27(void) {
    int64_t s = 0;
    for (int64_t i = 0; i < LOOP; i++) {
        if (((i % 7 == 0) && (i % 3 != 0)) || (i & 1)) s = (s + 1) % MOD;
        else s = (s + 2) % MOD;
    }
    return s;
}
static int64_t b28(void) {
    int64_t a[ARR], s = 0;
    for (int i = 0; i < ARR; i++) a[i] = (i * 29 + 11) % MOD2;
    for (int64_t i = 0; i < MID; i++) {
        int64_t p = i & (ARR - 1);
        a[p] = (a[p] + i) % MOD2;
        s = (s * 31 + a[p] + (i & 255)) % MOD;
    }
    return s;
}

struct Task { const char *name; int64_t (*fn)(void); };
static struct Task TASKS[] = {
    {"b01",b01},{"b02",b02},{"b03",b03},{"b04",b04},{"b05",b05},{"b06",b06},
    {"b07",b07},{"b08",b08},{"b09",b09},{"b10",b10},{"b11",b11},{"b12",b12},
    {"b13",b13},{"b14",b14},{"b15",b15},{"b16",b16},{"b17",b17},{"b18",b18},
    {"b19",b19},{"b20",b20},{"b21",b21},{"b22",b22},{"b23",b23},{"b24",b24},
    {"b25",b25},{"b26",b26},{"b27",b27},{"b28",b28},{"noop",NULL},{NULL,NULL}
};
/* 用法：bench_c <任务名> [重复次数]
 * 采集器用「单次进程内重复 N 次」自校准，摊薄进程启动开销；
 * 重复调用的是纯函数式任务，校验和恒定（用于跨语言一致性校验）。 */
int main(int argc, char **argv) {
    const char *name = argc > 1 ? argv[1] : "noop";
    long rep = argc > 2 ? atol(argv[2]) : 1;
    int64_t (*fn)(void) = NULL;
    for (int i = 0; TASKS[i].name; i++)
        if (strcmp(name, TASKS[i].name) == 0) { fn = TASKS[i].fn; break; }
    int64_t r = 0;
    for (long q = 0; q < rep; q++) r = fn ? fn() : 0;
    printf("%lld\n", (long long)r);
    return 0;
}
