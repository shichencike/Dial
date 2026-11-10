#include <stdint.h>
#include <stddef.h>

/*
 * int32 / float32 FFI 精确宽度探针（AB 组）。
 *
 * 痛点：Dial 过去只有 int(i64) / float(f64)。若用它们声明 extern fn 参数，
 * LLVM 生成的签名是 i64/f64，而 C 侧按 int32_t/float 读取——
 * 尤其 float：f64 的位模式低 32 位绝不是那个 float 值，C 侧必定读到垃圾。
 * 本探针的每个函数都用**真实 32 位 C 类型**，由 Dial 侧用 int32/float32
 * 声明，逐项验证参数、返回值、多参数、extern struct 字段全部精确对齐。
 */

int64_t s32_echo(int32_t v)      { return (int64_t)v; }
double  s32_f32_echo(float v)    { return (double)v; }
int32_t s32_ret_i32(int32_t v)   { return v + 1; }
float   s32_ret_f32(float v)     { return v * 2.0f; }

/* 多个 32 位参数连续传递：宽度错位会在这里累积暴露。 */
int64_t s32_sum4(int32_t a, int32_t b, int32_t c, int32_t d) {
    return (int64_t)a + (int64_t)b + (int64_t)c + (int64_t)d;
}
double s32_fsum2(float a, float b) { return (double)a + (double)b; }

/* C 内存布局：int64 / int32 / float / int64 → sizeof 24，偏移 0/8/12/16。 */
struct S32 { int64_t a; int32_t b; float c; int64_t d; };

int64_t s32_size(void)                { return (int64_t)sizeof(struct S32); }
int64_t s32_off_b(void)               { return (int64_t)offsetof(struct S32, b); }
int64_t s32_off_c(void)               { return (int64_t)offsetof(struct S32, c); }
int64_t s32_off_d(void)               { return (int64_t)offsetof(struct S32, d); }
int64_t s32_read_b(const void *p)     { return (int64_t)((const struct S32 *)p)->b; }
double  s32_read_c(const void *p)     { return (double)((const struct S32 *)p)->c; }

void s32_fill(void *p, int32_t b, float c) {
    struct S32 *q = (struct S32 *)p;
    q->a = 7; q->b = b; q->c = c; q->d = 9;
}
