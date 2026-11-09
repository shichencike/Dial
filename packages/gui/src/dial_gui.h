/*
 * dial_gui.h —— gui.dialpkg 内部公共头（不对外，不属于 .diali 契约）
 *
 * 分层：
 *   dial_gui_core.c    FFI 包装：句柄生命周期 / 错误槽 / 事件槽 / 调后端 vtable
 *   dial_gui_raster.c  自研 2D 光栅化器 + 内置点阵 + stb_truetype 字形
 *   dial_gui_widget.c  控件框架层：Widget 树 / 布局 / 命中 / packed 事件块（纯 C）
 *   dial_gui_look.c    成品控件外观层：默认主题 / 四态绘制 / 纯值事件推进（纯 C）
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
typedef struct GuiWidgetImpl  GuiWidgetImpl;

typedef struct GuiBackend GuiBackend;

/*
 * 控件框架层的事件纯值块（C 侧 owned，Dial 侧只读）。
 *
 * 内存宪法：绝不 C → Dial 反向回调。C 侧只写这块 packed struct 纯值数据，
 * Dial 侧主动 poll/wait 后经 rawptr<byte> + ptr_cast 读走；带字符串的字段
 * 是 C 侧**借用**的 const char*，Dial 侧必须 string_from_cstr 复制成自己的
 * ARC 对象。C 管 C 的，ARC 管 ARC 的，谁也不越界，零隐式开销。
 *
 * 三目标均 64 位：Dial int = 8 字节、rawptr = 8 字节，故本块恒为 104 字节。
 * 布局由下面的 _Static_assert 与 Dial demo 的 sizeof/offsetof 编译期内建
 * 双向锁死：任何字段增删都会两边同时报错，杜绝契约漂移。
 */
typedef struct __attribute__((packed)) GuiWidgetEvent {
    int64_t kind, x, y, button, key, w, h;      /*  0..55  纯值 */
    int64_t local_x, local_y, tag, userdata;    /* 56..87  纯值 */
    const char *win_title;                      /* 88  借用：窗口标题 */
    const char *widget_id;                      /* 96  借用：命中控件 id */
} GuiWidgetEvent;

_Static_assert(sizeof(int64_t) == 8 && sizeof(void *) == 8,
               "gui.dialpkg 三目标均为 64 位（Dial int / rawptr 均为 8 字节）");
_Static_assert(sizeof(GuiWidgetEvent) == 104,
               "GuiWidgetEvent 必须为 104 字节（Dial 侧 packed extern struct 同布局）");
_Static_assert(offsetof(GuiWidgetEvent, win_title) == 88, "win_title 偏移须为 88");
_Static_assert(offsetof(GuiWidgetEvent, widget_id) == 96, "widget_id 偏移须为 96");

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
    GuiWidgetImpl *root;       /* 挂载的控件树（可空；窗口拥有它） */
    GuiWindowImpl *next;       /* App 的窗口链表 */
};

