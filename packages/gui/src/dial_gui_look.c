/*
 * dial_gui_look.c —— gui.dialpkg 成品控件外观层（纯 C，三平台共用）
 *
 * 职责：八种控件（BOX / LABEL / BUTTON / CHECKBOX / TEXTBOX / RADIO /
 * SLIDER / PROGRESS）的主题表驱动绘制 + 四态（普通 / 悬停 / 按下 / 禁用）
 * 配色 + 纯值事件推进（hovered / pressed / checked / value / 点击聚焦）。
 *
 * 硬边界（与内存宪法一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象、不碰 ARC。
 *     状态全是 GuiWidgetImpl 上的 C 侧纯值；Dial 侧用 widget_handle 喂纯值
 *     事件、用 widget_draw 主动绘制，C 从不主动调 Dial 函数。
 *   - 配色一律走 dial_gui_theme.c 的 gui_theme_color(role)：默认逐字等于
 *     GUI_THEME_* 宏，可由 theme_set 按角色覆写、theme_reset 逐值复位。
 *     字体用静态零分配的内置点阵对象（kind=0），三平台逐像素一致。
 *
 * 绘制不依赖 abs_*：前序递归自己累加相对几何得到绝对坐标，与框架层 hit_node
 * 同口径；父先画、后加子在上，保证「后加子优先」的绘制层序与命中层序一致。
 */
#include "dial_gui.h"

#include <stdlib.h>
#include <string.h>

/* 内置 8×8 点阵字体只读 f->kind，其余字段全零即可；静态对象零分配、
 * 永不释放（GUI 单线程事件循环，无需加锁）。绝不传给 gui_font_free。 */
static GuiFontImpl g_builtin_font;

/* 简写：所有配色都从主题表取，绝不直接引用 GUI_THEME_* 编译期常量，
 * 这样 theme_set 一次改色，整树下一次 widget_draw 立即生效。 */
#define C(role) gui_theme_color(GUI_THEME_ROLE_##role)

/* ---- 纯值事件推进 ------------------------------------------------------ */

/* 先遍历整树清掉旧标记，保证 hovered / pressed 全局各只有一个；
 * 清 pressed 时一并清 right_pressed（右键按下态与左键按下态平行）。 */
static void clear_flags(GuiWidgetImpl *n, int hovered, int pressed) {
    if (!n) return;
    if (hovered) n->hovered = 0;
    if (pressed) { n->pressed = 0; n->right_pressed = 0; }
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        clear_flags(c, hovered, pressed);
}

/* 第 16 层：在树中找一个当前展开的组合框（弹出下拉项需先定位）。 */
static GuiWidgetImpl *find_open_combo(GuiWidgetImpl *n) {
    if (!n) return NULL;
    if (n->kind == GUI_WIDGET_COMBO && n->combo_open) return n;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling) {
        GuiWidgetImpl *r = find_open_combo(c);
        if (r) return r;
    }
    return NULL;
}

