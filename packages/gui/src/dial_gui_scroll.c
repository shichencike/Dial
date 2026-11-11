/*
 * dial_gui_scroll.c —— gui.dialpkg 第 9 层：滚动视图（纯 C，三平台共用）
 *
 * 职责：视口裁剪 + 滚动条几何 / 命中 / 绘制 + 偏移推进（wheel / drag / scroll_to），
 *      作为窗口级辅助覆盖层，让内容大于视口的控件树可滚动浏览。
 *
 * 硬边界（与第七层菜单 / 对话框一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象。偏移是纯值，
 *     由 Dial 侧主动 scroll_to / wheel / drag 推进，再用 offset_x / offset_y 读回。
 *   - 视口裁剪复用 dial_gui_set_clip（调用方在绘制时按 off_x/off_y 平移内容并设裁剪）；
 *     滚动条绘制走 gui_raster_*，几何 / 绘制 CRC 逐字节不漂移。
 *   - 窗口接管 scroll 的所有权，关闭窗口时一并释放（见 dial_gui_core.c）；
 *     未挂窗的纯内存对象用 dial_gui_scroll_free 释放，可在无 DISPLAY 环境单元测试。
 *
 * which 约定：0 = 水平滚动条，1 = 垂直滚动条（与 enable 的掩码位一致）。
 */
#include "dial_gui.h"

#include <stdlib.h>
#include <string.h>

/* 简写配色：一律从主题表取，theme_set 一次改色整树立即生效。*/
#define C(role) gui_theme_color(GUI_THEME_ROLE_##role)

#define SCROLL_BAR_MIN 10   /* 滚动条最小厚度（与 GUI_MENU_BAR_H 量级一致）*/

/* 计算某方向滚动条的「整体矩形」(rx,ry,rw,rh) 与「滑块矩形」(tx,ty,tw,th)，
 * 坐标均为窗口内坐标。Dial 侧不读矩形，本函数仅供 hit / draw / drag 内部复用。*/
static void scroll_bar_geom(GuiScroll *s, int which,
                            int *rx, int *ry, int *rw, int *rh,
                            int *tx, int *ty, int *tw, int *th) {
    int has_x = s->x_enabled && s->content_w > s->vp_w;
    int has_y = s->y_enabled && s->content_h > s->vp_h;
    int bs = s->bar_size < SCROLL_BAR_MIN ? SCROLL_BAR_MIN : s->bar_size;
    if (which == 1) {   /* 垂直滚动条：贴视口右缘 */
        *rx = s->vp_x + s->vp_w - bs;
        *ry = s->vp_y;
        *rw = bs;
        *rh = s->vp_h - (has_x ? bs : 0);
        int track = *rh > 0 ? *rh : 1;
        int thumb = track * s->vp_h / s->content_h; if (thumb < 8) thumb = 8;
        int maxoff = s->content_h - s->vp_h; if (maxoff < 0) maxoff = 0;
        int ty0 = *ry + (track - thumb) * s->off_y / maxoff;
        *tx = *rx; *ty = ty0; *tw = *rw; *th = thumb;
    } else {            /* 水平滚动条：贴视口底边 */
        *rx = s->vp_x;
        *ry = s->vp_y + s->vp_h - bs;
        *rw = s->vp_w - (has_y ? bs : 0);
        *rh = bs;
        int track = *rw > 0 ? *rw : 1;
        int thumb = track * s->vp_w / s->content_w; if (thumb < 8) thumb = 8;
        int maxoff = s->content_w - s->vp_w; if (maxoff < 0) maxoff = 0;
        int tx0 = *rx + (track - thumb) * s->off_x / maxoff;
        *tx = tx0; *ty = *ry; *tw = thumb; *th = *rh;
    }
}

/* 把当前偏移 clamp 到合法范围（由视口 / 内容 / 启用标志决定）。*/
static void clamp_off(GuiScroll *s) {
    int max_x = (s->x_enabled && s->content_w > s->vp_w) ? s->content_w - s->vp_w : 0;
    int max_y = (s->y_enabled && s->content_h > s->vp_h) ? s->content_h - s->vp_h : 0;
    if (s->off_x < 0) s->off_x = 0; if (s->off_x > max_x) s->off_x = max_x;
    if (s->off_y < 0) s->off_y = 0; if (s->off_y > max_y) s->off_y = max_y;
}

GuiScroll *dial_gui_scroll_new(void) {
    GuiScroll *s = (GuiScroll *)calloc(1, sizeof(GuiScroll));
    if (s) s->bar_size = SCROLL_BAR_MIN;   /* 失败返回 NULL；其余字段全零 */
    return s;
}

void dial_gui_scroll_free(GuiScroll *s) {
    if (!s) return;
    free(s);
}

int dial_gui_scroll_set_viewport(GuiScroll *s, int x, int y, int w, int h) {
    if (!s || w <= 0 || h <= 0) {
        gui_core_set_err(GUI_ERR_ARG, "scroll viewport 非法（尺寸须为正）");
        return -1;
    }
    s->vp_x = x; s->vp_y = y; s->vp_w = w; s->vp_h = h;
    clamp_off(s);
    return 0;
}

