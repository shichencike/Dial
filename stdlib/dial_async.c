#include "dial_async.h"
#include "dial_rt_trap.h"

#include <pthread.h>
#include <stdlib.h>
#include <string.h>

/*
 * dial_async.c —— Dial 协作式协程 / 有界线程池 / async 调度器（v1.21）
 *
 * 平台抽象：
 *   - 协程栈切换：POSIX 用 <ucontext.h>；Windows(MinGW) 用 Fiber API。
 *   - 线程池 / 同步：pthread（Windows 由 winpthreads 提供，与 dial_thread.c 同款）。
 *
 * 宪法一致性（语言层可见部分）：
 *   * 无闭包：入口一律是具名函数 + 显式 argcell（与 spawn 同款）。
 *   * 无用户可见全局可变状态：协程/线程池/任务全是不透明句柄，显式管理。
 *   * 无用户可见原子类型：内部同步只用 pthread mutex + cond（C 私有）。
 * 模块内的 TLS 调度锚与 async 任务表属于"调度器私有实现"，不暴露给 Dial。
 *
 * 调度模型（协作式、单线程 executor）：
 *   resume 严格串行——调度器 resume 一个协程，协程在 await 处 yield 切回
 *   同一个（每线程）调度上下文；调度器再 resume 下一个。所有协程共享一个
 *   scheduler 锚点，无需各自保存 back 上下文。
 */

#define CO_READY     0
#define CO_RUNNING   1
#define CO_SUSPENDED 2
#define CO_DONE      3

#define TASK_POOL    0
#define TASK_ASYNC   1

#define DEFAULT_STACK_BYTES (256 * 1024)

/* __thread：POSIX 与 MinGW-w64 的 gcc 均支持，作为每线程调度锚/当前协程。 */
#if defined(_WIN32) || defined(__MINGW32__)
#define DIAL_TLS __thread
#else
#define DIAL_TLS __thread
#endif

/* ========================================================================= */
/* 任务（pool / async 通用）                                                  */
/* ========================================================================= */

typedef struct DialTaskImpl {
    void             *argcell;   /* 拥有的参数单元（slot[0]=result），由 task_free 释放 */
    struct DialCoImpl *co;       /* async 任务的协程；pool 任务为 NULL */
    int               kind;      /* TASK_POOL / TASK_ASYNC */
    int               done;
    pthread_mutex_t   mu;
    pthread_cond_t    cv;
    struct DialTaskImpl *next;   /* async executor 活跃链表 */
} DialTaskImpl;

/* ========================================================================= */
/* 有栈协程：平台相关实现                                                     */
/* ========================================================================= */

typedef struct DialCoImpl {
    DialCoEntry fn;
    void       *argcell;
    int         state;
    int         started;
    int64_t     stack_bytes;
    void       *stack;       /* POSIX: 自建栈；Windows: NULL（CreateFiber 自管） */
    void       *ctx;         /* POSIX: ucontext_t* */
#if defined(_WIN32) || defined(__MINGW32__)
    void       *fiber;       /* HANDLE */
#endif
} DialCoImpl;

#if defined(_WIN32) || defined(__MINGW32__)
/* ---------------------------- Windows: Fiber ---------------------------- */
#include <windows.h>

static DIAL_TLS void *tls_main_fiber = NULL;   /* 该线程的主 fiber（调度锚） */
static DIAL_TLS DialCoImpl *tls_current_co = NULL;

static void ensure_main_fiber(void) {
    if (tls_main_fiber == NULL) {
        tls_main_fiber = ConvertThreadToFiber(NULL);
        if (!tls_main_fiber) dial_assert_fail("dial_async: ConvertThreadToFiber 失败");
    }
}

static void WINAPI fiber_entry(void *param) {
    DialCoImpl *c = (DialCoImpl *)param;
    tls_current_co = c;
    c->fn(c->argcell);
    c->state = CO_DONE;
    tls_current_co = NULL;
    /* 显式切回调度锚；不再返回，避免 fiber 终止时的二次切换。 */
    SwitchToFiber(tls_main_fiber);
}