int gui_widget_handle(GuiWidgetImpl *root, int kind, int x, int y, int button) {
    if (!root || !gui_widget_is_live(root) || button < 0) return -1;

    /* MOVE：清 hovered 后给命中控件置位；disabled 控件不悬停。 */
    if (kind == GUI_EV_MOUSE_MOVE) {
        clear_flags(root, 1, 0);
        GuiWidgetImpl *hit = gui_widget_hit(root, x, y, NULL, NULL);
        if (hit && !hit->disabled) hit->hovered = 1;
        return 0;
    }
    /* PRESS：清 pressed 后给命中控件置位；button 0（模拟）/ 1（左键）生效。
     * 第 16 层：button 3 = 右键（置 right_pressed 纯值态）；4 / 5 = 滚轮
     * （纯值推进，不读时钟，命中 SLIDER 即按其步进增减值）。 */
    if (kind == GUI_EV_MOUSE_PRESS) {
        clear_flags(root, 0, 1);
        if (button == 3) {
            GuiWidgetImpl *hit = gui_widget_hit(root, x, y, NULL, NULL);
            if (hit && !hit->disabled) hit->right_pressed = 1;
            return 0;
        }
        if (button == 4 || button == 5) {
            GuiWidgetImpl *hit = gui_widget_hit(root, x, y, NULL, NULL);
            if (hit && !hit->disabled && hit->kind == GUI_WIDGET_SLIDER) {
                int lo = hit->vmin, hi = hit->vmax;
                if (hi > lo) {
                    int span = hi - lo;
                    int step = span / 100;
                    if (step < 1) step = 1;
                    int oldv = hit->value;
                    int v = (button == 4) ? oldv - step : oldv + step;
                    if (v < lo) v = lo;
                    if (v > hi) v = hi;
                    if (v != oldv) {
                        hit->value = v;
                        gui_signal_emit(hit, GUI_SIG_VALUE_CHANGED, v, oldv, x, y);
                        return 1;
                    }
                }
            }
            return 0;
        }
        if (button == 0 || button == 1) {
            GuiWidgetImpl *hit = gui_widget_hit(root, x, y, NULL, NULL);
            if (hit && !hit->disabled) hit->pressed = 1;
        }
        return 0;
    }
    /* RELEASE：清 pressed；左键落在可交互控件上才「激活」一次。
     * 命中时顺带取回绝对 x，SLIDER 用它算局部 x，绝不依赖上一轮布局残留。 */
    if (kind == GUI_EV_MOUSE_RELEASE) {
        clear_flags(root, 0, 1);
        if (button != 0 && button != 1) return 0;   /* 右键 / 滚轮释放不激活 */

        /* 第 16 层：若已有组合框展开，优先判定其下拉项的命中（下拉区在控件
         * 矩形之外，普通 hit 命中不到，故用几何体单独算）。命中即选中并收起、
         * 发 SELECTED；点其它地方一律收起（不选中）。 */
        GuiWidgetImpl *oc = find_open_combo(root);
        if (oc) {
            int rx = x - oc->abs_x, ry = y - oc->abs_y;
            if (rx >= 0 && rx < oc->w && ry >= oc->h) {
                int row = (ry - oc->h) / GUI_MENU_ROW_H;
                if (row >= 0 && row < oc->item_count) {
                    oc->selected_idx = row;
                    oc->combo_open = 0;
                    gui_signal_emit(oc, GUI_SIG_SELECTED, oc->items[row].id, 0, x, y);
                    return 1;
                }
            }
            oc->combo_open = 0;
            return 0;
        }

        int hit_abs_x = 0;
        GuiWidgetImpl *hit = gui_widget_hit(root, x, y, &hit_abs_x, NULL);
        if (!hit || hit->disabled) return 0;
        /* 第 6 层信号槽埋点：状态推进后 emit 一条纯值信号。emit 内部先查
         * w->sig_mask，未订阅直接返回——零订阅=零开销，既有行为/CRC 不漂移。*/
        if (hit->kind == GUI_WIDGET_TEXTBOX) {
            /* 点击 TextBox 即把焦点移过去（焦点信号在 set_focus 内埋点）。 */
            gui_widget_set_focus(root, hit);
            return 1;
        }
        if (hit->kind == GUI_WIDGET_CHECKBOX) {
            int oldc = hit->checked;
            hit->checked = oldc ? 0 : 1;
            gui_signal_emit(hit, GUI_SIG_TOGGLED, hit->checked, oldc, x, y);
            return 1;
        }
        if (hit->kind == GUI_WIDGET_RADIO) {
            /* 选中本项；group != 0 时清掉同组其它 RADIO（纯 C 树遍历）。
             * 只对被点中的控件发 TOGGLED；同组被动清除不另发信号。 */
            int oldc = hit->checked;
            gui_widget_radio_select(root, hit);
            gui_signal_emit(hit, GUI_SIG_TOGGLED, hit->checked, oldc, x, y);
            return 1;
        }
        if (hit->kind == GUI_WIDGET_SLIDER) {
            /* 按命中点局部 x 比例设值，clamp 进 [vmin, vmax]；仅值真变才发信号。 */
            int oldv = hit->value;
            if (gui_widget_slider_set_from_x(hit, x - hit_abs_x))
                gui_signal_emit(hit, GUI_SIG_VALUE_CHANGED, hit->value, oldv, x, y);
            return 1;
        }
        if (hit->kind == GUI_WIDGET_BUTTON) {
            gui_signal_emit(hit, GUI_SIG_CLICKED, 1, 0, x, y);
            return 1;
        }
        /* 第 16 层：组合框（点击框体切换展开）+ 列表框（点击行选中）。 */
        if (hit->kind == GUI_WIDGET_COMBO) {
            hit->combo_open = hit->combo_open ? 0 : 1;   /* 切换下拉 */
            return 1;
        }
        if (hit->kind == GUI_WIDGET_LIST) {
            int row = (y - hit->abs_y) / GUI_MENU_ROW_H;
            if (row >= 0 && row < hit->item_count) {
                if (row < 64) hit->list_sel_bits = (int64_t)1 << row;
                else          hit->list_sel_bits = 0;
                gui_signal_emit(hit, GUI_SIG_SELECTED, hit->items[row].id, 0, x, y);
                return 1;
            }
            return 0;
        }
        /* PROGRESS 是只读展示控件，不参与激活。 */
        return 0;
    }
    /* 其它事件码（KEY / RESIZE / EXPOSE / CLOSE / QUIT …）不改变控件四态。 */
    return 0;
}

