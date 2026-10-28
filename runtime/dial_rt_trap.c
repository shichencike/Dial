#include "dial_rt_trap.h"

#include <stdio.h>
#include <unistd.h>

/*
 * Fail-Fast 的落地点。
 *
 * 诊断信息刻意**不使用** stdio（printf/fprintf）：进程马上就要因非法指令而死，
 * stdio 的缓冲区可能来不及 flush，诊断信息会丢失。
 * 直接用 write(2) 系统调用写 fd 2（stderr），无条件立即到达终端。
 */

/*
 * 崩溃前把 stdout 缓冲冲掉。
 * 否则「越界前已经 print 出来的内容」会随进程一起消失，让人误以为程序
 * 在更早的地方就死了。上下文完整，才谈得上可排查。
 */
static void dial_flush_stdout(void) {
    fflush(NULL);
}

/* 不依赖 strlen / <string.h>：一个循环解决问题（代码纪律 §20.4 保持直白） */
static void dial_emit(const char *s) {
    int64_t n = 0;
    while (s[n] != '\0') n++;
    (void)write(2, s, (size_t)n);
}

static void dial_emit_i64(int64_t v) {
    char buf[24];
    int pos = (int)(sizeof(buf) - 1);
    int neg = v < 0;
    /* 先按正数拆解；INT64_MIN 取负会溢出，故用无符号承接 */
    uint64_t u = neg ? (uint64_t)(-(v + 1)) + 1u : (uint64_t)v;
    buf[pos] = '\0';
    if (u == 0) {
        buf[--pos] = '0';
    }
    while (u > 0) {
        buf[--pos] = (char)('0' + (int)(u % 10u));
        u /= 10u;
    }
    if (neg) buf[--pos] = '-';
    (void)write(2, buf + pos, (size_t)(sizeof(buf) - 1 - (size_t)pos));
}

/*
 * 发射一条非法指令。
 * __builtin_trap() 在 x86-64 上生成 ud2，在 aarch64 上生成 udf #0；
 * clang / zig cc / gcc 全都支持。没有内置支持时退化为向 NULL 写（SIGSEGV）。
 */
void dial_trap(void) {
#if defined(__GNUC__) || defined(__clang__)
    __builtin_trap();
#else
    *(volatile int *)0 = 0;
#endif
}

void dial_bounds_fail(int64_t idx, int64_t len) {
    dial_flush_stdout();
    dial_emit("dial: 下标越界（Fail-Fast 终止）: index = ");
    dial_emit_i64(idx);
    dial_emit(", length = ");
    dial_emit_i64(len);
    dial_emit("\n");
    dial_trap();
}

void dial_null_fail(const char *what) {
    dial_flush_stdout();
    dial_emit("dial: 空指针访问（Fail-Fast 终止）: ");
    dial_emit(what);
    dial_emit("\n");
    dial_trap();
}

void dial_assert_fail(const char *what) {
    dial_flush_stdout();
    dial_emit("dial: 运行时断言失败（Fail-Fast 终止）: ");
    dial_emit(what);
    dial_emit("\n");
    dial_trap();
}
