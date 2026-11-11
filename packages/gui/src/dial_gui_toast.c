/*
 * dial_gui_toast.c —— gui.dialpkg 第 13 层：通知气泡 Toast（纯 C，三平台共用）
 *
 * 职责：一组「消息」按 FIFO 排队，每条自带生命周期；负责入队 / 出队 / 计时 /
 *      尺寸 / 绘制五件事。气泡自上而下堆叠在锚点处，最新的在最上。
 *
 * 硬边界（与第七 / 九 / 十 / 十一 / 十二层一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象。
 *   - **不读时钟**：生命周期完全由调用方以毫秒数推进（tick(dt_ms)），故三平台
 *     行为逐字节一致、可在无 DISPLAY 环境确定性单元测试。
 *
 * 所有权：每条消息的文本是 C 侧 strdup 自有副本，调用方不保留指针；free /
 *       clear 时一并释放。
 */
#include "dial_gui.h"

#include <stdlib.h>
#include <string.h>

/* 内置 8×8 点阵只读 f->kind=0，其余字段全零即可用于 gui_font_width / draw。*/
static GuiFontImpl g_builtin_font;

/* 简写配色：一律从主题表取，theme_set 一次改色整树立即生效。*/
#define C(role) gui_theme_color(GUI_THEME_ROLE_##role)

#define TOAST_PAD        8   /* 气泡内边距（像素）*/
#define TOAST_MIN_W     80   /* 气泡最小宽度 */
#define TOAST_MIN_H     28   /* 气泡最小高度 */
#define TOAST_DEFAULT_SPACING 6   /* 相邻气泡默认间距（像素）*/

/* 气泡种类（push 的 kind 参数）*/
#define TOAST_KIND_INFO  0
#define TOAST_KIND_WARN  1
#define TOAST_KIND_ERROR 2

/* 队列第 idx 条（0 = 最旧）。越界返回 NULL。*/
static GuiToastItem *toast_item_at(GuiToast *t, int idx) {
    if (!t || idx < 0 || idx >= t->count) return NULL;
    return &t->items[idx];
}

GuiToast *dial_gui_toast_new(void) {
    GuiToast *t = (GuiToast *)calloc(1, sizeof(GuiToast));
    if (t) {
        t->spacing = TOAST_DEFAULT_SPACING;
        t->w = TOAST_MIN_W;
        t->h = TOAST_MIN_H;
    }
    return t;
}

/* 释放队列里所有条目的文本（不清 count，由调用方决定是否顺带清零）。*/
static void toast_free_items(GuiToast *t) {
    if (!t) return;
    for (int i = 0; i < t->count; i++) {
        free(t->items[i].text);
        t->items[i].text = NULL;
    }
    t->count = 0;
}

int64_t dial_gui_toast_free(GuiToast *t) {
    if (!t) return -1;
    toast_free_items(t);
    free(t);
    return 0;
}

int64_t gui_toast_push(GuiToast *t, const char *text, int life_ms, int kind) {
    if (!t || !text || !text[0]) {
        gui_core_set_err(GUI_ERR_ARG, "空 GuiToast 句柄 / 空消息文本");
        return -1;
    }
    if (kind < TOAST_KIND_INFO || kind > TOAST_KIND_ERROR) {
        gui_core_set_err(GUI_ERR_ARG, "toast kind 越界（0 信息 / 1 警告 / 2 错误）");
        return -1;
    }
    /* 队列满：挤掉最旧的一条（FIFO），保证新消息总能进来。*/
    if (t->count >= GUI_TOAST_CAP) {
        free(t->items[0].text);
        for (int i = 1; i < t->count; i++) t->items[i - 1] = t->items[i];
        t->count--;
    }
    char *dup = (char *)malloc(strlen(text) + 1);
    if (!dup) {
        gui_core_set_err(GUI_ERR_ALLOC, "通知消息内存分配失败");
        return -1;
    }
    strcpy(dup, text);
    GuiToastItem *it = &t->items[t->count];
    it->text = dup;
    it->life_ms = life_ms;     /* <= 0 表示不过期 */
    it->elapsed_ms = 0;
    it->kind = kind;
    it->tag = 0;
    t->count++;
    return 0;
}

int64_t dial_gui_toast_count(GuiToast *t) {
    return t ? t->count : -1;
}

int64_t dial_gui_toast_clear(GuiToast *t) {
    if (!t) return -1;
    toast_free_items(t);
    return 0;
}

int64_t dial_gui_toast_tick(GuiToast *t, int dt_ms) {
    if (!t) return 0;
    if (dt_ms < 0) dt_ms = 0;
    /* 推进每条已存活时间，并把到期（life_ms > 0 且 elapsed >= life）的条目
     * 从队头挤掉（FIFO：最旧的先到期）。*/
    for (int i = 0; i < t->count; i++) {
        if (t->items[i].life_ms > 0) {
            t->items[i].elapsed_ms += dt_ms;
            if (t->items[i].elapsed_ms >= t->items[i].life_ms) {
                free(t->items[i].text);
                for (int k = i + 1; k < t->count; k++) t->items[k - 1] = t->items[k];
                t->count--;
                i--;   /* 索引前移，重新检查同一位置 */
                continue;
            }
        }
    }
    return t->count;
}

