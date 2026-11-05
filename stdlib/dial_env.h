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

#endif /* DIAL_ENV_H */
