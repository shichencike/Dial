/*
 * dial_gui_context.c —— gui.dialpkg 第 10 层：右键上下文菜单（纯 C，三平台共用）
 *
 * 职责：把「一份第 7 层菜单 + 一个弹出锚点」打包成可复用的上下文菜单对象，
 *      负责打开 / 关闭 / 命中 / 选中 / 绘制五件事。菜单项本身**完全复用**
 *      第 7 层的 GuiMenu（零新类型、零新 .diali type），本层只管几何与状态。
 *
 * 硬边界（与第七 / 九层一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象。打开 / 命中 /
 *     选中全是纯值，菜单项 id 由应用侧从返回值读走。
 *   - 命中行序与绘制行序**严格一致**（同为菜单链表序），故「点到的」
 *     与「看到的」永远是同一行。注意第 7 层 popup 的历史行序与绘制相反
 *     （见 dial_gui_menu.c 的 popup_hit 注释），本层不复制那一处历史包袱。
 *
 * 所有权：GuiContext **借用**菜单（set_menu 不转移所有权，free 也不释放菜单），
 *         菜单的生死仍由应用侧掌控；上下文菜单自身是纯内存对象，未挂任何
 *         窗口时用 dial_gui_context_free 释放，可在无 DISPLAY 环境单元测试。
 */
#include "dial_gui.h"

#include <stdlib.h>

/* 取菜单第 row 行的项（绘制序 = 链表序：GuiMenuItem 头插，故自上而下
 * 依次是「最后 add 的项」在前，与 gui_menu_draw_popup 的遍历序完全一致）。*/
static GuiMenuItem *ctx_item_at(GuiMenu *m, int row) {
    if (!m || row < 0 || row >= m->count) return NULL;
    GuiMenuItem *it = m->first;
    while (it && row > 0) { it = it->next; row--; }
    return it;
}

/* 命中测试：返回绘制行号（0 起）；未命中 / 分隔符 / 禁用项统一返回 -1。
 * 与 gui_menu_popup_w 的宽度、gui_menu_draw_popup 的行高口径一致。*/
static int64_t ctx_hit(GuiContext *c, int x, int y) {
    if (!c || !c->open || !c->menu) return -1;
    int w = gui_menu_popup_w(c->menu);
    int rows = c->menu->count;
    int h = rows * GUI_MENU_ROW_H;
    if (w <= 0 || h <= 0) return -1;
    if (x < c->x || x >= c->x + w) return -1;
    if (y < c->y || y >= c->y + h) return -1;
    int row = (y - c->y) / GUI_MENU_ROW_H;
    if (row < 0 || row >= rows) return -1;
    GuiMenuItem *it = ctx_item_at(c->menu, row);
    if (!it || it->sep || it->disabled) return -1;   /* 分隔符 / 禁用不可命中 */
    return row;
}

GuiContext *dial_gui_context_new(void) {
    GuiContext *c = (GuiContext *)calloc(1, sizeof(GuiContext));
    if (c) c->hover = -1;      /* 失败返回 NULL；其余字段全零（open=0）*/
    return c;
}

int64_t dial_gui_context_free(GuiContext *c) {
    if (!c) return -1;
    free(c);                   /* 借用菜单：不释放 */
    return 0;
}

int64_t dial_gui_context_set_menu(GuiContext *c, GuiMenu *m) {
    if (!c || !m) {
        gui_core_set_err(GUI_ERR_ARG, "空 GuiContext / 空 GuiMenu 句柄");
        return -1;
    }
    c->menu = m;               /* 借用：不接管所有权，重复设置不释放旧菜单 */
    c->hover = -1;
    return 0;
}

GuiMenu *dial_gui_context_menu(GuiContext *c) {
    return c ? c->menu : NULL;
}

int64_t dial_gui_context_set_tag(GuiContext *c, int64_t tag) {
    if (!c) return -1;
    c->tag = tag;
    return 0;
}

int64_t dial_gui_context_tag(GuiContext *c) {
    return c ? c->tag : -1;
}

int64_t dial_gui_context_open(GuiContext *c, int x, int y) {
    if (!c || !c->menu) {
        gui_core_set_err(GUI_ERR_ARG, "上下文菜单未绑定 GuiMenu");
        return -1;
    }
    c->x = x;
    c->y = y;
    c->open = 1;
    c->hover = -1;
    return 0;
}

int64_t dial_gui_context_close(GuiContext *c) {
    if (!c) return -1;
    c->open = 0;
    c->hover = -1;
    return 0;
}

int64_t dial_gui_context_is_open(GuiContext *c) {
    return c ? c->open : 0;
}

int64_t dial_gui_context_anchor_x(GuiContext *c) {
    return c ? c->x : -1;
}

int64_t dial_gui_context_anchor_y(GuiContext *c) {
    return c ? c->y : -1;
}

int64_t dial_gui_context_hit(GuiContext *c, int x, int y) {
    return ctx_hit(c, x, y);
}

int64_t dial_gui_context_hover(GuiContext *c, int x, int y) {
    if (!c) return -1;
    int64_t row = ctx_hit(c, x, y);
    c->hover = (int)row;       /* -1 = 未命中（绘制时不高亮）*/
    return row;
}

int64_t dial_gui_context_select(GuiContext *c, int x, int y) {
    if (!c) return -1;
    int64_t row = ctx_hit(c, x, y);
    if (row < 0) return -1;    /* 未命中：保持打开状态，由应用决定去留 */
    GuiMenuItem *it = ctx_item_at(c->menu, (int)row);
    if (!it) return -1;
    int64_t id = it->id;
    c->open = 0;               /* 选中即收起 */
    c->hover = -1;
    return id;
}

int64_t dial_gui_context_draw(GuiPainterImpl *p, GuiContext *c) {
    if (!p || !c) return -1;
    if (!c->open || !c->menu) return 0;   /* 未展开不绘制 */
    int save = c->menu->hover;
    c->menu->hover = c->hover;            /* 临时借出悬停行，绘制后原样还回 */
    gui_menu_draw_popup(p, c->menu, c->x, c->y);
    c->menu->hover = save;
    return 0;
}
