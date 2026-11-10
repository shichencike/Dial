/*
 * dial_gui_signal.c —— gui.dialpkg 第 6 层：信号槽 / 控件事件订阅（纯 C，三平台共用）
 *
 * 职责：把「点击 / 勾选翻转 / 文本变更 / 值变更 / 焦点变化 / 提交」这类
 *      **语义状态变化**做成可订阅的纯值信号块，压进每棵树一个环形队列，
 *      由 Dial 侧主动 pending → data(ptr_cast 读) → pop 消费。
 *
 * 硬边界（内存宪法，六层一律适用）：
 *   - **只 include dial_gui.h + libc**，绝不碰 X11 / Win32 任何 API；三份 .a
 *     的 `nm -u` 不会因本层新增任何平台符号。
 *   - **绝不 C → Dial 反向回调**：C 侧不保存函数指针、不认识 Dial 对象、不碰
 *     ARC。信号只写成一块 **100% 纯值**的 packed struct（连控件句柄都不存，
 *     只存 tag / userdata 纯值键与整数值），由 Dial 侧主动读走。控件被摘除 /
 *     树被释放后，已入队信号仍是可读的整数快照，绝不悬垂。
 *   - **零订阅 = 零开销**：gui_signal_emit 第一行就查 w->sig_mask，未订阅直接
 *     返回——不找根、不分配、不入队。因此不调用 subscribe 的旧程序行为、
 *     绘制 CRC、函数返回值逐字节不变（历史 523 ✅ 不漂移）。
 *
 * 队列口径（Fail-Fast 可观测，绝不静默损坏）：
 *   - 每棵树只在**根节点**挂一个环形队列，容量 GUI_SIGNAL_CAP=64，首次 emit
 *     时按需 calloc（零订阅时连队列都不分配）。
 *   - 队列满：覆盖最旧一条（保留最新），同时把根上的 sig_dropped 累计 +1，
 *     Dial 侧用 dial_gui_widget_signal_dropped 读到「我漏了多少条」，绝不静默。
 *   - calloc 失败同样累加 sig_dropped，任何路径的信号丢失都可被观测。
 *   - seq 单调递增、clear 不重置序号，便于 Dial 侧断言顺序 / 检测跳号。
 *
 * GUI 是单线程事件循环，全局无需加锁。
 */
#include "dial_gui.h"

#include <stdlib.h>

/* 队列完整定义放本 TU 私有；头文件只做前向声明，GuiWidgetImpl 存裸指针。 */
struct GuiSignalQueue {
    GuiWidgetSignal slots[GUI_SIGNAL_CAP];
    int  head;        /* 队首下标 */
    int  count;       /* 当前排队条数 [0..CAP] */
    int64_t seq;      /* 下一条信号的序号，单调递增 */
};

/* 从任意节点上溯到真正的根（队列只挂根）。只比较 parent 指针，不解引用外部句柄。 */
static GuiWidgetImpl *sig_root(GuiWidgetImpl *w) {
    if (!w) return NULL;
    while (w->parent) w = w->parent;
    return w;
}

/* create=1 才懒分配；create=0 只读，绝不为了查询而分配。 */
static GuiSignalQueue *sig_queue(GuiWidgetImpl *root, int create) {
    if (!root) return NULL;
    if (!root->sigq && create)
        root->sigq = (GuiSignalQueue *)calloc(1, sizeof(GuiSignalQueue));
    return root->sigq;
}

/*
 * 埋点入口：w 是「产生状态变化的控件」，signal 是单个 GUI_SIG_* 位值，
 * a/b 是该信号的主/副值，x/y 是事件坐标（键盘类信号传 0）。
 * 只有 w 订阅了 signal 才入队；否则立即返回（零开销）。
 */
void gui_signal_emit(GuiWidgetImpl *w, int64_t signal, int64_t a, int64_t b,
                     int x, int y) {
    if (!w) return;
    if (!(w->sig_mask & signal)) return;      /* 零订阅 = 零开销 */

    GuiWidgetImpl *root = sig_root(w);
    GuiSignalQueue *q = sig_queue(root, 1);
    if (!q) {                                 /* calloc 失败：仍让丢失可观测 */
        if (root) root->sig_dropped++;
        return;
    }
    if (q->count == GUI_SIGNAL_CAP) {         /* 满：覆盖最旧，保留最新 */
        q->head = (q->head + 1) % GUI_SIGNAL_CAP;
        q->count--;
        root->sig_dropped++;
    }
    int idx = (q->head + q->count) % GUI_SIGNAL_CAP;
    GuiWidgetSignal *s = &q->slots[idx];
    s->seq      = q->seq++;
    s->signal   = signal;
    s->tag      = w->tag;
    s->userdata = w->userdata;
    s->kind     = w->kind;
    s->a        = a;
    s->b        = b;
    s->x        = x;
    s->y        = y;
    q->count++;
}

int64_t gui_signal_pending(GuiWidgetImpl *root) {
    GuiSignalQueue *q = sig_queue(sig_root(root), 0);
    return q ? (int64_t)q->count : 0;
}

/* 返回队首纯值块地址（C 拥有），空队列返回 NULL；Dial 侧 ptr_cast 只读。 */
GuiWidgetSignal *gui_signal_peek(GuiWidgetImpl *root) {
    GuiSignalQueue *q = sig_queue(sig_root(root), 0);
    if (!q || q->count == 0) return NULL;
    return &q->slots[q->head];
}

/* 弹出队首：1=确实弹出一条 / 0=队列本空（正常态，不算错误）。 */
int gui_signal_pop(GuiWidgetImpl *root) {
    GuiSignalQueue *q = sig_queue(sig_root(root), 0);
    if (!q || q->count == 0) return 0;
    q->head = (q->head + 1) % GUI_SIGNAL_CAP;
    q->count--;
    return 1;
}

/* 清空队列但不重置 seq（序号保持单调，便于 Dial 侧检测跳号）。失败返回 -1。 */
int gui_signal_clear(GuiWidgetImpl *root) {
    GuiWidgetImpl *r = sig_root(root);
    if (!r) return -1;
    GuiSignalQueue *q = sig_queue(r, 0);
    if (q) { q->head = 0; q->count = 0; }
    return 0;
}

int64_t gui_signal_dropped(GuiWidgetImpl *root) {
    GuiWidgetImpl *r = sig_root(root);
    return r ? r->sig_dropped : 0;
}
