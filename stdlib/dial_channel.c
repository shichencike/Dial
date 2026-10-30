#include "dial_channel.h"
#include "dial_rt_trap.h"

#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <time.h>

/*
 * ============================================================================
 *  全局事件机制（v0.91）：select 多路复用的等待原语
 * ============================================================================
 *
 *  任何通道的状态变化都递增 g_epoch 并在全局条件变量上广播；
 *  select 侧「先读 epoch -> 再查就绪 -> 再等 epoch 变化」，因此不会丢唤醒。
 *
 *  锁纪律（死锁的唯一来源，必须严守）：
 *    - notify() 一律在**释放通道本地锁之后**调用；
 *    - pick_ready() 逐个加锁检查、逐个解锁，从不同时持有两把锁；
 *    - 于是「持通道锁 -> 取全局锁」与「持全局锁 -> 取通道锁」两种嵌套
 *      **都不存在**，全局锁与通道锁永不死锁。
 */
static pthread_mutex_t g_ev_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t  g_ev_cond  = PTHREAD_COND_INITIALIZER;
static _Atomic int64_t g_epoch    = 0;

int64_t dial_chan_epoch(void) {
    return atomic_load(&g_epoch);
}

/*
 * 通知：递增 epoch 并广播。
 * 必须在**不持有任何通道本地锁**时调用（见上方锁纪律）。
 */
static void dial_chan_notify(void) {
    pthread_mutex_lock(&g_ev_mutex);
    atomic_fetch_add(&g_epoch, 1);
    pthread_cond_broadcast(&g_ev_cond);
    pthread_mutex_unlock(&g_ev_mutex);
}

void dial_chan_wait(int64_t since) {
    pthread_mutex_lock(&g_ev_mutex);
    /* 用 while 而非 if：广播会唤醒所有等待者（惊群），每个都必须重新判定。 */
    while (atomic_load(&g_epoch) == since) {
        pthread_cond_wait(&g_ev_cond, &g_ev_mutex);
    }
    pthread_mutex_unlock(&g_ev_mutex);
}

/*
 * 在就绪通道中挑一个。
 *
 * 随机起点是**防饿死**的关键：若多个通道同时有数据而总是固定挑第一个，
 * 第二个通道的数据会被一直压着。起点用原子计数器取模——不能用 rand()，
 * 它不是线程安全的。
 */
int64_t dial_channel_pick_ready(DialChannel *chs, int64_t n);   /* 定义见下方 */

typedef struct {
    _Atomic int64_t  rc;

    pthread_mutex_t  m;
    pthread_cond_t   not_empty;   /* 队列由空变非空 */
    pthread_cond_t   not_full;    /* 队列由满变不满 */

    void           **buf;         /* 环形队列，只存指针（所有权） */
    int64_t          cap;
    int64_t          head;
    int64_t          n;

    /*
     * 关闭标志。仅在互斥量保护下读写（不设 atomic）——因为 closed 的判定必须与
     * n 的判定是**同一个原子瞬间**完成的，否则会漏判「刚关闭、还剩两个元素」。
     * 用 atomic 反而会让 done() 的两半读到的不是同一时刻的状态。
     */
    int              closed;
} DialChannelImpl;

/*
 * 超时用 CLOCK_REALTIME（pthread_cond_timedwait 的默认基准）。
 *
 * 曾考虑改用 CLOCK_MONOTONIC 以免系统时间被 NTP 调整时影响等待，但
 * pthread_condattr_setclock 在 macOS 上并不存在——为一个「极端情况下超时点
 * 偏移」的收益牺牲跨平台可移植性，不划算。
 *
 * 退而求其次的代价是可控的：系统时间前跳会让等待提前结束（多返回一次 NULL），
 * 后跳会让等待被延长。但**正确性不受影响**——超时判定后 while 循环会重新检查
 * 「队列是否非空」，绝不会因此吞掉元素或把元素重复交给两个消费者。
 */