DialCoroutine dial_co_new(DialCoEntry fn, void *argcell, int64_t stack_bytes) {
    DIAL_CHECK_NULL(fn, "dial_co_new: 协程入口为空");
    ensure_main_fiber();
    DialCoImpl *c = (DialCoImpl *)calloc(1, sizeof(DialCoImpl));
    if (!c) dial_assert_fail("dial_co_new: 内存分配失败");
    c->fn = fn;
    c->argcell = argcell;
    c->state = CO_READY;
    c->started = 0;
    c->stack_bytes = stack_bytes > 0 ? stack_bytes : DEFAULT_STACK_BYTES;
    c->fiber = CreateFiber((SIZE_T)c->stack_bytes, fiber_entry, c);
    if (!c->fiber) {
        free(c);
        dial_assert_fail("dial_co_new: CreateFiber 失败");
    }
    return (DialCoroutine)c;
}

int dial_co_resume(DialCoroutine h) {
    DialCoImpl *c = (DialCoImpl *)h;
    DIAL_HEAP_ASSERT(c != NULL, "resume 了空协程句柄");
    DIAL_HEAP_ASSERT(c->state != CO_DONE, "resume 了已结束的协程");
    DIAL_HEAP_ASSERT(c->state != CO_RUNNING, "重入 resume 同一个协程");
    c->started = 1;
    c->state = CO_RUNNING;
    tls_current_co = c;
    SwitchToFiber(c->fiber);
    return c->state == CO_DONE;
}

void dial_co_yield(void) {
    DialCoImpl *c = tls_current_co;
    if (!c) dial_assert_fail("yield 只能在协程（async fn）内调用");
    c->state = CO_SUSPENDED;
    SwitchToFiber(tls_main_fiber);
}

int dial_co_status(DialCoroutine h) {
    DialCoImpl *c = (DialCoImpl *)h;
    DIAL_HEAP_ASSERT(c != NULL, "status 了空协程句柄");
    return c->state;
}

void dial_co_free(DialCoroutine h) {
    DialCoImpl *c = (DialCoImpl *)h;
    if (!c) return;
    DIAL_HEAP_ASSERT(c->state == CO_DONE || c->state == CO_READY,
                     "释放了仍在运行/挂起的协程");
    if (c->fiber) DeleteFiber(c->fiber);
    free(c);
}

#else /* ---------------------------- POSIX: ucontext ---------------------------- */
#include <ucontext.h>

/* 共享的调度器上下文锚点 + 当前运行协程（均每线程）。 */
static DIAL_TLS ucontext_t   tls_scheduler_ctx;
static DIAL_TLS DialCoImpl  *tls_current_co = NULL;

static void co_trampoline(void) {
    /* makecontext 不传指针参数；当前协程从 TLS 取。 */
    DialCoImpl *c = tls_current_co;
    c->fn(c->argcell);
    c->state = CO_DONE;
    tls_current_co = NULL;
    /* 返回后由 uc_link（= tls_scheduler_ctx）自动切回调度器。 */
}

DialCoroutine dial_co_new(DialCoEntry fn, void *argcell, int64_t stack_bytes) {
    DIAL_CHECK_NULL(fn, "dial_co_new: 协程入口为空");
    DialCoImpl *c = (DialCoImpl *)calloc(1, sizeof(DialCoImpl));
    if (!c) dial_assert_fail("dial_co_new: 内存分配失败");
    c->fn = fn;
    c->argcell = argcell;
    c->state = CO_READY;
    c->started = 0;
    c->stack_bytes = stack_bytes > 0 ? stack_bytes : DEFAULT_STACK_BYTES;
    c->stack = malloc((size_t)c->stack_bytes);
    if (!c->stack) {
        free(c);
        dial_assert_fail("dial_co_new: 协程栈分配失败");
    }
    return (DialCoroutine)c;
}

