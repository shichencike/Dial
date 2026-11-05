#include "dial_env.h"
#include "dial_string.h"
#include "dial_array.h"
#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* 捕获的命令行参数（argv[1..]），建好后是只读的全局常量。 */
static void *g_args = NULL;   /* DialArray，kind=1 (RC) */

void dial_env_init(int64_t argc, char **argv) {
    /* 仅初始化一次：若已被调用（罕见），直接返回。 */
    if (g_args) return;

    /* 伪随机种子：让 random() 每次运行序列不同。 */
    srandom((unsigned int)time(NULL) ^ (unsigned int)(size_t)argv);

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
    struct timespec ts;
    if (clock_gettime(CLOCK_REALTIME, &ts) != 0) return 0.0;
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

int64_t dial_random(void) {
    /* 拼接两次 31 位 random()，覆盖 62 位；高位再叠一次，凑满 [0, 2^63)。 */
    uint64_t lo = (uint64_t)random();
    uint64_t hi = (uint64_t)random();
    uint64_t v = (hi << 31) | lo;
    return (int64_t)(v & 0x7FFFFFFFFFFFFFFFLL);
}

void dial_sleep(int64_t ms) {
    if (ms < 0) ms = 0;
    struct timespec req;
    req.tv_sec = (time_t)(ms / 1000);
    req.tv_nsec = (long)((ms % 1000) * 1000000);
    nanosleep(&req, NULL);
}