/* ---- 绘制 -------------------------------------------------------------- */

/*
 * 在 (x,y,w,h) 内画一行文本：center=1 水平居中，否则左对齐留 4px 边距；
 * 垂直一律居中。文本是 C 侧 NUL 结尾副本，长度用 strlen；内置点阵字高
 * 恰为 size（8 行 × sc，sc = size/8），因此 th 直接取 GUI_LOOK_FONT_SIZE。
 */
static void draw_text(GuiPainterImpl *p, const char *text,
                      int x, int y, int w, int h, gui_color_t c, int center) {
    if (!text || !*text || w <= 0 || h <= 0) return;
    int len = (int)strlen(text);
    int tw = gui_font_width(text, len, &g_builtin_font, GUI_LOOK_FONT_SIZE);
    int th = GUI_LOOK_FONT_SIZE;
    int ty = y + (h - th) / 2;
    if (ty < y) ty = y;
    int tx;
    if (center) {
        tx = x + (w - tw) / 2;
        if (tx < x) tx = x;
    } else {
        tx = x + 4;
    }
    gui_font_draw(p, tx, ty, text, len, &g_builtin_font, GUI_LOOK_FONT_SIZE, c);
}

/*
 * TextBox：白底 + 边框（焦点态加粗并换成高亮色）+ 选区背景 + 文本 + 光标竖线。
 * 文本区临时裁剪到 (ax+6, ay, w-12, h)，绝不溢出到相邻控件；绘制完恢复
 * 调用者原本的裁剪状态，不污染画布。光标恒定显示（不做闪烁），保证
 * get_pixel / painter_crc32 逐像素可断言、三平台一致。
 */
static void draw_textbox(GuiPainterImpl *p, GuiWidgetImpl *n,
                         int ax, int ay, int w, int h) {
    gui_color_t bg = n->disabled ? C(BTN_DIS) : C(CHECK_BG);
    gui_raster_fill_rect(p, ax, ay, w, h, bg);
    gui_color_t border = n->focused ? C(MARK) : C(BORDER);
    gui_raster_stroke_rect(p, ax, ay, w, h, n->focused ? 2 : 1, border);

    int tx = ax + 6, tw = w - 12;
    if (tw <= 0) return;
    int th = GUI_LOOK_FONT_SIZE;
    int ty = ay + (h - th) / 2;
    if (ty < ay) ty = ay;

    int saved_has = p->has_clip;
    int sx = p->clip_x, sy = p->clip_y, sw = p->clip_w, sh = p->clip_h;
    p->has_clip = 1;
    p->clip_x = tx; p->clip_y = ay; p->clip_w = tw; p->clip_h = h;

    const char *s = n->text ? n->text : "";
    int len = (int)strlen(s);
    int cursor = n->cursor < 0 ? 0 : (n->cursor > len ? len : n->cursor);
    int anchor = n->sel_anchor < 0 ? 0 : (n->sel_anchor > len ? len : n->sel_anchor);
    int a = anchor < cursor ? anchor : cursor;
    int b = anchor < cursor ? cursor : anchor;

    if (b > a && n->focused) {           /* 选区背景先铺，文字后画，保持可读 */
        int px0 = tx + gui_font_width(s, a, &g_builtin_font, GUI_LOOK_FONT_SIZE);
        int px1 = tx + gui_font_width(s, b, &g_builtin_font, GUI_LOOK_FONT_SIZE);
        gui_raster_fill_rect(p, px0, ty, px1 - px0, th, C(SEL));
    }
    gui_color_t tc = n->disabled ? C(TEXT_DIS) : C(TEXT);
    gui_font_draw(p, tx, ty, s, len, &g_builtin_font, GUI_LOOK_FONT_SIZE, tc);
    if (n->focused && !n->disabled) {
        int cx = tx + gui_font_width(s, cursor, &g_builtin_font, GUI_LOOK_FONT_SIZE);
        gui_raster_fill_rect(p, cx, ty, 2, th, C(MARK));
    }

    p->has_clip = saved_has;
    p->clip_x = sx; p->clip_y = sy; p->clip_w = sw; p->clip_h = sh;
}

