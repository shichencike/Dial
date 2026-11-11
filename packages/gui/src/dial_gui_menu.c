/*
 * dial_gui_menu.c —— gui.dialpkg 第 7 层：菜单 / 对话框（纯 C，三平台共用）
 *
 * 职责：顶栏菜单（menubar）+ 弹出菜单（popup）+ 模态对话框（dialog）三种
 *      **窗口级覆盖层**的纯值状态 / 几何 / 命中 / 绘制 / 选中事件分发。
 *
 * 硬边界（与内存宪法、前六层一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象、不碰 ARC。
 *     菜单项 / 对话框按钮的选中事件复用 win->root 控件树的 72 字节纯值信号
 *     队列（GuiWidgetSignal），Dial 侧主动 pending → data → pop 消费。
 *   - 覆盖层不参与控件树布局，因此控件几何 / painter CRC 逐字节不漂移；
 *     顶栏占用的顶部 22px 由应用自行在控件树顶部留白（首个子节点给 22px 垫片）。
 *   - 窗口接管 menubar / popup / dialog 的所有权，关闭窗口时一并释放，
 *     调用方不要再 free 已挂窗的对象（未挂窗的纯内存对象用 gui_menu_free /
 *     gui_dialog_free 释放，可在无 DISPLAY 环境单元测试）。
 */
#include "dial_gui.h"

#include <stdlib.h>
#include <string.h>

/* 内置 8×8 点阵只读 f->kind=0，其余字段全零即可用于 gui_font_width / draw。*/
static GuiFontImpl g_builtin_font;

/* 简写配色：一律从主题表取，theme_set 一次改色整树立即生效。*/
#define C(role) gui_theme_color(GUI_THEME_ROLE_##role)

/* 菜单项左/右内边距（像素），文本左右各留一半。*/
#define MENU_PAD   8
/* 弹出菜单每行高度：复用头文件共享常量 GUI_MENU_ROW_H（第 7 层 popup 与
 * 第 10 层上下文菜单行高必须逐像素一致，故只留一处真相源）。*/
#define POPUP_ROW  GUI_MENU_ROW_H
#define DIALOG_PAD 16   /* 对话框内边距 */

/* ====================================================================== *
 * 菜单（GuiMenu）：纯内存对象，无窗口
 * ====================================================================== */

GuiMenu *gui_menu_new(void) {
    GuiMenu *m = (GuiMenu *)calloc(1, sizeof(GuiMenu));
    return m;   /* 失败返回 NULL；count=0 / hover=-1 已归零 */
}

void gui_menu_free(GuiMenu *m) {
    if (!m) return;
    GuiMenuItem *it = m->first;
    while (it) {
        GuiMenuItem *n = it->next;
        free(it->label);
        free(it);
        it = n;
    }
    free(m);
}

int gui_menu_add_item(GuiMenu *m, int64_t id, const char *label) {
    if (!m || !label) return -1;
    GuiMenuItem *it = (GuiMenuItem *)calloc(1, sizeof(GuiMenuItem));
    if (!it) return -1;
    it->label = (char *)malloc(strlen(label) + 1);
    if (!it->label) { free(it); return -1; }
    strcpy(it->label, label);
    it->id = id;
    it->next = m->first;        /* 头插；命中/绘制按 count 顺序重排遍历 */
    m->first = it;
    m->count++;
    return 0;
}

int gui_menu_add_sep(GuiMenu *m) {
    if (!m) return -1;
    GuiMenuItem *it = (GuiMenuItem *)calloc(1, sizeof(GuiMenuItem));
    if (!it) return -1;
    it->sep = 1;
    it->next = m->first;
    m->first = it;
    m->count++;
    return 0;
}

int gui_menu_set_tag(GuiMenu *m, int64_t tag) {
    if (!m) return -1;
    m->tag = tag;
    return 0;
}
int64_t gui_menu_tag(GuiMenu *m) {
    return m ? m->tag : -1;
}
int gui_menu_count(GuiMenu *m) {
    return m ? m->count : 0;
}

/* 按插入序（0 = 最先插入）取第 idx 项；越界（含 idx >= count）返回 NULL。*/
static GuiMenuItem *menu_item_at(GuiMenu *m, int idx) {
    if (!m || idx < 0 || idx >= m->count) return NULL;
    int n = m->count - 1 - idx;   /* 头插，故第 idx 个 = 倒数第 (count-1-idx) 个 */
    GuiMenuItem *it = m->first;
    while (it && n > 0) { it = it->next; n--; }
    return it;
}

