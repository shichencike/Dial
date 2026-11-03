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
    // 与 retain/release 对齐：传入已是 freed 的指针（refcount <= 0）时立刻终止，
    // 而非去读 ((int64_t*)p)[-1] 这块已归还给分配器的内存（那是 use-after-free）。
    DIAL_HEAP_ASSERT(((int64_t *)p)[-1] > 0, "clone 了已释放的对象（use-after-free）");
    // 纯粹的 COW 克隆：
    //   - 唯一持有者（refcount==1）：直接返回同一块内存，**不改动引用计数**；
    //   - 多个持有者（refcount>1）：深拷贝出一份独立副本（引用计数为 1）。
    //
    // 这里刻意**不**再 +1：克隆是「借用者的只读视图」，本身不产生新的所有权。
    // 真正需要「一份独立副本」的调用方（各类型的 clone() 方法）在自己的 C 实现里
    // 用「先 retain 再 clone 再 release」强制走进深拷贝分支——那样既拿到独立副本，
    // 又不会像过去那样给原对象凭空 +1（那会让 COW 守卫 refcount>1 恒真）。
    if (((int64_t *)p)[-1] == 1) {
        return p;
    }
    void *n = dial_rc_alloc(payload_size);
    memcpy(n, p, (size_t)payload_size);
    return n;
}
