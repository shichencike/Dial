/*
 * dial_gui_platform.c —— 第 18 层：GPU 合成 + 原生平台集成「纯值契约层」
 *
 * 【本文件的定位：经用户授权新增的唯一一个 .c 文件】
 * 17 层以来宪法一直是「不新增 .c 文件、build.sh 源清单不变」。第 18 层为把
 * GPU 合成后端与原生平台集成（系统托盘 / 原生文件框 / 全局快捷键）的**对外
 * 契约**独立成层，经授权仅放宽「新增 .c」这一条（build.sh SRCS 同步 1 行）。
 * 其余宪法一律照守：纯 C、零链接期平台符号、零 C -> Dial 反向回调、
 * 逐像素 CRC 确定性、abi=6 / version=1.10.0 不变、旧签名一字不动。
 *
 * 【本轮做什么、不做什么】
 * 第 18 层：纯值契约层——extern fn + Fail-Fast 参数校验 + 能力探测 + 确定性降级码。
 * 第 19 层：真实 GPU 合成（受控破例）——追加 dial_gui_gpu_real_* 新 API，
 *   Linux 运行时 dlopen libEGL/libGLESv2，EGL 1.5 surfaceless + GLES2 恒等 shader
 *   把 CPU framebuffer 上传为纹理、离屏 FBO 合成、读回逐字节校验；**绝不修改源像素**。
 *   旧 9 个 API（dial_gui_gpu_available/try_composite 等）行为一字不动，仍恒降级 —— 
 *   它们被 use_gui_platform.dial 的 30 条断言钉死，是冻结的兼容 ABI。
 *
 * 【第 19 层破例边界（用户裁决「继续」= 采纳最小破例 + Linux 真合成）】
 * 破例仅一条：运行时 dlopen EGL/GLES 做离屏合成。其余宪法原样保留：
 *   - 零链接期平台符号（全部 dlsym 函数指针，不 include GL/EGL 头）；
 *   - 零 C -> Dial 反向回调（合成是纯同步调用，不回调 Dial）；
 *   - 逐像素 CRC 确定性（恒等 shader 读回与源逐字节相等，且不写 p->pixels）；
 *   - abi=6 / version=1.10.0 不变，旧签名一字不动（只追加新 extern fn）；
 *   - dlopen / 初始化失败一律软降级 -2，绝不因缺库让可执行文件加载失败；
 *   - Windows / macOS / Termux 无法实机验证，条件编译恒降级 -2（诚实标注）。
 */
#include "dial_gui.h"

#include <stdlib.h>
#include <string.h>

/* 进程级纯值能力位（无句柄、无状态、无时钟、无平台符号）。
 * 本轮全部恒为 0 / "none" ——真实后端未装载。留成 getter 而非编译期常量，
 * 是为了让下一批接入真实探测时签名不变、旧程序行为不漂移。 */
#define GPU_CAP_NONE   ((int64_t)0)

/* 借用串：C 侧静态常量，Dial 侧用 string_from_cstr 复制成 ARC string。
 * 返回 const char* 在 gui.diali 里声明为 rawptr<byte>（绝不返回 DialString）。*/
static const char *g_backend_name   = "none";
static const char *g_degrade_reason =
    "no GPU backend / no /dev/dri / headless: real composite & native dialog not loaded (deferred)";

/* 能力探测：本轮恒「未装载」= 0，绝不谎报可用（谎报会让 Dial 侧误以为能合成）。*/
int64_t dial_gui_gpu_available(void) {
    gui_core_ok();
    return 0;
}
const char *dial_gui_gpu_backend(void) {
    gui_core_ok();
    return g_backend_name;
}
int64_t dial_gui_gpu_caps(void) {
    gui_core_ok();
    return GPU_CAP_NONE;
}

/* 尝试 GPU 合成：先 Fail-Fast 校验画布句柄，再返回确定性降级码。
 * 本轮不读 / 不改 p->pixels 一个字节（绝不触碰 framebuffer，CRC 不漂移）。*/
int64_t dial_gui_gpu_try_composite(GuiPainterImpl *p) {
    if (!p || !p->pixels) {
        gui_core_set_err(GUI_ERR_ARG, "GPU 合成：空 / 已释放的 GuiPainter 句柄");
        return -1;
    }
    if (p->w <= 0 || p->h <= 0) {
        gui_core_set_err(GUI_ERR_ARG, "GPU 合成：画布尺寸非法");
        return -1;
    }
    /* 能力未装载 -> 确定性降级，不写错误槽（这是「正常的能力缺失」而非参数错误）。*/
    gui_core_ok();
    return GUI_PLATFORM_DEGRADE;
}
const char *dial_gui_gpu_degrade_reason(void) {
    gui_core_ok();
    return g_degrade_reason;
}

