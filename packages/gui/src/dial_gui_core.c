/*
 * dial_gui_core.c —— gui.dialpkg 的 FFI 包装层
 *
 * 只做四件事：句柄生命周期 / 线程局部错误槽 / 每 App 事件槽 / 调后端 vtable。
 * 不碰像素（dial_gui_raster.c），不碰平台 API（dial_gui_x11.c / win32.c）。
 * 契约逐条对应 gui.diali；错误一律返回码 + 错误槽，不抛 Dial 异常。
 *
 * 平台后端在编译期按目标选定：Windows 编 win32 那份、POSIX 编 x11 那份。
 * 两后端共用同一套结构体与 vtable，core 里没有任何 #ifdef 平台逻辑。
 */
#include "dial_gui.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(_WIN32) || defined(_WIN64)
#define GUI_IS_WIN32 1
#endif

static const GuiBackend *backend(void) {
#ifdef GUI_IS_WIN32
    return gui_backend_win32();
#else
    return gui_backend_posix();
#endif
}

/* ---- 线程局部错误槽 ---------------------------------------------------- */
static _Thread_local int64_t  g_err = GUI_OK;
static _Thread_local char     g_err_msg[256] = {0};

void gui_core_set_err(int64_t code, const char *msg) {
    g_err = code;
    if (!msg) msg = "";
    strncpy(g_err_msg, msg, sizeof(g_err_msg) - 1);
    g_err_msg[sizeof(g_err_msg) - 1] = '\0';
}
void gui_core_ok(void) { gui_core_set_err(GUI_OK, ""); }

int64_t dial_gui_last_error(void) { return g_err; }
DialString dial_gui_last_error_msg(void) {
    return dial_string_new(g_err_msg[0] ? g_err_msg : "无错误");
}
int64_t dial_gui_clear_error(void) { gui_core_ok(); return 0; }

/* ---- 字符串工具 -------------------------------------------------------- */
static const char *str_data(DialString s, int64_t *len) {
    if (!s) { *len = 0; return ""; }
    *len = dial_string_length(s);
    return dial_string_cstr(s);
}
static char *str_dup(DialString s) {
    int64_t n = 0;
    const char *p = str_data(s, &n);
    char *out = (char *)malloc((size_t)n + 1);
    if (!out) return NULL;
    memcpy(out, p, (size_t)n);
    out[n] = '\0';
    return out;
}

/* ---- 事件槽 ------------------------------------------------------------ */
void gui_core_emit(GuiAppImpl *app, int kind, int x, int y, int button, int key,
                   int w, int h, GuiWindowImpl *win, GuiWidgetImpl *hit) {
    if (!app) return;
    app->ev_kind = kind; app->ev_x = x; app->ev_y = y;
    app->ev_button = button; app->ev_key = key;
    app->ev_w = w; app->ev_h = h; app->ev_win = win;
    app->ev_widget = hit;
    /* QUIT 是 core 自己合成的窗口级事件，绝不带上一轮的陈旧修饰键。 */
    if (kind == GUI_EV_QUIT) app->ev_mods = 0;
    /* 只写纯值 + C 侧借用指针；绝不反向调用 Dial 函数。 */
    gui_widget_fill_event(&app->ev_data, kind, x, y, button, key, w, h, win, hit);
    if (kind == GUI_EV_QUIT) app->quit = 1;
}

int gui_core_has_window(GuiAppImpl *app, const GuiWindowImpl *win) {
    if (!app) return 0;
    for (GuiWindowImpl *w = app->windows; w; w = w->next)
        if (w == win) return 1;
    return 0;
}

/* ---- 句柄检查 ---------------------------------------------------------- */
static int app_ok(GuiAppImpl *app) {
    if (!app || !app->be || !app->native) {
        gui_core_set_err(GUI_ERR_ARG, "空 GuiApp 句柄（或显示连接未建立）");
        return 0;
    }
    return 1;
}
static int win_ok(GuiWindowImpl *win) {
    if (!win || !win->app || !win->native || !gui_core_has_window(win->app, win)) {
        gui_core_set_err(GUI_ERR_ARG, "空 / 已释放的 GuiWindow 句柄");
        return 0;
    }
    return 1;
}
static int painter_ok(GuiPainterImpl *p) {
    if (!p || !p->pixels) {
        gui_core_set_err(GUI_ERR_ARG, "空 / 已释放的 GuiPainter 句柄");
        return 0;
    }
    return 1;
}
/* 控件句柄不进 ARC：靠全局根链表做 DFS 比指针，与 gui_core_has_window 同口径。 */
static int widget_ok(GuiWidgetImpl *w) {
    if (!w || !gui_widget_is_live(w)) {
        gui_core_set_err(GUI_ERR_ARG, "空 / 已释放的 GuiWidget 句柄");
        return 0;
    }
    return 1;
}

/* ---- 应用 / 事件循环 --------------------------------------------------- */

GuiAppImpl *dial_gui_app_new(void) {
    gui_core_ok();
    GuiAppImpl *app = (GuiAppImpl *)calloc(1, sizeof(GuiAppImpl));
    if (!app) { gui_core_set_err(GUI_ERR_ALLOC, "GuiApp 内存分配失败"); return NULL; }
    const GuiBackend *be = backend();
    char err[256] = {0};
    if (!be->init(app, err, (int)sizeof(err))) {
        gui_core_set_err(GUI_ERR_NO_BACKEND, err);
        free(app);
        return NULL;
    }
    app->be = be;
    app->ev_kind = GUI_EV_NONE;
    return app;
}

int64_t dial_gui_app_free(GuiAppImpl *app) {
    if (!app) { gui_core_set_err(GUI_ERR_ARG, "空 GuiApp 句柄"); return -1; }
    GuiWindowImpl *w = app->windows;
    while (w) {
        GuiWindowImpl *next = w->next;
        if (app->be) app->be->window_destroy(w);
        gui_painter_dispose(w->painter);
        w->painter = NULL;
        /* 窗口拥有的控件树必须在 free(win) 之前释放，避免悬垂与二次释放。 */
        if (w->root) gui_widget_dispose_tree(w->root);
        /* 第 7 层：窗口拥有的覆盖层一并释放（菜单/对话框所有权已转移给窗口）。*/
    if (w->menubar) gui_menu_free(w->menubar);
    if (w->popup)   gui_menu_free(w->popup);
    if (w->dialog)  gui_dialog_free(w->dialog);
    if (w->scroll)  dial_gui_scroll_free(w->scroll);
        free(w);
        w = next;
    }
    app->windows = NULL;
    if (app->be) app->be->fini(app);
    free(app);
    gui_core_ok();
    return 0;
}

