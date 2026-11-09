/*
 * dial_gui.h —— gui.dialpkg 内部公共头（不对外，不属于 .diali 契约）
 *
 * 分层：
 *   dial_gui_core.c    FFI 包装：句柄生命周期 / 错误槽 / 事件槽 / 调后端 vtable
 *   dial_gui_raster.c  自研 2D 光栅化器 + 内置点阵 + stb_truetype 字形
 *   dial_gui_x11.c     POSIX 后端（X11 + XShm + XRender，dlopen 动态装载）
 *   dial_gui_win32.c   Windows 后端（GDI DIBSection，LoadLibraryA 动态装载）
 *
 * 三份 .a 都是真实现：Linux / Termux 编 x11 那一份，Windows 编 win32 那一份。
 * 平台库**不产生链接期符号依赖**，全部运行时动态解析，详见 build.sh 头注释。
 *
 * 与 stdlib 的唯一耦合：返回 string / bytes 需要构造 DialString 句柄，
 * 符号由 stdlib.dialpkg 在链接期解析（与 http / json / crypto 包同口径）。
 */
#ifndef DIAL_GUI_H
#define DIAL_GUI_H

#include <stdint.h>
#include <stddef.h>

#include "dial_string.h"
#include "dial_bytes.h"

/* ---- 错误码（dial_gui_last_error）------------------------------------ */
#define GUI_OK               0
#define GUI_ERR_ARG          1  /* 空句柄 / 非法参数（负尺寸、越界坐标等） */
#define GUI_ERR_ALLOC        2  /* 内存分配失败 */
#define GUI_ERR_NO_BACKEND   3  /* 平台动态库缺失或初始化失败 */
#define GUI_ERR_WINDOW       4  /* 窗口创建 / 尺寸 / 上屏失败 */
#define GUI_ERR_FONT         5  /* 字体文件读取 / 解析失败 */
#define GUI_ERR_UNSUPPORTED  6  /* 该后端不支持此操作 */

/* ---- 事件码（dial_gui_app_poll / wait 返回值）------------------------- */
#define GUI_EV_NONE          0
#define GUI_EV_MOUSE_PRESS   1
#define GUI_EV_MOUSE_RELEASE 2
#define GUI_EV_MOUSE_MOVE    3
#define GUI_EV_KEY_PRESS     4
#define GUI_EV_KEY_RELEASE   5
#define GUI_EV_RESIZE        6
#define GUI_EV_EXPOSE        7
#define GUI_EV_CLOSE         8
#define GUI_EV_QUIT          9

/* 颜色统一为 0xAARRGGBB，装进 uint32_t；Dial 侧用 int64 传（高 32 位为 0）。*/
typedef uint32_t gui_color_t;

static inline uint8_t gui_a(gui_color_t c) { return (uint8_t)(c >> 24); }
static inline uint8_t gui_r(gui_color_t c) { return (uint8_t)(c >> 16); }
static inline uint8_t gui_g(gui_color_t c) { return (uint8_t)(c >> 8);  }
static inline uint8_t gui_b(gui_color_t c) { return (uint8_t)(c);       }
static inline gui_color_t gui_rgba(uint8_t a, uint8_t r, uint8_t g, uint8_t b) {
    return ((gui_color_t)a << 24) | ((gui_color_t)r << 16) | ((gui_color_t)g << 8) | (gui_color_t)b;
}

/* ---- 结构体 ---------------------------------------------------------- */
typedef struct GuiPainterImpl GuiPainterImpl;
typedef struct GuiWindowImpl  GuiWindowImpl;
typedef struct GuiAppImpl     GuiAppImpl;
typedef struct GuiFontImpl    GuiFontImpl;

typedef struct GuiBackend GuiBackend;

struct GuiPainterImpl {
    uint32_t *pixels;      /* ARGB32，行主序，w*h 个；NULL 表示未分配 */
    int       w, h;
    int       aa;          /* 覆盖面积子采样倍率：1 / 2 / 4 / 8 */
    gui_color_t color;     /* 当前前景色 */
    int       has_clip;
    int       clip_x, clip_y, clip_w, clip_h;
    int       owned;       /* 1 = offscreen 独立拥有（要 painter_free）；0 = 窗口绑定 */
    int       ext_pixels;  /* 1 = pixels 指向后端外部缓冲（XShm 共享内存 / DIBSection），
                            *     dispose 不 free，由窗口后端释放 */
    GuiWindowImpl *window; /* 绑定的窗口（可空） */
};

struct GuiWindowImpl {
    GuiAppImpl    *app;
    GuiPainterImpl *painter;   /* 窗口拥有的离屏画布 */
    int            w, h;
    char           title[256];
    void          *native;     /* 后端私有数据（X11 窗口结构 / Win32 HWND 等） */
    char           filter[16]; /* XRender filter：fast/good/best/nearest */
    GuiWindowImpl *next;       /* App 的窗口链表 */
};