/* CheckBox 的勾：两段折线，纯整数坐标，无浮点。 */
static void draw_check_mark(GuiPainterImpl *p, int bx, int by, int box) {
    gui_color_t c = C(MARK);
    int x1 = bx + box / 5;
    int y1 = by + box / 2;
    int x2 = bx + box * 2 / 5;
    int y2 = by + box * 3 / 4;
    int x3 = bx + box * 4 / 5;
    int y3 = by + box / 4;
    gui_raster_line(p, x1, y1, x2, y2, 2, c);
    gui_raster_line(p, x2, y2, x3, y3, 2, c);
}

/* SLIDER / PROGRESS 共用的整数比例：把 value 映射到 [0, width]。
 * value 先 clamp 进 [vmin, vmax]；值域退化（span <= 0）时返回 0，
 * 绝不除零、绝不静默溢出。 */
static int value_fill_width(const GuiWidgetImpl *n, int width) {
    if (width <= 0) return 0;
    int span = n->vmax - n->vmin;
    if (span <= 0) return 0;
    int v = n->value < n->vmin ? n->vmin :
            (n->value > n->vmax ? n->vmax : n->value);
    return (int)(((int64_t)(v - n->vmin) * width) / span);
}

static void draw_node(GuiPainterImpl *p, GuiWidgetImpl *n,
                      int ax, int ay, int *count) {
    if (!n->visible) return;                   /* 不可见：跳过整个子树 */
    int w = n->w, h = n->h;

    if (w > 0 && h > 0) {
        gui_color_t tc = n->disabled ? C(TEXT_DIS) : C(TEXT);
        switch (n->kind) {
        case GUI_WIDGET_BOX: {
            gui_raster_fill_rect(p, ax, ay, w, h, C(BG));
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, C(BORDER));
            break;
        }
        case GUI_WIDGET_LABEL: {
            /* 只画文本，不铺背景、不描边框。 */
            draw_text(p, n->text, ax, ay, w, h, tc, 0);
            break;
        }
        case GUI_WIDGET_BUTTON: {
            gui_color_t bg;
            if (n->disabled)     bg = C(BTN_DIS);
            else if (n->pressed) bg = C(BTN_DOWN);
            else if (n->hovered) bg = C(BTN_HOVER);
            else                 bg = C(BTN);
            gui_raster_fill_rect(p, ax, ay, w, h, bg);
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, C(BORDER));
            draw_text(p, n->text, ax, ay, w, h, tc, 1);
            break;
        }
        case GUI_WIDGET_TEXTBOX: {
            draw_textbox(p, n, ax, ay, w, h);
            break;
        }
        case GUI_WIDGET_CHECKBOX: {
            int box = h - 8;
            if (box > 18) box = 18;
            if (box < 6)  box = 6;
            int bx = ax + 4;
            int by = ay + (h - box) / 2;
            gui_raster_fill_rect(p, bx, by, box, box, C(CHECK_BG));
            gui_color_t border = n->disabled ? C(BORDER) : C(MARK);
            gui_raster_stroke_rect(p, bx, by, box, box, 1, border);
            if (n->checked) draw_check_mark(p, bx, by, box);
            /* 文本区从方框右侧开始，左对齐（剩余宽度可能为负，draw_text 会跳过）。 */
            draw_text(p, n->text, bx + box + 6, ay,
                      w - (box + 10), h, tc, 0);
            break;
        }
        case GUI_WIDGET_RADIO: {
            /* 圆圈 + 选中点 + 右侧文本；checked 复用 CHECKBOX 的语义。 */
            int r = h / 2 - 4;
            if (r > 10) r = 10;
            if (r < 4)  r = 4;
            int cx = ax + 4 + r;
            int cy = ay + h / 2;
            gui_raster_fill_circle(p, cx, cy, r, C(CHECK_BG));
            gui_raster_stroke_circle(p, cx, cy, r, 1,
                                     n->disabled ? C(BORDER) : C(MARK));
            if (n->checked)
                gui_raster_fill_circle(p, cx, cy, r / 2 + 1,
                                       n->disabled ? C(TEXT_DIS) : C(MARK));
            draw_text(p, n->text, ax + 4 + 2 * r + 6, ay,
                      w - (2 * r + 10), h, tc, 0);
            break;
        }
        case GUI_WIDGET_SLIDER: {
            int tx = ax + 4, tw = w - 8;
            if (tw <= 0) break;
            int th = 6;
            int ty = ay + (h - th) / 2;
            gui_raster_fill_rect(p, tx, ty, tw, th, C(TRACK));
            int fill = value_fill_width(n, tw);
            gui_raster_fill_rect(p, tx, ty, fill, th,
                                 n->disabled ? C(BORDER) : C(FILL));
            int kx = tx + fill;
            if (kx < tx) kx = tx;
            if (kx > tx + tw) kx = tx + tw;
            int kr = 6;
            gui_raster_fill_circle(p, kx, ty + th / 2, kr,
                                   n->disabled ? C(BTN_DIS) : C(KNOB));
            gui_raster_stroke_circle(p, kx, ty + th / 2, kr, 1,
                                     n->disabled ? C(BORDER) : C(MARK));
            break;
        }
        case GUI_WIDGET_PROGRESS: {
            /* 只读展示：轨道 + 边框 + 按 value 比例填充，RELEASE 不激活。 */
            gui_raster_fill_rect(p, ax, ay, w, h, C(TRACK));
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, C(BORDER));
            int iw = w - 4, ih = h - 4;
            if (iw <= 0 || ih <= 0) break;
            int fill = value_fill_width(n, iw);
            gui_raster_fill_rect(p, ax + 2, ay + 2, fill, ih,
                                 n->disabled ? C(BORDER) : C(FILL));
            break;
        }
        case GUI_WIDGET_COMBO: {
            /* 只读框（白底 + 边框 + 当前选中文本 + 右侧下拉三角）；展开时
             * 在控件矩形下方画下拉面板（逐行 + 选中行高亮）。面板在控件矩形
             * 之外，普通 hit 命中不到，故选中由 widget_handle 用几何体单独判定。*/
            gui_raster_fill_rect(p, ax, ay, w, h, n->disabled ? C(BTN_DIS) : C(CHECK_BG));
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, C(BORDER));
            const char *cur = (n->selected_idx >= 0 && n->selected_idx < n->item_count
                               && n->items[n->selected_idx].label)
                              ? n->items[n->selected_idx].label : "";
            draw_text(p, cur, ax, ay, w - 18, h, tc, 0);
            int ax0 = ax + w - 14, ay0 = ay + h / 2 - 3;   /* 下拉三角（两道折线）*/
            gui_raster_line(p, ax0, ay0, ax0 + 5, ay0 + 5, 1, C(TEXT));
            gui_raster_line(p, ax0 + 5, ay0 + 5, ax0 + 10, ay0, 1, C(TEXT));
            if (n->combo_open && n->item_count > 0) {
                int ph = n->item_count * GUI_MENU_ROW_H;
                int py = ay + h;
                gui_raster_fill_rect(p, ax, py, w, ph, C(BG));
                gui_raster_stroke_rect(p, ax, py, w, ph, 1, C(BORDER));
                for (int i = 0; i < n->item_count; ++i) {
                    int ry = py + i * GUI_MENU_ROW_H;
                    if (i == n->selected_idx)
                        gui_raster_fill_rect(p, ax, ry, w, GUI_MENU_ROW_H, C(SEL));
                    draw_text(p, n->items[i].label ? n->items[i].label : "",
                              ax, ry, w, GUI_MENU_ROW_H, tc, 0);
                }
            }
            break;
        }
        case GUI_WIDGET_LIST: {
            /* 列表框：底板 + 边框 + 每行（选中行高亮）；列表项存储在 items，
             * 不是子控件，行号由几何直接算出（row = (y - top) / ROW_H）。*/
            gui_raster_fill_rect(p, ax, ay, w, h, n->disabled ? C(BTN_DIS) : C(CHECK_BG));
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, C(BORDER));
            for (int i = 0; i < n->item_count; ++i) {
                int ry = ay + i * GUI_MENU_ROW_H;
                if (ry + GUI_MENU_ROW_H > ay + h) break;    /* 超出控件高度不画 */
                int bit = (i < 64) ? (int)((n->list_sel_bits >> i) & 1) : 0;
                if (bit) gui_raster_fill_rect(p, ax, ry, w, GUI_MENU_ROW_H, C(SEL));
                draw_text(p, n->items[i].label ? n->items[i].label : "",
                          ax, ry, w, GUI_MENU_ROW_H, tc, 0);
            }
            break;
        }
        default: {
            /* set_kind 已挡住非法值；这里只做防御，未知种类按 BOX 兜底。 */
            gui_raster_fill_rect(p, ax, ay, w, h, C(BG));
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, C(BORDER));
            break;
        }
        }
        (*count)++;
    }

    /* 前序递归：父已画完，后加子覆盖在上（与命中层序一致）。 */
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        draw_node(p, c, ax + c->x, ay + c->y, count);
}

