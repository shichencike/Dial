#include <stdint.h>
#include <stddef.h>

/*
 * v1.04 FFI 导出专项：C 侧「桥」。
 *
 * 这个文件同时验证两个方向：
 *
 *   1) C -> Dial：直接调用 Dial 用 `@export` 导出的符号（dial_add / dial_scale）。
 *      必须是**裸 C 符号名**（无 mangling）+ C 调用约定 + C ABI 传参，
 *      否则 C 侧根本链接不上、或链接上了却传错参数。
 *
 *   2) Dial -> C：把 C 侧的 sizeof / alignof / offsetof 报给 Dial，
 *      让 Dial 把自己算出来的**同一组数字**也打出来——两组逐项相等，
 *      才算「Dial 的 packed struct 与 C 的 __attribute__((packed)) 逐字节一致」。
 *      这里刻意**不写死期望值**：布局随平台变，等值性才是真正要锁住的东西。
 */

/* 网络协议头：紧凑布局，字段紧密排列、无对齐 padding。
 * 与 Dial 的 `packed extern struct PktHdr` 逐字节对应。 */
#if defined(_MSC_VER)
#pragma pack(push, 1)
#endif
struct __attribute__((packed)) PktHdr {
    int64_t seq;
    uint8_t flags;
    uint8_t len;
};
#if defined(_MSC_VER)
#pragma pack(pop)
#endif

/* 同样的字段，但按 C 的自然对齐补 padding（seq 之后 flags 到下一个 8 字节边界）。
 * 与 Dial 的普通 `extern struct RegHdr` 逐字节对应。 */
struct RegHdr {
    int64_t seq;
    uint8_t flags;
    uint8_t len;
};

/* --- 1) C -> Dial：调用 @export 导出的符号 --- */

extern int64_t dial_add(int64_t a, int64_t b);
extern int64_t dial_scale(int64_t v, int64_t k);

int64_t ffi_bridge_add(int64_t a, int64_t b)   { return dial_add(a, b); }
int64_t ffi_bridge_scale(int64_t v, int64_t k) { return dial_scale(v, k); }

/* --- 2) Dial -> C：C 侧的布局读数 --- */

int64_t ffi_c_pkt_size(void)      { return (int64_t)sizeof(struct PktHdr); }
int64_t ffi_c_pkt_align(void)     { return (int64_t)_Alignof(struct PktHdr); }
int64_t ffi_c_pkt_off_flags(void) { return (int64_t)offsetof(struct PktHdr, flags); }
int64_t ffi_c_pkt_off_len(void)   { return (int64_t)offsetof(struct PktHdr, len); }

int64_t ffi_c_reg_size(void)      { return (int64_t)sizeof(struct RegHdr); }
int64_t ffi_c_reg_align(void)     { return (int64_t)_Alignof(struct RegHdr); }
int64_t ffi_c_reg_off_flags(void) { return (int64_t)offsetof(struct RegHdr, flags); }
int64_t ffi_c_reg_off_len(void)   { return (int64_t)offsetof(struct RegHdr, len); }