int64_t gui_menu_item_id(GuiMenu *m, int idx) {
    GuiMenuItem *it = menu_item_at(m, idx);
    return it ? it->id : -1;
}
const char *gui_menu_item_label(GuiMenu *m, int idx) {
    GuiMenuItem *it = menu_item_at(m, idx);
    return it ? (it->label ? it->label : "") : "";
}
int gui_menu_item_set_enabled(GuiMenu *m, int idx, int v) {
    GuiMenuItem *it = menu_item_at(m, idx);
    if (!it) return -1;
    it->disabled = v ? 1 : 0;
    return 0;
}

/* ---- 几何 / 命中 ------------------------------------------------------- */

/* 顶栏一项的宽度：文本宽 + 两侧内边距；分隔符宽度 0。*/
static int menu_bar_item_w(const GuiMenuItem *it) {
    if (!it || it->sep) return 0;
    int tw = gui_font_width(it->label ? it->label : "", -1, &g_builtin_font,
                            GUI_LOOK_FONT_SIZE);
    int w = tw + MENU_PAD * 2;
    return w < 24 ? 24 : w;
}
/* 弹出菜单宽度：取最长项 + 两侧内边距，下限 120。
 * 第 10 层上下文菜单复用本宽度口径，故为非 static（内部 C API，不进 .diali）。*/
int gui_menu_popup_w(GuiMenu *m) {
    int maxw = 120;
    for (GuiMenuItem *it = m->first; it; it = it->next) {
        if (it->sep) continue;
        int tw = gui_font_width(it->label ? it->label : "", -1, &g_builtin_font,
                                GUI_LOOK_FONT_SIZE);
        int w = tw + MENU_PAD * 2;
        if (w > maxw) maxw = w;
    }
    return maxw;
}

/* 顶栏第 idx 项下标命中：横排，从 x=0 起；-1 表示未命中。*/
static int bar_hit(GuiMenu *m, int win_w, int x, int y) {
    if (!m || y < 0 || y >= GUI_MENU_BAR_H) return -1;
    int cx = 0, i = 0;
    for (GuiMenuItem *it = m->first; it; it = it->next, i++) {
        int w = menu_bar_item_w(it);
        if (w == 0) continue;
        if (x >= cx && x < cx + w) return i;
        cx += w;
        (void)win_w;
    }
    return -1;
}
/* 弹出菜单第 idx 项下标命中：竖排，从 (px,py) 起，行高 POPUP_ROW。*/
static int popup_hit(GuiMenu *m, int px, int py, int x, int y) {
    if (!m) return -1;
    int w = gui_menu_popup_w(m);
    int rh = m->count * POPUP_ROW;
    if (x < px || x >= px + w || y < py || y >= py + rh) return -1;
    int row = (y - py) / POPUP_ROW;
    if (row < 0 || row >= m->count) return -1;
    GuiMenuItem *it = menu_item_at(m, row);
    if (!it || it->sep || it->disabled) return -1;   /* 分隔符/禁用不可命中 */
    return row;
}

/* ---- 绘制 -------------------------------------------------------------- */

void gui_menu_draw_bar(GuiPainterImpl *p, GuiMenu *m, int w) {
    if (!p || !m) return;
    gui_raster_fill_rect(p, 0, 0, w, GUI_MENU_BAR_H, C(BG));
    int cx = 0;
    int i = 0;
    for (GuiMenuItem *it = m->first; it; it = it->next, i++) {
        int iw = menu_bar_item_w(it);
        if (iw == 0) continue;
        if (i == m->hover && !it->sep)
            gui_raster_fill_rect(p, cx, 0, iw, GUI_MENU_BAR_H, C(BTN_HOVER));
        if (!it->sep && it->label)
            gui_font_draw(p, cx + MENU_PAD, (GUI_MENU_BAR_H - GUI_LOOK_FONT_SIZE) / 2,
                          it->label, (int)strlen(it->label), &g_builtin_font,
                          GUI_LOOK_FONT_SIZE, C(TEXT));
        cx += iw;
    }
    gui_raster_stroke_rect(p, 0, GUI_MENU_BAR_H - 1, w, 1, 1, C(BORDER));
}

