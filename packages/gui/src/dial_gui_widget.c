/*
 * dial_gui_widget.c —— gui.dialpkg 控件框架层（纯 C，三平台共用）
 *
 * 职责：GuiWidget 树 / 绝对·VBox·HBox 布局 / 点命中测试 / packed 事件块填充。
 *
 * 硬边界（与内存宪法一致）：
 *   - **只 include dial_gui.h + libc**，绝不碰 X11 / Win32 任何 API；
 *     这样三份 .a 的 `nm -u` 不会新增平台符号，Dial 侧零链接期依赖。
 *   - **绝不 C → Dial 反向回调**：本文件不保存任何函数指针、不认识 Dial
 *     对象、不碰 ARC。事件只写成一块纯值 packed struct，由 Dial 侧主动读走。
 *   - 所有几何运算是纯整数，无浮点、无隐式分配。
 *
 * 全局根链表只存 C 侧裸指针，用于 gui_widget_is_live() 防悬垂句柄校验
 * （与 core.c 的 gui_core_has_window 同口径：线性遍历比指针，不解引用目标）。
 * GUI 是单线程事件循环，无需加锁。
 */
#include "dial_gui.h"

#include <stdlib.h>

/* ---- 全局根链表（仅树根挂在这里）-------------------------------------- */
static GuiWidgetImpl *g_roots = NULL;

static void dispose_subtree(GuiWidgetImpl *n) {
    if (!n) return;
    GuiWidgetImpl *c = n->first_child;
    while (c) {
        GuiWidgetImpl *next = c->next_sibling;
        dispose_subtree(c);
        c = next;
    }
    free(n->id);
    free(n->text);   /* 外观层文本副本（与 id 同为 C 侧 strdup 自有） */
    free(n);
}

