#include "dial_callback.h"
#include "dial_rt_trap.h"

#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sched.h>

/*
 * dial_callback.c —— C → Dial 反向回调注册表（v1.21）
 *
 * 线程模型：
 *   - 全局表 s_table 由 s_mu 保护；条目含 thunk + 显式 state + 最近一次 result/done。
 *   - fire（无论同步还是平台线程）先持锁拷贝 thunk/state 到局部，解锁后再调用，
 *     这样回调体内若再次 register/free 其它条目不会与表操作死锁/失效。
 *   - 回调线程绝不触碰 Dial 的 ARC 对象：参数/结果是 int 位模式，state 是 rawptr。
 */

typedef struct {
    DialCallbackId id;
    DialCallbackFn thunk;
    void          *state;
    int64_t        result;
    int            done;
    int            in_use;
} CbEntry;

static pthread_mutex_t s_mu = PTHREAD_MUTEX_INITIALIZER;
static CbEntry *s_table = NULL;
static int64_t  s_cap = 0;
static DialCallbackId s_next_id = 1;   /* id 单调递增，绝不复用（防 ABA） */

static CbEntry *find_entry_locked(DialCallbackId id) {
    for (int64_t i = 0; i < s_cap; i++) {
        if (s_table[i].in_use && s_table[i].id == id) return &s_table[i];
    }
    return NULL;
}

static void ensure_capacity_locked(void) {
    if (s_cap == 0) {
        s_cap = 8;
        s_table = (CbEntry *)calloc((size_t)s_cap, sizeof(CbEntry));
        if (!s_table) dial_assert_fail("dial_callback: 注册表分配失败");
        return;
    }
    int all_used = 1;
    for (int64_t i = 0; i < s_cap; i++) if (!s_table[i].in_use) { all_used = 0; break; }
    if (all_used) {
        int64_t ncap = s_cap * 2;
        CbEntry *nt = (CbEntry *)calloc((size_t)ncap, sizeof(CbEntry));
        if (!nt) dial_assert_fail("dial_callback: 注册表扩容失败");
        memcpy(nt, s_table, (size_t)s_cap * sizeof(CbEntry));
        free(s_table);
        s_table = nt;
        s_cap = ncap;
    }
}

DialCallbackId dial_callback_register(DialCallbackFn thunk, void *state) {
    if (!thunk) dial_assert_fail("dial_callback_register: 空回调入口");
    pthread_mutex_lock(&s_mu);
    ensure_capacity_locked();
    DialCallbackId id = s_next_id++;
    CbEntry *slot = NULL;
    for (int64_t i = 0; i < s_cap; i++) {
        if (!s_table[i].in_use) { slot = &s_table[i]; break; }
    }
    if (!slot) { pthread_mutex_unlock(&s_mu); dial_assert_fail("dial_callback: 无空闲槽位"); }
    slot->id = id;
    slot->thunk = thunk;
    slot->state = state;
    slot->result = 0;
    slot->done = 0;
    slot->in_use = 1;
    pthread_mutex_unlock(&s_mu);
    return id;
}

static void *dial_callback_argcell(int64_t n) {
    if (n < 1) n = 1;
    void *p = calloc((size_t)n, sizeof(int64_t));
    if (!p) dial_assert_fail("dial_callback_argcell: 内存分配失败");
    return p;
}

/* 构造 argcell 并调用 thunk（thunk 写 slot[2]=result）。返回该结果。 */
static int64_t invoke_thunk(CbEntry *e, int64_t payload) {
    int64_t *cell = (int64_t *)dial_callback_argcell(3);
    cell[0] = payload;
    cell[1] = (int64_t)(uintptr_t)e->state;
    cell[2] = 0;
    e->thunk(cell);
    int64_t result = cell[2];
    free(cell);
    return result;
}

int64_t dial_callback_fire_sync(DialCallbackId id, int64_t payload) {
    CbEntry *e;
    pthread_mutex_lock(&s_mu);
    e = find_entry_locked(id);
    if (!e) { pthread_mutex_unlock(&s_mu); dial_assert_fail("fire 了已注销的 callback id"); }
    /* 拷贝 thunk/state 到本地快照，解锁后调用（回调体内可安全操作注册表）。 */
    CbEntry snap = *e;
    pthread_mutex_unlock(&s_mu);

    int64_t result = invoke_thunk(&snap, payload);
    pthread_mutex_lock(&s_mu);
    e = find_entry_locked(id);
    if (e) { e->result = result; e->done = 1; }
    pthread_mutex_unlock(&s_mu);
    return result;
}

