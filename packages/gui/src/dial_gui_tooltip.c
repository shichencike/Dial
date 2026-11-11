/*
 * dial_gui_tooltip.c —— gui.dialpkg 第 11 层：悬浮提示 Tooltip（纯 C，三平台共用）
 *
 * 职责：一小块带文本的提示框，支持「悬停一段时间才弹出」的延迟语义；
 *      负责文本存储 / 延迟推进 / 尺寸 / 位置 / 绘制五件事。
 *
 * 硬边界（与第七 / 九 / 十层一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象。
 *   - **不读时钟**：延迟完全由调用方以毫秒数推进（tick(dt_ms)），故三平台
 *     行为逐字节一致、可在无 DISPLAY 环境确定性单元测试。
 *
 * 所有权：文本是 C 侧 strdup 自有副本（与 GuiWidget.text / GuiMenuItem.label
 *         同口径），调用方不保留指针；free 时一并释放。
 */
#include "dial_gui.h"

#include <stdlib.h>
#include <string.h>

/* 内置 8×8 点阵只读 f->kind=0，其余字段全零即可用于 gui_font_width / draw。*/
static GuiFontImpl g_builtin_font;

/* 简写配色：一律从主题表取，theme_set 一次改色整树立即生效。*/
#define C(role) gui_theme_color(GUI_THEME_ROLE_##role)

#define TOOLTIP_PAD      6   /* 提示框内边距（像素）*/
#define TOOLTIP_MIN_W   40   /* 提示框最小宽度 */
#define TOOLTIP_MIN_H   20   /* 提示框最小高度 */
#define TOOLTIP_DEFAULT_DELAY 500   /* 默认悬停延迟（毫秒）*/

/* 文本未显式设尺寸时，按文本长度 + 内边距算（与内置点阵字宽一致）。*/
static void tooltip_auto_size(GuiTooltip *t) {
    if (!t) return;
    int tw = t->text ? gui_font_width(t->text, -1, &g_builtin_font, GUI_LOOK_FONT_SIZE) : 0;
    int w = tw + TOOLTIP_PAD * 2;
    if (w < TOOLTIP_MIN_W) w = TOOLTIP_MIN_W;
    int h = GUI_LOOK_FONT_SIZE + TOOLTIP_PAD * 2;
    if (h < TOOLTIP_MIN_H) h = TOOLTIP_MIN_H;
    t->w = w;
    t->h = h;
}

GuiTooltip *dial_gui_tooltip_new(void) {
    GuiTooltip *t = (GuiTooltip *)calloc(1, sizeof(GuiTooltip));
    if (t) {
        t->delay_ms = TOOLTIP_DEFAULT_DELAY;
        tooltip_auto_size(t);     /* 无文本时给最小尺寸；其余字段全零 */
    }
    return t;
}

int64_t dial_gui_tooltip_free(GuiTooltip *t) {
    if (!t) return -1;
    free(t->text);
    free(t);
    return 0;
}

int64_t gui_tooltip_set_text(GuiTooltip *t, const char *text) {
    if (!t) return -1;
    if (!text) {                  /* 清空文本：等价于无提示 */
        free(t->text);
        t->text = NULL;
        t->hover_ms = 0;
        t->shown = 0;
        tooltip_auto_size(t);
        return 0;
    }
    char *dup = (char *)malloc(strlen(text) + 1);
    if (!dup) return -1;
    strcpy(dup, text);
    free(t->text);
    t->text = dup;
    t->hover_ms = 0;             /* 换文本等价于换了目标：悬停计时归零 */
    t->shown = 0;
    tooltip_auto_size(t);
    return 0;
}

const char *dial_gui_tooltip_text(GuiTooltip *t) {
    return t ? (t->text ? t->text : "") : "";
}

int64_t dial_gui_tooltip_set_delay(GuiTooltip *t, int ms) {
    if (!t || ms < 0) {
        gui_core_set_err(GUI_ERR_ARG, "tooltip delay 非法（须非负）");
        return -1;
    }
    t->delay_ms = ms;
    return 0;
}

int64_t dial_gui_tooltip_delay(GuiTooltip *t) {
    return t ? t->delay_ms : -1;
}

int64_t dial_gui_tooltip_set_pos(GuiTooltip *t, int x, int y) {
    if (!t) return -1;
    t->x = x;
    t->y = y;
    return 0;
}

int64_t dial_gui_tooltip_pos_x(GuiTooltip *t) {
    return t ? t->x : -1;
}

int64_t dial_gui_tooltip_pos_y(GuiTooltip *t) {
    return t ? t->y : -1;
}

int64_t dial_gui_tooltip_set_size(GuiTooltip *t, int w, int h) {
    if (!t || w <= 0 || h <= 0) {
        gui_core_set_err(GUI_ERR_ARG, "tooltip size 非法（尺寸须为正）");
        return -1;
    }
    t->w = w;
    t->h = h;
    return 0;
}

int64_t dial_gui_tooltip_size_w(GuiTooltip *t) {
    return t ? t->w : -1;
}

int64_t dial_gui_tooltip_size_h(GuiTooltip *t) {
    return t ? t->h : -1;
}

int64_t dial_gui_tooltip_tick(GuiTooltip *t, int dt_ms) {
    if (!t) return 0;
    if (dt_ms < 0) dt_ms = 0;
    /* 无文本永不显示；计时只增不减（由 reset 归零）。*/
    if (!t->text || !t->text[0]) {
        t->shown = 0;
        return 0;
    }
    t->hover_ms += dt_ms;
    t->shown = (t->hover_ms >= t->delay_ms) ? 1 : 0;
    return t->shown;
}

int64_t dial_gui_tooltip_reset(GuiTooltip *t) {
    if (!t) return -1;
    t->hover_ms = 0;
    t->shown = 0;
    return 0;
}

int64_t dial_gui_tooltip_shown(GuiTooltip *t) {
    return t ? t->shown : 0;
}

int64_t dial_gui_tooltip_hover_ms(GuiTooltip *t) {
    return t ? t->hover_ms : -1;
}

int64_t dial_gui_tooltip_set_tag(GuiTooltip *t, int64_t tag) {
    if (!t) return -1;
    t->tag = tag;
    return 0;
}

int64_t dial_gui_tooltip_tag(GuiTooltip *t) {
    return t ? t->tag : -1;
}

int64_t dial_gui_tooltip_draw(GuiPainterImpl *p, GuiTooltip *t) {
    if (!p || !t) return -1;
    if (!t->shown || !t->text || !t->text[0]) return 0;   /* 未显示不绘制 */
    /* 底板：主题面板底色 + 细边框 */
    gui_raster_fill_rect(p, t->x, t->y, t->w, t->h, C(BG));
    gui_raster_stroke_rect(p, t->x, t->y, t->w, t->h, 1, C(BORDER));
    /* 文本：主题文本色 */
    gui_font_draw(p, t->x + TOOLTIP_PAD, t->y + (t->h - GUI_LOOK_FONT_SIZE) / 2,
                  t->text, (int)strlen(t->text), &g_builtin_font,
                  GUI_LOOK_FONT_SIZE, C(TEXT));
    return 0;
}
