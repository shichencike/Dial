/*
 * dial_gui_x11.c —— gui.dialpkg 的 Linux / Termux 后端
 *
 * 三件事，全部自己掌控：
 *   1) XShm：离屏像素放在 SYSV 共享内存段里，CPU 直接写；XShmAttach 让 X server
 *      也映到同一段，XShmPutImage 上传时**零拷贝**（不经过 socket 传像素）。
 *   2) XRender：把上传用的 Pixmap 与窗口各包成一个 Picture，SetPictureFilter
 *      设置平滑过滤，XRenderComposite(PictOpSrc) 合成上屏——窗口缩放 / 尺寸
 *      不一致时的抗锯齿由这一步负责，而不是让 X 服务器做最近邻放大。
 *   3) 事件循环：XConnectionNumber + poll 真正做到阻塞等待，事件结构按
 *      Xlib ABI 布局复刻，映射成 .diali 里平台无关的事件码。
 *
 * **零链接期依赖**：不 #include 任何 X11 头，所有 X11 / Xext / Xrender 函数
 * 都在 dial_gui_app_new() 里用 dlopen + dlsym 解析后通过函数指针调用。
 * 因此 .a 里没有任何 X11 未定义符号，dialc 的 final_link 不需要注入 -lX11，
 * 交叉编译 musl 也不需要宿主的 GUI SDK 头。缺库/缺扩展一律返回明确错误码，
 * 绝不静默降级成「只用基础 Xlib 画点线」。
 *
 * 已知边界（写进 README，不隐瞒）：纯静态 musl 产物的 dlopen 无法装载外部
 * .so，app_new 会返回 GUI_ERR_NO_BACKEND 并给出提示；glibc 动态环境与
 * Termux(bionic) 动态装载正常。
 */
#include "dial_gui.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <poll.h>
#include <sys/ipc.h>
#include <sys/shm.h>

/* ---- X11 ABI 常量（取自 X.h / Xlib.h / render.h，复刻而非 include）------ */
#define X_NONE            0UL
#define X_COPY_FROM_PARENT 0UL
#define X_INPUT_OUTPUT     1
#define X_ZPIXMAP          2
#define X_KEY_PRESS        2
#define X_KEY_RELEASE      3
#define X_BUTTON_PRESS     4
#define X_BUTTON_RELEASE   5
#define X_MOTION_NOTIFY    6
#define X_EXPOSE           12
#define X_DESTROY_NOTIFY   17
#define X_CONFIGURE_NOTIFY 22
#define X_CLIENT_MESSAGE   33

#define X_KEY_PRESS_MASK        (1L << 0)
#define X_KEY_RELEASE_MASK      (1L << 1)
#define X_BUTTON_PRESS_MASK     (1L << 2)
#define X_BUTTON_RELEASE_MASK   (1L << 3)
#define X_POINTER_MOTION_MASK   (1L << 6)
#define X_EXPOSURE_MASK         (1L << 15)
#define X_STRUCTURE_NOTIFY_MASK (1L << 17)

#define X_PICT_STANDARD_ARGB32 0
#define X_PICT_OP_SRC          1

typedef unsigned long gui_xid;

/* XShmSegmentInfo（X11/extensions/XShm.h）*/
typedef struct {
    gui_xid shmseg;
    int     shmid;
    char   *shmaddr;
    int     readOnly;
} gui_shm_seg;

/*
 * XEvent：Xlib 保证 union 固定 192 字节（long pad[24]）。这里按 Xlib 头文件
 * 的字段顺序复刻需要的几个视图，编译器产生的 padding 与上游一致（同为 LP64）。
 */
/* XErrorEvent（Xlib.h）：XSetErrorHandler 回调参数，按 LP64 布局复刻。 */
typedef struct {
    int type;
    void *display;
    gui_xid resourceid;
    unsigned long serial;
    unsigned char error_code;
    unsigned char request_code;
    unsigned char minor_code;
} gui_xerror_event;