static void dial_deadline_now(struct timespec *ts, int64_t timeout_ms) {
    clock_gettime(CLOCK_REALTIME, ts);
    ts->tv_sec  += timeout_ms / 1000;
    ts->tv_nsec += (timeout_ms % 1000) * 1000000L;
    if (ts->tv_nsec >= 1000000000L) {
        ts->tv_sec  += 1;
        ts->tv_nsec -= 1000000000L;
    }
}

/*
 * 在就绪通道中挑一个（v0.91 的 select 用它决定执行哪个 case）。
 *
 * 随机起点是**防饿死**的关键：若多个通道同时有数据而总是固定挑第一个，
 * 第二个通道的数据会被一直压着。起点用原子计数器取模——不能用 rand()，
 * 它不是线程安全的。
 *
 * 逐个加锁、逐个解锁，**从不同时持有两把锁**，因此与全局事件锁无嵌套、无死锁。
 */
int64_t dial_channel_pick_ready(DialChannel *chs, int64_t n) {
    if (!chs || n <= 0) return -2;          /* 没有任何 case：等价于全体 EOF */

    static _Atomic int64_t rr = 0;
    int64_t start = (int64_t)(atomic_fetch_add(&rr, 1) % n);
    if (start < 0) start += n;

    int64_t all_eof = 1;
    for (int64_t k = 0; k < n; k++) {
        int64_t i = (start + k) % n;
        DialChannelImpl *c = (DialChannelImpl *)chs[i];
        if (!c) {
            /* 空句柄：视为已结束（正常代码走不到这里，Sema 已保证类型正确） */
            DIAL_HEAP_ASSERT(0, "select 收到了空的 channel 句柄");
            continue;
        }
        pthread_mutex_lock(&c->m);
        int64_t items  = c->n;
        int64_t closed = c->closed;
        pthread_mutex_unlock(&c->m);

        if (items > 0) return i;             /* 就绪：可立即取走 */
        if (!closed) all_eof = 0;             /* 还没关闭 -> 不是 EOF */
    }
    return all_eof ? -2 : -1;
}

/*
 * ============================================================================
 *  选择器对象（v0.91）：select 的「挑一个就绪通道并取走元素」
 * ============================================================================
 */
struct DialSelector {
    DialChannel *chs;
    int64_t      n;
    int64_t      cap;
};

DialSelector *dial_sel_new(void) {
    DialSelector *s = (DialSelector *)calloc(1, sizeof(DialSelector));
    if (!s) return NULL;
    s->cap = 4;
    s->chs = (DialChannel *)calloc((size_t)s->cap, sizeof(DialChannel));
    if (!s->chs) {
        free(s);
        return NULL;
    }
    return s;
}

void dial_sel_add(DialSelector *s, DialChannel ch) {
    if (!s) return;
    if (s->n == s->cap) {
        int64_t cap = s->cap * 2;
        DialChannel *p = (DialChannel *)realloc(s->chs, (size_t)cap * sizeof(DialChannel));
        if (!p) return;              /* 分配失败：保持原状（少监听一个通道） */
        s->chs = p;
        s->cap = cap;
    }
    s->chs[s->n++] = ch;
}

int64_t dial_sel_pick(DialSelector *s) {
    if (!s) return -2;
    return dial_channel_pick_ready(s->chs, s->n);
}

/*
 * 从第 idx 个通道取走一个元素。
 *
 * 用的是**非阻塞**取：select 已经在 pick 阶段确认该通道「有数据」，
 * 但从那到此刻之间，别的消费者可能先一步把它取走了——此时返回 NULL，
 * 调用方应当**重新挑一个**（而不是当成 EOF）。
 *
 * 注意：这里不做 retain/release——所有权直接转移给接收方，
 * 这与 dial_channel_recv 的所有权规则完全一致。
 */
void *dial_sel_recv(DialSelector *s, int64_t idx) {
    if (!s || idx < 0 || idx >= s->n) return NULL;
    return dial_channel_try_recv(s->chs[idx]);
}

void dial_sel_free(DialSelector *s) {
    if (!s) return;
    free(s->chs);
    free(s);
}