int dial_gui_scroll_set_content(GuiScroll *s, int w, int h) {
    if (!s || w <= 0 || h <= 0) {
        gui_core_set_err(GUI_ERR_ARG, "scroll content 非法（尺寸须为正）");
        return -1;
    }
    s->content_w = w; s->content_h = h;
    clamp_off(s);
    return 0;
}

int dial_gui_scroll_enable(GuiScroll *s, int which, int on) {
    if (!s || which < 0 || which > 2) {
        gui_core_set_err(GUI_ERR_ARG, "scroll enable 越界（which: 0 横 / 1 纵 / 2 两者）");
        return -1;
    }
    int v = on ? 1 : 0;
    if (which == 0)      s->x_enabled = v;
    else if (which == 1) s->y_enabled = v;
    else                 { s->x_enabled = v; s->y_enabled = v; }
    clamp_off(s);
    return 0;
}

int dial_gui_scroll_set_tag(GuiScroll *s, int64_t tag) {
    if (!s) return -1;
    s->tag = tag;
    return 0;
}

int64_t dial_gui_scroll_tag(GuiScroll *s) {
    if (!s) return -1;
    return s->tag;
}

int dial_gui_scroll_offset_x(GuiScroll *s) {
    if (!s) return 0;
    return s->off_x;
}

int dial_gui_scroll_offset_y(GuiScroll *s) {
    if (!s) return 0;
    return s->off_y;
}

int dial_gui_scroll_scroll_to(GuiScroll *s, int x, int y) {
    if (!s) return -1;
    s->off_x = x; s->off_y = y;
    clamp_off(s);
    return 0;
}

int dial_gui_scroll_wheel(GuiScroll *s, int dy) {
    if (!s) return 0;
    s->off_y += dy;
    clamp_off(s);
    return s->off_y;
}

int dial_gui_scroll_hit(GuiScroll *s, int x, int y, int which) {
    if (!s) return 0;
    if (which != 0 && which != 1) return 0;
    if (which == 1 && !(s->y_enabled && s->content_h > s->vp_h)) return 0;
    if (which == 0 && !(s->x_enabled && s->content_w > s->vp_w)) return 0;
    int rx, ry, rw, rh, tx, ty, tw, th;
    scroll_bar_geom(s, which, &rx, &ry, &rw, &rh, &tx, &ty, &tw, &th);
    if (x >= rx && x < rx + rw && y >= ry && y < ry + rh) {
        if (x >= tx && x < tx + tw && y >= ty && y < ty + th) return 1;  /* 命中滑块 */
        return 2;                                                     /* 命中轨道 */
    }
    return 0;
}

int dial_gui_scroll_drag(GuiScroll *s, int x, int y, int which) {
    if (!s || (which != 0 && which != 1)) return (which == 1) ? s->off_y : s->off_x;
    int rx, ry, rw, rh, tx, ty, tw, th;
    scroll_bar_geom(s, which, &rx, &ry, &rw, &rh, &tx, &ty, &tw, &th);
    int track = (which == 1) ? rh : rw;
    int thumb = (which == 1) ? th : tw;
    int maxoff = (which == 1)
        ? (s->content_h > s->vp_h ? s->content_h - s->vp_h : 0)
        : (s->content_w > s->vp_w ? s->content_w - s->vp_w : 0);
    if (maxoff <= 0 || track <= thumb) return (which == 1) ? s->off_y : s->off_x;
    int pos;
    if (which == 1) pos = (y - ry - thumb / 2) * maxoff / (track - thumb);
    else             pos = (x - rx - thumb / 2) * maxoff / (track - thumb);
    if (pos < 0) pos = 0;
    if (pos > maxoff) pos = maxoff;
    if (which == 1) s->off_y = pos; else s->off_x = pos;
    return pos;
}

int dial_gui_scroll_set_bar_size(GuiScroll *s, int sz) {
    if (!s || sz < SCROLL_BAR_MIN) {
        gui_core_set_err(GUI_ERR_ARG, "scroll bar_size 过小");
        return -1;
    }
    s->bar_size = sz;
    return 0;
}

int dial_gui_scroll_draw_bar(GuiPainterImpl *p, GuiScroll *s, int which) {
    if (!p || !s) return -1;
    if (which == 1 && !(s->y_enabled && s->content_h > s->vp_h)) return 0;
    if (which == 0 && !(s->x_enabled && s->content_w > s->vp_w)) return 0;
    int rx, ry, rw, rh, tx, ty, tw, th;
    scroll_bar_geom(s, which, &rx, &ry, &rw, &rh, &tx, &ty, &tw, &th);
    gui_raster_fill_rect(p, rx, ry, rw, rh, C(TRACK));        /* 轨道 */
    gui_raster_stroke_rect(p, rx, ry, rw, rh, 1, C(BORDER));  /* 轨道边 */
    gui_raster_fill_rect(p, tx, ty, tw, th, C(BTN));          /* 滑块 */
    gui_raster_stroke_rect(p, tx, ty, tw, th, 1, C(BORDER));  /* 滑块边 */
    return 0;
}