const char *dial_gui_toast_item_text(GuiToast *t, int idx) {
    GuiToastItem *it = toast_item_at(t, idx);
    return it ? (it->text ? it->text : "") : "";
}

int64_t dial_gui_toast_item_kind(GuiToast *t, int idx) {
    GuiToastItem *it = toast_item_at(t, idx);
    return it ? it->kind : -1;
}

int64_t dial_gui_toast_item_elapsed(GuiToast *t, int idx) {
    GuiToastItem *it = toast_item_at(t, idx);
    return it ? it->elapsed_ms : -1;
}

int64_t dial_gui_toast_item_life(GuiToast *t, int idx) {
    GuiToastItem *it = toast_item_at(t, idx);
    return it ? it->life_ms : -1;
}

int64_t dial_gui_toast_item_tag(GuiToast *t, int idx) {
    GuiToastItem *it = toast_item_at(t, idx);
    return it ? it->tag : -1;
}

int64_t dial_gui_toast_item_set_tag(GuiToast *t, int idx, int64_t tag) {
    GuiToastItem *it = toast_item_at(t, idx);
    if (!it) return -1;
    it->tag = tag;
    return 0;
}

int64_t dial_gui_toast_set_pos(GuiToast *t, int x, int y) {
    if (!t) return -1;
    t->x = x;
    t->y = y;
    return 0;
}

int64_t dial_gui_toast_pos_x(GuiToast *t) {
    return t ? t->x : -1;
}

int64_t dial_gui_toast_pos_y(GuiToast *t) {
    return t ? t->y : -1;
}

int64_t dial_gui_toast_set_size(GuiToast *t, int w, int h) {
    if (!t || w <= 0 || h <= 0) {
        gui_core_set_err(GUI_ERR_ARG, "toast size 非法（尺寸须为正）");
        return -1;
    }
    t->w = w;
    t->h = h;
    t->fixed_size = 1;      /* 显式设过尺寸 -> 不再按文本自适应 */
    return 0;
}

int64_t dial_gui_toast_size_w(GuiToast *t) {
    return t ? t->w : -1;
}

int64_t dial_gui_toast_size_h(GuiToast *t) {
    return t ? t->h : -1;
}

int64_t dial_gui_toast_set_spacing(GuiToast *t, int sp) {
    if (!t || sp < 0) {
        gui_core_set_err(GUI_ERR_ARG, "toast spacing 非法（须非负）");
        return -1;
    }
    t->spacing = sp;
    return 0;
}

int64_t dial_gui_toast_spacing(GuiToast *t) {
    return t ? t->spacing : -1;
}

int64_t dial_gui_toast_set_tag(GuiToast *t, int64_t tag) {
    if (!t) return -1;
    t->tag = tag;
    return 0;
}

int64_t dial_gui_toast_tag(GuiToast *t) {
    return t ? t->tag : -1;
}

int64_t dial_gui_toast_draw(GuiPainterImpl *p, GuiToast *t) {
    if (!p || !t) return -1;
    if (t->count <= 0) return 0;         /* 队列空不绘制 */
    int y = t->y;
    for (int i = 0; i < t->count; i++) {
        GuiToastItem *it = &t->items[i];
        if (!it->text || !it->text[0]) continue;
        int w = t->w, h = t->h;
        if (!t->fixed_size) {
            /* 按文本自适应：宽度 = 文本宽 + 内边距，下限 TOAST_MIN_W。*/
            int tw = gui_font_width(it->text, -1, &g_builtin_font, GUI_LOOK_FONT_SIZE);
            w = tw + TOAST_PAD * 2;
            if (w < TOAST_MIN_W) w = TOAST_MIN_W;
            h = GUI_LOOK_FONT_SIZE + TOAST_PAD * 2;
            if (h < TOAST_MIN_H) h = TOAST_MIN_H;
        }
        /* 底板：按种类取主题色（信息=面板底 / 警告=按钮底 / 错误=选区底）*/
        gui_color_t bg = C(BG);
        if (it->kind == TOAST_KIND_WARN) bg = C(BTN);
        else if (it->kind == TOAST_KIND_ERROR) bg = C(SEL);
        gui_raster_fill_rect(p, t->x, y, w, h, bg);
        gui_raster_stroke_rect(p, t->x, y, w, h, 1, C(BORDER));
        /* 文本 */
        gui_font_draw(p, t->x + TOAST_PAD, y + (h - GUI_LOOK_FONT_SIZE) / 2,
                      it->text, (int)strlen(it->text), &g_builtin_font,
                      GUI_LOOK_FONT_SIZE, C(TEXT));
        y += h + t->spacing;   /* 下一条往下堆叠 */
    }
    return 0;
}
