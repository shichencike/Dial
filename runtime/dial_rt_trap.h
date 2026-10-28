#ifndef DIAL_RT_TRAP_H
#define DIAL_RT_TRAP_H

#include <stdint.h>

/*
 * ============================================================================
 *  Dial 错误处理铁律：Fail-Fast（借鉴 Zig）
 * ============================================================================
 *
 *  宁可当场段错误（SIGILL / SIGSEGV / Core Dump），
 *  也绝不允许产生未定义行为（UB）或静默产生错误数据。
 *
 *  与 C 的根本区别：C 的越界访问会“悄悄修改相邻内存然后继续跑”，
 *  使 Bug 在几万行之后才以完全无关的形态爆发。Dial 拒绝这种静默损坏——
 *  任何非法的运行时状态都必须在**发生的那一行**立刻终止进程。
 *
 *  实现：dial_trap() 直接发射一条非法指令（x86 的 ud2 / ARM 的 udf），
 *  由操作系统立刻接管并 Core Dump。调试器可以精确停在越界的那一条指令上。
 *
 * ----------------------------------------------------------------------------
 *  安全等级（编译期常量，由 dialc 通过 -DDIAL_SAFETY=N 注入）
 * ----------------------------------------------------------------------------
 *    DIAL_SAFETY = 2  Debug（默认）：全部检查开启，含冗余的内部一致性断言
 *    DIAL_SAFETY = 1  ReleaseSafe  ：保留边界/空指针检查（廉价，防 UB），
 *                                   关闭仅用于排查 Runtime 自身 Bug 的冗余断言
 *    DIAL_SAFETY = 0  ReleaseFast  ：关闭全部检查，越界即 UB（与 C 一致），
 *                                   用于榨取极限性能——后果由使用者自行承担
 *
 *  注意：等级是编译期常量，等级 0 时检查代码被编译器完全消除，零运行时开销。
 * ============================================================================
 */

#define DIAL_SAFETY_FAST  0
#define DIAL_SAFETY_SAFE  1
#define DIAL_SAFETY_DEBUG 2

#ifndef DIAL_SAFETY
#define DIAL_SAFETY DIAL_SAFETY_DEBUG
#endif

/* ---- 立即触发非法指令（不返回）。x86: ud2；ARM: udf ---- */
void dial_trap(void);

/* ---- 诊断性失败：先向 stderr 打印原因，再 dial_trap()。不返回。 ---- */
void dial_bounds_fail(int64_t idx, int64_t len);
void dial_null_fail(const char *what);
void dial_assert_fail(const char *what);

/*
 * 检查宏。条件是编译期常量，等级不足时整段被消除（零开销）。
 * 写成 do{...}while(0) 是为了在任何 if/else 语境下都是一条完整语句。
 */
#define DIAL_CHECK_BOUNDS(idx, len)                                            \
    do {                                                                       \
        if (DIAL_SAFETY >= DIAL_SAFETY_SAFE &&                                 \
            ((idx) < 0 || (idx) >= (len))) {                                   \
            dial_bounds_fail((idx), (len));                                    \
        }                                                                      \
    } while (0)

#define DIAL_CHECK_NULL(p, what)                                               \
    do {                                                                       \
        if (DIAL_SAFETY >= DIAL_SAFETY_SAFE && (p) == NULL) {                  \
            dial_null_fail(what);                                              \
        }                                                                      \
    } while (0)

/* 仅 Debug 开启：用于排查 Runtime 自身 Bug 的冗余一致性断言 */
#define DIAL_ASSERT(cond, what)                                                \
    do {                                                                       \
        if (DIAL_SAFETY >= DIAL_SAFETY_DEBUG && !(cond)) {                     \
            dial_assert_fail(what);                                            \
        }                                                                      \
    } while (0)

/*
 * 堆管理完整性断言：ReleaseSafe 也保留，只有 ReleaseFast 才关闭。
 *
 * 与 DIAL_ASSERT 的区别：后者只排查 Runtime 自己的 Bug；前者防的是**用户可见的
 * 内存破坏**（重复释放、use-after-free）——一旦发生，后续任何行为都不可预测，
 * 属于"崩溃保护"而非"冗余诊断"。开销仅一次比较，不值得为它牺牲默认安全性。
 */
#define DIAL_HEAP_ASSERT(cond, what)                                           \
    do {                                                                       \
        if (DIAL_SAFETY >= DIAL_SAFETY_SAFE && !(cond)) {                      \
            dial_assert_fail(what);                                            \
        }                                                                      \
    } while (0)

#endif /* DIAL_RT_TRAP_H */