int dial_co_resume(DialCoroutine h) {
    DialCoImpl *c = (DialCoImpl *)h;
    DIAL_HEAP_ASSERT(c != NULL, "resume 了空协程句柄");
    DIAL_HEAP_ASSERT(c->state != CO_DONE, "resume 了已结束的协程");
    DIAL_HEAP_ASSERT(c->state != CO_RUNNING, "重入 resume 同一个协程");
    if (!c->started) {
        ucontext_t *uc = (ucontext_t *)malloc(sizeof(ucontext_t));
        if (!uc) dial_assert_fail("dial_co_resume: 上下文分配失败");
        if (getcontext(uc) != 0) dial_assert_fail("dial_co_resume: getcontext 失败");
        uc->uc_stack.ss_sp = c->stack;
        uc->uc_stack.ss_size = (size_t)c->stack_bytes;
        uc->uc_link = &tls_scheduler_ctx;   /* 协程函数 return 后切回调度器 */
        makecontext(uc, co_trampoline, 0);
        c->ctx = uc;
        c->started = 1;
    }
    c->state = CO_RUNNING;
    tls_current_co = c;
    /* 保存调度器当前点到 tls_scheduler_ctx，切入协程。 */
    if (swapcontext(&tls_scheduler_ctx, (ucontext_t *)c->ctx) != 0) {
        dial_assert_fail("dial_co_resume: swapcontext 失败");
    }
    return c->state == CO_DONE;
}

void dial_co_yield(void) {
    DialCoImpl *c = tls_current_co;
    if (!c) dial_assert_fail("yield 只能在协程（async fn）内调用");
    c->state = CO_SUSPENDED;
    /* 保存协程当前点，切回调度器锚点。 */
    if (swapcontext((ucontext_t *)c->ctx, &tls_scheduler_ctx) != 0) {
        dial_assert_fail("dial_co_yield: swapcontext 失败");
    }
    /* 从 yield 恢复：state 已被 resume 重设为 RUNNING。 */
}

int dial_co_status(DialCoroutine h) {
    DialCoImpl *c = (DialCoImpl *)h;
    DIAL_HEAP_ASSERT(c != NULL, "status 了空协程句柄");
    return c->state;
}

void dial_co_free(DialCoroutine h) {
    DialCoImpl *c = (DialCoImpl *)h;
    if (!c) return;
    DIAL_HEAP_ASSERT(c->state == CO_DONE || c->state == CO_READY,
                     "释放了仍在运行/挂起的协程");
    if (c->ctx) free(c->ctx);
    free(c->stack);
    free(c);
}
#endif /* platform */

/* ========================================================================= */
/* 任务公共操作                                                              */
/* ========================================================================= */

static DialTaskImpl *task_new(void *argcell, DialCoImpl *co, int kind) {
    DialTaskImpl *t = (DialTaskImpl *)calloc(1, sizeof(DialTaskImpl));
    if (!t) dial_assert_fail("dial_async: task 分配失败");
    t->argcell = argcell;
    t->co = co;
    t->kind = kind;
    t->done = 0;
    pthread_mutex_init(&t->mu, NULL);
    pthread_cond_init(&t->cv, NULL);
    return t;
}

static void task_mark_done(DialTaskImpl *t) {
    pthread_mutex_lock(&t->mu);
    t->done = 1;
    pthread_cond_broadcast(&t->cv);
    pthread_mutex_unlock(&t->mu);
}

int dial_task_done(DialTask task) {
    DialTaskImpl *t = (DialTaskImpl *)task;
    DIAL_HEAP_ASSERT(t != NULL, "task_done 了空任务句柄");
    pthread_mutex_lock(&t->mu);
    int d = t->done;
    pthread_mutex_unlock(&t->mu);
    return d;
}