/* 只比较指针值，绝不读取 w 的内容——w 可能已经是悬垂地址。 */
static int tree_contains(const GuiWidgetImpl *n, const GuiWidgetImpl *w) {
    if (!n) return 0;
    if (n == w) return 1;
    for (const GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        if (tree_contains(c, w)) return 1;
    return 0;
}

int gui_widget_is_live(const GuiWidgetImpl *w) {
    if (!w) return 0;
    for (const GuiWidgetImpl *r = g_roots; r; r = r->next_root)
        if (tree_contains(r, w)) return 1;
    return 0;
}

/* ---- 树的生命周期 ------------------------------------------------------ */

GuiWidgetImpl *gui_widget_tree_new(int w, int h) {
    GuiWidgetImpl *r = (GuiWidgetImpl *)calloc(1, sizeof(GuiWidgetImpl));
    if (!r) return NULL;
    r->visible = 1;
    r->layout = GUI_LAYOUT_ABS;
    r->req_x = 0; r->req_y = 0;
    r->req_w = w; r->req_h = h;
    r->x = 0; r->y = 0; r->w = w; r->h = h;
    r->vmax = 100;          /* SLIDER / PROGRESS 默认值域 0..100 */
    r->next_root = g_roots;
    g_roots = r;
    return r;
}

void gui_widget_dispose_tree(GuiWidgetImpl *root) {
    if (!root) return;
    /* 从全局根链表摘除（仅当它确实是登记过的根）。 */
    GuiWidgetImpl **pp = &g_roots;
    while (*pp && *pp != root) pp = &(*pp)->next_root;
    if (*pp == root) *pp = root->next_root;
    /* 挂过窗口的树：清掉窗口侧指针，避免 window_close 二次释放。 */
    if (root->win) root->win->root = NULL;
    dispose_subtree(root);
}

GuiWidgetImpl *gui_widget_add(GuiWidgetImpl *parent) {
    if (!gui_widget_is_live(parent)) return NULL;
    GuiWidgetImpl *c = (GuiWidgetImpl *)calloc(1, sizeof(GuiWidgetImpl));
    if (!c) return NULL;
    c->visible = 1;
    c->layout = GUI_LAYOUT_ABS;
    c->parent = parent;
    c->vmax = 100;          /* SLIDER / PROGRESS 默认值域 0..100 */
    /* 追加到末尾：保持用户加入顺序，命中测试后加子优先与绘制层序一致。 */
    GuiWidgetImpl **pp = &parent->first_child;
    while (*pp) pp = &(*pp)->next_sibling;
    *pp = c;
    return c;
}

/* ---- 主题层配套：RADIO 同组互斥 / SLIDER 按坐标设值（纯 C 树操作）------ *
 * 两条都由 look.c 的 RELEASE 分支调用，只读写 GuiWidgetImpl 上的 C 侧纯值，
 * 绝不碰平台 API、绝不 C → Dial 反向回调。 */

static void radio_clear_group(GuiWidgetImpl *n, const GuiWidgetImpl *hit) {
    if (!n) return;
    if (n != hit && n->kind == GUI_WIDGET_RADIO && n->group == hit->group)
        n->checked = 0;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        radio_clear_group(c, hit);
}

void gui_widget_radio_select(GuiWidgetImpl *root, GuiWidgetImpl *hit) {
    if (!root || !hit) return;
    hit->checked = 1;
    /* group == 0 表示未分组：各自独立，不清其它 RADIO。 */
    if (hit->group != 0) radio_clear_group(root, hit);
}

int gui_widget_slider_set_from_x(GuiWidgetImpl *hit, int local_x) {
    if (!hit || hit->kind != GUI_WIDGET_SLIDER) return 0;
    int lo = hit->vmin, hi = hit->vmax;
    if (hi <= lo) return 0;
    int track = hit->w - 8;          /* 与 look.c 绘制的轨道内宽一致 */
    if (track <= 0) return 0;
    int lx = local_x - 4;            /* 轨道左内边距 4px */
    if (lx < 0) lx = 0;
    if (lx > track) lx = track;
    /* 纯整数比例，四舍五入到最近值；用 int64 中转避免中间乘积溢出。 */
    int span = hi - lo;
    int v = lo + (int)(((int64_t)lx * span + track / 2) / track);
    if (v < lo) v = lo;
    if (v > hi) v = hi;
    if (v == hit->value) return 0;
    hit->value = v;
    return 1;
}

int gui_widget_remove(GuiWidgetImpl *w) {
    if (!gui_widget_is_live(w) || !w->parent) return 0;
    GuiWidgetImpl *p = w->parent;
    GuiWidgetImpl **pp = &p->first_child;
    while (*pp && *pp != w) pp = &(*pp)->next_sibling;
    if (*pp != w) return 0;
    /* 摘除前先清掉指向被删子树的焦点指针，绝不留下悬垂裸指针。 */
    GuiWidgetImpl *r = p;
    while (r->parent) r = r->parent;
    if (r->focus && tree_contains(w, r->focus)) r->focus = NULL;
    *pp = w->next_sibling;
    w->parent = NULL;
    w->next_sibling = NULL;
    dispose_subtree(w);
    return 1;
}

int gui_widget_attach_window(GuiWidgetImpl *root, GuiWindowImpl *win) {
    if (!root || !win) return 0;
    if (!gui_widget_is_live(root)) return 0;
    /* 只接受「无父、未挂窗」的独立根，且窗口尚未挂树。 */
    if (root->parent || root->win || win->root) return 0;
    root->win = win;
    win->root = root;
    return 1;
}

/* ---- 布局（纯整数、幂等）---------------------------------------------- */

/*
 * 前序递归：先定当前节点的绝对坐标与实际尺寸，再给可见子节点分配。
 * 每次都从 req_* 重算，不依赖上一次的 w/h —— 因此重复调用不会叠加
 * padding / gap，也不会把上一轮弹性分配出来的尺寸误当成固定尺寸。
 */
static void layout_node(GuiWidgetImpl *n, int ax, int ay, int aw, int ah) {
    n->abs_x = ax; n->abs_y = ay;
    n->w = aw;     n->h = ah;

    int count = 0;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        if (c->visible) count++;
    if (count == 0) return;

    const int pad = n->padding > 0 ? n->padding : 0;
    const int gap = n->gap > 0 ? n->gap : 0;
    const int cx = ax + pad;
    const int cy = ay + pad;
    int cw = aw - 2 * pad;
    int ch = ah - 2 * pad;
    if (cw < 0) cw = 0;
    if (ch < 0) ch = 0;

    if (n->layout == GUI_LAYOUT_ABS) {
        for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling) {
            if (!c->visible) continue;
            c->x = c->req_x; c->y = c->req_y;
            c->w = c->req_w > 0 ? c->req_w : 0;
            c->h = c->req_h > 0 ? c->req_h : 0;
            layout_node(c, ax + c->x, ay + c->y, c->w, c->h);
        }
        return;
    }

    const int vertical = (n->layout == GUI_LAYOUT_VBOX);
    const int avail = vertical ? ch : cw;
    const int total_gap = gap * (count - 1);

    /* 主轴上 req > 0 视为固定尺寸，优先扣减；其余由弹性子节点均分余量。 */
    int fixed_sum = 0, flex_count = 0;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling) {
        if (!c->visible) continue;
        int want = vertical ? c->req_h : c->req_w;
        if (want > 0) fixed_sum += want; else flex_count++;
    }
    int rest = avail - total_gap - fixed_sum;
    int flex = (flex_count > 0 && rest > 0) ? rest / flex_count : 0;
    if (flex < 0) flex = 0;

    int pos = vertical ? cy : cx;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling) {
        if (!c->visible) continue;
        int want = vertical ? c->req_h : c->req_w;
        int main = want > 0 ? want : flex;
        int cross = vertical ? cw : ch;
        if (vertical) {
            c->x = pad; c->y = pos - ay;
            c->w = cross; c->h = main;
            layout_node(c, ax + c->x, pos, c->w, c->h);
        } else {
            c->x = pos - ax; c->y = pad;
            c->w = main; c->h = cross;
            layout_node(c, pos, ay + c->y, c->w, c->h);
        }
        pos += main + gap;
    }
}

