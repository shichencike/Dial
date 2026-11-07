#ifndef DIAL_STRING_H
#define DIAL_STRING_H

#include <stdint.h>
#include "dial_array.h"

/*
 * String 是不透明引用计数对象。Dial 侧只持有 i8*，绝不能直接读内存。
 * payload 布局：[ int64 len ][ char data[] ]（data 以 '\0' 结尾，便于调试）。
 */

typedef void *DialString;

/* 从 C 字符串字面量构造（Dial 的 "abc" 字面量走这里） */
DialString dial_string_new(const char *lit);

/* v1.00：布尔值转字符串（true / false），供字符串插值使用 */
DialString dial_bool_to_string(int64_t b);

int64_t  dial_string_length(DialString s);
DialString dial_string_concat(DialString a, DialString b);
int64_t  dial_string_get(DialString s, int64_t i);

/* 返回 1 表示相等，0 表示不等（Dial 的 s == t 走这里，而非裸 strcmp） */
int64_t  dial_string_eq(DialString a, DialString b);

DialString dial_string_substring(DialString s, int64_t from, int64_t to);

/* 查找子串首次出现位置（返回字节偏移），未找到返回 -1。 */
int64_t dial_string_index_of(DialString s, DialString sub);

/* 把所有 from 子串替换为 to（返回新字符串，原串不变）。 */
DialString dial_string_replace(DialString s, DialString from, DialString to);

/* 去除首尾空白（空格 / 制表 / 换行 / 回车），返回新字符串。 */
DialString dial_string_trim(DialString s);

/* 按分隔符 sep 切分，返回 string[]（kind=1 RC 的数组，每个元素是 retain 过的子串）。 */
DialArray dial_string_split(DialString s, DialString sep);

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

/* ---- v1.04 第 2 批 C1：字符串查询 / ASCII 规范化（方法）----
 * to_upper / to_lower 只变换 ASCII 的 A-Z / a-z：UTF-8 多字节序列的每个字节
 * 都 >= 0x80，不会被触碰，因此中文 / emoji 原样保留（不是「不支持」，是「不破坏」）。
 * 三个查询返回 1/0（Dial 的 bool），按**字节**比较，与 index_of 语义一致。 */
DialString dial_string_to_upper(DialString s);
DialString dial_string_to_lower(DialString s);
int64_t    dial_string_contains(DialString s, DialString sub);
int64_t    dial_string_starts_with(DialString s, DialString prefix);
int64_t    dial_string_ends_with(DialString s, DialString suffix);

/* ---- v1.04 第 2 批 C4：UTF-8 码点视图（方法）----
 * 关键裁决：length() / get(i) / substring(from,to) **保持字节语义不变**——
 * 它们被 416 项既有回归与所有索引/切片程序依赖，改成码点语义是静默破坏。
 * Unicode 能力一律走新增方法，索引单位明确写进方法名（char_*）。
 * 非法 UTF-8 序列（overlong / 代理区 / 截断 / >U+10FFFF）一律 Fail-Fast，
 * 绝不静默产出 U+FFFD 或跳字节（§20：静默乱码比当场崩溃危险）。 */
int64_t    dial_string_char_count(DialString s);          /* 码点个数 */
DialArray  dial_string_chars(DialString s);               /* int[]，每个码点一个 int */
int64_t    dial_string_char_at(DialString s, int64_t i);  /* 第 i 个码点（码点索引） */
DialString dial_string_char_slice(DialString s, int64_t from, int64_t to); /* 码点半开区间 */

/* ---- v1.04 第 2 批 C2/C3：严格数值解析与定点格式化（内建）----
 * Dial 没有 Option / Result（B3 属第 3 批），返回值只能是 int / float，
 * 任何「失败哨兵值」都会与合法值混淆——正是宪法禁止的静默损坏。
 * 因此 parse_* 对非法输入**当场 Fail-Fast**；is_* 提供显式、无副作用的事前探测，
 * 让调用方自己决定「不合法时怎么办」（零隐式转换 + 显式优于隐式）。 */
int64_t    dial_parse_int(DialString s);
double     dial_parse_float(DialString s);
int64_t    dial_is_int(DialString s);
int64_t    dial_is_float(DialString s);
/* 类型安全的定点格式化：digits 显式 int，编译期类型确定，不做 printf 式变参。
 * 拒绝运行期格式串（零隐式转换 + 静态强类型 + C 变参无类型检查三重理由）。 */
DialString dial_format_float(double v, int64_t digits);

#endif /* DIAL_STRING_H */