void dial_task_wait(DialTask task) {
    DialTaskImpl *t = (DialTaskImpl *)task;
    DIAL_HEAP_ASSERT(t != NULL, "task_wait 了空任务句柄");
    pthread_mutex_lock(&t->mu);
    while (!t->done) pthread_cond_wait(&t->cv, &t->mu);
    pthread_mutex_unlock(&t->mu);
}

uint64_t dial_task_result(DialTask task) {
    DialTaskImpl *t = (DialTaskImpl *)task;
    DIAL_HEAP_ASSERT(t != NULL, "task_result 了空任务句柄");
    DIAL_HEAP_ASSERT(t->done, "task_result 在任务完成前读取");
    /* argcell 布局：slot[0] = 8 字节结果（thunk 写入）。 */
    return ((uint64_t *)t->argcell)[0];
}

void dial_task_free(DialTask task) {
    DialTaskImpl *t = (DialTaskImpl *)task;
    if (!t) return;
    DIAL_HEAP_ASSERT(t->done, "task_free 了未完成的句柄");
    if (t->co) dial_co_free((DialCoroutine)t->co);
    free(t->argcell);
    pthread_cond_destroy(&t->cv);
    pthread_mutex_destroy(&t->mu);
    free(t);
}

/* ========================================================================= */
/* 有界线程池                                                                */
/* ========================================================================= */

typedef struct PoolJob {
    DialCoEntry        fn;
    void              *argcell;
    DialTaskImpl      *task;
    struct PoolJob    *next;
} PoolJob;

typedef struct {
    pthread_mutex_t mu;
    pthread_cond_t  cv;        /* 有新任务 */
    pthread_cond_t  cv_done;   /* pending 归零 */
    PoolJob        *head, *tail;
    int              nworkers;
    int              pending;
    int              closing;
    pthread_t       *workers;
} DialPoolImpl;

static void *pool_worker(void *p) {
    DialPoolImpl *pool = (DialPoolImpl *)p;
    for (;;) {
        pthread_mutex_lock(&pool->mu);
        while (!pool->head && !pool->closing) {
            pthread_cond_wait(&pool->cv, &pool->mu);
        }
        if (!pool->head && pool->closing) {
            pthread_mutex_unlock(&pool->mu);
            break;
        }
        PoolJob *job = pool->head;
        pool->head = job->next;
        if (!pool->head) pool->tail = NULL;
        pthread_mutex_unlock(&pool->mu);

        /* 执行具名入口：thunk 从 argcell 取参、调用目标、把返回值写 slot[0]。 */
        job->fn(job->argcell);
        task_mark_done(job->task);
        free(job);

        pthread_mutex_lock(&pool->mu);
        pool->pending--;
        if (pool->pending == 0) pthread_cond_broadcast(&pool->cv_done);
        pthread_mutex_unlock(&pool->mu);
    }
    return NULL;
}

DialPool dial_pool_new(int64_t nworkers) {
    if (nworkers < 1) nworkers = 1;
    if (nworkers > 1024) nworkers = 1024;   /* 防御性上限 */
    DialPoolImpl *pool = (DialPoolImpl *)calloc(1, sizeof(DialPoolImpl));
    if (!pool) dial_assert_fail("dial_pool_new: 内存分配失败");
    pthread_mutex_init(&pool->mu, NULL);
    pthread_cond_init(&pool->cv, NULL);
    pthread_cond_init(&pool->cv_done, NULL);
    pool->nworkers = (int)nworkers;
    pool->workers = (pthread_t *)calloc((size_t)nworkers, sizeof(pthread_t));
    if (!pool->workers) dial_assert_fail("dial_pool_new: workers 分配失败");
    for (int i = 0; i < pool->nworkers; i++) {
        if (pthread_create(&pool->workers[i], NULL, pool_worker, pool) != 0) {
            dial_assert_fail("dial_pool_new: pthread_create 失败");
        }
    }
    return (DialPool)pool;
}

