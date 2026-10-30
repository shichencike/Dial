/*
 * ============================================================================
 *  Dial C Runtime 并发压测：多消费者抢同一个通道（v0.9 新增测试组 F）
 * ============================================================================
 *
 *  这个测试**绕开编译器**，直接压 C Runtime 的 dial_channel_*，因为要验证的
 *  恰恰是「Dial 层看不见、但多消费者下会致命」的那一层：
 *
 *  1) **不丢唤醒**：v0.8/v0.81 的 notify 用的是 signal（只唤醒一个等待者）。
 *     在多消费者下，「队列非空」是共享状态——唤醒一个之后若队列仍非空，
 *     其余消费者会带着已经失效的等待条件继续睡死，表现为某个消费者永久阻塞、
 *     程序死锁，且极难复现。本测试用 8 个消费者 + 100 万个元素，
 *     只要有一个消费者被吞掉，join 就会永远挂住（测试进程超时被杀 -> 判失败）。
 *
 *  2) **每个元素恰好被取走一次**：用位图 + 原子 fetch_or 记录每个序号的领取情况。
 *     重复领取 = 同一指针被交给两个消费者 = 必然 double-free。
 *     位图用 __atomic_fetch_or 而非普通 |=，否则两个消费者同时置相邻位时会
 *     丢失更新，产生假的「重复」告警。
 *
 *  3) **不丢失**：收到的总数必须恰好等于发出的总数。
 *
 *  4) **不崩溃**：double free / 越界会直接 abort，测试进程非零退出即失败。
 *
 *  为什么元素用 int64_t 而不是 Dial 的 string：这里测的是**通道本身**的并发语义，
 *  与元素类型无关；用裸 int64 让「唯一持有者」这件事在 C 层一目了然。
 * ============================================================================
 */

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "dial_channel.h"

#define N_ITEMS      1000000   /* 100 万个元素 */
#define N_CONSUMERS  8         /* 8 个消费者抢同一个通道 */
#define CAPACITY     64        /* 队列很浅 -> 消费者必须频繁阻塞/唤醒，最容易暴露丢唤醒 */

static DialChannel           g_ch;
static _Atomic int64_t       g_received;
static _Atomic int64_t       g_duplicate;
static _Atomic int64_t       g_out_of_range;
static unsigned char        *g_seen;      /* 位图：每个序号是否已被领取 */

static void *producer_thread(void *arg) {
    (void)arg;
    for (int64_t i = 0; i < N_ITEMS; i++) {
        int64_t *cell = (int64_t *)malloc(sizeof(int64_t));
        if (!cell) {
            fprintf(stderr, "producer: malloc failed\n");
            abort();
        }
        *cell = i;
        dial_channel_send(g_ch, cell);   /* 移交所有权 */
    }
    dial_channel_close(g_ch);            /* EOF 广播：唤醒所有消费者收尾 */
    return NULL;
}

static void *consumer_thread(void *arg) {
    (void)arg;
    for (;;) {
        void *p = dial_channel_recv(g_ch);
        if (!p) break;                   /* EOF：已关闭且已空 */

        int64_t idx = *(int64_t *)p;
        if (idx < 0 || idx >= N_ITEMS) {
            atomic_fetch_add(&g_out_of_range, 1);
        } else {
            size_t byte = (size_t)idx >> 3;
            unsigned char mask = (unsigned char)(1u << (idx & 7));
            /* 原子置位：返回旧值，若旧值已含该位 -> 同一元素被领取了两次 */
            unsigned char old = (unsigned char)__atomic_fetch_or(&g_seen[byte], mask, __ATOMIC_RELAXED);
            if (old & mask) {
                atomic_fetch_add(&g_duplicate, 1);
            }
        }
        atomic_fetch_add(&g_received, 1);
        free(p);                          /* 唯一持有者负责最终释放 */
    }
    return NULL;
}

int main(void) {
    g_seen = (unsigned char *)calloc((size_t)((N_ITEMS + 7) / 8), 1);
    if (!g_seen) {
        fprintf(stderr, "calloc failed\n");
        return 2;
    }
    atomic_init(&g_received, 0);
    atomic_init(&g_duplicate, 0);
    atomic_init(&g_out_of_range, 0);

    g_ch = dial_channel_new(CAPACITY);

    pthread_t prod;
    pthread_t cons[N_CONSUMERS];
    if (pthread_create(&prod, NULL, producer_thread, NULL) != 0) {
        fprintf(stderr, "pthread_create(producer) failed\n");
        return 2;
    }
    for (int i = 0; i < N_CONSUMERS; i++) {
        if (pthread_create(&cons[i], NULL, consumer_thread, NULL) != 0) {
            fprintf(stderr, "pthread_create(consumer) failed\n");
            return 2;
        }
    }

    /* 若发生丢唤醒，这里的 join 会永远挂住 -> 由测试框架的 timeout 杀掉并判失败 */
    pthread_join(prod, NULL);
    for (int i = 0; i < N_CONSUMERS; i++) {
        pthread_join(cons[i], NULL);
    }

    int64_t got  = atomic_load(&g_received);
    int64_t dup  = atomic_load(&g_duplicate);
    int64_t oor  = atomic_load(&g_out_of_range);

    printf("received=%lld expected=%d duplicate=%lld out_of_range=%lld\n",
           (long long)got, N_ITEMS, (long long)dup, (long long)oor);

    if (got != N_ITEMS) {
        printf("FAIL: 元素总数不符（丢失或重复投递）\n");
        return 1;
    }
    if (dup != 0) {
        printf("FAIL: 同一元素被领取多次 -> 必然 double-free\n");
        return 1;
    }
    if (oor != 0) {
        printf("FAIL: 取到了越界的元素（内存已损坏）\n");
        return 1;
    }

    printf("PASS\n");
    return 0;
}
