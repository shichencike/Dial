#include "dial_rt_mem.h"
#include <string.h>

void *dial_rc_alloc(int64_t payload_size) {
    char *blk = (char *)malloc((size_t)(8 + payload_size));
    if (!blk) abort();
    *(int64_t *)blk = 1;       /* refcount = 1 */
    return blk + 8;            /* 不透明指针指向 payload */
}

void dial_rc_retain(void *p) {
    if (p) ((int64_t *)p)[-1] += 1;
}

void dial_rc_release(void *p) {
    if (!p) return;
    if (--((int64_t *)p)[-1] == 0) free((char *)p - 8);
}

void *dial_rc_clone(void *p, int64_t payload_size) {
    if (!p) return NULL;
    if (((int64_t *)p)[-1] == 1) return p;   /* 唯一持有 -> 无需拷贝（COW 命中） */
    void *n = dial_rc_alloc(payload_size);
    memcpy(n, p, (size_t)payload_size);
    return n;
}
