#ifndef DIAL_BYTES_H
#define DIAL_BYTES_H

#include <stdint.h>
#include "dial_string.h"

/*
 * bytes —— 不可变的二进制字节缓冲区（v0.98.2）。
 *
 * 与 string 的关系：
 *   - **内存布局相同**（都是 [refcount][len][data...]），因此实现上大量复用；
 *   - 但**类型完全独立**：string 承载文本（语义上是 UTF-8），bytes 承载任意字节
 *     （可含 '\0'、可非合法 UTF-8）。
 *
 * 为什么要有这个区分：零隐式转换铁律要求两者之间必须显式转换。
 * 若二者同型，「把一段文本当二进制写进文件」「把二进制当文本打印」
 * 这两类错误会被编译器放过——而它们正是二进制处理里最常见的 bug。
 * 有了独立类型，编译器能在编译期拦下它们。
 *
 * 所有函数都不接受也不产生隐式转换：bytes 的进出口都必须是 bytes。
 */

/* ---- 构造 ---- */

/* 从任意字节缓冲区构造（长度显式，可为二进制，不必以 '\0' 结尾） */
DialString dial_bytes_new(const char *buf, int64_t len);

/* 构造指定长度的零填充缓冲区（len < 0 视为 0） */
DialString dial_bytes_zero(int64_t len);

/* ---- 读取 ---- */

/* 字节长度（与 string 的 length 同义，都是字节数） */
int64_t dial_bytes_length(DialString b);

/* 取第 i 个字节的值（0~255）；越界由 DIAL_CHECK_BOUNDS 当场 Fail-Fast */
int64_t dial_bytes_get(DialString b, int64_t i);

/* ---- 内存管理（与 string 对称）---- */
void dial_bytes_retain(DialString b);
void dial_bytes_release(DialString b);

/*
 * 拷贝一份（COW 感知）。
 * bytes 不可变，所以「共享即安全」——但仍提供 clone 以对齐 string 的 API 形状，
 * 让「这个对象我也要独立持有一份」的意图显式化。
 */
DialString dial_bytes_clone(DialString b);

/* ---- 与 string 的显式互转（零隐式转换铁律要求必须显式调用）---- */

/*
 * string -> bytes：按**字节**搬运，不做编码转换（可逆）。
 */
DialString dial_bytes_from_string(DialString s);

/*
 * bytes -> string：按字节原样当作 UTF-8 文本。
 * 注意 bytes 可能是**非法 UTF-8**——Dial 不静默替换非法字节（那会破坏数据），
 * 也不抛错；需要严格合法时请调用方自行校验。
 */
DialString dial_bytes_to_string(DialString b);

/* ---- byte 与 int 的显式互转 ---- */

/*
 * int -> byte：带**范围校验**的窄化。
 * 越界（负数或 > 255）时当场 Fail-Fast，绝不静默截断——
 * C 里 `(uint8_t)300` 变成 44，那是被伪装成合法值的**数据损坏**。
 */
int64_t dial_int_to_byte(int64_t v);

/* int -> int32：返回**真实 32 位**（C ABI 宽度），越界 Fail-Fast，绝不静默截断。 */
int32_t dial_int_to_int32(int64_t v);

/* byte -> string：把字节的**数值**形式写成十进制文本（65 -> "65"） */
DialString dial_byte_to_string(int64_t b);

#endif /* DIAL_BYTES_H */
