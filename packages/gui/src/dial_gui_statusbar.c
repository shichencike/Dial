/*
 * dial_gui_statusbar.c —— gui.dialpkg 第 12 层：状态栏 StatusBar（纯 C，三平台共用）
 *
 * 职责：窗口底栏的纯值模型——左文本 + 右文本 + 可选进度条槽位；负责文本存储 /
 *      高度 / 值域 / 进度值 / 绘制五件事。
 *
 * 硬边界（与第七 / 九 / 十 / 十一层一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象。
 *   - 进度值是**纯值**并由调用方 set_value 推进，越界自动夹紧到 [vmin, vmax]，
 *     故本层不读时钟、不捕获鼠标，三平台行为逐字节一致。
 *
 * 所有权：左右两段文本都是 C 侧 strdup 自有副本（与 GuiWidget.text /
 *         GuiMenuItem.label 同口径），调用方不保留指针；free 时一并释放。
 */
#include "dial_gui.h"

#include <stdlib.h>
#include <string.h>

/* 内置 8×8 点阵只读 f->kind=0，其余字段全零即可用于 gui_font_width / draw。*/
static GuiFontImpl g_builtin_font;

/* 简写配色：一律从主题表取，theme_set 一次改色整树立即生效。*/
#define C(role) gui_theme_color(GUI_THEME_ROLE_##role)

#define SB_PAD        8   /* 状态栏左右内边距（像素）*/
#define SB_DEFAULT_H  22  /* 状态栏默认高度（像素，与顶栏同量级）*/
#define SB_PROG_W    120  /* 进度条槽位默认宽度（像素）*/
#define SB_MIN_H       8  /* 状态栏最小高度 */

/* 文本拷贝：空串等价于清空该段文本。*/
static int64_t sb_set_text(char **slot, const char *text) {
    if (!text || !text[0]) {
        free(*slot);
        *slot = NULL;
        return 0;
    }
    char *dup = (char *)malloc(strlen(text) + 1);
    if (!dup) return -1;
    strcpy(dup, text);
    free(*slot);
    *slot = dup;
    return 0;
}

GuiStatusBar *dial_gui_statusbar_new(void) {
    GuiStatusBar *sb = (GuiStatusBar *)calloc(1, sizeof(GuiStatusBar));
    if (sb) {
        sb->h = SB_DEFAULT_H;
        sb->vmin = 0;
        sb->vmax = 100;      /* 进度值域默认 0..100 */
        sb->value = 0;
    }
    return sb;
}

int64_t dial_gui_statusbar_free(GuiStatusBar *sb) {
    if (!sb) return -1;
    free(sb->left);
    free(sb->right);
    free(sb);
    return 0;
}

int64_t gui_statusbar_set_left(GuiStatusBar *sb, const char *text) {
    if (!sb) return -1;
    if (sb_set_text(&sb->left, text) < 0) {
        gui_core_set_err(GUI_ERR_ALLOC, "状态栏左文本内存分配失败");
        return -1;
    }
    return 0;
}

int64_t gui_statusbar_set_right(GuiStatusBar *sb, const char *text) {
    if (!sb) return -1;
    if (sb_set_text(&sb->right, text) < 0) {
        gui_core_set_err(GUI_ERR_ALLOC, "状态栏右文本内存分配失败");
        return -1;
    }
    return 0;
}

const char *dial_gui_statusbar_left(GuiStatusBar *sb) {
    return sb ? (sb->left ? sb->left : "") : "";
}

const char *dial_gui_statusbar_right(GuiStatusBar *sb) {
    return sb ? (sb->right ? sb->right : "") : "";
}

int64_t dial_gui_statusbar_set_height(GuiStatusBar *sb, int h) {
    if (!sb || h < SB_MIN_H) {
        gui_core_set_err(GUI_ERR_ARG, "状态栏高度非法（须 >= 8）");
        return -1;
    }
    sb->h = h;
    return 0;
}

int64_t dial_gui_statusbar_height(GuiStatusBar *sb) {
    return sb ? sb->h : -1;
}

int64_t dial_gui_statusbar_set_sep(GuiStatusBar *sb, int on) {
    if (!sb) return -1;
    sb->sep = on ? 1 : 0;
    return 0;
}

int64_t dial_gui_statusbar_sep(GuiStatusBar *sb) {
    return sb ? sb->sep : 0;
}

int64_t dial_gui_statusbar_enable_progress(GuiStatusBar *sb, int on) {
    if (!sb) return -1;
    sb->has_progress = on ? 1 : 0;
    return 0;
}

int64_t dial_gui_statusbar_has_progress(GuiStatusBar *sb) {
    return sb ? sb->has_progress : 0;
}

