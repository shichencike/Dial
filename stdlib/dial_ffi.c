#include "dial_ffi.h"
#include "dial_bytes.h"
#include <string.h>

/*
 * v1.04 FFI 数据总线。
 *
 * 这里刻意把「跳过 8 字节长度头」这一步收在 C Runtime 内：
 * 编译器只需声明并按名字调用，不必在 IR 里硬编码对象布局；
 * 用户更不需要在 Dial / C 两侧写 +8。布局若将来变化，只改这一处。
 */

void *dial_ffi_string_cstr(DialString s) {
    return (void *)((char *)s + 8);
}

void *dial_ffi_bytes_data(DialString b) {
    return (void *)((char *)b + 8);
}

DialString dial_ffi_string_from_cstr(const char *p) {
    if (!p) return dial_string_from_bytes("", 0);
    int64_t n = (int64_t)strlen(p);
    return dial_string_from_bytes(p, n);
}

DialString dial_ffi_bytes_from_ptr(const void *p, int64_t n) {
    return dial_bytes_new((const char *)p, n);
}