DialTask dial_pool_submit(DialPool pool_h, DialCoEntry fn, void *argcell) {
    DialPoolImpl *pool = (DialPoolImpl *)pool_h;
    DIAL_CHECK_NULL(pool, "dial_pool_submit: 空线程池句柄");
    DIAL_CHECK_NULL(fn, "dial_pool_submit: 空任务入口");
    PoolJob *job = (PoolJob *)calloc(1, sizeof(PoolJob));
    if (!job) dial_assert_fail("dial_pool_submit: job 分配失败");
    DialTaskImpl *task = task_new(argcell, NULL, TASK_POOL);
    job->fn = fn;
    job->argcell = argcell;
    job->task = task;

    pthread_mutex_lock(&pool->mu);
    DIAL_HEAP_ASSERT(!pool->closing, "向已关闭的线程池提交任务");
    job->next = NULL;
    if (pool->tail) pool->tail->next = job; else pool->head = job;
    pool->tail = job;
    pool->pending++;
    pthread_cond_signal(&pool->cv);
    pthread_mutex_unlock(&pool->mu);
    return (DialTask)task;
}

void dial_pool_join(DialPool pool_h) {
    DialPoolImpl *pool = (DialPoolImpl *)pool_h;
    DIAL_CHECK_NULL(pool, "dial_pool_join: 空线程池句柄");
    pthread_mutex_lock(&pool->mu);
    while (pool->pending > 0) pthread_cond_wait(&pool->cv_done, &pool->mu);
    pthread_mutex_unlock(&pool->mu);
}

void dial_pool_close(DialPool pool_h) {
    DialPoolImpl *pool = (DialPoolImpl *)pool_h;
    DIAL_CHECK_NULL(pool, "dial_pool_close: 空线程池句柄");
    dial_pool_join(pool_h);
    pthread_mutex_lock(&pool->mu);
    pool->closing = 1;
    pthread_cond_broadcast(&pool->cv);
    pthread_mutex_unlock(&pool->mu);
    for (int i = 0; i < pool->nworkers; i++) pthread_join(pool->workers[i], NULL);
    free(pool->workers);
    pthread_cond_destroy(&pool->cv_done);
    pthread_cond_destroy(&pool->cv);
    pthread_mutex_destroy(&pool->mu);
    free(pool);
}

/* ========================================================================= */
/* async executor（单线程协作调度）                                          */
/* ========================================================================= */

/* 调度器私有的活跃 async 任务链表（每线程，不暴露给 Dial）。 */
static DIAL_TLS DialTaskImpl *async_head = NULL;

DialTask dial_async_spawn(DialCoEntry fn, void *argcell) {
    DIAL_CHECK_NULL(fn, "dial_async_spawn: 空任务入口");
    DialCoroutine co = dial_co_new(fn, argcell, 0);
    DialTaskImpl *task = task_new(argcell, (DialCoImpl *)co, TASK_ASYNC);
    /* 挂入当前线程的活跃表尾。 */
    task->next = NULL;
    if (!async_head) {
        async_head = task;
    } else {
        DialTaskImpl *p = async_head;
        while (p->next) p = p->next;
        p->next = task;
    }
    return (DialTask)task;
}

void dial_async_await(DialTask task_h) {
    DialTaskImpl *t = (DialTaskImpl *)task_h;
    DIAL_HEAP_ASSERT(t != NULL, "await 了空任务句柄");
    /* 协作式等待：未完成就让出当前协程，由 run_all 推进其它任务。 */
    while (!dial_task_done(t)) {
        dial_co_yield();
    }
}

void dial_run_all(void) {
    /* round-robin：反复遍历活跃任务，resume 所有未完成的 async 协程，
     * 只要有任务从 yield 恢复并推进即继续，直到全部 done。 */
    for (;;) {
        int any_pending = 0;
        DialTaskImpl *p = async_head;
        while (p) {
            if (!p->done) {
                any_pending = 1;
                int finished = dial_co_resume((DialCoroutine)p->co);
                if (finished) task_mark_done(p);
            }
            p = p->next;
        }
        if (!any_pending) break;
    }
}