typedef union {
    int  type;
    long pad[24];
    struct {
        int type; unsigned long serial; int send_event; void *display;
        gui_xid window;
    } any;
    struct {
        int type; unsigned long serial; int send_event; void *display;
        gui_xid window; gui_xid root; gui_xid subwindow; unsigned long time;
        int x, y; int x_root, y_root; unsigned int state; unsigned int detail;
        int same_screen;
    } key;  /* XKeyEvent / XButtonEvent / XMotionEvent 共用前半布局 */
    struct {
        int type; unsigned long serial; int send_event; void *display;
        gui_xid window; int x, y; int width, height; int count;
    } expose;
    struct {
        int type; unsigned long serial; int send_event; void *display;
        gui_xid event; gui_xid window; int x, y; int width, height;
        int border_width; gui_xid above; int override_redirect;
    } configure;
    struct {
        int type; unsigned long serial; int send_event; void *display;
        gui_xid window; gui_xid message_type; int format;
        union { char b[20]; short s[10]; long l[5]; } data;
    } client;
} gui_xevent;

/* ---- 动态解析出的函数指针表 -------------------------------------------- */
typedef struct {
    void *h_x11, *h_xext, *h_render;

    void *(*XOpenDisplay)(const char *);
    int (*XCloseDisplay)(void *);
    int (*XDefaultScreen)(void *);
    void *(*XDefaultVisual)(void *, int);
    int (*XDefaultDepth)(void *, int);
    gui_xid (*XDefaultRootWindow)(void *);
    gui_xid (*XCreateSimpleWindow)(void *, gui_xid, int, int,
                                   unsigned int, unsigned int, unsigned int,
                                   unsigned long, unsigned long);
    int (*XStoreName)(void *, gui_xid, const char *);
    int (*XSelectInput)(void *, gui_xid, long);
    int (*XMapWindow)(void *, gui_xid);
    int (*XResizeWindow)(void *, gui_xid, unsigned int, unsigned int);
    int (*XDestroyWindow)(void *, gui_xid);
    int (*XFlush)(void *);
    int (*XSync)(void *, int);
    int (*XPending)(void *);
    int (*XNextEvent)(void *, gui_xevent *);
    gui_xid (*XInternAtom)(void *, const char *, int);
    int (*XSetWMProtocols)(void *, gui_xid, gui_xid *, int);
    void *(*XCreateGC)(void *, gui_xid, unsigned long, void *);
    int (*XFreeGC)(void *, void *);
    int (*XLookupString)(void *, char *, int, unsigned long *, void *);
    gui_xid (*XCreatePixmap)(void *, gui_xid, unsigned int, unsigned int, unsigned int);
    int (*XFreePixmap)(void *, gui_xid);
    int (*XFree)(void *);
    int (*XConnectionNumber)(void *);
    int (*XSetErrorHandler)(int (*)(void *, gui_xerror_event *));

    int (*XShmQueryExtension)(void *);
    void *(*XShmCreateImage)(void *, void *, unsigned int, int, char *,
                             gui_shm_seg *, unsigned int, unsigned int);
    int (*XShmAttach)(void *, gui_shm_seg *);
    int (*XShmDetach)(void *, gui_shm_seg *);
    int (*XShmPutImage)(void *, gui_xid, void *, void *, int, int, int, int,
                        unsigned int, unsigned int, int);

    int (*XRenderQueryExtension)(void *, int *, int *);
    void *(*XRenderFindVisualFormat)(void *, void *);
    gui_xid (*XRenderCreatePicture)(void *, gui_xid, void *, unsigned long, void *);
    void (*XRenderFreePicture)(void *, gui_xid);
    void (*XRenderSetPictureFilter)(void *, gui_xid, const char *, void *, int);
    void (*XRenderComposite)(void *, int, gui_xid, gui_xid, gui_xid,
                             int, int, int, int, int, int, unsigned int, unsigned int);
} gui_x11_api;

static gui_x11_api A;
static int g_api_loaded = 0;

/*
 * Xlib 默认错误处理器会**直接 exit(1)** —— 对一个库来说等于把宿主进程带走，
 * 且与 §20 Fail-Fast 的「明确报错」口径不符。这里换成自己的处理器：只记录
 * 错误码与请求号，由 setup_surface / window_create 在 XSync 之后检查并转成
 * GUI_ERR_WINDOW + 人类可读消息返回。X 请求是异步的，必须 XSync 才拿得到。
 */