/* 系统托盘：slot 0..15、flags 为位掩码（当前只校验低 8 位合法），恒降级。*/
int64_t dial_gui_tray_set(int64_t slot, int64_t flags) {
    if (slot < 0 || slot > 15 || flags < 0 || flags > 255) {
        gui_core_set_err(GUI_ERR_ARG, "系统托盘：slot 0..15、flags 0..255");
        return -1;
    }
    gui_core_ok();
    return GUI_PLATFORM_DEGRADE;
}

/* 原生文件框：mode 0=打开 / 1=保存 / 2=选目录，恒降级（不弹窗）。*/
int64_t dial_gui_native_file_dialog(int64_t mode) {
    if (mode < 0 || mode > 2) {
        gui_core_set_err(GUI_ERR_ARG, "原生文件框：mode 0=打开 / 1=保存 / 2=选目录");
        return -1;
    }
    gui_core_ok();
    return GUI_PLATFORM_DEGRADE;
}

/* 全局快捷键：id 0..1023、mods 位掩码低 8 位、key 0..0x10FFFF（码点），恒降级。*/
int64_t dial_gui_global_hotkey_register(int64_t id, int64_t mods, int64_t key) {
    if (id < 0 || id > 1023 || mods < 0 || mods > 255 || key < 0 || key > 0x10FFFF) {
        gui_core_set_err(GUI_ERR_ARG, "全局快捷键：id 0..1023、mods 0..255、key 0..0x10FFFF");
        return -1;
    }
    gui_core_ok();
    return GUI_PLATFORM_DEGRADE;
}

/* 降级码常量 getter：让 Dial 侧无需硬编码 -2 即可比对「能力未装载」。*/
int64_t dial_gui_platform_degrade_code(void) {
    gui_core_ok();
    return GUI_PLATFORM_DEGRADE;
}

/* ==========================================================================
 * 第 19 层：真实 GPU 合成后端（受控破例，只追加新 API，旧 9 个一字不动）
 * --------------------------------------------------------------------------
 * Linux：运行时 dlopen libEGL.so.1 / libGLESv2.so.2，EGL 1.5 surfaceless +
 *   GLES2 恒等 shader，把 CPU framebuffer 上传为纹理 → 离屏 FBO 合成 → 读回
 *   逐字节校验。恒等 shader + 同一 GL_RGBA 解释 ⇒ 读回字节 == 源字节（不 swizzle、
 *   不翻转）；绝不写 p->pixels，CRC 不漂移。
 * 其余平台：无法实机验证，条件编译恒降级 -2（诚实标注，不假装验证过）。
 * 所有 EGL/GLES 入口一律 dlsym 函数指针，零链接期符号、不 include 平台头。
 * ========================================================================== */
#if defined(__linux__)

#include <dlfcn.h>

/* ---- EGL 常量（自声明，不 include EGL 头）---- */
typedef void  *EGLDisplay; typedef void *EGLConfig;
typedef void  *EGLSurface; typedef void *EGLContext;
typedef unsigned int EGLBoolean; typedef int EGLint;
#define DIAL_EGL_DEFAULT_DISPLAY      ((void*)0)
#define DIAL_EGL_NO_DISPLAY           ((void*)0)
#define DIAL_EGL_NO_CONTEXT           ((void*)0)
#define DIAL_EGL_NO_SURFACE           ((void*)0)
#define DIAL_EGL_SURFACE_TYPE         0x3033
#define DIAL_EGL_PBUFFER_BIT          0x0001
#define DIAL_EGL_RENDERABLE_TYPE      0x3040
#define DIAL_EGL_OPENGL_ES2_BIT       0x0004
#define DIAL_EGL_NONE                 0x3038
#define DIAL_EGL_WIDTH                0x3057
#define DIAL_EGL_HEIGHT               0x3058
#define DIAL_EGL_OPENGL_ES_API        0x30A0
#define DIAL_EGL_CONTEXT_CLIENT_VERSION 0x3098
#define DIAL_EGL_PLATFORM_SURFACELESS_MESA 0x31DD

