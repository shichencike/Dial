#ifndef DIAL_ENV_H
#define DIAL_ENV_H

#include <stdint.h>

/*
 * 进程环境：命令行参数捕获（供 args() 内建）、时间戳、伪随机、休眠。
 * 由编译器在生成的 main 入口处调用 dial_env_init(argc, argv) 完成一次初始化。
 */

/* 由编译器在 main 入口调用一次：把 argv[1..] 捕获为 string[] 并存为全局。 */
void dial_env_init(int64_t argc, char **argv);

/* 返回捕获的命令行参数列表（string[]），每次都返回一份独立副本。 */
void *dial_args(void);

/* Unix 纪元起的秒数（double，含小数）。 */
double dial_now(void);

/* [0, 2^63) 的伪随机整数。 */
int64_t dial_random(void);

/* 休眠 ms 毫秒。 */
void dial_sleep(int64_t ms);

/* ---- v1.04 第 2 批 C9：进程环境只读查询 ----
 * 宪法 §1「绝不引入可变全局状态」：g_args / g_prog 都是 dial_env_init 时
 * 一次性捕获的**只读快照**，初始化后不再写入，不参与运行期可变状态。
 * 刻意**不提供 setenv**：改动进程环境就是引入可变全局状态（数据竞争根源）。
 * getenv 不存在时返回空串，用 has_env 显式区分「不存在」与「存在但为空」。 */
void *     dial_getenv(void *name);       /* DialString -> DialString */
int64_t    dial_has_env(void *name);      /* 是否存在 */
void *     dial_program_name(void);       /* argv[0] 只读快照 */
void *     dial_cwd(void);                /* 当前工作目录 string */

/* ---- v1.04 第 2 批 C10：本地时间 / 日期 / 单调计时 ----
 * local_parts 返回 int[]（8 个分量），由调用方自己格式化——拒绝 printf 式
 * 运行期格式串（静态强类型 + 零隐式转换）。monotonic_ms 走单调时钟，
 * 不受系统时间被调整影响，专用于计时；now() 仍是挂钟秒数（float）。 */
void *     dial_local_parts(void);        /* int[]: year,mon,mday,hour,min,sec,wday,yday */
void *     dial_date_string(void);        /* 固定 "YYYY-MM-DD HH:MM:SS"，本地时间 */
int64_t    dial_monotonic_ms(void);       /* 单调毫秒 */

#endif /* DIAL_ENV_H */
