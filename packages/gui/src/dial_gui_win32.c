/*
 * dial_gui_win32.c —— gui.dialpkg 的 Windows 后端（真实现，非 stub）
 *
 * 与 X11 后端严格对称：
 *   XShm 共享内存离屏像素  <->  CreateDIBSection 32bpp BGRA 离屏位图
 *   XShmPutImage 零拷贝上传 <->  SelectObject 到内存 DC（同一份 bits，零拷贝）
 *   XRender Picture + filter <->  SetStretchBltMode(HALFTONE) + StretchBlt
 *   自研光栅化器（跨平台共有）写进同一份 ARGB32/BGRA 像素
 *
 * 像素格式：DIBSection 用 biHeight = -h（top-down）32bpp BI_RGB，
 * 内存字节序是 B,G,R,A；我们的 uint32 0xAARRGGBB 在 little-endian 下正是
 * B,G,R,A —— 与 X11 24/32 位视觉一致，同一份 painter 代码两边通用。
 *
 * 零链接期依赖：除 kernel32 的 LoadLibraryA/GetProcAddress/GetModuleHandleA/Sleep
 * （Windows 链接器必然导入，等价于 Linux 上的 libc）之外，user32 / gdi32 的
 * 全部函数都在 app_new 里动态解析后通过函数指针调用，.a 里没有 GUI 未定义符号。
 *
 * 验收边界（README 如实标注）：沙箱只能把本文件交叉编译成
 * x86_64-pc-windows-gnu 的 .a 并做符号/魔数检查，**无法实机运行 Win32 程序**。
 * 事件循环/像素布局按 Win32 ABI 复刻，行为与 X11 端对齐，但未做实机验证。
 */
#include "dial_gui.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* x64 Windows 只有一个调用约定，CALLBACK/__stdcall 在 ABI 上等价为空。 */
#ifndef CALLBACK
#define CALLBACK
#endif

/* kernel32：Windows 链接器默认导入，直接声明调用（用来装载 user32/gdi32）。 */
extern void *LoadLibraryA(const char *);
extern void *GetProcAddress(void *, const char *);
extern void *GetModuleHandleA(const char *);
extern void  Sleep(unsigned int);

/* ---- Win32 ABI 常量与结构（按 SDK 布局复刻，不 include windows.h）------ */
#define W32_WS_OVERLAPPEDWINDOW 0x00CF0000u
#define W32_CW_USEDEFAULT       0x80000000
#define W32_SW_SHOW             5
#define W32_PM_REMOVE           0x0001u
#define W32_SRCCOPY             0x00CC0020u
#define W32_BI_RGB              0u
#define W32_DIB_RGB_COLORS      0u
#define W32_HALFTONE            4
#define W32_COLORONCOLOR        3
#define W32_GWLP_USERDATA       (-21)

#define W32_WM_DESTROY   0x0002u
#define W32_WM_SIZE      0x0005u
#define W32_WM_PAINT     0x000Fu
#define W32_WM_CLOSE     0x0010u
#define W32_WM_KEYDOWN   0x0100u
#define W32_WM_KEYUP     0x0101u
#define W32_WM_CHAR      0x0102u
#define W32_WM_MOUSEMOVE 0x0200u
#define W32_WM_LBUTTONDOWN 0x0201u
#define W32_WM_LBUTTONUP   0x0202u
#define W32_WM_RBUTTONDOWN 0x0204u
#define W32_WM_RBUTTONUP   0x0205u
#define W32_WM_MBUTTONDOWN 0x0207u
#define W32_WM_MBUTTONUP   0x0208u

#define W32_VK_ESCAPE 0x1B
#define W32_VK_RETURN 0x0D
#define W32_VK_TAB    0x09
#define W32_VK_BACK   0x08
#define W32_VK_FIRST_FUNC 0x21   /* PRIOR..DOWN / F1.. 走 VK，不产生 WM_CHAR */

typedef intptr_t  gui_lresult;
typedef uintptr_t gui_wparam;
typedef intptr_t  gui_lparam;

typedef struct {
    void        *hwnd;
    unsigned int message;
    unsigned int pad0;
    gui_wparam   wParam;
    gui_lparam   lParam;
    unsigned int time;
    int          pt_x, pt_y;
    unsigned int lPrivate;
} gui_w32_msg;   /* MSG，x64 下 48 字节 */