/* ---- GLES2 常量（自声明）---- */
#define DIAL_GL_TEXTURE_2D        0x0DE1
#define DIAL_GL_RGBA              0x1908
#define DIAL_GL_UNSIGNED_BYTE     0x1401
#define DIAL_GL_TEXTURE_MIN_FILTER 0x2801
#define DIAL_GL_TEXTURE_MAG_FILTER 0x2800
#define DIAL_GL_TEXTURE_WRAP_S    0x2802
#define DIAL_GL_TEXTURE_WRAP_T    0x2803
#define DIAL_GL_CLAMP_TO_EDGE     0x812F
#define DIAL_GL_NEAREST           0x2600
#define DIAL_GL_FRAGMENT_SHADER   0x8B30
#define DIAL_GL_VERTEX_SHADER     0x8B31
#define DIAL_GL_COMPILE_STATUS    0x8B81
#define DIAL_GL_LINK_STATUS       0x8B82
#define DIAL_GL_ARRAY_BUFFER      0x8892
#define DIAL_GL_STATIC_DRAW       0x88E4
#define DIAL_GL_FLOAT             0x1406
#define DIAL_GL_TRIANGLE_STRIP    0x0005
#define DIAL_GL_FRAMEBUFFER       0x8D40
#define DIAL_GL_COLOR_ATTACHMENT0 0x8CE0

/* ---- 函数指针类型（全部 dlsym）---- */
typedef EGLDisplay (*PFN_getplat)(int, void*, const int*);
typedef EGLBoolean (*PFN_init)(EGLDisplay, EGLint*, EGLint*);
typedef EGLBoolean (*PFN_choose)(EGLDisplay, const EGLint*, EGLConfig*, EGLint, EGLint*);
typedef EGLSurface (*PFN_pbuf)(EGLDisplay, EGLConfig, const EGLint*);
typedef EGLBoolean (*PFN_bindapi)(int);
typedef EGLContext (*PFN_ctx)(EGLDisplay, EGLConfig, EGLContext, const EGLint*);
typedef EGLBoolean (*PFN_make)(EGLDisplay, EGLSurface, EGLSurface, EGLContext);

typedef void   (*PFN_glGenTextures)(int, unsigned int*);
typedef void   (*PFN_glBindTexture)(int, unsigned int);
typedef void   (*PFN_glTexImage2D)(int,int,int,int,int,int,int,int,const void*);
typedef void   (*PFN_glTexParameteri)(int,int,int);
typedef unsigned int (*PFN_glCreateShader)(int);
typedef void   (*PFN_glShaderSource)(unsigned int,int,const char* const*,const int*);
typedef void   (*PFN_glCompileShader)(unsigned int);
typedef void   (*PFN_glGetShaderiv)(unsigned int,int,int*);
typedef unsigned int (*PFN_glCreateProgram)(void);
typedef void   (*PFN_glAttachShader)(unsigned int,unsigned int);
typedef void   (*PFN_glLinkProgram)(unsigned int);
typedef void   (*PFN_glGetProgramiv)(unsigned int,int,int*);
typedef void   (*PFN_glUseProgram)(unsigned int);
typedef int    (*PFN_glGetAttribLocation)(unsigned int,const char*);
typedef int    (*PFN_glGetUniformLocation)(unsigned int,const char*);
typedef void   (*PFN_glGenBuffers)(int,unsigned int*);
typedef void   (*PFN_glBindBuffer)(int,unsigned int);
typedef void   (*PFN_glBufferData)(int,long,const void*,int);
typedef void   (*PFN_glEnableVertexAttribArray)(unsigned int);
typedef void   (*PFN_glVertexAttribPointer)(unsigned int,int,int,int,int,void*);
typedef void   (*PFN_glUniform1i)(int,int);
typedef void   (*PFN_glDrawArrays)(int,int,int);
typedef void   (*PFN_glReadPixels)(int,int,int,int,int,int,void*);
typedef void   (*PFN_glViewport)(int,int,int,int);
typedef void   (*PFN_glFinish)(void);
typedef void   (*PFN_glGenFramebuffers)(int,unsigned int*);
typedef void   (*PFN_glBindFramebuffer)(int,unsigned int);
typedef void   (*PFN_glFramebufferTexture2D)(int,int,int,unsigned int,int);

