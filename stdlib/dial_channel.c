#include "dial_channel.h"
#include "dial_rt_trap.h"

#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>

typedef struct {
    _Atomic int64_t  rc;

    pthread_mutex_t  m;
    pthread_cond_t   not_empty;   /* 队列由空变非空 */
    pthread_cond_t   not_full;    /* 队列由满变非满 */

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
    c->cap  = cap;
    c->head = 0;
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
    pthread_cond_signal(&c->not_empty);
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
        pthread_cond_signal(&c->not_full);
    }
    pthread_mutex_unlock(&c->m);

    /* 已关闭且已空 -> NULL（EOF） */
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
        pthread_cond_signal(&c->not_full);
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