int64_t dial_gui_app_quit(GuiAppImpl *app) {
    if (!app_ok(app)) return -1;
    app->quit = 1;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_app_should_quit(GuiAppImpl *app) {
    if (!app_ok(app)) return -1;
    gui_core_ok();
    return app->quit ? 1 : 0;
}

int64_t dial_gui_app_poll(GuiAppImpl *app) {
    if (!app_ok(app)) return -1;
    if (app->quit) {
        gui_core_emit(app, GUI_EV_QUIT, 0, 0, 0, 0, 0, 0, NULL, NULL);
        gui_core_ok();
        return GUI_EV_QUIT;
    }
    int k = app->be->poll(app);
    gui_core_ok();
    return (int64_t)k;
}

int64_t dial_gui_app_wait(GuiAppImpl *app, int64_t timeout_ms) {
    if (!app_ok(app)) return -1;
    if (app->quit) {
        gui_core_emit(app, GUI_EV_QUIT, 0, 0, 0, 0, 0, 0, NULL, NULL);
        gui_core_ok();
        return GUI_EV_QUIT;
    }
    int k = app->be->wait(app, (int)timeout_ms);
    gui_core_ok();
    return (int64_t)k;
}

/* ---- 事件字段访问器 ---------------------------------------------------- */
int64_t dial_gui_event_kind(GuiAppImpl *app)   { return app_ok(app) ? app->ev_kind : -1; }
int64_t dial_gui_event_x(GuiAppImpl *app)      { return app_ok(app) ? app->ev_x : -1; }
int64_t dial_gui_event_y(GuiAppImpl *app)      { return app_ok(app) ? app->ev_y : -1; }
int64_t dial_gui_event_button(GuiAppImpl *app) { return app_ok(app) ? app->ev_button : -1; }
int64_t dial_gui_event_key(GuiAppImpl *app)    { return app_ok(app) ? app->ev_key : -1; }
/* 修饰键位掩码（GUI_MOD_*）：由后端 pump 写进事件槽，core 不覆盖。 */
int64_t dial_gui_event_mods(GuiAppImpl *app)   { return app_ok(app) ? app->ev_mods : 0; }
int64_t dial_gui_event_width(GuiAppImpl *app)  { return app_ok(app) ? app->ev_w : -1; }
int64_t dial_gui_event_height(GuiAppImpl *app) { return app_ok(app) ? app->ev_h : -1; }
GuiWindowImpl *dial_gui_event_window(GuiAppImpl *app) {
    return app_ok(app) ? app->ev_win : NULL;
}

/* ---- 窗口 -------------------------------------------------------------- */

GuiWindowImpl *dial_gui_window_new(GuiAppImpl *app, DialString title,
                                   int64_t w, int64_t h) {
    gui_core_ok();
    if (!app_ok(app)) return NULL;
    if (w <= 0 || h <= 0 || w > 16384 || h > 16384) {
        gui_core_set_err(GUI_ERR_ARG, "窗口尺寸非法（须在 1..16384）");
        return NULL;
    }
    char *t = str_dup(title);
    if (!t) { gui_core_set_err(GUI_ERR_ALLOC, "窗口标题内存分配失败"); return NULL; }

    GuiWindowImpl *win = (GuiWindowImpl *)calloc(1, sizeof(GuiWindowImpl));
    GuiPainterImpl *p = gui_painter_new((int)w, (int)h, 0);
    if (!win || !p) {
        free(win); gui_painter_dispose(p); free(t);
        gui_core_set_err(GUI_ERR_ALLOC, "GuiWindow 内存分配失败");
        return NULL;
    }
    win->app = app; win->painter = p;
    win->w = (int)w; win->h = (int)h;
    strncpy(win->title, t, sizeof(win->title) - 1);
    win->title[sizeof(win->title) - 1] = '\0';
    p->window = win;
    free(t);

    char err[256] = {0};
    if (!app->be->window_create(win, win->title, win->w, win->h,
                                err, (int)sizeof(err))) {
        gui_core_set_err(GUI_ERR_WINDOW, err[0] ? err : "窗口创建失败");
        gui_painter_dispose(p);
        free(win);
        return NULL;
    }
    win->next = app->windows;
    app->windows = win;
    app->window_count++;
    return win;
}

int64_t dial_gui_window_close(GuiWindowImpl *win) {
    if (!win_ok(win)) return -1;
    GuiAppImpl *app = win->app;
    GuiWindowImpl **pp = &app->windows;
    while (*pp && *pp != win) pp = &(*pp)->next;
    if (*pp == win) *pp = win->next;
    app->window_count--;
    app->be->window_destroy(win);
    gui_painter_dispose(win->painter);
    win->painter = NULL;
    if (win->root) gui_widget_dispose_tree(win->root);
    /* 第 7 层：窗口拥有的覆盖层一并释放。*/
    if (win->menubar) gui_menu_free(win->menubar);
    if (win->popup)   gui_menu_free(win->popup);
    if (win->dialog)  gui_dialog_free(win->dialog);
    /* 第 9~13 层是**借用**槽位：窗口关闭只丢弃指针，绝不释放对象；
     * 应用仍持有句柄，可继续 free / 复用（见 dial_gui.h 结构注释）。*/
    free(win);
    gui_core_ok();
    return 0;
}

/* ---- 第 14 层：窗口级覆盖层挂载（5 层 × set/get/clear，逻辑同构）-------- *
 * 5 层三件套的实现完全相同，用一个宏一次生成，避免 15 份复制粘贴出现
 * 「某一层忘了清指针 / 忘了设错误槽」这类不对称缺陷。
 *
 * 语义一律为**借用**：窗口只持指针，不转移所有权、不释放对象；clear 只把
 * 槽位清空。这与第 10 层 GuiContext「set_menu 不转移菜单所有权」的约定一致，
 * 也避免应用继续 tick / scroll_to / push 时拿到悬空句柄。
 */
#define DEFINE_WINDOW_OVERLAY(lname, Type, field)                                  \
    int64_t dial_gui_window_set_##lname(GuiWindowImpl *win, Type *o) {             \
        if (!win_ok(win) || !o) {                                                  \
            gui_core_set_err(GUI_ERR_ARG, "窗口覆盖层挂载失败（空窗口或空对象）");    \
            return -1;                                                             \
        }                                                                          \
        win->field = o;                                                            \
        gui_core_ok();                                                             \
        return 0;                                                                  \
    }                                                                              \
    Type *dial_gui_window_##lname(GuiWindowImpl *win) {                           \
        return win_ok(win) ? win->field : NULL;                                    \
    }                                                                              \
    int64_t dial_gui_window_clear_##lname(GuiWindowImpl *win) {                   \
        if (!win_ok(win)) {                                                        \
            gui_core_set_err(GUI_ERR_ARG, "窗口覆盖层卸载失败（空窗口）");           \
            return -1;                                                             \
        }                                                                          \
        win->field = NULL;                                                         \
        gui_core_ok();                                                             \
        return 0;                                                                  \
    }

DEFINE_WINDOW_OVERLAY(scroll,    GuiScroll,    scroll)
DEFINE_WINDOW_OVERLAY(context,   GuiContext,   context)
DEFINE_WINDOW_OVERLAY(tooltip,   GuiTooltip,   tooltip)
DEFINE_WINDOW_OVERLAY(statusbar, GuiStatusBar, statusbar)
DEFINE_WINDOW_OVERLAY(toast,     GuiToast,     toast)
#undef DEFINE_WINDOW_OVERLAY

/*
 * 返回码统一（与 http/json/crypto 包同口径）：**成功返回 0，失败返回 -1**。
 * 后端 vtable 内部用 1=成功 / 0=失败，在这里一次性转换，Dial 侧只见 0/-1。
 */
static int64_t be_result(int ok_backend, const char *fail_msg) {
    if (!ok_backend) { gui_core_set_err(GUI_ERR_WINDOW, fail_msg); return -1; }
    gui_core_ok();
    return 0;
}

int64_t dial_gui_window_show(GuiWindowImpl *win) {
    if (!win_ok(win)) return -1;
    return be_result(win->app->be->window_show(win), "窗口显示失败");
}
int64_t dial_gui_window_set_title(GuiWindowImpl *win, DialString title) {
    if (!win_ok(win)) return -1;
    char *t = str_dup(title);
    if (!t) { gui_core_set_err(GUI_ERR_ALLOC, "窗口标题内存分配失败"); return -1; }
    strncpy(win->title, t, sizeof(win->title) - 1);
    win->title[sizeof(win->title) - 1] = '\0';
    free(t);
    return be_result(win->app->be->window_set_title(win, win->title), "修改窗口标题失败");
}
int64_t dial_gui_window_resize(GuiWindowImpl *win, int64_t w, int64_t h) {
    if (!win_ok(win)) return -1;
    if (w <= 0 || h <= 0 || w > 16384 || h > 16384) {
        gui_core_set_err(GUI_ERR_ARG, "窗口尺寸非法（须在 1..16384）");
        return -1;
    }
    int64_t r = be_result(win->app->be->window_resize(win, (int)w, (int)h),
                          "窗口尺寸调整失败");
    if (r == 0) { win->w = (int)w; win->h = (int)h; }
    return r;
}
int64_t dial_gui_window_width(GuiWindowImpl *win)  { return win_ok(win) ? win->w : -1; }
int64_t dial_gui_window_height(GuiWindowImpl *win) { return win_ok(win) ? win->h : -1; }

GuiPainterImpl *dial_gui_window_painter(GuiWindowImpl *win) {
    return win_ok(win) ? win->painter : NULL;
}

int64_t dial_gui_window_flush(GuiWindowImpl *win) {
    if (!win_ok(win)) return -1;
    return be_result(win->app->be->window_flush(win), "窗口上屏失败");
}

int64_t dial_gui_window_set_filter(GuiWindowImpl *win, DialString filter) {
    if (!win_ok(win)) return -1;
    char *f = str_dup(filter);
    if (!f) { gui_core_set_err(GUI_ERR_ALLOC, "过滤器名内存分配失败"); return -1; }
    int r = win->app->be->window_set_filter(win, f);
    free(f);
    if (!r) {
        gui_core_set_err(GUI_ERR_ARG, "未知过滤器（只接受 fast/good/best/nearest）");
        return -1;
    }
    gui_core_ok();
    return 0;
}

DialString dial_gui_window_backend(GuiWindowImpl *win) {
    if (!win_ok(win)) return dial_string_new("");
    const char *s = win->app->be->window_backend(win);
    return dial_string_new(s ? s : "");
}

/* ---- 离屏画布 ---------------------------------------------------------- */

GuiPainterImpl *dial_gui_painter_offscreen(int64_t w, int64_t h) {
    gui_core_ok();
    if (w <= 0 || h <= 0 || w > 16384 || h > 16384) {
        gui_core_set_err(GUI_ERR_ARG, "画布尺寸非法（须在 1..16384）");
        return NULL;
    }
    GuiPainterImpl *p = gui_painter_new((int)w, (int)h, 1);
    if (!p) gui_core_set_err(GUI_ERR_ALLOC, "GuiPainter 内存分配失败");
    return p;
}

int64_t dial_gui_painter_free(GuiPainterImpl *p) {
    if (!p) { gui_core_set_err(GUI_ERR_ARG, "空 GuiPainter 句柄"); return -1; }
    if (!p->owned || p->ext_pixels) {
        gui_core_set_err(GUI_ERR_ARG, "该画布由窗口拥有，请用 dial_gui_window_close 释放");
        return -1;
    }
    gui_painter_dispose(p);
    gui_core_ok();
    return 0;
}