#define DIAL_GL_FN(name) static PFN_gl##name fn_gl##name
DIAL_GL_FN(GenTextures);      DIAL_GL_FN(BindTexture);
DIAL_GL_FN(TexImage2D);       DIAL_GL_FN(TexParameteri);
DIAL_GL_FN(CreateShader);     DIAL_GL_FN(ShaderSource);
DIAL_GL_FN(CompileShader);    DIAL_GL_FN(GetShaderiv);
DIAL_GL_FN(CreateProgram);    DIAL_GL_FN(AttachShader);
DIAL_GL_FN(LinkProgram);      DIAL_GL_FN(GetProgramiv);
DIAL_GL_FN(UseProgram);       DIAL_GL_FN(GetAttribLocation);
DIAL_GL_FN(GetUniformLocation); DIAL_GL_FN(GenBuffers);
DIAL_GL_FN(BindBuffer);       DIAL_GL_FN(BufferData);
DIAL_GL_FN(EnableVertexAttribArray); DIAL_GL_FN(VertexAttribPointer);
DIAL_GL_FN(Uniform1i);        DIAL_GL_FN(DrawArrays);
DIAL_GL_FN(ReadPixels);       DIAL_GL_FN(Viewport);
DIAL_GL_FN(Finish);           DIAL_GL_FN(GenFramebuffers);
DIAL_GL_FN(BindFramebuffer);  DIAL_GL_FN(FramebufferTexture2D);
#undef DIAL_GL_FN

static PFN_getplat     pfn_getplat;
static PFN_init        pfn_init;
static PFN_choose      pfn_choose;
static PFN_pbuf        pfn_pbuf;
static PFN_bindapi     pfn_bindapi;
static PFN_ctx         pfn_ctx;
static PFN_make        pfn_make;

static int         s_real_state = 0;   /* 0=未探测 1=可用 -1=降级 */
static int         s_last_exact = 0;   /* 上次 composite 是否逐字节精确 */
static const char *s_real_backend = "none";
static const char *s_real_reason  =
    "EGL surfaceless backend not initialized (no libEGL/libGLESv2 or forced degrade)";
static EGLDisplay  s_dpy;
static EGLConfig   s_cfg;
static EGLSurface  s_surf;
static EGLContext  s_ctx;
static unsigned int s_prog, s_vbo, s_tex_in, s_tex_out, s_fbo;
static int         s_a_pos, s_u_tex;

#define DIAL_LOAD(h, sym, ty, field) do {                                  \
    *(void**)(&(field)) = dlsym((h), #sym);                                 \
    if (!(field)) { s_real_reason = "missing EGL/GLES symbol: " #sym;       \
                    goto real_init_fail; }                                  \
} while (0)