void gui_menu_draw_popup(GuiPainterImpl *p, GuiMenu *m, int px, int py) {
    if (!p || !m) return;
    int w = gui_menu_popup_w(m);
    int h = m->count * POPUP_ROW;
    gui_raster_fill_rect(p, px, py, w, h, gui_theme_color(GUI_THEME_ROLE_CHECK_BG));
    int i = 0;
    for (GuiMenuItem *it = m->first; it; it = it->next, i++) {
        int ry = py + i * POPUP_ROW;
        if (it->sep) {
            gui_raster_stroke_rect(p, px + 4, ry + POPUP_ROW / 2, w - 8, 1, 1, C(BORDER));
            continue;
        }
        if (i == m->hover)
            gui_raster_fill_rect(p, px, ry, w, POPUP_ROW, C(BTN_HOVER));
        gui_color_t col = it->disabled ? C(TEXT_DIS) : C(TEXT);
        if (!it->disabled && it->label)
            gui_font_draw(p, px + MENU_PAD, ry + (POPUP_ROW - GUI_LOOK_FONT_SIZE) / 2,
                          it->label, (int)strlen(it->label), &g_builtin_font,
                          GUI_LOOK_FONT_SIZE, col);
    }
    gui_raster_stroke_rect(p, px, py, w, h, 1, C(BORDER));
}

/* ====================================================================== *
 * 对话框（GuiDialog）：纯内存对象，无窗口
 * ====================================================================== */

GuiDialog *gui_dialog_new(void) {
    GuiDialog *d = (GuiDialog *)calloc(1, sizeof(GuiDialog));
    if (d) d->result = GUI_DLG_RESULT_NONE;   /* open=0 / buttons=0 已归零 */
    return d;
}
void gui_dialog_free(GuiDialog *d) {
    if (!d) return;
    free(d->title);
    free(d->message);
    free(d);
}
int gui_dialog_set_title(GuiDialog *d, const char *title) {
    if (!d || !title) return -1;
    free(d->title);
    d->title = (char *)malloc(strlen(title) + 1);
    if (!d->title) return -1;
    strcpy(d->title, title);
    return 0;
}
int gui_dialog_set_message(GuiDialog *d, const char *msg) {
    if (!d || !msg) return -1;
    free(d->message);
    d->message = (char *)malloc(strlen(msg) + 1);
    if (!d->message) return -1;
    strcpy(d->message, msg);
    return 0;
}
int gui_dialog_set_buttons(GuiDialog *d, int mask) {
    if (!d) return -1;
    /* 只接受 GUI_DLG_* 位组合；未知位明确拒绝（Fail-Fast）。*/
    if (mask < 0 || (mask & ~(GUI_DLG_OK | GUI_DLG_CANCEL | GUI_DLG_YES | GUI_DLG_NO)) != 0)
        return -1;
    d->buttons = mask;
    return 0;
}
int gui_dialog_result(GuiDialog *d) {
    return d ? d->result : GUI_DLG_RESULT_NONE;
}
int gui_dialog_open(GuiDialog *d) {
    return d ? d->open : 0;
}

/* 对话框几何：box 在窗口内居中；按钮按 buttons 位从左到右排。
 * 返回 1 = 成功计算并写入 d->w/d->h；按钮 rect 通过 out_* 数组传出（最多 4）。*/