DialChannel dial_channel_new(int64_t cap) {
    if (cap < 1) cap = 1;

    DialChannelImpl *c = (DialChannelImpl *)calloc(1, sizeof(DialChannelImpl));
    if (!c) return NULL;
    c->buf = (void **)calloc((size_t)cap, sizeof(void *));
    if (!c->buf) {
        free(c);
        return NULL;
    }
    atomic_init(&c->rc, 1);
    pthread_mutex_init(&c->m, NULL);
    pthread_cond_init(&c->not_empty, NULL);
    pthread_cond_init(&c->not_full, NULL);
    c->cap  = cap;    c->head = 0;
    c->n    = 0;
    return (DialChannel)c;
}

void dial_channel_send(DialChannel ch, void *p) {
    DIAL_HEAP_ASSERT(ch != NULL, "往已释放的 channel 发送数据");
    DialChannelImpl *c = (DialChannelImpl *)ch;

    pthread_mutex_lock(&c->m);
    if (c->closed) {
        pthread_mutex_unlock(&c->m);
        DIAL_HEAP_ASSERT(0, "往已关闭的 channel 发送数据（close 之后禁止再 send）");
    }
    while (c->n == c->cap) {
        pthread_cond_wait(&c->not_full, &c->m);
    }
    c->buf[(c->head + c->n) % c->cap] = p;
    c->n++;
    /*
     * 广播而非 signal：唤醒的是「队列非空」这一**共享状态**的等待者。
     * signal 只唤醒一个，若队列里还剩元素，其余消费者会带着已经失效的
     * 等待条件继续睡死——经典的丢唤醒，多消费者下表现为某个消费者永久阻塞。
     */
    pthread_cond_broadcast(&c->not_empty);
    int64_t left = c->n;
    pthread_mutex_unlock(&c->m);

    /* 全局事件通知（放锁之后调用，锁纪律见文件头）。 */
    if (left > 0) dial_chan_notify();
}

void *dial_channel_recv(DialChannel ch) {
    DIAL_HEAP_ASSERT(ch != NULL, "从已释放的 channel 接收数据");
    DialChannelImpl *c = (DialChannelImpl *)ch;

    pthread_mutex_lock(&c->m);
    /* 等待条件从「队列为空」改为「队列为空 且 尚未关闭」——
     * 一旦关闭且取空，就带着 NULL 返回，而不是继续等一个永远不会来的元素。 */
    while (c->n == 0 && !c->closed) {
        pthread_cond_wait(&c->not_empty, &c->m);
    }
    void *p = NULL;
    if (c->n > 0) {
        p = c->buf[c->head];
        c->head = (c->head + 1) % c->cap;
        c->n--;
        /* 同上：腾出位置后广播，让所有等着「有空位」的发送者重新判定。 */
        pthread_cond_broadcast(&c->not_full);
    }
    int64_t left = c->n;
    pthread_mutex_unlock(&c->m);

    /* 队列里还有数据 -> 别的 select 等待者可能正在等这个通道，必须唤醒。 */
    if (left > 0) dial_chan_notify();

    /* 已关闭且已空 -> NULL（EOF） */
    return p;
}