/* 懒初始化：只做一次。返回 1=真实后端可用，0=降级。 */
static int real_gpu_init(void) {
    if (s_real_state != 0) return s_real_state == 1;
    s_real_state = -1; /* 先置降级，全部成功后再翻 1 */

    /* 测试钩子：强制降级路径（无需真缺库即可验证软降级） */
    if (getenv("DIAL_GUI_GPU_FORCE_DEGRADE")) {
        s_real_reason = "forced degrade via DIAL_GUI_GPU_FORCE_DEGRADE";
        return 0;
    }

    void *egl  = dlopen("libEGL.so.1",   RTLD_LAZY | RTLD_LOCAL);
    void *gles = dlopen("libGLESv2.so.2", RTLD_LAZY | RTLD_LOCAL);
    if (!egl || !gles) {
        s_real_reason = "dlopen libEGL/libGLESv2 failed (no software GPU)";
        goto real_init_fail;
    }

    DIAL_LOAD(egl, eglGetPlatformDisplay, PFN_getplat, pfn_getplat);
    DIAL_LOAD(egl, eglInitialize,         PFN_init,    pfn_init);
    DIAL_LOAD(egl, eglChooseConfig,       PFN_choose,  pfn_choose);
    DIAL_LOAD(egl, eglCreatePbufferSurface, PFN_pbuf,  pfn_pbuf);
    DIAL_LOAD(egl, eglBindAPI,            PFN_bindapi, pfn_bindapi);
    DIAL_LOAD(egl, eglCreateContext,      PFN_ctx,     pfn_ctx);
    DIAL_LOAD(egl, eglMakeCurrent,        PFN_make,    pfn_make);

    s_dpy = pfn_getplat(DIAL_EGL_PLATFORM_SURFACELESS_MESA,
                        DIAL_EGL_DEFAULT_DISPLAY, (const int*)0);
    if (!s_dpy || s_dpy == DIAL_EGL_NO_DISPLAY) {
        s_real_reason = "EGL surfaceless display unavailable";
        goto real_init_fail;
    }
    {
        EGLint maj, min;
        if (!pfn_init(s_dpy, &maj, &min)) {
            s_real_reason = "eglInitialize failed";
            goto real_init_fail;
        }
    }
    {
        EGLint attr[] = { DIAL_EGL_SURFACE_TYPE, DIAL_EGL_PBUFFER_BIT,
                          DIAL_EGL_RENDERABLE_TYPE, DIAL_EGL_OPENGL_ES2_BIT,
                          DIAL_EGL_NONE };
        EGLint n = 0;
        if (!pfn_choose(s_dpy, attr, &s_cfg, 1, &n) || n < 1) {
            s_real_reason = "eglChooseConfig failed (no GLES2 config)";
            goto real_init_fail;
        }
    }
    {
        EGLint attr[] = { DIAL_EGL_WIDTH, 4, DIAL_EGL_HEIGHT, 4, DIAL_EGL_NONE };
        s_surf = pfn_pbuf(s_dpy, s_cfg, attr);
    }
    pfn_bindapi(DIAL_EGL_OPENGL_ES_API);
    {
        EGLint attr[] = { DIAL_EGL_CONTEXT_CLIENT_VERSION, 2, DIAL_EGL_NONE };
        s_ctx = pfn_ctx(s_dpy, s_cfg, DIAL_EGL_NO_CONTEXT, attr);
    }
    if (!s_ctx || !pfn_make(s_dpy, s_surf, s_surf, s_ctx)) {
        s_real_reason = "EGL GLES2 context / makeCurrent failed";
        goto real_init_fail;
    }

    /* GLES 函数指针 */
#define DIAL_LOAD_GL(sym) DIAL_LOAD(gles, sym, void, fn_##sym)
    DIAL_LOAD_GL(glGenTextures);       DIAL_LOAD_GL(glBindTexture);
    DIAL_LOAD_GL(glTexImage2D);        DIAL_LOAD_GL(glTexParameteri);
    DIAL_LOAD_GL(glCreateShader);      DIAL_LOAD_GL(glShaderSource);
    DIAL_LOAD_GL(glCompileShader);     DIAL_LOAD_GL(glGetShaderiv);
    DIAL_LOAD_GL(glCreateProgram);     DIAL_LOAD_GL(glAttachShader);
    DIAL_LOAD_GL(glLinkProgram);       DIAL_LOAD_GL(glGetProgramiv);
    DIAL_LOAD_GL(glUseProgram);        DIAL_LOAD_GL(glGetAttribLocation);
    DIAL_LOAD_GL(glGetUniformLocation);DIAL_LOAD_GL(glGenBuffers);
    DIAL_LOAD_GL(glBindBuffer);        DIAL_LOAD_GL(glBufferData);
    DIAL_LOAD_GL(glEnableVertexAttribArray); DIAL_LOAD_GL(glVertexAttribPointer);
    DIAL_LOAD_GL(glUniform1i);         DIAL_LOAD_GL(glDrawArrays);
    DIAL_LOAD_GL(glReadPixels);        DIAL_LOAD_GL(glViewport);
    DIAL_LOAD_GL(glFinish);            DIAL_LOAD_GL(glGenFramebuffers);
    DIAL_LOAD_GL(glBindFramebuffer);   DIAL_LOAD_GL(glFramebufferTexture2D);
#undef DIAL_LOAD_GL

    /* 恒等 shader：采样即输出，不改任何像素通道（宪法「只上传不改像素」）。
     * vertex 的 v=p*0.5+0.5；readPixels 原点左下，上传同序 ⇒ 逐字节相等。 */
    {
        const char *vs = "attribute vec2 p;varying vec2 v;"
                         "void main(){v=p*0.5+0.5;gl_Position=vec4(p,0.0,1.0);}";
        const char *fs = "precision mediump float;varying vec2 v;uniform sampler2D t;"
                         "void main(){gl_FragColor=texture2D(t,v);}";
        int ok = 0;
        unsigned int vsh = fn_glCreateShader(DIAL_GL_VERTEX_SHADER);
        fn_glShaderSource(vsh, 1, &vs, (const int*)0); fn_glCompileShader(vsh);
        unsigned int fsh = fn_glCreateShader(DIAL_GL_FRAGMENT_SHADER);
        fn_glShaderSource(fsh, 1, &fs, (const int*)0); fn_glCompileShader(fsh);
        fn_glGetShaderiv(fsh, DIAL_GL_COMPILE_STATUS, &ok);
        if (!ok) { s_real_reason = "GLES fragment shader compile failed"; goto real_init_fail; }
        s_prog = fn_glCreateProgram();
        fn_glAttachShader(s_prog, vsh); fn_glAttachShader(s_prog, fsh);
        fn_glLinkProgram(s_prog); fn_glGetProgramiv(s_prog, DIAL_GL_LINK_STATUS, &ok);
        if (!ok) { s_real_reason = "GLES program link failed"; goto real_init_fail; }
        fn_glUseProgram(s_prog);
        s_a_pos = fn_glGetAttribLocation(s_prog, "p");
        s_u_tex = fn_glGetUniformLocation(s_prog, "t");
        fn_glUniform1i(s_u_tex, 0);
    }
    {
        const float quad[8] = { -1,-1, 1,-1, -1,1, 1,1 };
        fn_glGenBuffers(1, &s_vbo);
        fn_glBindBuffer(DIAL_GL_ARRAY_BUFFER, s_vbo);
        fn_glBufferData(DIAL_GL_ARRAY_BUFFER, (long)sizeof(quad), quad, DIAL_GL_STATIC_DRAW);
    }
    fn_glGenTextures(1, &s_tex_in);
    fn_glGenTextures(1, &s_tex_out);
    fn_glGenFramebuffers(1, &s_fbo);

    s_real_backend = "egl-surfaceless-sw";
    s_real_reason  = "ok";
    s_real_state = 1;
    return 1;

real_init_fail:
    s_real_backend = "none";
    s_real_state = -1;
    return 0;
}