int64_t dial_gui_statusbar_set_range(GuiStatusBar *sb, int vmin, int vmax) {
    if (!sb || vmax < vmin) {
        gui_core_set_err(GUI_ERR_ARG, "状态栏进度值域非法（须 vmax >= vmin）");
        return -1;
    }
    sb->vmin = vmin;
    sb->vmax = vmax;
    if (sb->value < vmin) sb->value = vmin;   /* 值域收窄后同步夹紧当前值 */
    if (sb->value > vmax) sb->value = vmax;
    return 0;
}

int64_t dial_gui_statusbar_value_min(GuiStatusBar *sb) {
    return sb ? sb->vmin : -1;
}

int64_t dial_gui_statusbar_value_max(GuiStatusBar *sb) {
    return sb ? sb->vmax : -1;
}

int64_t dial_gui_statusbar_set_value(GuiStatusBar *sb, int v) {
    if (!sb) return -1;
    if (v < sb->vmin) v = sb->vmin;
    if (v > sb->vmax) v = sb->vmax;
    sb->value = v;
    return 0;
}

int64_t dial_gui_statusbar_value(GuiStatusBar *sb) {
    return sb ? sb->value : -1;
}

int64_t dial_gui_statusbar_set_tag(GuiStatusBar *sb, int64_t tag) {
    if (!sb) return -1;
    sb->tag = tag;
    return 0;
}

int64_t dial_gui_statusbar_tag(GuiStatusBar *sb) {
    return sb ? sb->tag : -1;
}

int64_t dial_gui_statusbar_draw(GuiPainterImpl *p, GuiStatusBar *sb, int win_w) {
    if (!p || !sb) return -1;
    if (win_w <= 0) return -1;
    int h = sb->h;
    /* 状态栏贴画布底边：y0 = 画布高 - 状态栏高；画布比状态栏还矮时贴顶。*/
    int y0 = p->h - h;
    if (y0 < 0) y0 = 0;
    /* 底板：面板底色 + 顶边细线 */
    gui_raster_fill_rect(p, 0, y0, win_w, h, C(BG));
    gui_raster_stroke_rect(p, 0, y0, win_w - 1, 1, 1, C(BORDER));
    /* 右文本：右对齐，贴右内边距 */
    int right_w = 0;
    if (sb->right && sb->right[0]) {
        int tw = gui_font_width(sb->right, -1, &g_builtin_font, GUI_LOOK_FONT_SIZE);
        right_w = tw + SB_PAD;
        gui_font_draw(p, win_w - tw - SB_PAD, y0 + (h - GUI_LOOK_FONT_SIZE) / 2,
                      sb->right, (int)strlen(sb->right), &g_builtin_font,
                      GUI_LOOK_FONT_SIZE, C(TEXT));
    }
    /* 左文本：贴左内边距 */
    if (sb->left && sb->left[0]) {
        gui_font_draw(p, SB_PAD, y0 + (h - GUI_LOOK_FONT_SIZE) / 2,
                      sb->left, (int)strlen(sb->left), &g_builtin_font,
                      GUI_LOOK_FONT_SIZE, C(TEXT));
    }
    /* 分隔竖线：左文本与右侧区域之间 */
    if (sb->sep && sb->left && sb->left[0]) {
        int lw = gui_font_width(sb->left, -1, &g_builtin_font, GUI_LOOK_FONT_SIZE);
        int sx = SB_PAD + lw + SB_PAD / 2;
        if (sx < win_w - right_w)
            gui_raster_stroke_rect(p, sx, y0 + 4, 1, h - 8, 1, C(BORDER));
    }
    /* 进度条槽位：贴右，轨道 + 填充（主题色），值域比例换算 */
    if (sb->has_progress) {
        int pw = SB_PROG_W;
        if (pw > win_w / 3) pw = win_w / 3;
        if (pw < 20) pw = 20;
        int px = win_w - pw - SB_PAD;
        int ph = h - 8;
        if (ph < 4) ph = 4;
        int py = y0 + (h - ph) / 2;
        gui_raster_fill_rect(p, px, py, pw, ph, C(TRACK));
        int span = sb->vmax - sb->vmin;
        int filled = 0;
        if (span > 0) filled = pw * (sb->value - sb->vmin) / span;
        else filled = (sb->value >= sb->vmax) ? pw : 0;
        if (filled > 0) {
            if (filled > pw) filled = pw;
            gui_raster_fill_rect(p, px, py, filled, ph, C(FILL));
        }
        gui_raster_stroke_rect(p, px, py, pw, ph, 1, C(BORDER));
    }
    return 0;
}
