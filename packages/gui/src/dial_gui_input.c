/*
 * dial_gui_input.c —— gui.dialpkg 输入层（第四层，纯 C，三平台共用）
 *
 * 职责：TextBox 单行编辑（可打印字符 / 退格 / 删除 / 光标 / 选区 / max_len）
 *      + 焦点链（Tab / Shift+Tab 环形切换 / 点击聚焦 / 显式 set_focus）。
 *
 * 硬边界（与内存宪法一致，五层一律适用）：
 *   - **只 include dial_gui.h + libc**，绝不碰 X11 / Win32 任何 API；三份 .a
 *     的 `nm -u` 不会因本层新增任何平台符号。
 *   - **绝不 C → Dial 反向回调**：焦点是根上的 C 侧裸指针、文本是控件自己的
 *     strdup 副本、状态全是纯值。键盘事件由 Dial 侧 poll/wait 后主动用
 *     gui_widget_key(root, kind, key, mods) 喂进来，C 从不主动调 Dial 函数。
 *   - 光标 / 选区一律是 **UTF-8 字节偏移**，移动时按字符边界对齐，绝不把
 *     多字节序列从中间切断；文本上限按字节计（GUI_TEXT_MAX_LEN 兜底）。
 *
 * GUI 是单线程事件循环，全局根链表与焦点指针都无需加锁。
 */
#include "dial_gui.h"

#include <stdlib.h>
#include <string.h>

/* ---- UTF-8 字节偏移工具 ------------------------------------------------- */

static int text_len(const GuiWidgetImpl *w) {
    return w->text ? (int)strlen(w->text) : 0;
}
static int is_cont(unsigned char c) { return (c & 0xC0) == 0x80; }

/* 把 pos 对齐到 [0,len] 内的字符起始字节（落在续字节上向前退）。 */
static int align_pos(const char *s, int len, int pos) {
    if (pos < 0) return 0;
    if (pos > len) return len;
    while (pos > 0 && pos < len && s && is_cont((unsigned char)s[pos])) pos--;
    return pos;
}
static int next_pos(const char *s, int len, int pos) {
    if (pos >= len) return len;
    pos++;
    while (pos < len && s && is_cont((unsigned char)s[pos])) pos++;
    return pos;
}
static int prev_pos(const char *s, int pos) {
    if (pos <= 0) return 0;
    pos--;
    while (pos > 0 && s && is_cont((unsigned char)s[pos])) pos--;
    return pos;
}
static void sel_range(const GuiWidgetImpl *w, int *a, int *b) {
    int lo = w->sel_anchor < w->cursor ? w->sel_anchor : w->cursor;
    int hi = w->sel_anchor < w->cursor ? w->cursor : w->sel_anchor;
    *a = lo; *b = hi;
}

