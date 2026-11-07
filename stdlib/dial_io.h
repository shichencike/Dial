#ifndef DIAL_IO_H
#define DIAL_IO_H

#include <stdint.h>
#include "dial_string.h"

void dial_print_int(int64_t v);
void dial_print_float(double v);
void dial_print_bool(int64_t b);   /* 内部以 0/1 表示 false/true */
/*
 * 打印一个 char（v0.92）：输出该码点对应的 **UTF-8 字符**，而不是码点数字。
 * 码点是 Unicode 标量值；输出时按 UTF-8 编码为 1~4 字节后写出。
 * 非法码点（负数、> 0x10FFFF、UTF-16 代理区 D800~DFFF）一律按 Fail-Fast 处理：
 * 当场向 stderr 打印诊断并终止，绝不输出半个坏字符——
 * 静默产出乱码比当场崩溃更危险，它会在下游变成无法复现的数据损坏。
 */
void dial_print_char(int64_t code);
/*
 * 原样输出一个字符串，**不追加换行**（v0.98.2）。
 *
 * 这是「连续输出」的基本积木：print(s) 永远在末尾补一个 '\n'，
 * 因此拼行（表格、进度条、逐字动画、协议帧）无法用 print 表达。
 * print_raw(s) 只把 s 的字节原封不动写到 stdout，不解释、不转义、不补换行。
 *
 * 语义边界：
 *   - s 为空串时是合法的 no-op（输出 0 字节）；
 *   - s 含 '\0' 时按**字节长度**写出（不依赖 C 字符串的 NUL 终止语义）；
 *   - 写失败（如 stdout 已关闭）被忽略，不抛错——与 dial_print_string 一致
 *     （stdout 写失败时进程本身通常已处于不可写状态，继续执行没有意义）。
 */
void dial_print_raw(DialString s);

/*
 * 输出单个字符，**不追加换行**（v0.98.2）。
 *
 * 与 dial_print_char 的分工：
 *     print('x')    -> x\n   （带换行，行式输出）
 *     write_char('x') -> x   （不带换行，连续输出）
 *
 * 码点校验规则与 dial_print_char 完全一致（合法范围 0~0x10FFFF 且不含
 * UTF-16 代理区 D800~DFFF）：非法码点一律 Fail-Fast，绝不输出半个坏字符。
 */
void dial_write_char(int64_t code);

/*
 * 标量版本的无换行输出（v0.98.2）。
 *
 * 与 dial_print_int / dial_print_float / dial_print_bool 的文本形态**完全一致**，
 * 唯一区别是末尾不补 '\n'：
 *     print(42)      -> 42\n
 *     print_raw(42)  -> 42
 *
 * 之所以在 C 层单列一组（而不是 Dial 侧先 str(v) 再 print_raw）：
 * str(v) 会分配一个带引用计数的堆字符串，而它只是中转——用完即弃却仍占着
 * 引用计数，属于实打实的泄漏。这里用栈缓冲，零堆分配。
 */
void dial_print_raw_int(int64_t v);
void dial_print_raw_float(double v);
void dial_print_raw_bool(int64_t b);

void dial_print_string(DialString s);

/* 从 stdin 读取一行（去掉结尾换行），返回不透明 String 指针 */
DialString dial_read_line(void);

/*
 * 文件 IO（v0.61）：文件句柄是不透明的 FILE*（经 void* 传递）。
 * 用户显式 open / close，不由 ARC 管理（与内存宪法 §19.3「显式指针回收」一致）。
 * 错误处理遵循「无异常」原则：打开失败返回 NULL、读失败返回空串、写失败返回 -1。
 */
typedef void *DialFile;

DialFile  dial_file_open(DialString path, DialString mode);
DialString dial_file_read(DialFile f, int64_t n);   /* 读最多 n 字节，失败返回空串 */
int64_t   dial_file_write(DialFile f, DialString content); /* 返回写入字节数，失败返回 -1 */
void      dial_file_close(DialFile f);              /* 空指针安全 */

/* ---- stderr 输出（v1.04 C8）：与 dial_print_* 一一对称，只换输出流 ----
 * 目的：让日志 / 诊断与正常输出分流。刻意**不引入可变全局状态**（宪法 §1
 * 「绝不引入可变全局状态」）：没有「当前输出流」开关、没有全局 FILE*，
 * 每个函数直接写 stderr，纯函数式、线程安全、可推理。
 * 文本形态（含是否补 '\n'）与 stdout 版本完全一致。 */
void dial_eprint_int(int64_t v);
void dial_eprint_float(double v);
void dial_eprint_bool(int64_t b);
void dial_eprint_char(int64_t code);
void dial_eprint_string(DialString s);
void dial_eprint_raw(DialString s);
void dial_eprint_write_char(int64_t code);
void dial_eprint_raw_int(int64_t v);
void dial_eprint_raw_float(double v);
void dial_eprint_raw_bool(int64_t b);

#endif /* DIAL_IO_H */
