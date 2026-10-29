#ifndef DIAL_IO_H
#define DIAL_IO_H

#include <stdint.h>
#include "dial_string.h"

void dial_print_int(int64_t v);
void dial_print_float(double v);
void dial_print_bool(int64_t b);   /* 内部以 0/1 表示 false/true */
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
