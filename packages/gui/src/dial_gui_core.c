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
                   int w, int h, GuiWindowImpl *win) {
    if (!app) return;
    app->ev_kind = kind; app->ev_x = x; app->ev_y = y;
    app->ev_button = button; app->ev_key = key;
    app->ev_w = w; app->ev_h = h; app->ev_win = win;
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
        gui_core_emit(app, GUI_EV_QUIT, 0, 0, 0, 0, 0, 0, NULL);
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
        gui_core_emit(app, GUI_EV_QUIT, 0, 0, 0, 0, 0, 0, NULL);
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
    free(win);
    gui_core_ok();
    return 0;
}

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
