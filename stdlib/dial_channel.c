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
    while (c->n == 0) {
        pthread_cond_wait(&c->not_empty, &c->m);
    }
    void *p = c->buf[c->head];
    c->head = (c->head + 1) % c->cap;
    c->n--;
    pthread_cond_signal(&c->not_full);
    pthread_mutex_unlock(&c->m);

    return p;
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
