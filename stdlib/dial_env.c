#include "dial_env.h"
#include "dial_string.h"
#include "dial_array.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <stdlib.h>
#include <string.h>
#include <time.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <unistd.h>
#endif

/* 捕获的命令行参数（argv[1..]），建好后是只读的全局常量。 */
static void *g_args = NULL;   /* DialArray，kind=1 (RC) */

/*
 * 跨平台 PRNG：xorshift64*，纯 64 位整数运算。
 * 不依赖 libc 的 srandom()/random()（MinGW-w64 不提供，报 implicit declaration），
 * 也不依赖 rand()（Windows 上 RAND_MAX 仅 32767，拼不出 63 位随机数）。
 * 同一套实现在 Linux glibc / Termux musl / Windows MinGW 上行为完全一致，
 * 也让 zig cc 交叉编译到 x86_64-windows-gnu 时能直接通过。
 */
static uint64_t g_rng_state = 0;

static uint64_t dial_rng_next(void) {
    uint64_t x = g_rng_state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    g_rng_state = x;
    return x * 0x2545F4914F6CDD1DULL;
}

void dial_env_init(int64_t argc, char **argv) {
    /* 仅初始化一次：若已被调用（罕见），直接返回。 */
    if (g_args) return;

    /* 种子：时间 ^ argv 地址 ^ 黄金比例常数。xorshift 在全 0 下会退化，故保证非 0。 */
    g_rng_state = (uint64_t)(unsigned int)time(NULL)
                ^ ((uint64_t)(uintptr_t)argv << 1)
                ^ 0x9E3779B97F4A7C15ULL;
    if (g_rng_state == 0) g_rng_state = 0x9E3779B97F4A7C15ULL;

    DialArray arr = dial_array_new(1 /* RC：元素为 string */);
    for (int64_t i = 1; i < argc; i++) {
        /* 复制 argv[i]：Dial 字符串独立持有，不受进程环境后续改动影响。 */
        DialString s = dial_string_new(argv[i]);
        arr = dial_array_push(arr, (int64_t)s);
        dial_rc_release(s);    /* push 已 retain，本地引用可释放 */
    }
    g_args = arr;
}

void *dial_args(void) {
    if (!g_args) return dial_array_new(1 /* RC */);
    return dial_array_clone(g_args);
}

double dial_now(void) {
#ifdef _WIN32
    /* FILETIME：1601-01-01 起每 100ns 一跳；减去 1601→1970 的秒数得到 Unix 纪元秒。
       MinGW 的 clock_gettime 依赖 winpthreads，这里改用 Win32 原生时钟，无额外依赖。 */
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    uint64_t ticks = ((uint64_t)ft.dwHighDateTime << 32) | (uint64_t)ft.dwLowDateTime;
    return (double)ticks / 1e7 - 11644473600.0;
#else
    struct timespec ts;
    if (clock_gettime(CLOCK_REALTIME, &ts) != 0) return 0.0;
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
#endif
}

int64_t dial_random(void) {
    /* xorshift64* 一次产出 64 位；掩掉符号位，得到 [0, 2^63) 的非负整数。 */
    return (int64_t)(dial_rng_next() & 0x7FFFFFFFFFFFFFFFLL);
}

void dial_sleep(int64_t ms) {
    if (ms < 0) ms = 0;
#ifdef _WIN32
    /* DWORD 上限约 49 天；超出时钳制，避免截断回绕成极短休眠。 */
    if (ms > 0xFFFFFFFFLL) ms = 0xFFFFFFFFLL;
    Sleep((DWORD)ms);
#else
    struct timespec req;
    req.tv_sec = (time_t)(ms / 1000);
    req.tv_nsec = (long)((ms % 1000) * 1000000);
    nanosleep(&req, NULL);
#endif
}