void *dial_channel_recv_timeout(DialChannel ch, int64_t timeout_ms) {
    DIAL_HEAP_ASSERT(ch != NULL, "从已释放的 channel 接收数据");
    DialChannelImpl *c = (DialChannelImpl *)ch;

    if (timeout_ms < 0) timeout_ms = 0;

    /* 绝对超时时刻（与条件变量同一时钟基准）。 */
    struct timespec ts;
    dial_deadline_now(&ts, timeout_ms);

    pthread_mutex_lock(&c->m);
    /*
     * 等待条件同样是「队列为空 且 尚未关闭」。
     *
     * 超时与到达同时发生时，以「有元素」为准：pthread_cond_timedwait 返回
     * ETIMEDOUT 后会回到 while 条件重新判定，若此时 n > 0 条件不再成立，
     * 于是正常取走元素——绝不会吞掉一个刚到的元素。
     */
    while (c->n == 0 && !c->closed) {
        int rc = pthread_cond_timedwait(&c->not_empty, &c->m, &ts);
        if (rc == ETIMEDOUT) {
            /* 再看一眼：元素可能恰好在超时这一刻到达。 */
            if (c->n == 0) break;
        }
    }
    void *p = NULL;
    if (c->n > 0) {
        p = c->buf[c->head];
        c->head = (c->head + 1) % c->cap;
        c->n--;
        pthread_cond_broadcast(&c->not_full);
    }
    int64_t left = c->n;
    pthread_mutex_unlock(&c->m);

    if (left > 0) dial_chan_notify();

    /* 超时或已关闭且已空 -> NULL */
    return p;
}

void *dial_channel_try_recv(DialChannel ch) {
    DIAL_HEAP_ASSERT(ch != NULL, "从已释放的 channel 接收数据");
    DialChannelImpl *c = (DialChannelImpl *)ch;

    pthread_mutex_lock(&c->m);
    void *p = NULL;
    if (c->n > 0) {
        p = c->buf[c->head];
        c->head = (c->head + 1) % c->cap;
        c->n--;
        pthread_cond_broadcast(&c->not_full);
    }
    int64_t left = c->n;
    pthread_mutex_unlock(&c->m);

    if (left > 0) dial_chan_notify();

    /* 队列空（无论是否已关闭）-> NULL */
    return p;
}

void dial_channel_close(DialChannel ch) {
    DIAL_HEAP_ASSERT(ch != NULL, "关闭已释放的 channel");
    DialChannelImpl *c = (DialChannelImpl *)ch;

    pthread_mutex_lock(&c->m);
    c->closed = 1;
    /* 必须 broadcast 而非 signal：关闭的语义是「唤醒所有等待者」，
     * 只唤醒一个会让其余线程永远睡着（它们等的是一个不会再发生的事件）。 */
    pthread_cond_broadcast(&c->not_empty);
    pthread_cond_broadcast(&c->not_full);
    pthread_mutex_unlock(&c->m);

    /* 关闭改变了 EOF 判定——正等着「全体 EOF」的 select 必须被唤醒。 */
    dial_chan_notify();
}

int64_t dial_channel_closed(DialChannel ch) {
    DIAL_HEAP_ASSERT(ch != NULL, "查询已释放的 channel");
    DialChannelImpl *c = (DialChannelImpl *)ch;
    pthread_mutex_lock(&c->m);
    int64_t r = c->closed ? 1 : 0;
    pthread_mutex_unlock(&c->m);
    return r;
}

int64_t dial_channel_len(DialChannel ch) {
    DIAL_HEAP_ASSERT(ch != NULL, "查询已释放的 channel");
    DialChannelImpl *c = (DialChannelImpl *)ch;
    pthread_mutex_lock(&c->m);
    int64_t r = c->n;
    pthread_mutex_unlock(&c->m);
    return r;
}

int64_t dial_channel_done(DialChannel ch) {
    DIAL_HEAP_ASSERT(ch != NULL, "查询已释放的 channel");
    DialChannelImpl *c = (DialChannelImpl *)ch;
    pthread_mutex_lock(&c->m);
    int64_t r = (c->closed && c->n == 0) ? 1 : 0;
    pthread_mutex_unlock(&c->m);
    return r;
}

void dial_channel_retain(DialChannel ch) {
    if (!ch) return;
    atomic_fetch_add(&((DialChannelImpl *)ch)->rc, 1);
}

void dial_channel_release(DialChannel ch) {
    if (!ch) return;
    DialChannelImpl *c = (DialChannelImpl *)ch;
    if (atomic_fetch_sub(&c->rc, 1) == 1) {
        pthread_mutex_destroy(&c->m);
        pthread_cond_destroy(&c->not_empty);
        pthread_cond_destroy(&c->not_full);
        free(c->buf);
        free(c);
    }
}