/* Unicode 码点 → UTF-8；非法码点（含代理区）返回 0 字节，绝不写出坏序列。 */
static int utf8_encode(int cp, char out[4]) {
    if (cp < 0 || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) return 0;
    if (cp < 0x80) { out[0] = (char)cp; return 1; }
    if (cp < 0x800) {
        out[0] = (char)(0xC0 | (cp >> 6));
        out[1] = (char)(0x80 | (cp & 0x3F));
        return 2;
    }
    if (cp < 0x10000) {
        out[0] = (char)(0xE0 | (cp >> 12));
        out[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[2] = (char)(0x80 | (cp & 0x3F));
        return 3;
    }
    out[0] = (char)(0xF0 | (cp >> 18));
    out[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
    out[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
    out[3] = (char)(0x80 | (cp & 0x3F));
    return 4;
}

/*
 * 用 ins[0..n) 替换 [a,b)。返回 1 成功 / 0 被 max_len 拒绝 / -1 分配失败。
 * 绝不在原缓冲上原地挪动：一律新分配一块再整体换掉，避免越界与别名问题。
 */
static int replace_range(GuiWidgetImpl *w, int a, int b,
                         const char *ins, int n) {
    const char *old = w->text ? w->text : "";
    int len = text_len(w);
    a = align_pos(old, len, a);
    b = align_pos(old, len, b);
    if (a > b) { int t = a; a = b; b = t; }
    int newlen = len - (b - a) + n;
    if (newlen > GUI_TEXT_MAX_LEN) return 0;
    if (w->max_len > 0 && newlen > w->max_len) return 0;
    char *buf = (char *)malloc((size_t)newlen + 1);
    if (!buf) return -1;
    if (a > 0) memcpy(buf, old, (size_t)a);
    if (n > 0 && ins) memcpy(buf + a, ins, (size_t)n);
    if (len - b > 0) memcpy(buf + a + n, old + b, (size_t)(len - b));
    buf[newlen] = '\0';
    free(w->text);
    w->text = buf;
    return 1;
}

/* ---- 树与焦点 ---------------------------------------------------------- */

static int in_tree(const GuiWidgetImpl *n, const GuiWidgetImpl *w) {
    if (!n) return 0;
    if (n == w) return 1;
    for (const GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        if (in_tree(c, w)) return 1;
    return 0;
}
static GuiWidgetImpl *tree_root(GuiWidgetImpl *w) {
    while (w && w->parent) w = w->parent;
    return w;
}
static int focusable(const GuiWidgetImpl *w) {
    return w->visible && !w->disabled && w->kind == GUI_WIDGET_TEXTBOX;
}
static int count_focusable(const GuiWidgetImpl *n) {
    if (!n || !n->visible) return 0;
    int k = focusable(n) ? 1 : 0;
    for (const GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        k += count_focusable(c);
    return k;
}
static void fill_focusable(GuiWidgetImpl *n, GuiWidgetImpl **arr, int *cnt) {
    if (!n || !n->visible) return;
    if (focusable(n)) arr[(*cnt)++] = n;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        fill_focusable(c, arr, cnt);
}
static void clear_focused(GuiWidgetImpl *n) {
    if (!n) return;
    n->focused = 0;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        clear_focused(c);
}

GuiWidgetImpl *gui_widget_focus(GuiWidgetImpl *root) {
    if (!root || !gui_widget_is_live(root)) return NULL;
    GuiWidgetImpl *r = tree_root(root);
    GuiWidgetImpl *f = r ? r->focus : NULL;
    return (f && gui_widget_is_live(f)) ? f : NULL;
}

int gui_widget_set_focus(GuiWidgetImpl *root, GuiWidgetImpl *w) {
    if (!root || !gui_widget_is_live(root)) return -1;
    GuiWidgetImpl *r = tree_root(root);
    if (!r) return -1;
    if (w && (!in_tree(r, w) || !focusable(w))) return -1;
    clear_focused(r);
    r->focus = w;
    if (w) w->focused = 1;
    return 1;
}

GuiWidgetImpl *gui_widget_focus_next(GuiWidgetImpl *root, int backward) {
    if (!root || !gui_widget_is_live(root)) return NULL;
    GuiWidgetImpl *r = tree_root(root);
    if (!r) return NULL;
    int n = count_focusable(r);
    if (n <= 0) {
        clear_focused(r);
        r->focus = NULL;
        return NULL;
    }
    GuiWidgetImpl **arr = (GuiWidgetImpl **)malloc((size_t)n * sizeof(*arr));
    if (!arr) return NULL;
    int cnt = 0;
    fill_focusable(r, arr, &cnt);
    int idx = -1;
    GuiWidgetImpl *cur = gui_widget_focus(r);
    for (int i = 0; i < cnt; i++)
        if (arr[i] == cur) { idx = i; break; }
    int next;
    if (idx < 0)   next = backward ? cnt - 1 : 0;
    else if (backward) next = (idx - 1 + cnt) % cnt;
    else               next = (idx + 1) % cnt;
    GuiWidgetImpl *target = arr[next];
    free(arr);
    clear_focused(r);
    r->focus = target;
    target->focused = 1;
    return target;
}

/* ---- 光标 / 选区（供 core 的 getter/setter 调用）------------------------ */

int gui_widget_select(GuiWidgetImpl *w, int a, int b) {
    if (!w || !gui_widget_is_live(w) || w->kind != GUI_WIDGET_TEXTBOX) return -1;
    const char *s = w->text ? w->text : "";
    int len = text_len(w);
    int lo = align_pos(s, len, a);
    int hi = align_pos(s, len, b);
    if (lo > hi) { int t = lo; lo = hi; hi = t; }
    w->sel_anchor = lo;
    w->cursor = hi;
    return 0;
}

int gui_widget_set_cursor(GuiWidgetImpl *w, int pos) {
    if (!w || !gui_widget_is_live(w) || w->kind != GUI_WIDGET_TEXTBOX) return -1;
    const char *s = w->text ? w->text : "";
    int c = align_pos(s, text_len(w), pos);
    w->cursor = c;
    w->sel_anchor = c;      /* 显式移动光标同时清选区 */
    return 0;
}

/* ---- 键盘纯值事件推进 -------------------------------------------------- */

/*
 * Dial 侧典型写法（绝不反向回调）：
 *   var k: int = dial_gui_app_wait(app, 100);
 *   if (k == 4) dial_gui_widget_key(root, k, dial_gui_event_key(app),
 *                                  dial_gui_event_mods(app));
 * 返回 0 无变化 / 1 文本或光标变化 / 2 焦点变化 / 3 Enter 提交 / <0 参数错。
 */
int gui_widget_key(GuiWidgetImpl *root, int kind, int key, int mods) {
    if (!root || !gui_widget_is_live(root)) return -1;
    if (kind != GUI_EV_KEY_PRESS) return 0;      /* RELEASE 不编辑、不切焦点 */
    GuiWidgetImpl *r = tree_root(root);
    if (!r) return -1;

    if (key == GUI_KEY_TAB) {
        GuiWidgetImpl *f = gui_widget_focus_next(r, (mods & GUI_MOD_SHIFT) ? 1 : 0);
        return f ? 2 : 0;
    }

    GuiWidgetImpl *w = gui_widget_focus(r);
    if (!w || w->kind != GUI_WIDGET_TEXTBOX || w->disabled) return 0;

    if (key == GUI_KEY_ENTER)  return 3;        /* 提交信号，不改文本 */
    if (key == GUI_KEY_ESCAPE) {                /* 清选区 */
        int c = align_pos(w->text ? w->text : "", text_len(w), w->cursor);
        w->cursor = c; w->sel_anchor = c;
        return 1;
    }

    const char *s = w->text ? w->text : "";
    int len = text_len(w);

    if (key == GUI_KEY_LEFT || key == GUI_KEY_RIGHT ||
        key == GUI_KEY_HOME || key == GUI_KEY_END) {
        int np = w->cursor;
        if (key == GUI_KEY_LEFT)  np = prev_pos(s, np);
        if (key == GUI_KEY_RIGHT) np = next_pos(s, len, np);
        if (key == GUI_KEY_HOME)  np = 0;
        if (key == GUI_KEY_END)   np = len;
        np = align_pos(s, len, np);
        w->cursor = np;
        if (!(mods & GUI_MOD_SHIFT)) w->sel_anchor = np;
        return 1;
    }

    int a = 0, b = 0;
    sel_range(w, &a, &b);
    if (key == GUI_KEY_BACKSPACE || key == GUI_KEY_DELETE) {
        if (a == b) {
            if (key == GUI_KEY_BACKSPACE) a = prev_pos(s, a);
            else                          b = next_pos(s, len, b);
        }
        if (a == b) return 0;
        int rc = replace_range(w, a, b, NULL, 0);
        if (rc <= 0) return rc;                 /* 0 被上限拒绝 / -1 分配失败 */
        w->cursor = a; w->sel_anchor = a;
        return 1;
    }

    /* 其它表内 / 表外功能键（Shift / Ctrl / Alt / F1.. / UNKNOWN）不编辑。 */
    if (key < 0x20 || key >= GUI_KEY_BASE || key == 0x7F) return 0;

    char buf[4];
    int n = utf8_encode(key, buf);
    if (n <= 0) return 0;
    int rc = replace_range(w, a, b, buf, n);
    if (rc <= 0) return rc;
    w->cursor = a + n; w->sel_anchor = w->cursor;   /* 插入替换选区并清选区 */
    return 1;
}
