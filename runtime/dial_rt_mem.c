#include "dial_rt_mem.h"
#include "dial_rt_trap.h"
#include <string.h>

void *dial_rc_alloc(int64_t payload_size) {
    char *blk = (char *)malloc((size_t)(8 + payload_size));
    /* 分配失败绝不返回 NULL 让上层“带病运行”——那是静默损坏的开端。 */
    if (!blk) dial_assert_fail("内存分配失败（OOM）");
    *(int64_t *)blk = 1;       /* refcount = 1 */
    return blk + 8;            /* 不透明指针指向 payload */
}

void dial_rc_retain(void *p) {
    if (!p) return;
    /*
     * refcount <= 0 说明这块内存已经被释放过了（use-after-free）。
     * C 会让它“看起来还能用”，Dial 在这里立刻炸。
     */
    DIAL_HEAP_ASSERT(((int64_t *)p)[-1] > 0, "retain 了已释放的对象（use-after-free）");
    ((int64_t *)p)[-1] += 1;
}

void dial_rc_release(void *p) {
    if (!p) return;
    /* 多释放一次（double free / 引用计数下溢）同样当场终止。 */
    DIAL_HEAP_ASSERT(((int64_t *)p)[-1] > 0, "release 了引用计数已归零的对象（double free）");
    if (--((int64_t *)p)[-1] == 0) free((char *)p - 8);
}

void *dial_rc_clone(void *p, int64_t payload_size) {
    if (!p) return NULL;
    if (((int64_t *)p)[-1] == 1) return p;   /* 唯一持有 -> 无需拷贝（COW 命中） */
    void *n = dial_rc_alloc(payload_size);
    memcpy(n, p, (size_t)payload_size);
    return n;
}