struct GuiAppImpl {
    const GuiBackend *be;
    void             *native;       /* 后端私有数据 */
    /* 最近事件槽（每 App 一份；单线程 GUI 事件循环，无需加锁）*/
    int  ev_kind, ev_x, ev_y, ev_button, ev_key, ev_w, ev_h;
    GuiWindowImpl *ev_win;
    int  quit;
    GuiWindowImpl *windows;
    int  window_count;
};

struct GuiFontImpl {
    int            kind;       /* 0 = 内置点阵，1 = TrueType */
    unsigned char *ttf;        /* 字体文件全内容（TTF 时持有） */
    int            ttf_len;
    void          *stb;        /* stbtt_fontinfo*（TTF 时持有） */
};

/*
 * 后端 vtable。core.c 只认这张表，不 #include 任何 X11 / windows.h，
 * 因此交叉编译三平台时不需要各平台 GUI SDK 头文件。
 *
 * err / errcap：后端把人类可读的失败原因写进 err，core 再转进线程局部错误槽。
 */
struct GuiBackend {
    const char *name;   /* "x11" / "win32" */
    int  (*init)(GuiAppImpl *app, char *err, int errcap);
    void (*fini)(GuiAppImpl *app);
    int  (*window_create)(GuiWindowImpl *win, const char *title, int w, int h, char *err, int errcap);
    void (*window_destroy)(GuiWindowImpl *win);
    int  (*window_flush)(GuiWindowImpl *win);
    int  (*window_resize)(GuiWindowImpl *win, int w, int h);
    int  (*window_set_title)(GuiWindowImpl *win, const char *title);
    int  (*window_show)(GuiWindowImpl *win);
    int  (*window_set_filter)(GuiWindowImpl *win, const char *filter);
    const char *(*window_backend)(GuiWindowImpl *win);
    int  (*poll)(GuiAppImpl *app);                 /* 非阻塞，泵一次 */
    int  (*wait)(GuiAppImpl *app, int timeout_ms); /* 阻塞等待 */
};

const GuiBackend *gui_backend_posix(void);
const GuiBackend *gui_backend_win32(void);

/* ---- 光栅化器（dial_gui_raster.c）------------------------------------ */
GuiPainterImpl *gui_painter_new(int w, int h, int owned);
void            gui_painter_dispose(GuiPainterImpl *p);   /* 只释放像素缓冲与结构体 */
int             gui_painter_resize(GuiPainterImpl *p, int w, int h);

void      gui_raster_clear(GuiPainterImpl *p, gui_color_t color);
void      gui_raster_set_pixel(GuiPainterImpl *p, int x, int y, gui_color_t c);
gui_color_t gui_raster_get_pixel(const GuiPainterImpl *p, int x, int y);
void      gui_raster_fill_rect(GuiPainterImpl *p, int x, int y, int w, int h, gui_color_t c);
void      gui_raster_stroke_rect(GuiPainterImpl *p, int x, int y, int w, int h, int t, gui_color_t c);
void      gui_raster_line(GuiPainterImpl *p, int x0, int y0, int x1, int y1, int t, gui_color_t c);
void      gui_raster_fill_circle(GuiPainterImpl *p, int cx, int cy, int r, gui_color_t c);
void      gui_raster_stroke_circle(GuiPainterImpl *p, int cx, int cy, int r, int t, gui_color_t c);
void      gui_raster_fill_ellipse(GuiPainterImpl *p, int cx, int cy, int rx, int ry, gui_color_t c);
void      gui_raster_fill_trapezoid(GuiPainterImpl *p, int y0, int y1,
                                    double x_tl, double x_tr, double x_bl, double x_br,
                                    gui_color_t c);
void      gui_raster_blit(GuiPainterImpl *p, int dx, int dy, int w, int h,
                          const unsigned char *src, int src_len, gui_color_t tint);
uint32_t  gui_raster_crc32(const GuiPainterImpl *p);

/* ---- 字体（dial_gui_raster.c）---------------------------------------- */
GuiFontImpl *gui_font_default(void);
GuiFontImpl *gui_font_load(const char *path, char *err, int errcap);
void         gui_font_free(GuiFontImpl *f);
/* 返回推进后的 x；text 允许内嵌 NUL？Dial string 按长度处理，这里按 len。 */
int          gui_font_draw(GuiPainterImpl *p, int x, int y, const char *text, int len,
                           GuiFontImpl *f, int size, gui_color_t c);
int          gui_font_width(const char *text, int len, GuiFontImpl *f, int size);

/* ---- core 提供给后端/其他 TU 的辅助 ---------------------------------- */
void gui_core_set_err(int64_t code, const char *msg);
void gui_core_ok(void);
/* 把最近事件写进 app 的事件槽（后端调用）。 */
void gui_core_emit(GuiAppImpl *app, int kind, int x, int y, int button, int key,
                   int w, int h, GuiWindowImpl *win);
/* 按窗口句柄找 App 内已登记的窗口（防悬垂句柄）。 */
int gui_core_has_window(GuiAppImpl *app, const GuiWindowImpl *win);

#endif /* DIAL_GUI_H */