/* 平台线程参数包：id + payload + delay。 */
typedef struct {
    DialCallbackId id;
    int64_t        payload;
    int64_t        delay_ms;
} ThreadFire;

static void *callback_thread_routine(void *p) {
    ThreadFire *tf = (ThreadFire *)p;
    DialCallbackId id = tf->id;
    int64_t payload = tf->payload;
    int64_t delay_ms = tf->delay_ms;
    free(tf);
    if (delay_ms > 0) usleep((useconds_t)(delay_ms * 1000));

    pthread_mutex_lock(&s_mu);
    CbEntry *e = find_entry_locked(id);
    if (!e) { pthread_mutex_unlock(&s_mu); return NULL; } /* 已注销，放弃回调 */
    CbEntry snap = *e;
    pthread_mutex_unlock(&s_mu);

    int64_t result = invoke_thunk(&snap, payload);

    pthread_mutex_lock(&s_mu);
    e = find_entry_locked(id);
    if (e) { e->result = result; e->done = 1; }
    pthread_mutex_unlock(&s_mu);
    return NULL;
}

void *dial_callback_thread_fire(DialCallbackId id, int64_t payload, int64_t delay_ms) {
    pthread_mutex_lock(&s_mu);
    if (!find_entry_locked(id)) {
        pthread_mutex_unlock(&s_mu);
        dial_assert_fail("callback_thread 了已注销的 callback id");
    }
    pthread_mutex_unlock(&s_mu);

    ThreadFire *tf = (ThreadFire *)malloc(sizeof(ThreadFire));
    if (!tf) dial_assert_fail("dial_callback_thread_fire: 参数包分配失败");
    tf->id = id; tf->payload = payload; tf->delay_ms = delay_ms;

    pthread_t *tid = (pthread_t *)malloc(sizeof(pthread_t));
    if (!tid) { free(tf); dial_assert_fail("dial_callback_thread_fire: 线程句柄分配失败"); }
    if (pthread_create(tid, NULL, callback_thread_routine, tf) != 0) {
        free(tf); free(tid);
        dial_assert_fail("dial_callback_thread_fire: pthread_create 失败");
    }
    return tid;   /* Dial 侧 join 后需要 free(tid)：由 callback_thread_join 负责 */
}

/* join 一个 callback_thread 句柄并释放（对齐 dial_thread_join）。 */
void dial_callback_join(void *tid) {
    if (!tid) return;
    pthread_join(*(pthread_t *)tid, NULL);
    free(tid);
}

int64_t dial_callback_result(DialCallbackId id) {
    pthread_mutex_lock(&s_mu);
    CbEntry *e = find_entry_locked(id);
    if (!e) { pthread_mutex_unlock(&s_mu); dial_assert_fail("result 了已注销的 callback id"); }
    int64_t r = e->result;
    pthread_mutex_unlock(&s_mu);
    return r;
}

int dial_callback_done(DialCallbackId id) {
    pthread_mutex_lock(&s_mu);
    CbEntry *e = find_entry_locked(id);
    if (!e) { pthread_mutex_unlock(&s_mu); dial_assert_fail("done 了已注销的 callback id"); }
    int d = e->done;
    pthread_mutex_unlock(&s_mu);
    return d;
}

void dial_callback_wait(DialCallbackId id) {
    /* 简单实现：done 是布尔且有内存屏障（mutex），短自旋 + sched_yield 即可。
     * 复杂场景用 cond；这里回调是短小标量函数，自旋开销可忽略。 */
    while (!dial_callback_done(id)) { sched_yield(); }
}

void dial_callback_free(DialCallbackId id) {
    pthread_mutex_lock(&s_mu);
    CbEntry *e = find_entry_locked(id);
    if (!e) { pthread_mutex_unlock(&s_mu); return; }
    e->in_use = 0;
    e->thunk = NULL;
    e->state = NULL;
    pthread_mutex_unlock(&s_mu);
}

int64_t dial_callback_count(void) {
    pthread_mutex_lock(&s_mu);
    int64_t n = 0;
    for (int64_t i = 0; i < s_cap; i++) if (s_table[i].in_use) n++;
    pthread_mutex_unlock(&s_mu);
    return n;
}