int64_t dial_gui_gpu_real_available(void) {
    gui_core_ok();
    return real_gpu_init() ? 1 : 0;
}
const char *dial_gui_gpu_real_backend(void) {
    gui_core_ok();
    (void)real_gpu_init();
    return s_real_backend;
}
int64_t dial_gui_gpu_real_caps(void) {
    gui_core_ok();
    if (!real_gpu_init()) return 0;
    /* bit0=纹理上传 bit1=FBO读回 bit2=恒等shader（不改像素） */
    return 1 | 2 | 4;
}
const char *dial_gui_gpu_real_degrade_reason(void) {
    gui_core_ok();
    (void)real_gpu_init();
    return s_real_reason;
}
int64_t dial_gui_gpu_real_last_exact(void) {
    gui_core_ok();
    return s_last_exact ? 1 : 0;
}
int64_t dial_gui_gpu_real_composite(GuiPainterImpl *p) {
    if (!p || !p->pixels) {
        gui_core_set_err(GUI_ERR_ARG, "真实GPU合成：空 / 已释放的 GuiPainter 句柄");
        return -1;
    }
    if (p->w <= 0 || p->h <= 0) {
        gui_core_set_err(GUI_ERR_ARG, "真实GPU合成：画布尺寸非法");
        return -1;
    }
    s_last_exact = 0;
    if (!real_gpu_init()) {
        gui_core_ok();
        return GUI_PLATFORM_DEGRADE;  /* 软降级，不写错误槽 */
    }

    size_t bytes = (size_t)p->w * (size_t)p->h * 4u;
    /* 1) 上传源 framebuffer 为纹理（只读 p->pixels，绝不写） */
    fn_glBindTexture(DIAL_GL_TEXTURE_2D, s_tex_in);
    fn_glTexParameteri(DIAL_GL_TEXTURE_2D, DIAL_GL_TEXTURE_MIN_FILTER, DIAL_GL_NEAREST);
    fn_glTexParameteri(DIAL_GL_TEXTURE_2D, DIAL_GL_TEXTURE_MAG_FILTER, DIAL_GL_NEAREST);
    fn_glTexParameteri(DIAL_GL_TEXTURE_2D, DIAL_GL_TEXTURE_WRAP_S, DIAL_GL_CLAMP_TO_EDGE);
    fn_glTexParameteri(DIAL_GL_TEXTURE_2D, DIAL_GL_TEXTURE_WRAP_T, DIAL_GL_CLAMP_TO_EDGE);
    fn_glTexImage2D(DIAL_GL_TEXTURE_2D, 0, DIAL_GL_RGBA, p->w, p->h, 0,
                    DIAL_GL_RGBA, DIAL_GL_UNSIGNED_BYTE, (const void*)p->pixels);

    /* 2) 离屏 FBO：输出纹理同尺寸（NPOT 安全：CLAMP_TO_EDGE + NEAREST） */
    fn_glBindFramebuffer(DIAL_GL_FRAMEBUFFER, s_fbo);
    fn_glBindTexture(DIAL_GL_TEXTURE_2D, s_tex_out);
    fn_glTexParameteri(DIAL_GL_TEXTURE_2D, DIAL_GL_TEXTURE_MIN_FILTER, DIAL_GL_NEAREST);
    fn_glTexParameteri(DIAL_GL_TEXTURE_2D, DIAL_GL_TEXTURE_MAG_FILTER, DIAL_GL_NEAREST);
    fn_glTexParameteri(DIAL_GL_TEXTURE_2D, DIAL_GL_TEXTURE_WRAP_S, DIAL_GL_CLAMP_TO_EDGE);
    fn_glTexParameteri(DIAL_GL_TEXTURE_2D, DIAL_GL_TEXTURE_WRAP_T, DIAL_GL_CLAMP_TO_EDGE);
    fn_glTexImage2D(DIAL_GL_TEXTURE_2D, 0, DIAL_GL_RGBA, p->w, p->h, 0,
                    DIAL_GL_RGBA, DIAL_GL_UNSIGNED_BYTE, (const void*)0);
    fn_glFramebufferTexture2D(DIAL_GL_FRAMEBUFFER, DIAL_GL_COLOR_ATTACHMENT0,
                              DIAL_GL_TEXTURE_2D, s_tex_out, 0);

    /* 3) 重绑输入纹理供采样，恒等 shader 离屏绘制 */
    fn_glBindTexture(DIAL_GL_TEXTURE_2D, s_tex_in);
    fn_glUseProgram(s_prog);
    fn_glUniform1i(s_u_tex, 0);
    fn_glViewport(0, 0, p->w, p->h);
    fn_glBindBuffer(DIAL_GL_ARRAY_BUFFER, s_vbo);
    fn_glEnableVertexAttribArray((unsigned int)s_a_pos);
    fn_glVertexAttribPointer((unsigned int)s_a_pos, 2, DIAL_GL_FLOAT, 0, 0, (void*)0);
    fn_glDrawArrays(DIAL_GL_TRIANGLE_STRIP, 0, 4);
    fn_glFinish();

    /* 4) 读回并逐字节校验；不写 p->pixels 一个字节 */
    {
        uint32_t *out = (uint32_t*)malloc(bytes);
        if (!out) { fn_glBindFramebuffer(DIAL_GL_FRAMEBUFFER, 0);
                    gui_core_ok(); return GUI_PLATFORM_DEGRADE; }
        fn_glReadPixels(0, 0, p->w, p->h, DIAL_GL_RGBA, DIAL_GL_UNSIGNED_BYTE, out);
        s_last_exact = (memcmp(out, p->pixels, bytes) == 0) ? 1 : 0;
        free(out);
    }
    fn_glBindFramebuffer(DIAL_GL_FRAMEBUFFER, 0);
    gui_core_ok();
    /* 逐字节精确才算真实合成成功；否则视为能力不匹配，确定性降级。 */
    return s_last_exact ? 0 : GUI_PLATFORM_DEGRADE;
}

#else /* !__linux__ —— 其余平台无法实机验证，恒降级，不假装验证过 */

int64_t dial_gui_gpu_real_available(void) { gui_core_ok(); return 0; }
const char *dial_gui_gpu_real_backend(void) { gui_core_ok(); return "none"; }
int64_t dial_gui_gpu_real_caps(void) { gui_core_ok(); return 0; }
const char *dial_gui_gpu_real_degrade_reason(void) {
    gui_core_ok();
    return "real GPU composite not available on this platform (not verified)";
}
int64_t dial_gui_gpu_real_last_exact(void) { gui_core_ok(); return 0; }
int64_t dial_gui_gpu_real_composite(GuiPainterImpl *p) {
    if (!p || !p->pixels || p->w <= 0 || p->h <= 0) {
        gui_core_set_err(GUI_ERR_ARG, "真实GPU合成：非法 GuiPainter 句柄");
        return -1;
    }
    gui_core_ok();
    return GUI_PLATFORM_DEGRADE;
}

#endif /* __linux__ */