int gui_widget_draw(GuiPainterImpl *p, GuiWidgetImpl *root) {
    if (!p || !p->pixels) return -1;
    if (!root || !gui_widget_is_live(root)) return -1;
    int count = 0;
    draw_node(p, root, 0, 0, &count);
    return count;                 /* >= 0 表示成功；core 包装转成 0 / -1 */
}

/* ---- 第 16 层：组合框 / 列表框实现（纯 C，零平台符号、零反向回调）------ *
 * 与外观层硬边界一致：只读写 GuiWidgetImpl 上的 C 侧纯值 / 自有副本，绝不碰
 * 平台 API、绝不 C → Dial 反向回调。label 是 strdup 副本，替换 / 释放由
 * widget.c 的 dispose_subtree 负责 free；项容量固定 GUI_COMBO_CAP（无动态扩容）。
 * 单选态：ComboBox 用 selected_idx；ListBox 用 64 位位图 list_sel_bits。 */

static int combo_idx_ok(const GuiWidgetImpl *w, int idx) {
    return w && idx >= 0 && idx < w->item_count;
}

int64_t dial_gui_combo_add_item(GuiWidgetImpl *w, int64_t id, DialString label) {
    if (!w || !gui_widget_is_live(w)) { gui_core_set_err(GUI_ERR_ARG, "组合框项添加失败（空句柄）"); return -1; }
    if (w->item_count >= GUI_COMBO_CAP) { gui_core_set_err(GUI_ERR_ARG, "组合框项已满"); return -1; }
    int64_t n = label ? dial_string_length(label) : 0;
    const char *p = label ? dial_string_cstr(label) : "";
    char *dup = (char *)malloc((size_t)n + 1);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "组合框项文本内存分配失败"); return -1; }
    memcpy(dup, p, (size_t)n); dup[n] = '\0';
    w->items[w->item_count].id = id;
    w->items[w->item_count].label = dup;
    w->item_count++;
    gui_core_ok();
    return 0;
}

