#include <stdint.h>
#include <stddef.h>
#include <string.h>

/*
 * v1.04 FFI 专项：供 tests/run_tests.sh 的 P 组编译成 libffi_demo.a，
 * 再由 tests/ffi/ffi_link_demo.dial 用 `link "libffi_demo.a";` 静态链入。
 *
 * 这里刻意用**显式宽度**的 C 类型（int64_t / uint8_t / int），与 Dial 的
 * FFI 类型映射一一对应：
 *   Dial int   <-> int64_t
 *   Dial float <-> double
 *   Dial byte  <-> uint8_t
 *   Dial bool  <-> int（C 默认整型提升后的 0/1）
 *   Dial rawptr<T> / fn(A):R <-> void* / 函数指针
 */

/* 与 Dial 的 `extern struct Point { x: int, y: byte, flag: bool }` 对应。
 * LLVM 的非 packed struct 按 target DataLayout 自动插 padding，与 C 编译器
 * 完全一致——P 组会逐项比对 sizeof 与 offsetof。 */
struct Point { int64_t x; uint8_t y; _Bool flag; };

int64_t ffi_add(int64_t a, int64_t b) { return a + b; }

int64_t ffi_point_size(void)        { return (int64_t)sizeof(struct Point); }
int64_t ffi_point_offset_x(void)    { return (int64_t)offsetof(struct Point, x); }
int64_t ffi_point_offset_y(void)    { return (int64_t)offsetof(struct Point, y); }
int64_t ffi_point_offset_flag(void) { return (int64_t)offsetof(struct Point, flag); }

/* C 侧按自己的布局读字段：验证 Dial 经 rawptr<Point> 写进去的值
 * 真的落在 C 认为的偏移上，而不是 Dial 另算了一套 padding。 */
int64_t ffi_point_read_x(const void *p)    { return ((const struct Point *)p)->x; }
int64_t ffi_point_read_y(const void *p)    { return (int64_t)((const struct Point *)p)->y; }
int64_t ffi_point_read_flag(const void *p) { return (int64_t)((const struct Point *)p)->flag; }

/* 反向：C 侧写，Dial 侧经裸指针读回来。 */
void ffi_point_fill(void *p, int64_t x, uint8_t y, int flag) {
    struct Point *q = (struct Point *)p;
    q->x = x;
    q->y = y;
    q->flag = (_Bool)flag;
}

/* C 回调：接收 Dial 具名函数指针 + 显式状态 rawptr<void>。
 * 回调签名 int64_t (*)(int64_t, void*) 与 Dial 的 fn(int, rawptr<void>): int
 * 在 C ABI 上逐字节一致，C 侧可以安全调用 Dial 函数。 */
int64_t ffi_call_cb(int64_t (*cb)(int64_t, void *), int64_t arg, void *state) {
    if (!cb) return -1;
    return cb(arg, state);
}
