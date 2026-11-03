#ifndef DIAL_STRING_H
#define DIAL_STRING_H

#include <stdint.h>

/*
 * String 是不透明引用计数对象。Dial 侧只持有 i8*，绝不能直接读内存。
 * payload 布局：[ int64 len ][ char data[] ]（data 以 '\0' 结尾，便于调试）。
 */

typedef void *DialString;

/* 从 C 字符串字面量构造（Dial 的 "abc" 字面量走这里） */
DialString dial_string_new(const char *lit);

int64_t  dial_string_length(DialString s);
DialString dial_string_concat(DialString a, DialString b);
int64_t  dial_string_get(DialString s, int64_t i);

/* 返回 1 表示相等，0 表示不等（Dial 的 s == t 走这里，而非裸 strcmp） */
int64_t  dial_string_eq(DialString a, DialString b);

DialString dial_string_substring(DialString s, int64_t from, int64_t to);

/* COW 感知克隆：唯一持有返回自身，否则深拷贝 */
DialString dial_string_clone(DialString s);

void dial_string_retain(DialString s);
void dial_string_release(DialString s);

/* 取得内部 C 字符串（data 起始于 s+8）；仅供内部 / IO 使用，外部不得释放 */
const char *dial_string_cstr(DialString s);

/* 从任意字节缓冲区构造字符串（长度显式，可为二进制，不必以 '\0' 结尾） */
DialString dial_string_from_bytes(const char *buf, int64_t len);

/* 打印并换行（对接 Dial 的 print(s)） */
void dial_print_string(DialString s);

/* 类型 -> 字符串（CVE-2024-DIAL-006 新增）：让 `print("x=" + str(n))` 成为可能。 */
DialString dial_int_to_string(int64_t v);
DialString dial_float_to_string(double v);

/*
 * char -> 字符串（v0.98.2）：把一个 Unicode 码点编码成 UTF-8（1~4 字节），
 * 构造长度为 1~4 的字符串。支撑 `str(c)` 与 `str(c) + "suffix"`。
 * 非法码点（负数、> 0x10FFFF、UTF-16 代理区 D800~DFFF）一律 Fail-Fast。
 */
DialString dial_char_to_string(int64_t code);

/*
 * 码点 -> UTF-8 字节序列（v0.98.2）：运行时内**唯一**的编码实现，
 * 由 dial_print_char / dial_write_char / dial_char_to_string 共用。
 *
 * buf 至少 5 字节。返回写入的字节数（1~4）；码点非法时返回 -1 且**不写任何字节**。
 */
int dial_encode_utf8(int64_t code, char *buf);

#endif /* DIAL_STRING_H */