static int dialog_geom(GuiDialog *d, int win_w, int win_h,
                       int *bx, int *by, int *bw, int *bh,
                       int out_btn[4][4], int *out_nbtn) {
    if (!d || win_w <= 0 || win_h <= 0) return -1;
    int title_w = d->title ? gui_font_width(d->title, -1, &g_builtin_font,
                                            GUI_LOOK_FONT_SIZE) : 0;
    int msg_w = d->message ? gui_font_width(d->message, -1, &g_builtin_font,
                                            GUI_LOOK_FONT_SIZE) : 0;
    int inner = title_w > msg_w ? title_w : msg_w;
    /* 按钮行宽度 */
    const char *names[4] = {"OK", "Cancel", "Yes", "No"};
    int order[4] = {GUI_DLG_OK, GUI_DLG_CANCEL, GUI_DLG_YES, GUI_DLG_NO};
    int nbtn = 0, btn_total = 0;
    for (int k = 0; k < 4; k++) {
        if (d->buttons & order[k]) {
            int bw2 = gui_font_width(names[k], -1, &g_builtin_font, GUI_LOOK_FONT_SIZE)
                      + MENU_PAD * 2;
            out_btn[nbtn][2] = bw2 < 60 ? 60 : bw2;   /* w */
            out_btn[nbtn][3] = order[k];              /* 结果码 */
            btn_total += out_btn[nbtn][2] + 8;
            nbtn++;
        }
    }
    if (nbtn == 0) return -1;   /* 未设任何按钮：模态对话框无意义 */
    if (btn_total > inner) inner = btn_total;
    d->w = inner + DIALOG_PAD * 2;
    if (d->w < 220) d->w = 220;
    d->h = GUI_MENU_BAR_H + 28 + 44;   /* 标题条 + 消息行 + 按钮行 */
    *bx = (win_w - d->w) / 2;
    *by = (win_h - d->h) / 2;
    /* 按钮水平居中排布 */
    int total = 0;
    for (int k = 0; k < nbtn; k++) total += out_btn[k][2] + 8;
    total -= 8;
    int cx = *bx + (d->w - total) / 2;
    int by_btn = *by + d->h - 36;
    for (int k = 0; k < nbtn; k++) {
        out_btn[k][0] = cx;            /* x */
        out_btn[k][1] = by_btn;        /* y */
        out_btn[k][3] = out_btn[k][3]; /* 结果码（保持）*/
        cx += out_btn[k][2] + 8;
    }
    *out_nbtn = nbtn;
    *bw = d->w; *bh = d->h;
    return 1;
}

/* 对话框按钮命中：返回被点按钮的结果码（1/2/4/8），未命中返回 0。*/
static int dialog_hit(GuiDialog *d, int win_w, int win_h, int x, int y) {
    int bx, by, bw, bh, nbtn = 0;
    int btn[4][4];
    if (!dialog_geom(d, win_w, win_h, &bx, &by, &bw, &bh, btn, &nbtn))
        return 0;
    for (int k = 0; k < nbtn; k++) {
        int x0 = btn[k][0], y0 = btn[k][1], w0 = btn[k][2];
        if (x >= x0 && x < x0 + w0 && y >= y0 && y < y0 + 28)
            return btn[k][3];
    }
    return 0;
}

void gui_dialog_draw(GuiPainterImpl *p, GuiDialog *d, int win_w, int win_h) {
    if (!p || !d) return;
    int bx, by, bw, bh, nbtn = 0;
    int btn[4][4];
    if (!dialog_geom(d, win_w, win_h, &bx, &by, &bw, &bh, btn, &nbtn)) return;
    /* 遮罩：半透明深灰压暗背景（A=0x80）*/
    gui_raster_fill_rect(p, 0, 0, win_w, win_h,
                          gui_rgba(0x80, 0x10, 0x14, 0x1A));
    /* 对话框主体 */
    gui_raster_fill_rect(p, bx, by, bw, bh, gui_theme_color(GUI_THEME_ROLE_CHECK_BG));
    gui_raster_stroke_rect(p, bx, by, bw, bh, 1, C(BORDER));
    /* 标题条 */
    gui_raster_fill_rect(p, bx, by, bw, GUI_MENU_BAR_H, C(BTN));
    if (d->title)
        gui_font_draw(p, bx + DIALOG_PAD, by + (GUI_MENU_BAR_H - GUI_LOOK_FONT_SIZE) / 2,
                      d->title, (int)strlen(d->title), &g_builtin_font,
                      GUI_LOOK_FONT_SIZE, C(TEXT));
    /* 消息行 */
    if (d->message)
        gui_font_draw(p, bx + DIALOG_PAD, by + GUI_MENU_BAR_H + 6,
                      d->message, (int)strlen(d->message), &g_builtin_font,
                      GUI_LOOK_FONT_SIZE, C(TEXT));
    /* 按钮 */
    const char *names[4] = {"OK", "Cancel", "Yes", "No"};
    int code_to_name[4] = {GUI_DLG_OK, GUI_DLG_CANCEL, GUI_DLG_YES, GUI_DLG_NO};
    for (int k = 0; k < nbtn; k++) {
        int code = btn[k][3];
        const char *nm = "?";
        for (int c = 0; c < 4; c++) if (code_to_name[c] == code) { nm = names[c]; break; }
        int x0 = btn[k][0], y0 = btn[k][1], w0 = btn[k][2];
        gui_raster_fill_rect(p, x0, y0, w0, 28, C(BTN));
        gui_raster_stroke_rect(p, x0, y0, w0, 28, 1, C(BORDER));
        int tw = gui_font_width(nm, -1, &g_builtin_font, GUI_LOOK_FONT_SIZE);
        gui_font_draw(p, x0 + (w0 - tw) / 2, y0 + (28 - GUI_LOOK_FONT_SIZE) / 2,
                      nm, (int)strlen(nm), &g_builtin_font, GUI_LOOK_FONT_SIZE, C(TEXT));
    }
}