int64_t dial_gui_combo_clear(GuiWidgetImpl *w) {
    if (!w) return -1;
    for (int i = 0; i < w->item_count; ++i) free(w->items[i].label);
    w->item_count = 0;
    w->selected_idx = -1;
    w->combo_open = 0;
    w->list_sel_bits = 0;
    gui_core_ok();
    return 0;
}

int64_t dial_gui_combo_count(GuiWidgetImpl *w) {
    return w ? (int64_t)w->item_count : -1;
}

int64_t dial_gui_combo_item_id(GuiWidgetImpl *w, int idx) {
    if (!combo_idx_ok(w, idx)) { gui_core_set_err(GUI_ERR_ARG, "组合框项索引越界"); return -1; }
    gui_core_ok();
    return w->items[idx].id;
}

const char *dial_gui_combo_item_label(GuiWidgetImpl *w, int idx) {
    if (!combo_idx_ok(w, idx)) return "";
    return w->items[idx].label ? w->items[idx].label : "";
}

int64_t dial_gui_combo_select(GuiWidgetImpl *w, int idx) {
    if (!combo_idx_ok(w, idx)) { gui_core_set_err(GUI_ERR_ARG, "组合框选中索引越界"); return -1; }
    w->selected_idx = idx;
    w->combo_open = 0;
    gui_core_ok();
    return 0;
}

