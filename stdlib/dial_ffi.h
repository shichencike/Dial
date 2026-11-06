#ifndef DIAL_FFI_H
#define DIAL_FFI_H

#include <stdint.h>
#include "dial_string.h"

/*
 * v1.04 FFI：Dial <-> C 的数据总线（实现见 stdlib/dial_ffi.c）。
 *
 * Dial 的 string / bytes 对象布局为 [ int64 len ][ char data[] ]，
 * 对外暴露的句柄指向 len，data 起始于 handle + 8。
 * 这层「+8」的知识**只准留在 C Runtime 里**：编译器生成对这些函数的调用，
 * 用户在 Dial 侧只写 s.as_c_str() / b.as_ptr()，绝不手写 offset hack。
 *
 * 所有权分两类，务必分清：
 *   - *_cstr / *_data 返回的是**借用**指针：不转移所有权、不受 ARC 管理，
 *     生命周期完全绑定原 Dial 对象（原对象一释放，指针立即悬垂）。
 *   - *_from_cstr / *_from_ptr 返回的是**新对象**：从 C 缓冲区复制一份，
 *     受 ARC 管理，调用方按普通 Dial 值使用即可。
 */

/* 借用：取得 C 侧数据区地址（string 保证 NUL 结尾）。不得 free、不得长期持有。 */
void *dial_ffi_string_cstr(DialString s);
void *dial_ffi_bytes_data(DialString b);

/* 复制：从 C 侧缓冲区构造新的、受 ARC 管理的 Dial 对象。
 * string_from_cstr 按 NUL 结尾求长度（NULL 视为空串，不会 strlen(NULL)）；
 * bytes_from_ptr 的长度必须由调用方显式给出——C 缓冲区可以含 '\0'，
 * 绝不能靠 strlen 猜。 */
DialString dial_ffi_string_from_cstr(const char *p);
DialString dial_ffi_bytes_from_ptr(const void *p, int64_t n);

#endif /* DIAL_FFI_H */
