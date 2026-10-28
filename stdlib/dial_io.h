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

#endif /* DIAL_IO_H */