int gui_widget_layout_apply(GuiWidgetImpl *root, int w, int h) {
    if (!root || !gui_widget_is_live(root) || w <= 0 || h <= 0) return 0;
    layout_node(root, 0, 0, w, h);
    return 1;
}

/* ---- 命中测试 ---------------------------------------------------------- */

/*
 * 递归累加相对几何得到绝对坐标，不依赖是否调用过 layout_apply。
 * 后加子优先：遍历兄弟链表时取最后一个命中的子节点（与自绘层序一致）。
 */
static GuiWidgetImpl *hit_node(GuiWidgetImpl *n, int ax, int ay,
                               int px, int py, int *hx, int *hy) {
    if (!n->visible) return NULL;
    if (px < ax || px >= ax + n->w || py < ay || py >= ay + n->h) return NULL;

    GuiWidgetImpl *best = NULL;
    int bx = 0, by = 0;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling) {
        int tx = 0, ty = 0;
        GuiWidgetImpl *r = hit_node(c, ax + c->x, ay + c->y, px, py, &tx, &ty);
        if (r) { best = r; bx = tx; by = ty; }
    }
    if (best) { *hx = bx; *hy = by; return best; }
    *hx = ax; *hy = ay;
    return n;
}

GuiWidgetImpl *gui_widget_hit(GuiWidgetImpl *root, int x, int y,
                              int *out_abs_x, int *out_abs_y) {
    if (out_abs_x) *out_abs_x = 0;
    if (out_abs_y) *out_abs_y = 0;
    if (!root || !gui_widget_is_live(root)) return NULL;
    int ax = 0, ay = 0;
    GuiWidgetImpl *hit = hit_node(root, root->abs_x, root->abs_y, x, y, &ax, &ay);
    if (hit) {
        /* 写回绝对坐标：core 的 emit 据此算 local_x / local_y。 */
        hit->abs_x = ax; hit->abs_y = ay;
        if (out_abs_x) *out_abs_x = ax;
        if (out_abs_y) *out_abs_y = ay;
    }
    return hit;
}

/* ---- packed 事件纯值块 ------------------------------------------------- */

/*
 * 只写纯值 + 两个 C 侧借用指针。字符串字段的所有权始终在 C：
 *   win_title -> GuiWindowImpl.title[256]
 *   widget_id -> 控件的 strdup 副本
 * Dial 侧必须 string_from_cstr 复制，绝不长期持有；下一次 poll/wait 或
 * 窗口 / 树释放后即失效。QUIT 与窗口级事件 hit == NULL，两指针置 NULL
 * （string_from_cstr(NULL) 返回空串）。
 */
void gui_widget_fill_event(GuiWidgetEvent *ev, int kind, int x, int y,
                           int button, int key, int w, int h,
                           GuiWindowImpl *win, GuiWidgetImpl *hit) {
    if (!ev) return;
    ev->kind = kind; ev->x = x; ev->y = y;
    ev->button = button; ev->key = key; ev->w = w; ev->h = h;
    ev->local_x = hit ? x - hit->abs_x : 0;
    ev->local_y = hit ? y - hit->abs_y : 0;
    ev->tag = hit ? hit->tag : 0;
    ev->userdata = hit ? hit->userdata : 0;
    ev->win_title = win ? win->title : NULL;
    ev->widget_id = hit ? hit->id : NULL;
}