int64_t dial_gui_combo_selected(GuiWidgetImpl *w) {
    return w ? (int64_t)w->selected_idx : -1;
}

int64_t dial_gui_combo_open(GuiWidgetImpl *w) {
    if (!w) return -1;
    w->combo_open = 1; gui_core_ok(); return 0;
}

int64_t dial_gui_combo_close(GuiWidgetImpl *w) {
    if (!w) return -1;
    w->combo_open = 0; gui_core_ok(); return 0;
}

int64_t dial_gui_combo_is_open(GuiWidgetImpl *w) {
    return w ? (int64_t)w->combo_open : 0;
}

int64_t dial_gui_list_add_item(GuiWidgetImpl *w, int64_t id, DialString label) {
    if (!w || !gui_widget_is_live(w)) { gui_core_set_err(GUI_ERR_ARG, "列表框项添加失败（空句柄）"); return -1; }
    if (w->item_count >= GUI_COMBO_CAP) { gui_core_set_err(GUI_ERR_ARG, "列表框项已满"); return -1; }
    int64_t n = label ? dial_string_length(label) : 0;
    const char *p = label ? dial_string_cstr(label) : "";
    char *dup = (char *)malloc((size_t)n + 1);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "列表框项文本内存分配失败"); return -1; }
    memcpy(dup, p, (size_t)n); dup[n] = '\0';
    w->items[w->item_count].id = id;
    w->items[w->item_count].label = dup;
    w->item_count++;
    gui_core_ok();
    return 0;
}

int64_t dial_gui_list_clear(GuiWidgetImpl *w) {
    if (!w) return -1;
    for (int i = 0; i < w->item_count; ++i) free(w->items[i].label);
    w->item_count = 0;
    w->selected_idx = -1;
    w->combo_open = 0;
    w->list_sel_bits = 0;
    gui_core_ok();
    return 0;
}

int64_t dial_gui_list_count(GuiWidgetImpl *w) {
    return w ? (int64_t)w->item_count : -1;
}

int64_t dial_gui_list_item_id(GuiWidgetImpl *w, int idx) {
    if (!combo_idx_ok(w, idx)) { gui_core_set_err(GUI_ERR_ARG, "列表框项索引越界"); return -1; }
    gui_core_ok();
    return w->items[idx].id;
}

const char *dial_gui_list_item_label(GuiWidgetImpl *w, int idx) {
    if (!combo_idx_ok(w, idx)) return "";
    return w->items[idx].label ? w->items[idx].label : "";
}

int64_t dial_gui_list_select(GuiWidgetImpl *w, int idx) {
    if (!combo_idx_ok(w, idx)) { gui_core_set_err(GUI_ERR_ARG, "列表框选中索引越界"); return -1; }
    w->list_sel_bits = (idx < 64) ? ((int64_t)1 << idx) : 0;
    gui_core_ok();
    return 0;
}

int64_t dial_gui_list_select_toggle(GuiWidgetImpl *w, int idx) {
    if (!combo_idx_ok(w, idx)) { gui_core_set_err(GUI_ERR_ARG, "列表框切换索引越界"); return -1; }
    if (idx < 64) w->list_sel_bits ^= ((int64_t)1 << idx);
    gui_core_ok();
    return 0;
}

int64_t dial_gui_list_is_selected(GuiWidgetImpl *w, int idx) {
    if (!combo_idx_ok(w, idx)) return -1;
    return (idx < 64) ? (int)((w->list_sel_bits >> idx) & 1) : 0;
}

int64_t dial_gui_list_selected_count(GuiWidgetImpl *w) {
    if (!w) return -1;
    int c = 0;
    for (int i = 0; i < w->item_count && i < 64; ++i)
        if ((w->list_sel_bits >> i) & 1) c++;
    return c;
}

int64_t dial_gui_list_selected_first(GuiWidgetImpl *w) {
    if (!w) return -1;
    for (int i = 0; i < w->item_count && i < 64; ++i)
        if ((w->list_sel_bits >> i) & 1) return i;
    return -1;
}

int64_t dial_gui_widget_right_pressed(GuiWidgetImpl *w) {
    return w ? (int64_t)w->right_pressed : -1;
}
