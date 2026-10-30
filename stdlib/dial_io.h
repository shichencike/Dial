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

#endif /* DIAL_IO_H */