static _Thread_local int  g_xerr = 0;
static _Thread_local char g_xerr_detail[160] = {0};

static int gui_xerr_handler(void *dpy, gui_xerror_event *e) {
    (void)dpy;
    if (!e) { g_xerr = -1; return 0; }
    g_xerr = (int)e->error_code;
    snprintf(g_xerr_detail, sizeof(g_xerr_detail),
             "X11 错误 %u（request %u.%u, serial %lu）：%s",
             (unsigned)e->error_code, (unsigned)e->request_code,
             (unsigned)e->minor_code, e->serial,
             e->error_code == 10 ? "BadAccess（共享内存段未被正确 attach，检查 shmid）"
                                 : "请求被 X server 拒绝");
    return 0;
}
static void xerr_reset(void) { g_xerr = 0; g_xerr_detail[0] = '\0'; }
static int xerr_check(char *err, int errcap) {
    if (!g_xerr) return 1;
    snprintf(err, (size_t)errcap, "%s", g_xerr_detail);
    return 0;
}

static void *open_lib(const char *const *names) {
    for (int i = 0; names[i]; ++i) {
        void *h = dlopen(names[i], RTLD_LAZY | RTLD_LOCAL);
        if (h) return h;
    }
    return NULL;
}

#define LOAD(h, name)                                                        \
    do {                                                                     \
        void *q_ = dlsym((h), #name);                                         \
        if (!q_) {                                                           \
            snprintf(err, (size_t)errcap, "动态库缺少符号 %s", #name);         \
            return 0;                                                        \
        }                                                                    \
        memcpy(&(A.name), &q_, sizeof(q_));                                   \
    } while (0)

static int load_api(char *err, int errcap) {
    if (g_api_loaded) return 1;
    static const char *const x11_names[]    = { "libX11.so.6", "libX11.so", NULL };
    static const char *const xext_names[]   = { "libXext.so.6", "libXext.so", NULL };
    static const char *const xren_names[]   = { "libXrender.so.1", "libXrender.so", NULL };
    A.h_x11 = open_lib(x11_names);
    if (!A.h_x11) { snprintf(err, (size_t)errcap, "无法装载 libX11.so（请安装 X11 客户端库）"); return 0; }
    A.h_xext = open_lib(xext_names);
    if (!A.h_xext) { snprintf(err, (size_t)errcap, "无法装载 libXext.so（XShm 需要）"); return 0; }
    A.h_render = open_lib(xren_names);
    if (!A.h_render) { snprintf(err, (size_t)errcap, "无法装载 libXrender.so（XRender 抗锯齿合成需要）"); return 0; }

    LOAD(A.h_x11, XOpenDisplay); LOAD(A.h_x11, XCloseDisplay);
    LOAD(A.h_x11, XDefaultScreen); LOAD(A.h_x11, XDefaultVisual);
    LOAD(A.h_x11, XDefaultDepth); LOAD(A.h_x11, XDefaultRootWindow);
    LOAD(A.h_x11, XCreateSimpleWindow); LOAD(A.h_x11, XStoreName);
    LOAD(A.h_x11, XSelectInput); LOAD(A.h_x11, XMapWindow);
    LOAD(A.h_x11, XResizeWindow); LOAD(A.h_x11, XDestroyWindow);
    LOAD(A.h_x11, XFlush); LOAD(A.h_x11, XSync);
    LOAD(A.h_x11, XPending); LOAD(A.h_x11, XNextEvent);
    LOAD(A.h_x11, XInternAtom); LOAD(A.h_x11, XSetWMProtocols);
    LOAD(A.h_x11, XCreateGC); LOAD(A.h_x11, XFreeGC);
    LOAD(A.h_x11, XLookupString); LOAD(A.h_x11, XCreatePixmap);
    LOAD(A.h_x11, XFreePixmap); LOAD(A.h_x11, XFree);
    LOAD(A.h_x11, XConnectionNumber); LOAD(A.h_x11, XSetErrorHandler);

    LOAD(A.h_xext, XShmQueryExtension); LOAD(A.h_xext, XShmCreateImage);
    LOAD(A.h_xext, XShmAttach); LOAD(A.h_xext, XShmDetach);
    LOAD(A.h_xext, XShmPutImage);

    LOAD(A.h_render, XRenderQueryExtension); LOAD(A.h_render, XRenderFindVisualFormat);
    LOAD(A.h_render, XRenderCreatePicture); LOAD(A.h_render, XRenderFreePicture);
    LOAD(A.h_render, XRenderSetPictureFilter); LOAD(A.h_render, XRenderComposite);

    g_api_loaded = 1;
    return 1;
}
#undef LOAD

/* 不 dlclose：进程内可能还有其他 App 正持有函数指针；常驻到进程退出更安全。 */

/* ---- 后端私有数据 ------------------------------------------------------ */
typedef struct {
    void   *dpy;
    int     screen;
    void   *visual;
    gui_xid root;
    int     depth;
    gui_xid wm_delete;
    int     render_ok;
} gui_x11_app;

typedef struct {
    gui_xid win;
    void   *gc;
    int     shmid;
    void   *shmaddr;
    size_t  shm_size;
    gui_shm_seg seg;
    void   *ximage;
    gui_xid pixmap;
    gui_xid src_pic;
    gui_xid dst_pic;
    char    filter[16];
    char    backend[160];
} gui_x11_win;

static GuiWindowImpl *find_win_obj(GuiAppImpl *app, gui_xid xwin) {
    for (GuiWindowImpl *w = app->windows; w; w = w->next) {
        gui_x11_win *n = (gui_x11_win *)w->native;
        if (n && n->win == xwin) return w;
    }
    return NULL;
}

/* 释放一份 XShm 段与 XRender 合成链（resize 时复用）。 */
static void release_surface(gui_x11_app *a, gui_x11_win *n) {
    if (!a || !n) return;
    if (n->src_pic) { A.XRenderFreePicture(a->dpy, n->src_pic); n->src_pic = 0; }
    if (n->dst_pic) { A.XRenderFreePicture(a->dpy, n->dst_pic); n->dst_pic = 0; }
    if (n->pixmap)  { A.XFreePixmap(a->dpy, n->pixmap);          n->pixmap = 0; }
    if (n->ximage)  { A.XShmDetach(a->dpy, &n->seg); A.XFree(n->ximage); n->ximage = NULL; }
    if (n->shmaddr && n->shmaddr != (void *)-1) shmdt(n->shmaddr);
    n->shmaddr = NULL;
    if (n->shmid >= 0) shmctl(n->shmid, IPC_RMID, NULL);
    n->shmid = -1;
}

/*
 * 建立 XShm 离屏缓冲 + XRender 合成链。
 * 成功后把窗口画布的 pixels 直接指向共享内存（ext_pixels=1），
 * 从此 CPU 画像素 = 直接写 X server 也能看到的同一段内存。
 */
static int setup_surface(GuiWindowImpl *win, gui_x11_app *a, gui_x11_win *n,
                         int w, int h, char *err, int errcap) {
    n->shmid = -1;
    n->shmaddr = NULL;
    n->ximage = NULL;
    n->pixmap = n->src_pic = n->dst_pic = 0;
    xerr_reset();

    size_t bytes = (size_t)w * (size_t)h * 4u;
    n->shmid = shmget(IPC_PRIVATE, bytes, IPC_CREAT | 0600);
    if (n->shmid < 0) { snprintf(err, (size_t)errcap, "shmget 失败（%zu 字节）", bytes); return 0; }
    n->shmaddr = shmat(n->shmid, NULL, 0);
    if (n->shmaddr == (void *)-1 || !n->shmaddr) {
        snprintf(err, (size_t)errcap, "shmat 失败"); return 0;
    }
    n->shm_size = bytes;
    memset(n->shmaddr, 0, bytes);

    memset(&n->seg, 0, sizeof(n->seg));
    /*
     * **必须自己填 shmid**：XShmCreateImage 只填 shmaddr / shmseg，不会把
     * shmget 返回的内核 id 写进 shminfo；XShmAttach 靠它去 attach 同一段。
     * 漏填时 shmid=0，恰好第一次 IPC_PRIVATE 段号为 0 会「碰巧成功」，之后
     * 段号递增就稳定 BadAccess —— 这是最隐蔽的一类间歇性 bug，绝不留着。
     */
    n->seg.shmid = n->shmid;
    n->seg.shmaddr = (char *)n->shmaddr;
    n->seg.readOnly = 0;
    n->ximage = A.XShmCreateImage(a->dpy, a->visual, (unsigned int)a->depth,
                                  X_ZPIXMAP, (char *)n->shmaddr, &n->seg,
                                  (unsigned int)w, (unsigned int)h);
    if (!n->ximage) { snprintf(err, (size_t)errcap, "XShmCreateImage 失败"); return 0; }
    if (!A.XShmAttach(a->dpy, &n->seg)) {
        snprintf(err, (size_t)errcap, "XShmAttach 失败（X server 不支持 MIT-SHM）"); return 0;
    }
    /* X 请求异步；立刻同步一次，把 BadAccess 之类转成返回码而不是让宿主崩。 */
    A.XSync(a->dpy, 0);
    if (!xerr_check(err, errcap)) return 0;
    /* 标记删除：所有 attach 方都脱离后由内核回收，进程崩溃也不留段。 */
    shmctl(n->shmid, IPC_RMID, NULL);

    n->pixmap = A.XCreatePixmap(a->dpy, a->root, (unsigned int)w, (unsigned int)h,
                                (unsigned int)a->depth);
    void *fmt = A.XRenderFindVisualFormat(a->dpy, a->visual);
    if (!n->pixmap || !fmt) {
        snprintf(err, (size_t)errcap, "XRenderFindVisualFormat / XCreatePixmap 失败"); return 0;
    }
    n->src_pic = A.XRenderCreatePicture(a->dpy, n->pixmap, fmt, 0, NULL);
    n->dst_pic = A.XRenderCreatePicture(a->dpy, n->win, fmt, 0, NULL);
    if (!n->src_pic || !n->dst_pic) {
        snprintf(err, (size_t)errcap, "XRenderCreatePicture 失败"); return 0;
    }
    A.XRenderSetPictureFilter(a->dpy, n->src_pic, n->filter, NULL, 0);
    A.XSync(a->dpy, 0);
    if (!xerr_check(err, errcap)) return 0;

    /* CPU 像素缓冲换成共享内存：先释放 gui_painter_new 分配的普通内存。 */
    if (win->painter) {
        if (!win->painter->ext_pixels) free(win->painter->pixels);
        win->painter->pixels = (uint32_t *)n->shmaddr;
        win->painter->w = w; win->painter->h = h;
        win->painter->ext_pixels = 1;
        win->painter->has_clip = 0;
    }
    snprintf(n->backend, sizeof(n->backend),
             "x11/xshm/xrender argb32 filter=%s visual_depth=%d", n->filter, a->depth);
    return 1;
}

/* ---- vtable ------------------------------------------------------------ */

static int x11_init(GuiAppImpl *app, char *err, int errcap) {
    if (!load_api(err, errcap)) return 0;
    /* 先装错误处理器再发任何 X 请求：默认处理器会 exit(1) 带走宿主进程。 */
    A.XSetErrorHandler(gui_xerr_handler);
    gui_x11_app *a = (gui_x11_app *)calloc(1, sizeof(gui_x11_app));
    if (!a) { snprintf(err, (size_t)errcap, "内存分配失败"); return 0; }
    a->dpy = A.XOpenDisplay(NULL);
    if (!a->dpy) {
        free(a);
        snprintf(err, (size_t)errcap,
                 "无法连接 X11 显示：DISPLAY 未设置或 Xvfb 未启动（无头环境请先启动 Xvfb）");
        return 0;
    }
    a->screen = A.XDefaultScreen(a->dpy);
    a->visual = A.XDefaultVisual(a->dpy, a->screen);
    a->depth  = A.XDefaultDepth(a->dpy, a->screen);
    a->root   = A.XDefaultRootWindow(a->dpy);
    a->wm_delete = A.XInternAtom(a->dpy, "WM_DELETE_WINDOW", 0);

    if (!A.XShmQueryExtension(a->dpy)) {
        A.XCloseDisplay(a->dpy); free(a);
        snprintf(err, (size_t)errcap, "X server 不支持 MIT-SHM 扩展（XShm 为硬性要求，不降级）");
        return 0;
    }
    int ev = 0, er = 0;
    a->render_ok = A.XRenderQueryExtension(a->dpy, &ev, &er) ? 1 : 0;
    if (!a->render_ok) {
        A.XCloseDisplay(a->dpy); free(a);
        snprintf(err, (size_t)errcap, "X server 不支持 RENDER 扩展（XRender 抗锯齿合成为硬性要求，不降级）");
        return 0;
    }
    app->native = a;
    return 1;
}

static void x11_fini(GuiAppImpl *app) {
    gui_x11_app *a = (gui_x11_app *)app->native;
    if (a && a->dpy) A.XCloseDisplay(a->dpy);
    free(a);
    app->native = NULL;
}

static int x11_window_create(GuiWindowImpl *win, const char *title, int w, int h,
                             char *err, int errcap) {
    gui_x11_app *a = (gui_x11_app *)win->app->native;
    gui_x11_win *n = (gui_x11_win *)calloc(1, sizeof(gui_x11_win));
    if (!a || !n) { free(n); snprintf(err, (size_t)errcap, "内存分配失败"); return 0; }
    strncpy(n->filter, "good", sizeof(n->filter) - 1);
    n->shmid = -1;

    n->win = A.XCreateSimpleWindow(a->dpy, a->root, 40, 40,
                                   (unsigned int)w, (unsigned int)h, 0, 0, 0);
    if (!n->win) { free(n); snprintf(err, (size_t)errcap, "XCreateSimpleWindow 失败"); return 0; }
    A.XStoreName(a->dpy, n->win, title);
    A.XSelectInput(a->dpy, n->win,
                   X_KEY_PRESS_MASK | X_KEY_RELEASE_MASK |
                   X_BUTTON_PRESS_MASK | X_BUTTON_RELEASE_MASK |
                   X_POINTER_MOTION_MASK | X_EXPOSURE_MASK | X_STRUCTURE_NOTIFY_MASK);
    gui_xid protos[1] = { a->wm_delete };
    A.XSetWMProtocols(a->dpy, n->win, protos, 1);
    A.XMapWindow(a->dpy, n->win);
    n->gc = A.XCreateGC(a->dpy, n->win, 0, NULL);
    if (!n->gc) { free(n); snprintf(err, (size_t)errcap, "XCreateGC 失败"); return 0; }
    xerr_reset();
    A.XSync(a->dpy, 0);
    if (!xerr_check(err, errcap)) {
        A.XFreeGC(a->dpy, n->gc);
        A.XDestroyWindow(a->dpy, n->win);
        free(n);
        return 0;
    }

    win->native = n;
    if (!setup_surface(win, a, n, w, h, err, errcap)) {
        release_surface(a, n);
        A.XFreeGC(a->dpy, n->gc);
        A.XDestroyWindow(a->dpy, n->win);
        win->native = NULL;
        free(n);
        return 0;
    }
    return 1;
}

static void x11_window_destroy(GuiWindowImpl *win) {
    gui_x11_app *a = (gui_x11_app *)win->app->native;
    gui_x11_win *n = (gui_x11_win *)win->native;
    if (!a || !n) return;
    release_surface(a, n);
    if (n->gc) A.XFreeGC(a->dpy, n->gc);
    if (n->win) A.XDestroyWindow(a->dpy, n->win);
    A.XFlush(a->dpy);
    free(n);
    win->native = NULL;
    /* 画布的 pixels 指向已 shmdt 的共享内存，置空避免 dispose 误 free。 */
    if (win->painter) { win->painter->pixels = NULL; win->painter->ext_pixels = 1; }
}

static int x11_window_flush(GuiWindowImpl *win) {
    gui_x11_app *a = (gui_x11_app *)win->app->native;
    gui_x11_win *n = (gui_x11_win *)win->native;
    if (!a || !n || !n->ximage) return 0;
    xerr_reset();
    /* 1) XShm 零拷贝把共享内存上传到中转 Pixmap。 */
    A.XShmPutImage(a->dpy, n->pixmap, n->gc, n->ximage, 0, 0, 0, 0,
                   (unsigned int)win->w, (unsigned int)win->h, 0);
    /* 2) XRender 把 Pixmap Picture 合成到窗口 Picture（filter 负责平滑/AA）。 */
    A.XRenderComposite(a->dpy, X_PICT_OP_SRC, n->src_pic, X_NONE, n->dst_pic,
                       0, 0, 0, 0, 0, 0,
                       (unsigned int)win->w, (unsigned int)win->h);
    A.XSync(a->dpy, 0);
    if (g_xerr) {
        gui_core_set_err(GUI_ERR_WINDOW, g_xerr_detail);
        return 0;
    }
    A.XFlush(a->dpy);
    return 1;
}

static int x11_window_resize(GuiWindowImpl *win, int w, int h) {
    gui_x11_app *a = (gui_x11_app *)win->app->native;
    gui_x11_win *n = (gui_x11_win *)win->native;
    if (!a || !n || w <= 0 || h <= 0) return 0;
    char err[256];
    release_surface(a, n);
    A.XResizeWindow(a->dpy, n->win, (unsigned int)w, (unsigned int)h);
    A.XSync(a->dpy, 0);
    if (!setup_surface(win, a, n, w, h, err, (int)sizeof(err))) {
        gui_core_set_err(GUI_ERR_WINDOW, err);
        return 0;
    }
    win->w = w; win->h = h;
    return 1;
}

static int x11_window_set_title(GuiWindowImpl *win, const char *title) {
    gui_x11_app *a = (gui_x11_app *)win->app->native;
    gui_x11_win *n = (gui_x11_win *)win->native;
    if (!a || !n) return 0;
    A.XStoreName(a->dpy, n->win, title);
    A.XFlush(a->dpy);
    return 1;
}

static int x11_window_show(GuiWindowImpl *win) {
    gui_x11_app *a = (gui_x11_app *)win->app->native;
    gui_x11_win *n = (gui_x11_win *)win->native;
    if (!a || !n) return 0;
    A.XMapWindow(a->dpy, n->win);
    A.XFlush(a->dpy);
    return 1;
}

static int x11_window_set_filter(GuiWindowImpl *win, const char *filter) {
    gui_x11_app *a = (gui_x11_app *)win->app->native;
    gui_x11_win *n = (gui_x11_win *)win->native;
    if (!a || !n || !filter || !*filter) return 0;
    if (strcmp(filter, "fast") && strcmp(filter, "good") &&
        strcmp(filter, "best") && strcmp(filter, "nearest")) return 0;
    strncpy(n->filter, filter, sizeof(n->filter) - 1);
    n->filter[sizeof(n->filter) - 1] = '\0';
    A.XRenderSetPictureFilter(a->dpy, n->src_pic, n->filter, NULL, 0);
    A.XFlush(a->dpy);
    snprintf(n->backend, sizeof(n->backend),
             "x11/xshm/xrender argb32 filter=%s visual_depth=%d", n->filter, a->depth);
    return 1;
}

static const char *x11_window_backend(GuiWindowImpl *win) {
    gui_x11_win *n = (gui_x11_win *)win->native;
    return n ? n->backend : "x11/uninitialized";
}

/* UTF-8 首码点（键盘事件用；与 raster.c 的解码器同规则）。 */
static int first_codepoint(const char *b, int n) {
    if (n <= 0) return 0;
    unsigned char c = (unsigned char)b[0];
    if (c < 0x80) return c;
    int need = (c & 0xE0) == 0xC0 ? 2 : (c & 0xF0) == 0xE0 ? 3 :
               (c & 0xF8) == 0xF0 ? 4 : 0;
    if (!need || n < need) return 0xFFFD;
    uint32_t v = c & (0x1F >> (need - 2));
    for (int i = 1; i < need; ++i) {
        unsigned char cc = (unsigned char)b[i];
        if ((cc & 0xC0) != 0x80) return 0xFFFD;
        v = (v << 6) | (cc & 0x3F);
    }
    return (int)v;
}

static int pump(GuiAppImpl *app, int blocking, int timeout_ms) {
    gui_x11_app *a = (gui_x11_app *)app->native;
    if (!a || !a->dpy) return GUI_EV_NONE;
    if (blocking && A.XPending(a->dpy) == 0) {
        struct pollfd pfd;
        pfd.fd = A.XConnectionNumber(a->dpy);
        pfd.events = POLLIN;
        pfd.revents = 0;
        if (pfd.fd >= 0) poll(&pfd, 1, timeout_ms < 0 ? -1 : timeout_ms);
    }
    if (A.XPending(a->dpy) == 0) return GUI_EV_NONE;

    gui_xevent ev;
    memset(&ev, 0, sizeof(ev));
    A.XNextEvent(a->dpy, &ev);
    GuiWindowImpl *w = find_win_obj(app, ev.any.window);

    int kind = GUI_EV_NONE, x = 0, y = 0, button = 0, key = 0, ww = 0, hh = 0;
    switch (ev.type) {
    case X_KEY_PRESS: case X_KEY_RELEASE: {
        char buf[32]; unsigned long sym = 0;
        int n = A.XLookupString(&ev, buf, (int)sizeof(buf), &sym, NULL);
        key = n > 0 ? first_codepoint(buf, n) : (int)(sym & 0xFFFFu);
        x = ev.key.x; y = ev.key.y;
        kind = ev.type == X_KEY_PRESS ? GUI_EV_KEY_PRESS : GUI_EV_KEY_RELEASE;
        break;
    }
    case X_BUTTON_PRESS: case X_BUTTON_RELEASE:
        x = ev.key.x; y = ev.key.y;
        button = (int)ev.key.detail;
        kind = ev.type == X_BUTTON_PRESS ? GUI_EV_MOUSE_PRESS : GUI_EV_MOUSE_RELEASE;
        break;
    case X_MOTION_NOTIFY:
        x = ev.key.x; y = ev.key.y;
        kind = GUI_EV_MOUSE_MOVE;
        break;
    case X_EXPOSE:
        kind = GUI_EV_EXPOSE;
        break;
    case X_CONFIGURE_NOTIFY:
        ww = ev.configure.width; hh = ev.configure.height;
        kind = GUI_EV_RESIZE;
        break;
    case X_CLIENT_MESSAGE:
        if (ev.client.message_type == a->wm_delete &&
            ev.client.data.l[0] == (long)a->wm_delete)
            kind = GUI_EV_CLOSE;
        break;
    case X_DESTROY_NOTIFY:
        kind = GUI_EV_CLOSE;
        break;
    default:
        break;
    }
    if (kind == GUI_EV_NONE) return GUI_EV_NONE;
    /*
     * 控件命中测试放在 pump 里 —— 这是 Dial 的 poll/wait 调用线程，
     * 不是 Xlib 回调。gui_widget_hit 是纯 C、零平台符号、零 ARC 交互，
     * 因此不会把跨线程引用计数问题带进来（内存宪法：绝不 C → Dial 回调）。
     */
    GuiWidgetImpl *hit = NULL;
    if (w && w->root &&
        (kind == GUI_EV_MOUSE_PRESS || kind == GUI_EV_MOUSE_RELEASE ||
         kind == GUI_EV_MOUSE_MOVE)) {
        hit = gui_widget_hit(w->root, x, y, NULL, NULL);
    }
    gui_core_emit(app, kind, x, y, button, key, ww, hh, w, hit);
    return kind;
}

static int x11_poll(GuiAppImpl *app)   { return pump(app, 0, 0); }
static int x11_wait(GuiAppImpl *app, int timeout_ms) { return pump(app, 1, timeout_ms); }

static const GuiBackend g_backend = {
    "x11",
    x11_init, x11_fini,
    x11_window_create, x11_window_destroy, x11_window_flush, x11_window_resize,
    x11_window_set_title, x11_window_show, x11_window_set_filter,
    x11_window_backend,
    x11_poll, x11_wait,
};

const GuiBackend *gui_backend_posix(void) { return &g_backend; }