typedef struct {
    unsigned int style;
    unsigned int pad0;
    void        *lpfnWndProc;
    int          cbClsExtra;
    int          cbWndExtra;
    void        *hInstance;
    void        *hIcon;
    void        *hCursor;
    void        *hbrBackground;
    const char  *lpszMenuName;
    const char  *lpszClassName;
} gui_wndclass;  /* WNDCLASSA，x64 下 72 字节 */

typedef struct {
    unsigned int   biSize;
    int            biWidth;
    int            biHeight;
    unsigned short biPlanes;
    unsigned short biBitCount;
    unsigned int   biCompression;
    unsigned int   biSizeImage;
    int            biXPelsPerMeter;
    int            biYPelsPerMeter;
    unsigned int   biClrUsed;
    unsigned int   biClrImportant;
} gui_bmi_header; /* BITMAPINFOHEADER，40 字节 */

typedef struct {
    gui_bmi_header bmiHeader;
    unsigned int   bmiColors[1];
} gui_bitmap_info;

typedef struct { int left, top, right, bottom; } gui_rect;

/* ---- 动态解析出的 user32 / gdi32 函数指针 ------------------------------ */
typedef struct {
    void *h_user32, *h_gdi32;
    unsigned short (*RegisterClassA)(const gui_wndclass *);
    void *(*CreateWindowExA)(unsigned int, const char *, const char *, unsigned int,
                             int, int, int, int, void *, void *, void *, void *);
    int  (*ShowWindow)(void *, int);
    int  (*UpdateWindow)(void *);
    int  (*DestroyWindow)(void *);
    int  (*SetWindowTextA)(void *, const char *);
    int  (*SetWindowPos)(void *, void *, int, int, int, int, unsigned int);
    int  (*GetClientRect)(void *, gui_rect *);
    int  (*AdjustWindowRect)(gui_rect *, unsigned int, int);
    int  (*PeekMessageA)(gui_w32_msg *, void *, unsigned int, unsigned int, unsigned int);
    int  (*GetMessageA)(gui_w32_msg *, void *, unsigned int, unsigned int);
    int  (*TranslateMessage)(const gui_w32_msg *);
    gui_lresult (*DispatchMessageA)(const gui_w32_msg *);
    gui_lresult (*DefWindowProcA)(void *, unsigned int, gui_wparam, gui_lparam);
    void (*PostQuitMessage)(int);
    void *(*LoadCursorA)(void *, const char *);
    int  (*ValidateRect)(void *, const gui_rect *);
    void *(*GetDC)(void *);
    int  (*ReleaseDC)(void *, void *);

    void *(*CreateDIBSection)(void *, const gui_bitmap_info *, unsigned int,
                              void **, void *, unsigned int);
    void *(*CreateCompatibleDC)(void *);
    void *(*SelectObject)(void *, void *);
    int  (*DeleteObject)(void *);
    int  (*DeleteDC)(void *);
    int  (*StretchBlt)(void *, int, int, int, int, void *, int, int, int, int, unsigned int);
    int  (*SetStretchBltMode)(void *, int);
    int  (*GdiFlush)(void);
} gui_w32_api;

static gui_w32_api U;
static int g_api_loaded = 0;

