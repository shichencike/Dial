/*
 * dial_gui_look.c —— gui.dialpkg 成品控件外观层（纯 C，三平台共用）
 *
 * 职责：Label / Button / CheckBox 的默认主题绘制 + 四态（普通 / 悬停 / 按下 /
 * 禁用）配色 + 纯值事件推进（hovered / pressed / checked）。
 *
 * 硬边界（与内存宪法一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象、不碰 ARC。
 *     状态全是 GuiWidgetImpl 上的 C 侧纯值；Dial 侧用 widget_handle 喂纯值
 *     事件、用 widget_draw 主动绘制，C 从不主动调 Dial 函数。
 *   - 颜色写死在 dial_gui.h 的 GUI_THEME_*（内置浅色主题，本版不暴露 setter），
 *     字体用静态零分配的内置点阵对象（kind=0），绘制结果三平台逐像素一致。
 *
 * 绘制不依赖 abs_*：前序递归自己累加相对几何得到绝对坐标，与框架层 hit_node
 * 同口径；父先画、后加子在上，保证「后加子优先」的绘制层序与命中层序一致。
 */
#include "dial_gui.h"

#include <string.h>

/* 内置 8×8 点阵字体只读 f->kind，其余字段全零即可；静态对象零分配、
 * 永不释放（GUI 单线程事件循环，无需加锁）。绝不传给 gui_font_free。 */
static GuiFontImpl g_builtin_font;

/* ---- 纯值事件推进 ------------------------------------------------------ */

/* 先遍历整树清掉旧标记，保证 hovered / pressed 全局各只有一个。 */
static void clear_flags(GuiWidgetImpl *n, int hovered, int pressed) {
    if (!n) return;
    if (hovered) n->hovered = 0;
    if (pressed) n->pressed = 0;
    for (GuiWidgetImpl *c = n->first_child; c; c = c->next_sibling)
        clear_flags(c, hovered, pressed);
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
    /* PRESS：清 pressed 后给命中控件置位。button 0（模拟）/ 1（左键）生效。 */
    if (kind == GUI_EV_MOUSE_PRESS) {
        clear_flags(root, 0, 1);
        if (button == 0 || button == 1) {
            GuiWidgetImpl *hit = gui_widget_hit(root, x, y, NULL, NULL);
            if (hit && !hit->disabled) hit->pressed = 1;
        }
        return 0;
    }
    /* RELEASE：清 pressed；左键落在可交互控件上才「激活」一次。 */
    if (kind == GUI_EV_MOUSE_RELEASE) {
        clear_flags(root, 0, 1);
        if (button != 0 && button != 1) return 0;
        GuiWidgetImpl *hit = gui_widget_hit(root, x, y, NULL, NULL);
        if (!hit || hit->disabled) return 0;
        if (hit->kind == GUI_WIDGET_CHECKBOX) {
            hit->checked = hit->checked ? 0 : 1;
            return 1;
        }
        if (hit->kind == GUI_WIDGET_BUTTON) return 1;
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

/* CheckBox 的勾：两段折线，纯整数坐标，无浮点。 */
static void draw_check_mark(GuiPainterImpl *p, int bx, int by, int box) {
    gui_color_t c = GUI_THEME_MARK;
    int x1 = bx + box / 5;
    int y1 = by + box / 2;
    int x2 = bx + box * 2 / 5;
    int y2 = by + box * 3 / 4;
    int x3 = bx + box * 4 / 5;
    int y3 = by + box / 4;
    gui_raster_line(p, x1, y1, x2, y2, 2, c);
    gui_raster_line(p, x2, y2, x3, y3, 2, c);
}

static void draw_node(GuiPainterImpl *p, GuiWidgetImpl *n,
                      int ax, int ay, int *count) {
    if (!n->visible) return;                   /* 不可见：跳过整个子树 */
    int w = n->w, h = n->h;

    if (w > 0 && h > 0) {
        gui_color_t tc = n->disabled ? GUI_THEME_TEXT_DIS : GUI_THEME_TEXT;
        switch (n->kind) {
        case GUI_WIDGET_BOX: {
            gui_raster_fill_rect(p, ax, ay, w, h, GUI_THEME_BG);
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, GUI_THEME_BORDER);
            break;
        }
        case GUI_WIDGET_LABEL: {
            /* 只画文本，不铺背景、不描边框。 */
            draw_text(p, n->text, ax, ay, w, h, tc, 0);
            break;
        }
        case GUI_WIDGET_BUTTON: {
            gui_color_t bg;
            if (n->disabled)     bg = GUI_THEME_BTN_DIS;
            else if (n->pressed) bg = GUI_THEME_BTN_DOWN;
            else if (n->hovered) bg = GUI_THEME_BTN_HOVER;
            else                 bg = GUI_THEME_BTN;
            gui_raster_fill_rect(p, ax, ay, w, h, bg);
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, GUI_THEME_BORDER);
            draw_text(p, n->text, ax, ay, w, h, tc, 1);
            break;
        }
        case GUI_WIDGET_CHECKBOX: {
            int box = h - 8;
            if (box > 18) box = 18;
            if (box < 6)  box = 6;
            int bx = ax + 4;
            int by = ay + (h - box) / 2;
            gui_raster_fill_rect(p, bx, by, box, box, GUI_THEME_CHECK_BG);
            gui_color_t border = n->disabled ? GUI_THEME_BORDER : GUI_THEME_MARK;
            gui_raster_stroke_rect(p, bx, by, box, box, 1, border);
            if (n->checked) draw_check_mark(p, bx, by, box);
            /* 文本区从方框右侧开始，左对齐（剩余宽度可能为负，draw_text 会跳过）。 */
            draw_text(p, n->text, bx + box + 6, ay,
                      w - (box + 10), h, tc, 0);
            break;
        }
        default: {
            /* set_kind 已挡住非法值；这里只做防御，未知种类按 BOX 兜底。 */
            gui_raster_fill_rect(p, ax, ay, w, h, GUI_THEME_BG);
            gui_raster_stroke_rect(p, ax, ay, w, h, 1, GUI_THEME_BORDER);
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