struct GuiAppImpl {
    const GuiBackend *be;
    void             *native;       /* 后端私有数据 */
    /* 最近事件槽（每 App 一份；单线程 GUI 事件循环，无需加锁）*/
    int  ev_kind, ev_x, ev_y, ev_button, ev_key, ev_w, ev_h;
    GuiWindowImpl *ev_win;
    /* 控件框架层：命中控件 + packed 纯值块（内嵌，非堆分配）*/
    GuiWidgetImpl *ev_widget;
    GuiWidgetEvent ev_data;
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

/* ---- 控件框架层：Widget 树（纯 C，无任何平台符号）-------------------- */
#define GUI_LAYOUT_ABS   0
#define GUI_LAYOUT_VBOX  1
#define GUI_LAYOUT_HBOX  2

/* ---- 成品控件外观层：种类 + 内置浅色默认主题 -------------------------- *
 * 颜色常量写死在 C（gui.diali 里逐条列出色值），本版不暴露 theme setter；
 * Dial 侧只传控件种类 / 文本 / 状态，绘制配色一律由包内部决定，保证三平台
 * 逐像素一致、可被 painter_crc32 稳定断言。 */
#define GUI_WIDGET_BOX      0
#define GUI_WIDGET_LABEL    1
#define GUI_WIDGET_BUTTON   2
#define GUI_WIDGET_CHECKBOX 3

#define GUI_LOOK_FONT_SIZE  16   /* 内置 8×8 点阵，sc = size/8 = 2，每字符 18×16 */

#define GUI_THEME_BG        ((gui_color_t)0xFFF4F6F9u)  /* 面板 / BOX 背景 */
#define GUI_THEME_BORDER    ((gui_color_t)0xFFC7CEDAu)  /* 细边框 */
#define GUI_THEME_TEXT      ((gui_color_t)0xFF202733u)  /* 文本 */
#define GUI_THEME_TEXT_DIS  ((gui_color_t)0xFFA9B0BDu)  /* 禁用态文本 */
#define GUI_THEME_BTN       ((gui_color_t)0xFFE7ECF4u)  /* 按钮 普通 */
#define GUI_THEME_BTN_HOVER ((gui_color_t)0xFFD3E0FAu)  /* 按钮 悬停 */
#define GUI_THEME_BTN_DOWN  ((gui_color_t)0xFFB2C8F2u)  /* 按钮 按下 */
#define GUI_THEME_BTN_DIS   ((gui_color_t)0xFFF0F2F5u)  /* 按钮 禁用 */
#define GUI_THEME_CHECK_BG  ((gui_color_t)0xFFFFFFFFu)  /* 勾选框底 */
#define GUI_THEME_MARK      ((gui_color_t)0xFF2F6FE0u)  /* 勾 / 高亮 */

struct GuiWidgetImpl {
    GuiWidgetImpl *parent;
    GuiWidgetImpl *first_child;
    GuiWidgetImpl *next_sibling;
    GuiWidgetImpl *next_root;   /* 全局根链表（仅根使用）：防悬垂句柄校验 */
    GuiWindowImpl *win;         /* 挂窗后非 NULL；独立纯内存树为 NULL */
    int  req_x, req_y, req_w, req_h; /* set_rect 写入的期望几何（相对父）；
                                      * 布局每次只从这里重算，保证幂等 */
    int  x, y, w, h;            /* 实际几何（相对父）：getter 与命中测试用 */
    int  abs_x, abs_y;          /* 绝对坐标（layout_apply / hit 写），供 local_x/y */
    int  visible;
    char *id;                   /* C 侧 strdup 自有副本（NULL = 未设置） */
    int64_t tag;
    int64_t userdata;
    int  layout;                /* GUI_LAYOUT_* */
    int  padding;
    int  gap;
    /* ---- 成品控件外观层（dial_gui_look.c）------------------------------ */
    int  kind;                  /* GUI_WIDGET_*，默认 0 BOX */
    char *text;                 /* C 侧 strdup 自有副本（NULL = 无文本）；
                                 * dispose_subtree 负责 free，与 id 同口径 */
    int  checked;               /* CHECKBOX 勾选态 */
    int  disabled;              /* 禁用态：不 hover / press / 激活，走禁用配色 */
    int  hovered;               /* 运行时：悬停（gui_widget_handle 推进） */
    int  pressed;               /* 运行时：按下（gui_widget_handle 推进） */
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

/* ---- core 提供给后端 / 其他 TU 的辅助 -------------------------------- */
void gui_core_set_err(int64_t code, const char *msg);
void gui_core_ok(void);
/*
 * 把最近事件写进 app 的事件槽（后端 pump 调用）。
 * hit 为命中控件（窗口级 / QUIT 事件传 NULL）；命中控件的 abs_x/abs_y
 * 已由 gui_widget_hit 写回，emit 据此算 local_x/local_y。
 */
void gui_core_emit(GuiAppImpl *app, int kind, int x, int y, int button, int key,
                   int w, int h, GuiWindowImpl *win, GuiWidgetImpl *hit);
/* 按窗口句柄找 App 内已登记的窗口（防悬垂句柄）。 */
int gui_core_has_window(GuiAppImpl *app, const GuiWindowImpl *win);

/* ---- 控件框架层：dial_gui_widget.c（纯 C，三平台共用，无平台符号）---- */
GuiWidgetImpl *gui_widget_tree_new(int w, int h);
void           gui_widget_dispose_tree(GuiWidgetImpl *root);
GuiWidgetImpl *gui_widget_add(GuiWidgetImpl *parent);
int            gui_widget_remove(GuiWidgetImpl *w);
int            gui_widget_layout_apply(GuiWidgetImpl *root, int w, int h);
/* 后加子优先、取最深可见节点；命中时把绝对坐标写回 hit->abs_* 并输出。 */
GuiWidgetImpl *gui_widget_hit(GuiWidgetImpl *root, int x, int y,
                              int *out_abs_x, int *out_abs_y);
/* 遍历全局根链表做 DFS 比指针（与 gui_core_has_window 同口径的防悬垂校验）。*/
int            gui_widget_is_live(const GuiWidgetImpl *w);
int            gui_widget_attach_window(GuiWidgetImpl *root, GuiWindowImpl *win);
void           gui_widget_fill_event(GuiWidgetEvent *ev, int kind,
                                     int x, int y, int button, int key,
                                     int w, int h, GuiWindowImpl *win,
                                     GuiWidgetImpl *hit);

/* ---- 成品控件外观层：dial_gui_look.c（纯 C，三平台共用，无平台符号）---- *
 * 与框架层同一条硬边界：只 include dial_gui.h + libc，绝不碰 X11 / Win32，
 * 绝不保存函数指针、绝不 C → Dial 反向回调。 */
/* 把一条纯值事件喂进树推进四态；返回 1=本次激活 / 0=无 / 负值=参数错误。 */
int            gui_widget_handle(GuiWidgetImpl *root, int kind,
                                 int x, int y, int button);
/* 按默认主题 + 当前四态把整棵树画到 GuiPainter；返回绘制节点数（>=0）/ <0 失败。 */
int            gui_widget_draw(GuiPainterImpl *p, GuiWidgetImpl *root);

#endif /* DIAL_GUI_H */