#define LOAD(h, name)                                                        \
    do {                                                                     \
        void *q_ = GetProcAddress((h), #name);                                \
        if (!q_) {                                                           \
            snprintf(err, (size_t)errcap, "动态库缺少符号 %s", #name);         \
            return 0;                                                        \
        }                                                                    \
        memcpy(&(U.name), &q_, sizeof(q_));                                   \
    } while (0)

static int load_api(char *err, int errcap) {
    if (g_api_loaded) return 1;
    U.h_user32 = LoadLibraryA("user32.dll");
    if (!U.h_user32) { snprintf(err, (size_t)errcap, "无法装载 user32.dll"); return 0; }
    U.h_gdi32 = LoadLibraryA("gdi32.dll");
    if (!U.h_gdi32) { snprintf(err, (size_t)errcap, "无法装载 gdi32.dll"); return 0; }

    LOAD(U.h_user32, RegisterClassA); LOAD(U.h_user32, CreateWindowExA);
    LOAD(U.h_user32, ShowWindow); LOAD(U.h_user32, UpdateWindow);
    LOAD(U.h_user32, DestroyWindow); LOAD(U.h_user32, SetWindowTextA);
    LOAD(U.h_user32, SetWindowPos); LOAD(U.h_user32, GetClientRect);
    LOAD(U.h_user32, AdjustWindowRect); LOAD(U.h_user32, PeekMessageA);
    LOAD(U.h_user32, GetMessageA); LOAD(U.h_user32, TranslateMessage);
    LOAD(U.h_user32, DispatchMessageA); LOAD(U.h_user32, DefWindowProcA);
    LOAD(U.h_user32, PostQuitMessage); LOAD(U.h_user32, LoadCursorA);
    LOAD(U.h_user32, ValidateRect); LOAD(U.h_user32, GetDC);
    LOAD(U.h_user32, ReleaseDC);

    LOAD(U.h_gdi32, CreateDIBSection); LOAD(U.h_gdi32, CreateCompatibleDC);
    LOAD(U.h_gdi32, SelectObject); LOAD(U.h_gdi32, DeleteObject);
    LOAD(U.h_gdi32, DeleteDC); LOAD(U.h_gdi32, StretchBlt);
    LOAD(U.h_gdi32, SetStretchBltMode); LOAD(U.h_gdi32, GdiFlush);

    g_api_loaded = 1;
    return 1;
}
#undef LOAD

/* ---- 事件队列（WndProc 入队，poll/wait 出队）--------------------------- */
typedef struct {
    int   kind, x, y, button, key, w, h;
    void *hwnd;
} gui_w32_event;

#define QCAP 256
static gui_w32_event Q[QCAP];
static int q_head = 0, q_tail = 0;

static void q_push(int kind, int x, int y, int button, int key,
                   int w, int h, void *hwnd) {
    int next = (q_tail + 1) % QCAP;
    if (next == q_head) return;  /* 满了就丢最旧事件之外的那个，绝不越界 */
    gui_w32_event *e = &Q[q_tail];
    e->kind = kind; e->x = x; e->y = y; e->button = button; e->key = key;
    e->w = w; e->h = h; e->hwnd = hwnd;
    q_tail = next;
}
static int q_pop(gui_w32_event *out) {
    if (q_head == q_tail) return 0;
    *out = Q[q_head];
    q_head = (q_head + 1) % QCAP;
    return 1;
}

static int loword(gui_lparam l)  { return (int)(short)(unsigned short)(l & 0xFFFFu); }
static int hiword(gui_lparam l)  { return (int)(short)(unsigned short)((l >> 16) & 0xFFFFu); }
/* 功能键用 0x10000|VK 标记，避免与 Unicode 码点冲突（见 README 键码表）。 */
static int vk_code(unsigned int vk) { return (int)(0x10000u | vk); }

static gui_lresult CALLBACK wndproc(void *hwnd, unsigned int msg,
                                    gui_wparam wp, gui_lparam lp) {
    switch (msg) {
    case W32_WM_CHAR:
        q_push(GUI_EV_KEY_PRESS, 0, 0, 0, (int)wp, 0, 0, hwnd);
        return 0;
    case W32_WM_KEYDOWN:
        if (wp < 0x20u || wp >= 0x70u || wp == W32_VK_ESCAPE ||
            wp == W32_VK_RETURN || wp == W32_VK_TAB || wp == W32_VK_BACK)
            q_push(GUI_EV_KEY_PRESS, 0, 0, 0, vk_code((unsigned int)wp), 0, 0, hwnd);
        return 0;
    case W32_WM_KEYUP:
        q_push(GUI_EV_KEY_RELEASE, 0, 0, 0, vk_code((unsigned int)wp), 0, 0, hwnd);
        return 0;
    case W32_WM_MOUSEMOVE:
        q_push(GUI_EV_MOUSE_MOVE, loword(lp), hiword(lp), 0, 0, 0, 0, hwnd);
        return 0;
    case W32_WM_LBUTTONDOWN: case W32_WM_RBUTTONDOWN: case W32_WM_MBUTTONDOWN: {
        int b = msg == W32_WM_LBUTTONDOWN ? 1 : msg == W32_WM_RBUTTONDOWN ? 3 : 2;
        q_push(GUI_EV_MOUSE_PRESS, loword(lp), hiword(lp), b, 0, 0, 0, hwnd);
        return 0;
    }
    case W32_WM_LBUTTONUP: case W32_WM_RBUTTONUP: case W32_WM_MBUTTONUP: {
        int b = msg == W32_WM_LBUTTONUP ? 1 : msg == W32_WM_RBUTTONUP ? 3 : 2;
        q_push(GUI_EV_MOUSE_RELEASE, loword(lp), hiword(lp), b, 0, 0, 0, hwnd);
        return 0;
    }
    case W32_WM_SIZE:
        q_push(GUI_EV_RESIZE, 0, 0, 0, 0, loword(lp), hiword(lp), hwnd);
        return 0;
    case W32_WM_PAINT:
        U.ValidateRect(hwnd, NULL);
        q_push(GUI_EV_EXPOSE, 0, 0, 0, 0, 0, 0, hwnd);
        return 0;
    case W32_WM_CLOSE:
        q_push(GUI_EV_CLOSE, 0, 0, 0, 0, 0, 0, hwnd);
        return 0;  /* 不 DestroyWindow：退出与否由 Dial 侧决定 */
    case W32_WM_DESTROY:
        U.PostQuitMessage(0);
        return 0;
    default:
        break;
    }
    return U.DefWindowProcA(hwnd, msg, wp, lp);
}

/* ---- 后端私有数据 ------------------------------------------------------ */
typedef struct {
    void *hInst;
} gui_w32_app;

typedef struct {
    void  *hwnd;
    void  *memdc;
    void  *hbitmap;
    void  *oldbmp;
    void  *bits;
    gui_bitmap_info bmi;
    char   filter[16];
    char   backend[160];
} gui_w32_win;

static int g_class_registered = 0;
static const char CLASS_NAME[] = "DialGui_v1_WindowClass";

static GuiWindowImpl *find_win_obj(GuiAppImpl *app, void *hwnd) {
    for (GuiWindowImpl *w = app->windows; w; w = w->next) {
        gui_w32_win *n = (gui_w32_win *)w->native;
        if (n && n->hwnd == hwnd) return w;
    }
    return NULL;
}

static void release_surface(gui_w32_win *n) {
    if (!n) return;
    if (n->memdc && n->hbitmap && n->oldbmp) U.SelectObject(n->memdc, n->oldbmp);
    if (n->memdc) U.DeleteDC(n->memdc);
    if (n->hbitmap) U.DeleteObject(n->hbitmap);
    n->memdc = NULL; n->hbitmap = NULL; n->oldbmp = NULL; n->bits = NULL;
}

static int setup_surface(GuiWindowImpl *win, gui_w32_win *n, int w, int h,
                         char *err, int errcap) {
    memset(&n->bmi, 0, sizeof(n->bmi));
    n->bmi.bmiHeader.biSize = (unsigned int)sizeof(gui_bmi_header);
    n->bmi.bmiHeader.biWidth = w;
    n->bmi.bmiHeader.biHeight = -h;   /* 负高度 = top-down，与我们行主序缓冲一致 */
    n->bmi.bmiHeader.biPlanes = 1;
    n->bmi.bmiHeader.biBitCount = 32;
    n->bmi.bmiHeader.biCompression = W32_BI_RGB;
    void *bits = NULL;
    n->hbitmap = U.CreateDIBSection(NULL, &n->bmi, W32_DIB_RGB_COLORS,
                                    &bits, NULL, 0);
    if (!n->hbitmap || !bits) {
        snprintf(err, (size_t)errcap, "CreateDIBSection 失败（%dx%d 32bpp）", w, h);
        return 0;
    }
    n->bits = bits;
    n->memdc = U.CreateCompatibleDC(NULL);
    if (!n->memdc) { snprintf(err, (size_t)errcap, "CreateCompatibleDC 失败"); return 0; }
    n->oldbmp = U.SelectObject(n->memdc, n->hbitmap);
    memset(bits, 0, (size_t)w * (size_t)h * 4u);

    if (win->painter) {
        if (!win->painter->ext_pixels) free(win->painter->pixels);
        win->painter->pixels = (uint32_t *)bits;
        win->painter->w = w; win->painter->h = h;
        win->painter->ext_pixels = 1;
        win->painter->has_clip = 0;
    }
    snprintf(n->backend, sizeof(n->backend),
             "win32/gdi/dibsection bgra filter=%s stretch=%s",
             n->filter,
             (strcmp(n->filter, "good") == 0 || strcmp(n->filter, "best") == 0)
                 ? "halftone" : "coloroncolor");
    return 1;
}

/* ---- vtable ------------------------------------------------------------ */

static int w32_init(GuiAppImpl *app, char *err, int errcap) {
    if (!load_api(err, errcap)) return 0;
    gui_w32_app *a = (gui_w32_app *)calloc(1, sizeof(gui_w32_app));
    if (!a) { snprintf(err, (size_t)errcap, "内存分配失败"); return 0; }
    a->hInst = GetModuleHandleA(NULL);
    if (!g_class_registered) {
        gui_wndclass wc;
        memset(&wc, 0, sizeof(wc));
        wc.lpfnWndProc = (void *)wndproc;
        wc.hInstance = a->hInst;
        wc.hCursor = U.LoadCursorA(NULL, (const char *)(intptr_t)32512); /* IDC_ARROW */
        wc.lpszClassName = CLASS_NAME;
        if (!U.RegisterClassA(&wc)) {
            free(a);
            snprintf(err, (size_t)errcap, "RegisterClassA 失败（无法注册 Dial GUI 窗口类）");
            return 0;
        }
        g_class_registered = 1;
    }
    q_head = q_tail = 0;
    app->native = a;
    return 1;
}

static void w32_fini(GuiAppImpl *app) {
    free(app->native);
    app->native = NULL;
}

static int w32_window_create(GuiWindowImpl *win, const char *title, int w, int h,
                             char *err, int errcap) {
    gui_w32_app *a = (gui_w32_app *)win->app->native;
    gui_w32_win *n = (gui_w32_win *)calloc(1, sizeof(gui_w32_win));
    if (!a || !n) { free(n); snprintf(err, (size_t)errcap, "内存分配失败"); return 0; }
    strncpy(n->filter, "good", sizeof(n->filter) - 1);

    gui_rect r = { 0, 0, w, h };
    U.AdjustWindowRect(&r, W32_WS_OVERLAPPEDWINDOW, 0);
    int outer_w = r.right - r.left, outer_h = r.bottom - r.top;
    n->hwnd = U.CreateWindowExA(0, CLASS_NAME, title, W32_WS_OVERLAPPEDWINDOW,
                                (int)W32_CW_USEDEFAULT, (int)W32_CW_USEDEFAULT,
                                outer_w, outer_h, NULL, NULL, a->hInst, NULL);
    if (!n->hwnd) { free(n); snprintf(err, (size_t)errcap, "CreateWindowExA 失败"); return 0; }
    win->native = n;
    if (!setup_surface(win, n, w, h, err, errcap)) {
        release_surface(n);
        U.DestroyWindow(n->hwnd);
        win->native = NULL;
        free(n);
        return 0;
    }
    U.ShowWindow(n->hwnd, W32_SW_SHOW);
    U.UpdateWindow(n->hwnd);
    return 1;
}

static void w32_window_destroy(GuiWindowImpl *win) {
    gui_w32_win *n = (gui_w32_win *)win->native;
    if (!n) return;
    release_surface(n);
    if (n->hwnd) U.DestroyWindow(n->hwnd);
    free(n);
    win->native = NULL;
    if (win->painter) { win->painter->pixels = NULL; win->painter->ext_pixels = 1; }
}

static int w32_window_flush(GuiWindowImpl *win) {
    gui_w32_win *n = (gui_w32_win *)win->native;
    if (!n || !n->hwnd || !n->memdc) return 0;
    void *hdc = U.GetDC(n->hwnd);
    if (!hdc) return 0;
    int mode = (strcmp(n->filter, "good") == 0 || strcmp(n->filter, "best") == 0)
               ? W32_HALFTONE : W32_COLORONCOLOR;
    U.SetStretchBltMode(hdc, mode);
    /* 客户区尺寸与 DIB 不一致（用户拉窗口）时，这一步就是 GDI 侧的抗锯齿缩放，
       对应 X11 端 XRender Picture 的 SetPictureFilter。 */
    gui_rect rc;
    int dw = win->w, dh = win->h;
    if (U.GetClientRect(n->hwnd, &rc) && rc.right > rc.left && rc.bottom > rc.top) {
        dw = rc.right - rc.left; dh = rc.bottom - rc.top;
    }
    U.StretchBlt(hdc, 0, 0, dw, dh, n->memdc, 0, 0,
                 win->painter ? win->painter->w : win->w,
                 win->painter ? win->painter->h : win->h, W32_SRCCOPY);
    U.ReleaseDC(n->hwnd, hdc);
    U.GdiFlush();
    return 1;
}

static int w32_window_resize(GuiWindowImpl *win, int w, int h) {
    gui_w32_win *n = (gui_w32_win *)win->native;
    if (!n || w <= 0 || h <= 0) return 0;
    char err[256];
    gui_rect r = { 0, 0, w, h };
    U.AdjustWindowRect(&r, W32_WS_OVERLAPPEDWINDOW, 0);
    U.SetWindowPos(n->hwnd, NULL, 0, 0, r.right - r.left, r.bottom - r.top, 0x0006u);
    release_surface(n);
    if (!setup_surface(win, n, w, h, err, (int)sizeof(err))) {
        gui_core_set_err(GUI_ERR_WINDOW, err);
        return 0;
    }
    win->w = w; win->h = h;
    return 1;
}

static int w32_window_set_title(GuiWindowImpl *win, const char *title) {
    gui_w32_win *n = (gui_w32_win *)win->native;
    return (n && n->hwnd) ? U.SetWindowTextA(n->hwnd, title) : 0;
}

static int w32_window_show(GuiWindowImpl *win) {
    gui_w32_win *n = (gui_w32_win *)win->native;
    if (!n || !n->hwnd) return 0;
    /* ShowWindow 返回的是「之前是否可见」，0 不代表失败——这里只报成功。 */
    U.ShowWindow(n->hwnd, W32_SW_SHOW);
    U.UpdateWindow(n->hwnd);
    return 1;
}

static int w32_window_set_filter(GuiWindowImpl *win, const char *filter) {
    gui_w32_win *n = (gui_w32_win *)win->native;
    if (!n || !filter || !*filter) return 0;
    if (strcmp(filter, "fast") && strcmp(filter, "good") &&
        strcmp(filter, "best") && strcmp(filter, "nearest")) return 0;
    strncpy(n->filter, filter, sizeof(n->filter) - 1);
    n->filter[sizeof(n->filter) - 1] = '\0';
    snprintf(n->backend, sizeof(n->backend),
             "win32/gdi/dibsection bgra filter=%s stretch=%s",
             n->filter,
             (strcmp(n->filter, "good") == 0 || strcmp(n->filter, "best") == 0)
                 ? "halftone" : "coloroncolor");
    return 1;
}

static const char *w32_window_backend(GuiWindowImpl *win) {
    gui_w32_win *n = (gui_w32_win *)win->native;
    return n ? n->backend : "win32/uninitialized";
}

static void dispatch_pending(int blocking, int timeout_ms) {
    gui_w32_msg msg;
    if (blocking && timeout_ms < 0) {
        if (U.GetMessageA(&msg, NULL, 0, 0) > 0) {
            U.TranslateMessage(&msg);
            U.DispatchMessageA(&msg);
        }
        return;
    }
    int waited = 0;
    for (;;) {
        if (U.PeekMessageA(&msg, NULL, 0, 0, W32_PM_REMOVE)) {
            U.TranslateMessage(&msg);
            U.DispatchMessageA(&msg);
            if (!blocking) continue;
            if (q_head != q_tail) return;
        }
        if (!blocking) return;
        if (waited >= timeout_ms) return;
        Sleep(2);
        waited += 2;
    }
}

static int pump(GuiAppImpl *app, int blocking, int timeout_ms) {
    dispatch_pending(blocking, timeout_ms);
    gui_w32_event e;
    if (!q_pop(&e)) return GUI_EV_NONE;
    GuiWindowImpl *w = find_win_obj(app, e.hwnd);
    gui_core_emit(app, e.kind, e.x, e.y, e.button, e.key, e.w, e.h, w);
    return e.kind;
}

static int w32_poll(GuiAppImpl *app) { return pump(app, 0, 0); }
static int w32_wait(GuiAppImpl *app, int timeout_ms) { return pump(app, 1, timeout_ms); }

static const GuiBackend g_backend = {
    "win32",
    w32_init, w32_fini,
    w32_window_create, w32_window_destroy, w32_window_flush, w32_window_resize,
    w32_window_set_title, w32_window_show, w32_window_set_filter,
    w32_window_backend,
    w32_poll, w32_wait,
};

const GuiBackend *gui_backend_win32(void) { return &g_backend; }
