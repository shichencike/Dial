#include "dial_thread.h"
#include "dial_rt_trap.h"

#include <pthread.h>
#include <stdlib.h>

/*
 * 线程句柄内部：只含 pthread_t。
 *
 * 关键点：Impl **不含**任何被子线程读取的字段，因此 join/detach 可以在
 * pthread_join/pthread_detach 返回后立即 free(impl)，无需与子线程做atomic 握手。
 */
typedef struct {
    pthread_t tid;
} DialThreadImpl;

/* pthread_create 的启动参数包（堆上分配，被子线程第一时间取走并释放）。 */
typedef struct {
    DialThreadFn fn;
    void        *argcell;
} DialThreadStart;

static void *dial_thread_start_routine(void *p) {
    DialThreadStart *sa = (DialThreadStart *)p;
    /* 先复制到局部，再释放参数包：此后 join/detach 侧的 free(impl) 与本线程再无交集。 */
    DialThreadFn fn     = sa->fn;
    void        *argcell = sa->argcell;
    free(sa);

    fn(argcell);

    /* argcell 的所有权在 spawn 时就已移交给本线程，用完即释放。 */
    free(argcell);
    return NULL;
}

void *dial_thread_argcell(int64_t n) {
    if (n < 1) n = 1;
    /* 每槽固定 8 字节：指针、i64、f64 都能直接 store；i1 由 IR 侧 zext 后存入。 */
    void *p = calloc((size_t)n, 8);
    if (!p) {
        dial_assert_fail("dial_thread_argcell: 内存分配失败");
    }
    return p;
}

DialThread dial_thread_new(DialThreadFn fn, void *argcell) {
    DIAL_CHECK_NULL(fn, "dial_thread_new: 线程入口函数为空");

    DialThreadImpl *t = (DialThreadImpl *)malloc(sizeof(DialThreadImpl));
    if (!t) return NULL;

    DialThreadStart *sa = (DialThreadStart *)malloc(sizeof(DialThreadStart));
    if (!sa) {
        free(t);
        return NULL;
    }
    sa->fn      = fn;
    sa->argcell = argcell;

    if (pthread_create(&t->tid, NULL, dial_thread_start_routine, sa) != 0) {
        free(sa);
        free(t);
        return NULL;
    }
    return (DialThread)t;
}

void dial_thread_join(DialThread t) {
    DIAL_HEAP_ASSERT(t != NULL, "join 了一个已回收的 thread 句柄（重复 join / join 后又 detach）");
    DialThreadImpl *impl = (DialThreadImpl *)t;
    pthread_join(impl->tid, NULL);
    free(impl);
}

void dial_thread_detach(DialThread t) {
    DIAL_HEAP_ASSERT(t != NULL, "detach 了一个已回收的 thread 句柄（重复 detach / detach 后又 join）");
    DialThreadImpl *impl = (DialThreadImpl *)t;
    pthread_detach(impl->tid);
    free(impl);
}
