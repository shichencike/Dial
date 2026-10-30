#include "dial_channel.h"
#include "dial_rt_trap.h"

#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <time.h>

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
    pthread_mutex_unlock(&c->m);
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
    pthread_mutex_unlock(&c->m);

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
    pthread_mutex_unlock(&c->m);

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
    pthread_mutex_unlock(&c->m);

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