int64_t dial_gui_painter_width(GuiPainterImpl *p)  { return painter_ok(p) ? p->w : -1; }
int64_t dial_gui_painter_height(GuiPainterImpl *p) { return painter_ok(p) ? p->h : -1; }

int64_t dial_gui_set_aa(GuiPainterImpl *p, int64_t level) {
    if (!painter_ok(p)) return -1;
    if (level != 1 && level != 2 && level != 4 && level != 8) {
        gui_core_set_err(GUI_ERR_ARG, "抗锯齿倍率只接受 1/2/4/8");
        return -1;
    }
    p->aa = (int)level;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_set_color(GuiPainterImpl *p, int64_t color) {
    if (!painter_ok(p)) return -1;
    p->color = (gui_color_t)(uint32_t)(uint64_t)color;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_clear(GuiPainterImpl *p, int64_t color) {
    if (!painter_ok(p)) return -1;
    gui_raster_clear(p, (gui_color_t)(uint32_t)(uint64_t)color);
    gui_core_ok();
    return 0;
}
int64_t dial_gui_set_clip(GuiPainterImpl *p, int64_t x, int64_t y, int64_t w, int64_t h) {
    if (!painter_ok(p)) return -1;
    if (w <= 0 || h <= 0) { gui_core_set_err(GUI_ERR_ARG, "裁剪区尺寸非法"); return -1; }
    p->has_clip = 1;
    p->clip_x = (int)x; p->clip_y = (int)y;
    p->clip_w = (int)w; p->clip_h = (int)h;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_clear_clip(GuiPainterImpl *p) {
    if (!painter_ok(p)) return -1;
    p->has_clip = 0;
    gui_core_ok();
    return 0;
}

int64_t dial_gui_fill_rect(GuiPainterImpl *p, int64_t x, int64_t y, int64_t w, int64_t h) {
    if (!painter_ok(p)) return -1;
    gui_raster_fill_rect(p, (int)x, (int)y, (int)w, (int)h, p->color);
    gui_core_ok(); return 0;
}
int64_t dial_gui_stroke_rect(GuiPainterImpl *p, int64_t x, int64_t y, int64_t w, int64_t h, int64_t t) {
    if (!painter_ok(p)) return -1;
    gui_raster_stroke_rect(p, (int)x, (int)y, (int)w, (int)h, (int)t, p->color);
    gui_core_ok(); return 0;
}
int64_t dial_gui_draw_line(GuiPainterImpl *p, int64_t x0, int64_t y0,
                           int64_t x1, int64_t y1, int64_t t) {
    if (!painter_ok(p)) return -1;
    gui_raster_line(p, (int)x0, (int)y0, (int)x1, (int)y1, (int)t, p->color);
    gui_core_ok(); return 0;
}
int64_t dial_gui_fill_circle(GuiPainterImpl *p, int64_t cx, int64_t cy, int64_t r) {
    if (!painter_ok(p)) return -1;
    gui_raster_fill_circle(p, (int)cx, (int)cy, (int)r, p->color);
    gui_core_ok(); return 0;
}
int64_t dial_gui_stroke_circle(GuiPainterImpl *p, int64_t cx, int64_t cy,
                               int64_t r, int64_t t) {
    if (!painter_ok(p)) return -1;
    gui_raster_stroke_circle(p, (int)cx, (int)cy, (int)r, (int)t, p->color);
    gui_core_ok(); return 0;
}
int64_t dial_gui_fill_ellipse(GuiPainterImpl *p, int64_t cx, int64_t cy,
                              int64_t rx, int64_t ry) {
    if (!painter_ok(p)) return -1;
    gui_raster_fill_ellipse(p, (int)cx, (int)cy, (int)rx, (int)ry, p->color);
    gui_core_ok(); return 0;
}
int64_t dial_gui_fill_trapezoid_aa(GuiPainterImpl *p, int64_t y0, int64_t y1,
                                   double x_tl, double x_tr,
                                   double x_bl, double x_br) {
    if (!painter_ok(p)) return -1;
    gui_raster_fill_trapezoid(p, (int)y0, (int)y1, x_tl, x_tr, x_bl, x_br, p->color);
    gui_core_ok(); return 0;
}

int64_t dial_gui_get_pixel(GuiPainterImpl *p, int64_t x, int64_t y) {
    if (!painter_ok(p)) return -1;
    gui_core_ok();
    return (int64_t)(uint64_t)gui_raster_get_pixel(p, (int)x, (int)y);
}
int64_t dial_gui_set_pixel(GuiPainterImpl *p, int64_t x, int64_t y, int64_t color) {
    if (!painter_ok(p)) return -1;
    gui_raster_set_pixel(p, (int)x, (int)y, (gui_color_t)(uint32_t)(uint64_t)color);
    gui_core_ok(); return 0;
}
int64_t dial_gui_blit_pixels(GuiPainterImpl *p, int64_t dx, int64_t dy,
                             int64_t w, int64_t h, DialString pixels) {
    if (!painter_ok(p)) return -1;
    int64_t n = 0;
    const char *buf = str_data(pixels, &n);
    if (n < w * h * 4) {
        gui_core_set_err(GUI_ERR_ARG, "像素缓冲长度不足（需要 w*h*4 字节）");
        return -1;
    }
    gui_raster_blit(p, (int)dx, (int)dy, (int)w, (int)h,
                    (const unsigned char *)buf, (int)n, 0);
    gui_core_ok(); return 0;
}
int64_t dial_gui_painter_crc32(GuiPainterImpl *p) {
    if (!painter_ok(p)) return -1;
    gui_core_ok();
    return (int64_t)(uint64_t)gui_raster_crc32(p);
}

/* ---- 字体 / 文本 ------------------------------------------------------- */

GuiFontImpl *dial_gui_font_default(void) {
    gui_core_ok();
    GuiFontImpl *f = gui_font_default();
    if (!f) gui_core_set_err(GUI_ERR_ALLOC, "内置点阵字体分配失败");
    return f;
}
GuiFontImpl *dial_gui_font_load(DialString path) {
    gui_core_ok();
    char *p = str_dup(path);
    if (!p) { gui_core_set_err(GUI_ERR_ALLOC, "字体路径内存分配失败"); return NULL; }
    char err[256] = {0};
    GuiFontImpl *f = gui_font_load(p, err, (int)sizeof(err));
    free(p);
    if (!f) gui_core_set_err(GUI_ERR_FONT, err[0] ? err : "字体加载失败");
    return f;
}
int64_t dial_gui_font_kind(GuiFontImpl *f) {
    if (!f) { gui_core_set_err(GUI_ERR_ARG, "空 GuiFont 句柄"); return -1; }
    gui_core_ok();
    return f->kind;
}
int64_t dial_gui_font_free(GuiFontImpl *f) {
    if (!f) { gui_core_set_err(GUI_ERR_ARG, "空 GuiFont 句柄"); return -1; }
    gui_font_free(f);
    gui_core_ok();
    return 0;
}

int64_t dial_gui_draw_text(GuiPainterImpl *p, int64_t x, int64_t y,
                           DialString text, GuiFontImpl *f, int64_t size) {
    if (!painter_ok(p)) return -1;
    if (!f || size <= 0) { gui_core_set_err(GUI_ERR_ARG, "字体句柄为空或字号非法"); return -1; }
    int64_t n = 0;
    const char *t = str_data(text, &n);
    int r = gui_font_draw(p, (int)x, (int)y, t, (int)n, f, (int)size, p->color);
    gui_core_ok();
    return (int64_t)r;
}
int64_t dial_gui_text_width(DialString text, GuiFontImpl *f, int64_t size) {
    if (!f || size <= 0) { gui_core_set_err(GUI_ERR_ARG, "字体句柄为空或字号非法"); return -1; }
    int64_t n = 0;
    const char *t = str_data(text, &n);
    int r = gui_font_width(t, (int)n, f, (int)size);
    gui_core_ok();
    return (int64_t)r;
}

/* ---- 平台信息 ---------------------------------------------------------- */
DialString dial_gui_backend(void) {
    gui_core_ok();
    const GuiBackend *be = backend();
    return dial_string_new(be->name);
}

/* ====================================================================== *
 * 控件框架层（gui.diali 后半段，28 个 extern fn）
 *
 * 全部只做：句柄校验 → 调 dial_gui_widget.c 的纯 C 逻辑 → 返回码 / 错误槽。
 * 不碰像素、不碰平台 API、绝不反向调用 Dial 函数。
 * ====================================================================== */

/* ---- 树 ---------------------------------------------------------------- */

GuiWidgetImpl *dial_gui_widget_tree(int64_t w, int64_t h) {
    gui_core_ok();
    if (w <= 0 || h <= 0 || w > 16384 || h > 16384) {
        gui_core_set_err(GUI_ERR_ARG, "控件树尺寸非法（须在 1..16384）");
        return NULL;
    }
    GuiWidgetImpl *r = gui_widget_tree_new((int)w, (int)h);
    if (!r) gui_core_set_err(GUI_ERR_ALLOC, "GuiWidget 内存分配失败");
    return r;
}

int64_t dial_gui_widget_tree_free(GuiWidgetImpl *root) {
    if (!widget_ok(root)) return -1;
    if (root->parent || root->win) {
        gui_core_set_err(GUI_ERR_ARG, "已挂到窗口的树由窗口释放，不要调用 widget_tree_free");
        return -1;
    }
    gui_widget_dispose_tree(root);
    gui_core_ok();
    return 0;
}

GuiWidgetImpl *dial_gui_widget_add(GuiWidgetImpl *parent) {
    gui_core_ok();
    if (!widget_ok(parent)) return NULL;
    GuiWidgetImpl *c = gui_widget_add(parent);
    if (!c) gui_core_set_err(GUI_ERR_ALLOC, "GuiWidget 内存分配失败");
    return c;
}

int64_t dial_gui_widget_remove(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    if (!w->parent) {
        gui_core_set_err(GUI_ERR_ARG, "根节点不可摘除，请用 widget_tree_free");
        return -1;
    }
    if (!gui_widget_remove(w)) {
        gui_core_set_err(GUI_ERR_ARG, "摘除控件失败（父节点链表不一致）");
        return -1;
    }
    gui_core_ok();
    return 0;
}

GuiWidgetImpl *dial_gui_widget_parent(GuiWidgetImpl *w)        { return widget_ok(w) ? w->parent : NULL; }
GuiWidgetImpl *dial_gui_widget_first_child(GuiWidgetImpl *w)  { return widget_ok(w) ? w->first_child : NULL; }
GuiWidgetImpl *dial_gui_widget_next_sibling(GuiWidgetImpl *w) { return widget_ok(w) ? w->next_sibling : NULL; }

/* ---- 几何 -------------------------------------------------------------- */

int64_t dial_gui_widget_set_rect(GuiWidgetImpl *w, int64_t x, int64_t y,
                                 int64_t ww, int64_t hh) {
    if (!widget_ok(w)) return -1;
    if (ww > 16384 || hh > 16384) {
        gui_core_set_err(GUI_ERR_ARG, "控件尺寸上限 16384；<= 0 表示该轴弹性分配");
        return -1;
    }
    w->req_x = (int)x; w->req_y = (int)y;
    w->req_w = (int)ww; w->req_h = (int)hh;
    /* 同步实际几何：即使尚未 layout_apply，getter 与命中测试也正确。 */
    w->x = (int)x; w->y = (int)y;
    w->w = ww > 0 ? (int)ww : 0;
    w->h = hh > 0 ? (int)hh : 0;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_x(GuiWidgetImpl *w)      { return widget_ok(w) ? w->x : -1; }
int64_t dial_gui_widget_y(GuiWidgetImpl *w)      { return widget_ok(w) ? w->y : -1; }
int64_t dial_gui_widget_width(GuiWidgetImpl *w)  { return widget_ok(w) ? w->w : -1; }
int64_t dial_gui_widget_height(GuiWidgetImpl *w) { return widget_ok(w) ? w->h : -1; }

/* ---- 可见 / id / 纯值标记 ---------------------------------------------- */

int64_t dial_gui_widget_set_visible(GuiWidgetImpl *w, int64_t v) {
    if (!widget_ok(w)) return -1;
    w->visible = v ? 1 : 0;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_visible(GuiWidgetImpl *w) { return widget_ok(w) ? w->visible : -1; }

int64_t dial_gui_widget_set_id(GuiWidgetImpl *w, DialString id) {
    if (!widget_ok(w)) return -1;
    /* C 侧 strdup 自有副本：输入 Dial string 不被接管，ARC 边界不越界。 */
    char *p = str_dup(id);
    if (!p) { gui_core_set_err(GUI_ERR_ALLOC, "控件 id 内存分配失败"); return -1; }
    free(w->id);
    w->id = p;
    gui_core_ok();
    return 0;
}
/* 返回 C 侧借用指针；Dial 侧必须 string_from_cstr 复制。 */
void *dial_gui_widget_id(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return NULL;
    return (void *)(w->id ? w->id : "");
}

int64_t dial_gui_widget_set_tag(GuiWidgetImpl *w, int64_t t) {
    if (!widget_ok(w)) return -1;
    w->tag = t; gui_core_ok(); return 0;
}
int64_t dial_gui_widget_tag(GuiWidgetImpl *w) { return widget_ok(w) ? w->tag : -1; }
int64_t dial_gui_widget_set_userdata(GuiWidgetImpl *w, int64_t u) {
    if (!widget_ok(w)) return -1;
    w->userdata = u; gui_core_ok(); return 0;
}
int64_t dial_gui_widget_userdata(GuiWidgetImpl *w) { return widget_ok(w) ? w->userdata : -1; }

/* ---- 布局 -------------------------------------------------------------- */

int64_t dial_gui_widget_set_layout(GuiWidgetImpl *w, int64_t mode) {
    if (!widget_ok(w)) return -1;
    if (mode != GUI_LAYOUT_ABS && mode != GUI_LAYOUT_VBOX &&
        mode != GUI_LAYOUT_HBOX && mode != GUI_LAYOUT_GRID) {
        gui_core_set_err(GUI_ERR_ARG, "布局模式只接受 0 ABS / 1 VBOX / 2 HBOX / 3 GRID");
        return -1;
    }
    w->layout = (int)mode;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_set_padding(GuiWidgetImpl *w, int64_t n) {
    if (!widget_ok(w)) return -1;
    w->padding = n > 0 ? (int)(n < 16384 ? n : 16384) : 0;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_set_gap(GuiWidgetImpl *w, int64_t n) {
    if (!widget_ok(w)) return -1;
    w->gap = n > 0 ? (int)(n < 16384 ? n : 16384) : 0;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_layout_apply(GuiWidgetImpl *root, int64_t w, int64_t h) {
    if (!widget_ok(root)) return -1;
    if (!gui_widget_layout_apply(root, (int)w, (int)h)) {
        gui_core_set_err(GUI_ERR_ARG, "布局尺寸非法（须为正）");
        return -1;
    }
    gui_core_ok();
    return 0;
}

/* ---- 第 16 层：网格布局（容器声明 cols/rows，子节点声明占格）------------- */

int64_t dial_gui_widget_set_grid(GuiWidgetImpl *w, int64_t cols, int64_t rows) {
    if (!widget_ok(w)) return -1;
    if (cols < 1 || rows < 1 || cols > 256 || rows > 256) {
        gui_core_set_err(GUI_ERR_ARG, "网格列/行须为 1..256（越界明确拒绝而非静默截断）");
        return -1;
    }
    w->grid_cols = (int)cols;
    w->grid_rows = (int)rows;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_set_grid_cell(GuiWidgetImpl *w, int64_t col, int64_t row,
                                      int64_t cspan, int64_t rspan) {
    if (!widget_ok(w)) return -1;
    if (col < 0 || row < 0 || cspan < 1 || rspan < 1 ||
        col > 255 || row > 255 || cspan > 256 || rspan > 256) {
        gui_core_set_err(GUI_ERR_ARG, "网格单元须 0..255 列/行、跨度 1..256（越界拒绝）");
        return -1;
    }
    w->grid_col = (int)col;
    w->grid_row = (int)row;
    w->grid_cspan = (int)cspan;
    w->grid_rspan = (int)rspan;
    gui_core_ok();
    return 0;
}

/* ---- 命中 / 挂窗 ------------------------------------------------------- */

GuiWidgetImpl *dial_gui_widget_hit(GuiWidgetImpl *root, int64_t x, int64_t y) {
    gui_core_ok();
    if (!widget_ok(root)) return NULL;
    return gui_widget_hit(root, (int)x, (int)y, NULL, NULL);
}

int64_t dial_gui_window_set_root(GuiWindowImpl *win, GuiWidgetImpl *root) {
    if (!win_ok(win) || !widget_ok(root)) return -1;
    if (!gui_widget_attach_window(root, win)) {
        gui_core_set_err(GUI_ERR_ARG,
                         "挂树失败：root 必须无父且未挂窗，且 win 尚未挂树");
        return -1;
    }
    gui_core_ok();
    return 0;
}

/* ---- 主动事件分发 ------------------------------------------------------ */

GuiWidgetImpl *dial_gui_event_widget(GuiAppImpl *app) {
    return app_ok(app) ? app->ev_widget : NULL;
}
/* 返回内嵌 packed 纯值块地址（非堆分配）；Dial 侧 ptr_cast 后只读。 */
void *dial_gui_event_data(GuiAppImpl *app) {
    return app_ok(app) ? (void *)&app->ev_data : NULL;
}

/* ====================================================================== *
 * 成品控件外观层（追加当轮的 gui.diali 末段，12 个追加 extern fn）
 *
 * 全部只做：句柄校验 → 读 / 写 GuiWidgetImpl 上的 C 侧纯值 → 或调
 * dial_gui_look.c 的纯 C 逻辑。不碰像素细节（交给 look.c / raster.c）、
 * 不碰平台 API、绝不反向调用 Dial 函数。旧 83 个包装零改动。
 * ====================================================================== */

/* ---- 控件种类 ---------------------------------------------------------- */

int64_t dial_gui_widget_set_kind(GuiWidgetImpl *w, int64_t kind) {
    if (!widget_ok(w)) return -1;
    /* 8 / 9 是第 7 层信号 kind 标记（MENU / DIALOG），不是可绘制的真实控件：
     * 上界虽扩到 11（容纳 10 COMBO / 11 LIST），仍必须显式拒绝 8 / 9，
     * 否则会把控件切成「未知种类」而落进 draw 的 BOX 兜底，静默破坏配色 / 激活。*/
    if (kind < GUI_WIDGET_BOX || kind > GUI_WIDGET_LIST ||
        kind == GUI_WIDGET_MENU || kind == GUI_WIDGET_DIALOG) {
        gui_core_set_err(GUI_ERR_ARG,
                         "控件种类只接受 0 BOX / 1 LABEL / 2 BUTTON / 3 CHECKBOX / "
                         "4 TEXTBOX / 5 RADIO / 6 SLIDER / 7 PROGRESS / 10 COMBO / 11 LIST"
                         "（8 / 9 为信号标记，不是控件）");
        return -1;
    }
    w->kind = (int)kind;
    /* 切成 / 切出 TEXTBOX 时把编辑态归零，绝不留着旧光标越界；若它正持有
     * 焦点又被切成别的种类，主动让焦点出栈（纯 C 树操作，绝不反向回调）。*/
    if (w->kind != GUI_WIDGET_TEXTBOX) {
        w->cursor = 0; w->sel_anchor = 0; w->max_len = 0;
        if (w->focused) gui_widget_set_focus(w, NULL);
    }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_kind(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->kind : -1;
}

/* ---- 文本（C 侧 strdup 自有副本）--------------------------------------- */

int64_t dial_gui_widget_set_text(GuiWidgetImpl *w, DialString text) {
    if (!widget_ok(w)) return -1;
    char *p = str_dup(text);
    if (!p) { gui_core_set_err(GUI_ERR_ALLOC, "控件文本内存分配失败"); return -1; }
    const char *old = w->text ? w->text : "";
    int old_len = (int)strlen(old);
    int changed = (strcmp(old, p) != 0);
    free(w->text);
    w->text = p;
    /* 显式换文本后旧光标 / 选区一律归零，绝不越界（键盘输入另走 input.c）。*/
    w->cursor = 0; w->sel_anchor = 0;
    /* 第 17 层：显式 set_text 是程序化赋值，建立新基线并清 undo/redo 两栈，
     * 绝不把程序化赋值记成一次用户编辑（快照只在键盘编辑时入栈）。*/
    gui_widget_undo_clear(w);
    /* 第 6 层：文本真变化才发一次 TEXT_CHANGED（未订阅=零开销）。*/
    if (changed)
        gui_signal_emit(w, GUI_SIG_TEXT_CHANGED, (int)strlen(p), old_len, 0, 0);
    gui_core_ok();
    return 0;
}
/* 返回 C 侧借用指针；Dial 侧必须 string_from_cstr 复制成 ARC string。 */
void *dial_gui_widget_text(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return NULL;
    return (void *)(w->text ? w->text : "");
}

/* ---- 勾选 / 禁用 / 运行时态 -------------------------------------------- */

int64_t dial_gui_widget_set_checked(GuiWidgetImpl *w, int64_t v) {
    if (!widget_ok(w)) return -1;
    int oldc = w->checked;
    w->checked = v ? 1 : 0;
    /* 第 6 层：勾选态真变化才发一次 TOGGLED（a=新 / b=旧）。*/
    if (oldc != w->checked)
        gui_signal_emit(w, GUI_SIG_TOGGLED, w->checked, oldc, 0, 0);
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_checked(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->checked : -1;
}
int64_t dial_gui_widget_set_disabled(GuiWidgetImpl *w, int64_t v) {
    if (!widget_ok(w)) return -1;
    w->disabled = v ? 1 : 0;
    if (w->disabled) {
        w->hovered = 0; w->pressed = 0;
        /* 禁用控件不可聚焦：让焦点出栈，避免焦点停在不可交互控件上。 */
        if (w->focused) gui_widget_set_focus(w, NULL);
    }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_disabled(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->disabled : -1;
}
int64_t dial_gui_widget_hovered(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->hovered : -1;
}
int64_t dial_gui_widget_pressed(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->pressed : -1;
}

/* ---- 纯值事件推进 / 整树绘制 ------------------------------------------- */

int64_t dial_gui_widget_handle(GuiWidgetImpl *root, int64_t kind, int64_t x,
                                int64_t y, int64_t button) {
    if (!widget_ok(root)) return -1;
    int r = gui_widget_handle(root, (int)kind, (int)x, (int)y, (int)button);
    if (r < 0) {
        gui_core_set_err(GUI_ERR_ARG, "widget_handle 参数非法（负 button / 无效根）");
        return -1;
    }
    gui_core_ok();
    return (int64_t)r;              /* 1 = 本次激活 / 0 = 无激活 */
}

int64_t dial_gui_widget_draw(GuiWidgetImpl *root, GuiPainterImpl *p) {
    if (!widget_ok(root) || !painter_ok(p)) return -1;
    if (gui_widget_draw(p, root) < 0) {
        gui_core_set_err(GUI_ERR_ARG, "控件树绘制失败（空画布 / 无效根）");
        return -1;
    }
    gui_core_ok();
    return 0;
}

/* ====================================================================== *
 * 输入层（追加当轮的 gui.diali 末段，14 个追加 extern fn：含 dial_gui_event_mods）
 *
 * 与前三层的硬边界一致：句柄校验 → 调 dial_gui_input.c 的纯 C 逻辑，或
 * 读 / 写 GuiWidgetImpl 上的 C 侧纯值。不碰像素细节、不碰平台 API、
 * 绝不反向调用 Dial 函数。旧 95 个包装零改动，abi = 6 不变。
 * ====================================================================== */

/* ---- 焦点链 ------------------------------------------------------------ */

int64_t dial_gui_widget_set_focus(GuiWidgetImpl *root, GuiWidgetImpl *w) {
    if (!widget_ok(root)) return -1;
    if (gui_widget_set_focus(root, w) < 0) {
        gui_core_set_err(GUI_ERR_ARG,
                         "设置焦点失败：目标不在该树内，或不是可见 / 未禁用的 TEXTBOX");
        return -1;
    }
    gui_core_ok();
    return 0;
}
GuiWidgetImpl *dial_gui_widget_focus(GuiWidgetImpl *root) {
    gui_core_ok();
    return widget_ok(root) ? gui_widget_focus(root) : NULL;
}
int64_t dial_gui_widget_focused(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->focused : -1;
}
GuiWidgetImpl *dial_gui_widget_focus_next(GuiWidgetImpl *root) {
    gui_core_ok();
    return widget_ok(root) ? gui_widget_focus_next(root, 0) : NULL;
}
GuiWidgetImpl *dial_gui_widget_focus_prev(GuiWidgetImpl *root) {
    gui_core_ok();
    return widget_ok(root) ? gui_widget_focus_next(root, 1) : NULL;
}

/* ---- 光标 / 选区（UTF-8 字节偏移）-------------------------------------- */

int64_t dial_gui_widget_cursor(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->cursor : -1;
}
int64_t dial_gui_widget_set_cursor(GuiWidgetImpl *w, int64_t pos) {
    if (!widget_ok(w)) return -1;
    if (gui_widget_set_cursor(w, (int)pos) < 0) {
        gui_core_set_err(GUI_ERR_ARG, "移动光标失败：该控件不是 TEXTBOX");
        return -1;
    }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_sel_start(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    return w->sel_anchor < w->cursor ? w->sel_anchor : w->cursor;
}
int64_t dial_gui_widget_sel_end(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    return w->sel_anchor < w->cursor ? w->cursor : w->sel_anchor;
}
int64_t dial_gui_widget_select(GuiWidgetImpl *w, int64_t a, int64_t b) {
    if (!widget_ok(w)) return -1;
    if (gui_widget_select(w, (int)a, (int)b) < 0) {
        gui_core_set_err(GUI_ERR_ARG, "设置选区失败：该控件不是 TEXTBOX");
        return -1;
    }
    gui_core_ok();
    return 0;
}

/* ---- 输入上限 ---------------------------------------------------------- */

int64_t dial_gui_widget_set_max_len(GuiWidgetImpl *w, int64_t n) {
    if (!widget_ok(w)) return -1;
    if (n < 0 || n > GUI_TEXT_MAX_LEN) {
        gui_core_set_err(GUI_ERR_ARG,
                         "max_len 须为 0（不限）..4096，越界明确拒绝而非静默截断");
        return -1;
    }
    w->max_len = (int)n;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_max_len(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->max_len : -1;
}

/* ---- 键盘纯值事件推进 -------------------------------------------------- */

int64_t dial_gui_widget_key(GuiWidgetImpl *root, int64_t kind, int64_t key,
                            int64_t mods) {
    if (!widget_ok(root)) return -1;
    int r = gui_widget_key(root, (int)kind, (int)key, (int)mods);
    if (r < 0) {
        gui_core_set_err(GUI_ERR_ARG, "widget_key 参数非法（无效根 / 内存分配失败）");
        return -1;
    }
    gui_core_ok();
    return (int64_t)r;   /* 0 无变化 / 1 文本或光标变化 / 2 焦点变化 / 3 Enter */
}

/* ====================================================================== *
 * 第 5 层：主题层 + 控件扩展（gui.diali 末段，11 个追加 extern fn）
 *
 * 与前四层硬边界一致：句柄校验 → 调 dial_gui_theme.c 的颜色表 / 读写
 * GuiWidgetImpl 上的 C 侧纯值。不碰像素细节（交给 look.c / raster.c）、
 * 不碰平台 API、绝不反向调用 Dial 函数。旧 109 个包装零改动，
 * abi = 6 / version = 1.10.0 / GuiWidgetEvent 104 字节布局全部不变。
 *
 * Fail-Fast（§20）：role 越界 / 颜色高 32 位非 0 / value 越界 / 值域倒置 /
 * 值域过大一律返回 -1 并写错误槽，绝不静默截断或溢出。
 * ====================================================================== */

/* ---- 主题颜色表（进程级静态，GUI 单线程事件循环无需加锁）-------------- */

int64_t dial_gui_theme_role_count(void) {
    gui_core_ok();
    return (int64_t)gui_theme_role_count();
}
int64_t dial_gui_theme_get(int64_t role) {
    int64_t c = gui_theme_get((int)role);
    if (c < 0) {
        gui_core_set_err(GUI_ERR_ARG, "主题角色越界（合法范围 0..13）");
        return -1;
    }
    gui_core_ok();
    return c;
}
int64_t dial_gui_theme_set(int64_t role, int64_t color) {
    if (role < 0 || role >= GUI_THEME_ROLE_COUNT) {
        gui_core_set_err(GUI_ERR_ARG, "主题角色越界（合法范围 0..13）");
        return -1;
    }
    if (color < 0 || color > 0xFFFFFFFFLL) {
        gui_core_set_err(GUI_ERR_ARG,
                         "主题颜色须为 0..0xFFFFFFFF 的 0xAARRGGBB（高 32 位必须为 0）");
        return -1;
    }
    if (gui_theme_set((int)role, (gui_color_t)color) < 0) {
        gui_core_set_err(GUI_ERR_ARG, "主题颜色覆写失败");
        return -1;
    }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_theme_reset(void) {
    gui_theme_reset();
    gui_core_ok();
    return 0;
}

/* ---- 控件值 / 值域 / 单选组（纯值读写）-------------------------------- */

int64_t dial_gui_widget_value(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->value : -1;
}
int64_t dial_gui_widget_set_value(GuiWidgetImpl *w, int64_t v) {
    if (!widget_ok(w)) return -1;
    if (v < w->vmin || v > w->vmax) {
        gui_core_set_err(GUI_ERR_ARG,
                         "控件值越出 [vmin, vmax]，明确拒绝而非静默 clamp");
        return -1;
    }
    int oldv = w->value;
    w->value = (int)v;
    /* 第 6 层：值真变化才发一次 VALUE_CHANGED（a=新 / b=旧）。*/
    if (oldv != w->value)
        gui_signal_emit(w, GUI_SIG_VALUE_CHANGED, w->value, oldv, 0, 0);
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_set_range(GuiWidgetImpl *w, int64_t lo, int64_t hi) {
    if (!widget_ok(w)) return -1;
    if (lo > hi) {
        gui_core_set_err(GUI_ERR_ARG, "值域非法：lo 不得大于 hi");
        return -1;
    }
    /* 值域跨度须能安全装进 int（SLIDER 绘制用整数比例），明确拒绝溢出。 */
    if (lo < -1000000000LL || hi > 1000000000LL) {
        gui_core_set_err(GUI_ERR_ARG, "值域绝对值过大（须在 ±1000000000 内）");
        return -1;
    }
    w->vmin = (int)lo;
    w->vmax = (int)hi;
    /* 已有 value 按新值域 clamp，getter 立刻可读回（不是不可见损坏）。*/
    if (w->value < w->vmin) w->value = w->vmin;
    if (w->value > w->vmax) w->value = w->vmax;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_min(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->vmin : -1;
}
int64_t dial_gui_widget_max(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->vmax : -1;
}
int64_t dial_gui_widget_set_group(GuiWidgetImpl *w, int64_t g) {
    if (!widget_ok(w)) return -1;
    w->group = g;      /* 纯值键，0 = 未分组；任意 int64 都合法 */
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_group(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->group : -1;
}

/* ====================================================================== *
 * 第 6 层：信号槽 / 控件事件订阅（gui.diali 末段，10 个追加 extern fn）
 *
 * 与前五层硬边界一致：句柄校验 → 读写 GuiWidgetImpl 上的 C 侧纯值 / 调
 * dial_gui_signal.c 的纯 C 队列。不碰像素细节、不碰平台 API、绝不反向调用
 * Dial 函数。旧 120 个包装零改动，abi = 6 / version = 1.10.0 /
 * GuiWidgetEvent 104 字节布局 / GuiWidgetSignal 72 字节布局全部不变。
 *
 * 信号块 100% 纯值、零指针零句柄：控件被摘除 / 树释放后，已入队信号仍是
 * 自洽整数快照，读数绝不悬垂。订阅掩码 0（默认）= 不订阅 = 零开销，
 * 因此不调用 subscribe 的旧程序行为与 CRC 逐字节不漂移。
 * ====================================================================== */

/* ---- 常量 -------------------------------------------------------------- */

int64_t dial_gui_signal_count(void) {
    gui_core_ok();
    return (int64_t)GUI_SIG_COUNT;
}
int64_t dial_gui_signal_capacity(void) {
    gui_core_ok();
    return (int64_t)GUI_SIGNAL_CAP;
}

/* ---- 订阅掩码（每控件一份 int64 纯值）---------------------------------- */

/* 覆盖式设置订阅掩码；0 = 取消全部订阅；越界 / 未知位明确拒绝（Fail-Fast）。*/
int64_t dial_gui_widget_subscribe(GuiWidgetImpl *w, int64_t mask) {
    if (!widget_ok(w)) return -1;
    if (mask < 0 || (mask & ~GUI_SIG_ALL) != 0) {
        gui_core_set_err(GUI_ERR_ARG,
                         "订阅掩码非法：只接受 0 或 GUI_SIG_* 位组合（0..63）");
        return -1;
    }
    w->sig_mask = mask;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_subscribed(GuiWidgetImpl *w) {
    return widget_ok(w) ? w->sig_mask : -1;
}
int64_t dial_gui_widget_unsubscribe(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    w->sig_mask = 0;
    gui_core_ok();
    return 0;
}

/* ---- 每树环形信号队列（Dial 主动 pending → data → pop）------------------ */

int64_t dial_gui_widget_signal_pending(GuiWidgetImpl *root) {
    return widget_ok(root) ? gui_signal_pending(root) : -1;
}
/* 返回队首 72 字节纯值块地址（C 借用）；空队列返回 NULL。Dial 侧须在下次
 * emit / pop / clear 之前 ptr_cast 读完，绝不长期持有（纯值无悬垂但会脏读）。*/
void *dial_gui_widget_signal_data(GuiWidgetImpl *root) {
    if (!widget_ok(root)) return NULL;
    return (void *)gui_signal_peek(root);
}
int64_t dial_gui_widget_signal_pop(GuiWidgetImpl *root) {
    if (!widget_ok(root)) return -1;
    int r = gui_signal_pop(root);
    gui_core_ok();
    return (int64_t)r;              /* 1 = 弹出一条 / 0 = 队列本空 */
}
int64_t dial_gui_widget_signal_clear(GuiWidgetImpl *root) {
    if (!widget_ok(root)) return -1;
    if (gui_signal_clear(root) < 0) {
        gui_core_set_err(GUI_ERR_ARG, "清空信号队列失败（无效根句柄）");
        return -1;
    }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_signal_dropped(GuiWidgetImpl *root) {
    return widget_ok(root) ? gui_signal_dropped(root) : -1;
}

/* ====================================================================== *
 * 第 7 层：菜单 / 对话框（gui.diali 末段，24 个追加 extern fn）
 *
 * 与前六层硬边界一致：句柄校验 → 调 dial_gui_menu.c 的纯 C 逻辑 / 读写
 * GuiWindowImpl 上的 C 侧纯值。不碰像素细节（交给 menu.c / raster.c）、
 * 不碰平台 API、绝不反向调用 Dial 函数。旧 130 个包装零改动，
 * abi = 6 / version = 1.10.0 / 所有既有纯值块布局全部不变。
 *
 * 菜单项 / 对话框按钮的选中事件复用 win->root 控件树信号队列
 * （GUI_SIG_SELECTED / GUI_SIG_SUBMITTED），因此要求窗口先 set_root；
 * 未 set_root 调用 set_menubar / popup_menu / dialog_show 一律 Fail-Fast -1。
 * 所有权：传给这三者的菜单/对话框由窗口在关闭时释放，调用方不要再 free。
 * ====================================================================== */

/* ---- 菜单（纯内存，无窗口）-------------------------------------------- */

GuiMenu *dial_gui_menu_new(void) {
    gui_core_ok();
    GuiMenu *m = gui_menu_new();
    if (!m) gui_core_set_err(GUI_ERR_ALLOC, "GuiMenu 内存分配失败");
    return m;
}
int64_t dial_gui_menu_free(GuiMenu *m) {
    if (!m) { gui_core_set_err(GUI_ERR_ARG, "空 GuiMenu 句柄"); return -1; }
    gui_menu_free(m);
    gui_core_ok();
    return 0;
}
int64_t dial_gui_menu_add_item(GuiMenu *m, int64_t id, DialString label) {
    if (!m) { gui_core_set_err(GUI_ERR_ARG, "空 GuiMenu 句柄"); return -1; }
    int64_t n = 0;
    const char *t = str_data(label, &n);
    char *dup = (char *)malloc((size_t)n + 1);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "菜单项标签内存分配失败"); return -1; }
    memcpy(dup, t, (size_t)n); dup[n] = '\0';
    if (gui_menu_add_item(m, id, dup) < 0) {
        free(dup);
        gui_core_set_err(GUI_ERR_ALLOC, "GuiMenuItem 内存分配失败");
        return -1;
    }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_menu_add_sep(GuiMenu *m) {
    if (!m) { gui_core_set_err(GUI_ERR_ARG, "空 GuiMenu 句柄"); return -1; }
    if (gui_menu_add_sep(m) < 0) { gui_core_set_err(GUI_ERR_ALLOC, "分隔符分配失败"); return -1; }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_menu_set_tag(GuiMenu *m, int64_t tag) {
    if (!m) { gui_core_set_err(GUI_ERR_ARG, "空 GuiMenu 句柄"); return -1; }
    gui_menu_set_tag(m, tag);
    gui_core_ok();
    return 0;
}
int64_t dial_gui_menu_tag(GuiMenu *m) {
    return m ? gui_menu_tag(m) : -1;
}
int64_t dial_gui_menu_count(GuiMenu *m) {
    return m ? (int64_t)gui_menu_count(m) : -1;
}
int64_t dial_gui_menu_item_id(GuiMenu *m, int64_t idx) {
    if (!m || idx < 0) { gui_core_set_err(GUI_ERR_ARG, "空 GuiMenu / 越界下标"); return -1; }
    int64_t r = gui_menu_item_id(m, (int)idx);
    if (r < 0) gui_core_set_err(GUI_ERR_ARG, "菜单项下标越界");
    return r;
}
/* 返回 C 侧借用指针；Dial 侧必须 string_from_cstr 复制成 ARC string。*/
void *dial_gui_menu_item_label(GuiMenu *m, int64_t idx) {
    if (!m || idx < 0) return NULL;
    return (void *)gui_menu_item_label(m, (int)idx);
}
int64_t dial_gui_menu_item_set_enabled(GuiMenu *m, int64_t idx, int64_t v) {
    if (!m || idx < 0) { gui_core_set_err(GUI_ERR_ARG, "空 GuiMenu / 越界下标"); return -1; }
    if (gui_menu_item_set_enabled(m, (int)idx, (int)v) < 0) {
        gui_core_set_err(GUI_ERR_ARG, "菜单项下标越界");
        return -1;
    }
    gui_core_ok();
    return 0;
}

/* ---- 悬浮提示 Tooltip（第 11 层；唯一需要 DialString 的入口走 FFI 包装）---- */

/* 文本入口：Dial 侧 string -> C 字符串副本（C 侧再 strdup 一份自有副本）。*/
int64_t dial_gui_tooltip_set_text(GuiTooltip *t, DialString text) {
    if (!t) { gui_core_set_err(GUI_ERR_ARG, "空 GuiTooltip 句柄"); return -1; }
    char *dup = str_dup(text);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "提示文本内存分配失败"); return -1; }
    int64_t r = gui_tooltip_set_text(t, dup);
    free(dup);
    if (r < 0) { gui_core_set_err(GUI_ERR_ALLOC, "提示文本内存分配失败"); return -1; }
    gui_core_ok();
    return 0;
}

/* ---- 状态栏 StatusBar（第 12 层；文本入口走 FFI 包装）------------------- */

/* 左文本入口：Dial 侧 string -> C 字符串副本（C 侧再 strdup 一份自有副本）。*/
int64_t dial_gui_statusbar_set_left(GuiStatusBar *sb, DialString text) {
    if (!sb) { gui_core_set_err(GUI_ERR_ARG, "空 GuiStatusBar 句柄"); return -1; }
    char *dup = str_dup(text);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "状态栏左文本内存分配失败"); return -1; }
    int64_t r = gui_statusbar_set_left(sb, dup);
    free(dup);
    if (r < 0) { gui_core_set_err(GUI_ERR_ALLOC, "状态栏左文本内存分配失败"); return -1; }
    gui_core_ok();
    return 0;
}

/* 右文本入口：同左文本。*/
int64_t dial_gui_statusbar_set_right(GuiStatusBar *sb, DialString text) {
    if (!sb) { gui_core_set_err(GUI_ERR_ARG, "空 GuiStatusBar 句柄"); return -1; }
    char *dup = str_dup(text);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "状态栏右文本内存分配失败"); return -1; }
    int64_t r = gui_statusbar_set_right(sb, dup);
    free(dup);
    if (r < 0) { gui_core_set_err(GUI_ERR_ALLOC, "状态栏右文本内存分配失败"); return -1; }
    gui_core_ok();
    return 0;
}

/* ---- 通知气泡 Toast（第 13 层；入队入口走 FFI 包装）-------------------- */

/* 入队入口：Dial 侧 string -> C 字符串副本（C 侧再 strdup 一份自有副本）。*/
int64_t dial_gui_toast_push(GuiToast *t, DialString text, int64_t life_ms, int64_t kind) {
    if (!t) { gui_core_set_err(GUI_ERR_ARG, "空 GuiToast 句柄"); return -1; }
    char *dup = str_dup(text);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "通知消息内存分配失败"); return -1; }
    int64_t r = gui_toast_push(t, dup, (int)life_ms, (int)kind);
    free(dup);
    if (r < 0) return -1;   /* 内部已写好错误槽（空文本 / kind 越界 / 分配失败）*/
    gui_core_ok();
    return 0;
}

/* ---- 顶栏菜单 / 弹出菜单（窗口级，所有权转移给窗口）------------------- */

int64_t dial_gui_window_set_menubar(GuiWindowImpl *win, GuiMenu *m) {    if (!win_ok(win)) return -1;
    if (!win->root) {
        gui_core_set_err(GUI_ERR_ARG, "菜单需要窗口先 set_root（选中信号进 root 树队列）");
        return -1;
    }
    if (!m) { gui_core_set_err(GUI_ERR_ARG, "空 GuiMenu 句柄"); return -1; }
    if (win->menubar && win->menubar != m) gui_menu_free(win->menubar);
    win->menubar = m;
    win->menubar_h = GUI_MENU_BAR_H;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_window_popup_menu(GuiWindowImpl *win, GuiMenu *m,
                                   int64_t x, int64_t y) {
    if (!win_ok(win)) return -1;
    if (!win->root) {
        gui_core_set_err(GUI_ERR_ARG, "弹出菜单需要窗口先 set_root（选中信号进 root 树队列）");
        return -1;
    }
    if (!m) { gui_core_set_err(GUI_ERR_ARG, "空 GuiMenu 句柄"); return -1; }
    if (x < 0 || y < 0 || x > 16384 || y > 16384) {
        gui_core_set_err(GUI_ERR_ARG, "弹出菜单锚点越界");
        return -1;
    }
    if (win->popup && win->popup != m) gui_menu_free(win->popup);
    win->popup = m;
    win->popup_x = (int)x;
    win->popup_y = (int)y;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_window_close_popup(GuiWindowImpl *win) {
    if (!win_ok(win)) return -1;
    if (win->popup) { gui_menu_free(win->popup); win->popup = NULL; }
    gui_core_ok();
    return 0;
}

/* ---- 对话框（窗口级，show 后所有权转移给窗口）------------------------- */

GuiDialog *dial_gui_dialog_new(void) {
    gui_core_ok();
    GuiDialog *d = gui_dialog_new();
    if (!d) gui_core_set_err(GUI_ERR_ALLOC, "GuiDialog 内存分配失败");
    return d;
}
int64_t dial_gui_dialog_free(GuiDialog *d) {
    if (!d) { gui_core_set_err(GUI_ERR_ARG, "空 GuiDialog 句柄"); return -1; }
    /* 仅用于未挂窗的对话框；已 show 的对话框由窗口释放，调用方不要再 free。*/
    gui_dialog_free(d);
    gui_core_ok();
    return 0;
}
int64_t dial_gui_dialog_set_title(GuiDialog *d, DialString title) {
    if (!d) { gui_core_set_err(GUI_ERR_ARG, "空 GuiDialog 句柄"); return -1; }
    int64_t n = 0;
    const char *t = str_data(title, &n);
    char *dup = (char *)malloc((size_t)n + 1);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "对话框标题内存分配失败"); return -1; }
    memcpy(dup, t, (size_t)n); dup[n] = '\0';
    if (gui_dialog_set_title(d, dup) < 0) { free(dup); return -1; }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_dialog_set_message(GuiDialog *d, DialString msg) {
    if (!d) { gui_core_set_err(GUI_ERR_ARG, "空 GuiDialog 句柄"); return -1; }
    int64_t n = 0;
    const char *t = str_data(msg, &n);
    char *dup = (char *)malloc((size_t)n + 1);
    if (!dup) { gui_core_set_err(GUI_ERR_ALLOC, "对话框消息内存分配失败"); return -1; }
    memcpy(dup, t, (size_t)n); dup[n] = '\0';
    if (gui_dialog_set_message(d, dup) < 0) { free(dup); return -1; }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_dialog_set_buttons(GuiDialog *d, int64_t mask) {
    if (!d) { gui_core_set_err(GUI_ERR_ARG, "空 GuiDialog 句柄"); return -1; }
    if (gui_dialog_set_buttons(d, (int)mask) < 0) {
        gui_core_set_err(GUI_ERR_ARG,
                         "按钮掩码只接受 GUI_DLG_OK=1 / CANCEL=2 / YES=4 / NO=8 的组合");
        return -1;
    }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_dialog_show(GuiWindowImpl *win, GuiDialog *d) {
    if (!win_ok(win)) return -1;
    if (!win->root) {
        gui_core_set_err(GUI_ERR_ARG, "对话框需要窗口先 set_root（按钮信号进 root 树队列）");
        return -1;
    }
    if (!d) { gui_core_set_err(GUI_ERR_ARG, "空 GuiDialog 句柄"); return -1; }
    if (win->dialog && win->dialog != d) gui_dialog_free(win->dialog);
    win->dialog = d;
    d->open = 1;
    d->result = GUI_DLG_RESULT_NONE;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_dialog_hide(GuiWindowImpl *win) {
    if (!win_ok(win)) return -1;
    if (win->dialog) win->dialog->open = 0;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_dialog_result(GuiDialog *d) {
    return d ? (int64_t)gui_dialog_result(d) : (int64_t)GUI_DLG_RESULT_NONE;
}
int64_t dial_gui_dialog_open(GuiDialog *d) {
    return d ? (int64_t)gui_dialog_open(d) : 0;
}

/* ---- 统一绘制 + 事件分发（菜单/对话框优先，否则委派控件树）------------- */

int64_t dial_gui_window_draw(GuiWindowImpl *win) {
    if (!win_ok(win)) return -1;
    if (gui_window_draw(win) < 0) {
        gui_core_set_err(GUI_ERR_ARG, "窗口绘制失败（空画布）");
        return -1;
    }
    gui_core_ok();
    return 0;
}
int64_t dial_gui_window_handle(GuiWindowImpl *win, int64_t kind, int64_t x,
                               int64_t y, int64_t button) {
    if (!win_ok(win)) return -1;
    int r = gui_window_handle(win, (int)kind, (int)x, (int)y, (int)button);
    if (r < 0) {
        gui_core_set_err(GUI_ERR_ARG, "window_handle 参数非法（负 button / 无效窗口）");
        return -1;
    }
    gui_core_ok();
    return (int64_t)r;   /* 3 = 本次菜单/对话框激活 / 1 = 控件激活 / 0 = 无 */
}

/* ====================================================================== *
 * 第 17 层（16 个追加 extern fn）：TextBox 撤销 / 重做 + Slider 拖动 +
 * 每控件主题覆写 + Grid 自动换行。
 *
 * 与前十六层同一条硬边界：只做「句柄校验 → 读写 GuiWidgetImpl 上的 C 侧
 * 纯值」，绝不碰平台 API、绝不 C → Dial 反向回调、旧 302 个签名一字不动。
 * 全部 : int 一律 int64_t（Dial int 是 64 位，返回 -1 不能被零扩展）。
 * ====================================================================== */

/* ---- TextBox 撤销 / 重做 ---------------------------------------------- *
 * 快照只在键盘编辑（input.c replace_range）成功改文本时入栈；显式
 * set_text 建立新基线并清两栈。栈空返回 0（不是错误，绝不写错误槽）。*/
int64_t dial_gui_widget_undo(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    int r = gui_widget_undo(w);
    gui_core_ok();
    return (int64_t)r;                       /* 1 撤销成功 / 0 栈空 */
}
int64_t dial_gui_widget_redo(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    int r = gui_widget_redo(w);
    gui_core_ok();
    return (int64_t)r;                       /* 1 重做成功 / 0 栈空 */
}
int64_t dial_gui_widget_can_undo(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    gui_core_ok();
    return w->undo_len > 0 ? 1 : 0;
}
int64_t dial_gui_widget_can_redo(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    gui_core_ok();
    return w->redo_len > 0 ? 1 : 0;
}
int64_t dial_gui_widget_undo_depth(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    gui_core_ok();
    return (int64_t)w->undo_len;
}
int64_t dial_gui_widget_redo_depth(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    gui_core_ok();
    return (int64_t)w->redo_len;
}
int64_t dial_gui_widget_undo_clear(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    gui_widget_undo_clear(w);
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_undo_capacity(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    gui_core_ok();
    return (int64_t)GUI_UNDO_CAP;
}

/* ---- Slider 拖动（widget_handle 推进的纯值）---------------------------- */
int64_t dial_gui_widget_dragging(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    gui_core_ok();
    return w->drag_active ? 1 : 0;
}

/* ---- 每控件主题覆写 --------------------------------------------------- *
 * ov_mask 的 bit r 标记角色 r 被覆写，未覆写角色回落进程级全局表；
 * 因此 color=0x00000000 也是合法覆写值，不与「未覆写」哨兵混淆。
 * role 越界 / color 高 32 位非 0 一律 Fail-Fast 返回 -1 并写错误槽。 */
static int widget_role_ok(int64_t role) {
    return role >= 0 && role < GUI_THEME_ROLE_COUNT;
}
int64_t dial_gui_widget_set_color(GuiWidgetImpl *w, int64_t role, int64_t color) {
    if (!widget_ok(w)) return -1;
    if (!widget_role_ok(role)) {
        gui_core_set_err(GUI_ERR_ARG, "每控件主题角色越界（合法范围 0..13）");
        return -1;
    }
    if (color < 0 || color > 0xFFFFFFFFLL) {
        gui_core_set_err(GUI_ERR_ARG,
                         "每控件主题颜色须为 0..0xFFFFFFFF 的 0xAARRGGBB（高 32 位必须为 0）");
        return -1;
    }
    w->ov_mask |= (uint64_t)1 << (int)role;
    w->ov_color[(int)role] = color;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_color(GuiWidgetImpl *w, int64_t role) {
    if (!widget_ok(w)) return -1;
    if (!widget_role_ok(role)) {
        gui_core_set_err(GUI_ERR_ARG, "每控件主题角色越界（合法范围 0..13）");
        return -1;
    }
    gui_core_ok();
    if (w->ov_mask & ((uint64_t)1 << (int)role))
        return w->ov_color[(int)role];       /* 覆写优先 */
    return (int64_t)gui_theme_color((int)role);   /* 否则回落全局表 */
}
int64_t dial_gui_widget_has_color(GuiWidgetImpl *w, int64_t role) {
    if (!widget_ok(w)) return -1;
    if (!widget_role_ok(role)) {
        gui_core_set_err(GUI_ERR_ARG, "每控件主题角色越界（合法范围 0..13）");
        return -1;
    }
    gui_core_ok();
    return (w->ov_mask & ((uint64_t)1 << (int)role)) ? 1 : 0;
}
int64_t dial_gui_widget_clear_color(GuiWidgetImpl *w, int64_t role) {
    if (!widget_ok(w)) return -1;
    if (!widget_role_ok(role)) {
        gui_core_set_err(GUI_ERR_ARG, "每控件主题角色越界（合法范围 0..13）");
        return -1;
    }
    w->ov_mask &= ~((uint64_t)1 << (int)role);
    w->ov_color[(int)role] = 0;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_clear_colors(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    w->ov_mask = 0;
    for (int i = 0; i < GUI_THEME_ROLE_COUNT; ++i) w->ov_color[i] = 0;
    gui_core_ok();
    return 0;
}

/* ---- Grid 自动换行 ---------------------------------------------------- *
 * 容器级开关：打开后子节点按加入顺序自动依次占格，显式 grid_col/row 被忽略；
 * cspan/rspan 仍由子节点显式声明。溢出 cols*rows 夹到最后一行。 */
int64_t dial_gui_widget_set_grid_autoflow(GuiWidgetImpl *w, int64_t on) {
    if (!widget_ok(w)) return -1;
    w->grid_autoflow = on ? 1 : 0;
    gui_core_ok();
    return 0;
}
int64_t dial_gui_widget_grid_autoflow(GuiWidgetImpl *w) {
    if (!widget_ok(w)) return -1;
    gui_core_ok();
    return w->grid_autoflow ? 1 : 0;
}
