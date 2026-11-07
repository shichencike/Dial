#include "dial_env.h"
#include "dial_string.h"
#include "dial_array.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <stdlib.h>
#include <string.h>
#include <stdio.h>    /* v1.04 第 2 批：snprintf（date_string） */
#include <time.h>
#ifdef _WIN32
#include <windows.h>
#include <direct.h>   /* v1.04 第 2 批：_getcwd（MinGW 下声明在此） */
#else
#include <unistd.h>
#endif

/* 捕获的命令行参数（argv[1..]），建好后是只读的全局常量。
 * v1.04 C9：argv[0] 同样在 init 时复制成只读快照，供 program_name() 取用。 */
static void *g_args = NULL;   /* DialArray，kind=1 (RC) */
static void *g_prog = NULL;   /* DialString，argv[0] 的只读副本 */

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
    /* argv[0]：程序名/路径，同样是只读快照（不是可变全局状态）。 */
    g_prog = dial_string_new(argc > 0 && argv[0] ? argv[0] : "");
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

/* ==========================================================================
 * v1.04 第 2 批 C9：进程环境只读查询
 *
 * 宪法 §1「绝不引入可变全局状态」：本组全是**读**，不写任何进程状态，
 * 也不提供 setenv。g_prog / g_args 是 init 时的一次性只读快照。
 * getenv 不存在时返回空串，用 has_env 显式区分「不存在」与「存在但为空」。
 * ========================================================================== */

void *dial_getenv(void *name) {
    if (!name) return dial_string_new("");
    const char *v = getenv(dial_string_cstr(name));
    return dial_string_new(v ? v : "");
}

int64_t dial_has_env(void *name) {
    if (!name) return 0;
    return getenv(dial_string_cstr(name)) != NULL ? 1 : 0;
}

void *dial_program_name(void) {
    if (!g_prog) return dial_string_new("");
    dial_rc_retain(g_prog);       /* 交出一份新拥有的引用给调用方 */
    return g_prog;
}

void *dial_cwd(void) {
#ifdef _WIN32
    char buf[4096];
    if (!_getcwd(buf, (int)sizeof(buf))) return dial_string_new("");
#else
    char buf[4096];
    if (!getcwd(buf, sizeof(buf))) return dial_string_new("");
#endif
    return dial_string_new(buf);
}

/* ==========================================================================
 * v1.04 第 2 批 C10：本地时间 / 日期 / 单调计时
 * ========================================================================== */

/* 把当前本地时间拆成 struct tm；跨平台避开 localtime 的静态缓冲（线程不安全）。 */
static int dial_local_tm(struct tm *out) {
    time_t t = time(NULL);
    if (t == (time_t)-1) return 0;
#ifdef _WIN32
    return localtime_s(out, &t) == 0;
#else
    return localtime_r(&t, out) != NULL;
#endif
}

void *dial_local_parts(void) {
    struct tm tm;
    if (!dial_local_tm(&tm)) dial_assert_fail("local_parts: 无法取得本地时间");
    DialArray a = dial_array_new(0 /* INT */);
    int64_t parts[8] = {
        (int64_t)tm.tm_year + 1900, (int64_t)tm.tm_mon + 1, (int64_t)tm.tm_mday,
        (int64_t)tm.tm_hour, (int64_t)tm.tm_min, (int64_t)tm.tm_sec,
        (int64_t)tm.tm_wday, (int64_t)tm.tm_yday,
    };
    for (int i = 0; i < 8; i++) a = dial_array_push(a, parts[i]);
    return a;
}

void *dial_date_string(void) {
    struct tm tm;
    if (!dial_local_tm(&tm)) dial_assert_fail("date_string: 无法取得本地时间");
    char buf[32];
    int n = snprintf(buf, sizeof(buf), "%04d-%02d-%02d %02d:%02d:%02d",
                     tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
                     tm.tm_hour, tm.tm_min, tm.tm_sec);
    if (n < 0) n = 0;
    return dial_string_from_bytes(buf, (int64_t)n);
}

int64_t dial_monotonic_ms(void) {
#ifdef _WIN32
    /* GetTickCount64：系统启动以来的毫秒，单调、不受系统时间调整影响。 */
    return (int64_t)GetTickCount64();
#else
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return 0;
    return (int64_t)ts.tv_sec * 1000 + (int64_t)ts.tv_nsec / 1000000;
#endif
}