/* ====================================================================== *
 * 窗口级统一绘制 + 事件分发（菜单 / 对话框优先，否则委派控件树）
 * ====================================================================== */

int gui_window_draw(GuiWindowImpl *win) {
    if (!win || !win->painter) return -1;
    GuiPainterImpl *p = win->painter;
    /* 背景铺底（与控件树默认 BG 一致），再画控件树，覆盖层最后叠加。*/
    gui_raster_clear(p, C(BG));
    if (win->root) {
        if (gui_widget_draw(p, win->root) < 0) return -1;
    }
    if (win->menubar)
        gui_menu_draw_bar(p, win->menubar, win->w);
    if (win->popup)
        gui_menu_draw_popup(p, win->popup, win->popup_x, win->popup_y);
    if (win->dialog && win->dialog->open)
        gui_dialog_draw(p, win->dialog, win->w, win->h);
    return 0;
}

int gui_window_handle(GuiWindowImpl *win, int kind, int x, int y, int button) {
    if (!win) return -1;

    /* 1) 对话框（最高优先级，模态）：按钮点击 → 结果码 + SUBMITTED 信号。*/
    if (win->dialog && win->dialog->open) {
        if (kind == GUI_EV_MOUSE_MOVE) return 0;
        if (kind == GUI_EV_MOUSE_RELEASE && (button == 0 || button == 1)) {
            int res = dialog_hit(win->dialog, win->w, win->h, x, y);
            if (res > 0) {
                win->dialog->result = res;
                win->dialog->open = 0;
                if (win->root)
                    gui_signal_emit_ex(win->root, GUI_SIG_SUBMITTED, res, 0, x, y,
                                       GUI_WIDGET_DIALOG, win->dialog->tag,
                                       win->dialog->userdata);
                return 3;   /* 本次激活 */
            }
        }
        return 0;   /* 模态：对话框外点击忽略 */
    }

    /* 2) 弹出菜单：命中项 → SELECTED 信号；外部点击 → 关闭。*/
    if (win->popup) {
        if (kind == GUI_EV_MOUSE_MOVE) {
            win->popup->hover = popup_hit(win->popup, win->popup_x, win->popup_y, x, y);
            return 0;
        }
        if (kind == GUI_EV_MOUSE_RELEASE && (button == 0 || button == 1)) {
            int idx = popup_hit(win->popup, win->popup_x, win->popup_y, x, y);
            if (idx >= 0) {
                GuiMenuItem *it = menu_item_at(win->popup, idx);
                if (it && !it->sep && !it->disabled && win->root)
                    gui_signal_emit_ex(win->root, GUI_SIG_SELECTED, it->id, 0, x, y,
                                       GUI_WIDGET_MENU, win->popup->tag,
                                       win->popup->userdata);
                gui_menu_free(win->popup);   /* 弹出菜单所有权归窗口，选中即释放 */
                win->popup = NULL;
                return 3;
            }
            gui_menu_free(win->popup);
            win->popup = NULL;
            return 0;   /* 外部点击关闭弹窗 */
        }
        return 0;
    }

    /* 3) 顶栏菜单：悬停高亮 / 释放选中 → SELECTED 信号。*/
    if (win->menubar) {
        if (kind == GUI_EV_MOUSE_MOVE) {
            win->menubar->hover = bar_hit(win->menubar, win->w, x, y);
            return 0;
        }
        if (kind == GUI_EV_MOUSE_RELEASE && (button == 0 || button == 1)) {
            int idx = bar_hit(win->menubar, win->w, x, y);
            if (idx >= 0) {
                GuiMenuItem *it = menu_item_at(win->menubar, idx);
                if (it && !it->sep && !it->disabled && win->root)
                    gui_signal_emit_ex(win->root, GUI_SIG_SELECTED, it->id, 0, x, y,
                                       GUI_WIDGET_MENU, win->menubar->tag,
                                       win->menubar->userdata);
                return 3;
            }
        }
    }

    /* 4) 委派控件树（旧行为不变）。*/
    if (win->root)
        return gui_widget_handle(win->root, kind, x, y, button);
    return 0;
}
