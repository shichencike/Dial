/*
 * winapi_impl.c —— Windows API 薄封装（kernel32 / user32 / gdi32 代表函数）的唯一 C 源文件。
 *
 * 设计立场（与 packages/winapi/winapi.diali 头注释一致）：
 *   - 薄封装：Win32 函数名 / 参数名一个都不改；本文件只做「装载 + 调用 + 错误搬运」。
 *   - 不 include windows.h（否则 CreateWindowExW 等宏名会与我们自己的 C 函数冲突）；
 *     句柄 typedef 与函数指针类型全部自己声明。
 *   - kernel32 的装载器（LoadLibraryA / GetProcAddress）直接链接（Windows 上 kernel32
 *     总是已加载）；user32 / gdi32 的函数全部运行时 GetProcAddress 解析，零链接期符号依赖。
 *   - W 系列（MessageBoxW / CreateWindowExW / ...）收 UTF-16：本文件把 Dial 的 UTF-8
 *     string 转成 UTF-16LE 再调用；A 系列（LoadLibraryA）直接传 UTF-8 字节。
 *   - 非 Windows 平台（Linux / musl）：编译进桩实现，每个函数把错误槽置为
 *     WINAPI_ERR_PLATFORM 并返回 -1（句柄类返回 0），明确告知「本平台不支持」。
 *
 * 与 gui 包同口径：平台库一律运行时动态装载，.a 自包含，Dial 侧不需要写 link、
 * 链接期也不需要 -luser32 / -lgdi32；缺库返回明确错误码而非让可执行文件加载失败。
 */

#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include "dial_string.h"

/* ---- 错误码 -------------------------------------------------------------- */
#define WINAPI_OK          0
#define WINAPI_ERR_PLATFORM 1   /* 非 Windows 平台（桩返回） */
#define WINAPI_ERR_ARG     2    /* 空句柄 / 非法参数 */
#define WINAPI_ERR_LOAD    3    /* DLL / 函数装载失败 */

static int64_t g_err = 0;
static char    g_errmsg[192];

int64_t        winapi_last_error(void)      { return g_err; }
const char    *winapi_last_error_msg(void)  { return g_errmsg; }

/* 不透明句柄类型（跨平台一致，两个分支都用） */
typedef void *HMODULE;
typedef void *HWND;
typedef void *HINSTANCE;
typedef void *HDC;
typedef void *HBRUSH;
typedef void *HMENU;
typedef void *HGDIOBJ;

/* 空句柄构造器（Dial 侧 FFI 粘合层，非 Win32 函数）：返回 (void*)0。 */
HMENU     winapi_null_hmenu(void)     { return (HMENU)0; }
HINSTANCE winapi_null_hinstance(void) { return (HINSTANCE)0; }
HGDIOBJ   winapi_null_hgdiobj(void)   { return (HGDIOBJ)0; }

/* ======================================================================== */
#ifdef _WIN32
/* ---- Windows 真实现 ----------------------------------------------------- */

/* kernel32：真实导入通过 __asm__ 重命名为 _real_*，避免与下方同名 Dial 包装函数
 * （导出符号）冲突——包装函数体外壳就是 Dial 链接的名字，真实函数必须换个符号调用。
 * 这两个装载器（LoadLibraryA / GetProcAddress）直接链接 kernel32 导入库；其余
 * kernel32 函数也直接链接（总是可用），user32 / gdi32 才运行时 GetProcAddress 解析。 */
extern HMODULE      __stdcall _real_LoadLibraryA(const char *)     __asm__("LoadLibraryA");
extern int          __stdcall _real_FreeLibrary(HMODULE)           __asm__("FreeLibrary");
extern void        *__stdcall _real_GetProcAddress(HMODULE, const char *) __asm__("GetProcAddress");
extern HMODULE      __stdcall _real_GetModuleHandleW(const wchar_t *) __asm__("GetModuleHandleW");
extern unsigned int  __stdcall _real_GetLastError(void)            __asm__("GetLastError");
extern void         __stdcall _real_SetLastError(unsigned int)     __asm__("SetLastError");
extern void         __stdcall _real_Sleep(unsigned int)            __asm__("Sleep");
extern void         __stdcall _real_ExitProcess(unsigned int)      __asm__("ExitProcess");
extern unsigned int  __stdcall _real_GetCurrentProcessId(void)     __asm__("GetCurrentProcessId");
extern unsigned int  __stdcall _real_GetCurrentThreadId(void)      __asm__("GetCurrentThreadId");
extern unsigned int  __stdcall _real_GetTickCount(void)            __asm__("GetTickCount");

/* user32 / gdi32：运行时解析的函数指针 typedef */
typedef HWND   (__stdcall *PFN_CreateWindowExW)(unsigned int, const wchar_t *, const wchar_t *, unsigned int, int, int, int, int, HWND, HMENU, HINSTANCE, void *);
typedef int    (__stdcall *PFN_MessageBoxW)(HWND, const wchar_t *, const wchar_t *, unsigned int);
typedef int    (__stdcall *PFN_DestroyWindow)(HWND);
typedef int    (__stdcall *PFN_ShowWindow)(HWND, int);
typedef int    (__stdcall *PFN_UpdateWindow)(HWND);
typedef void   (__stdcall *PFN_PostQuitMessage)(int);
typedef int64_t (__stdcall *PFN_DefWindowProcW)(HWND, int64_t, int64_t, int64_t);
typedef int64_t (__stdcall *PFN_SendMessageW)(HWND, int64_t, int64_t, int64_t);
typedef int    (__stdcall *PFN_PostMessageW)(HWND, int64_t, int64_t, int64_t);
typedef HWND   (__stdcall *PFN_FindWindowW)(const wchar_t *, const wchar_t *);
typedef int    (__stdcall *PFN_SetWindowTextW)(HWND, const wchar_t *);
typedef int    (__stdcall *PFN_MoveWindow)(HWND, int, int, int, int, int);
typedef int    (__stdcall *PFN_SetForegroundWindow)(HWND);
typedef HDC    (__stdcall *PFN_GetDC)(HWND);
typedef int    (__stdcall *PFN_ReleaseDC)(HWND, HDC);
typedef int    (__stdcall *PFN_TextOutW)(HDC, int, int, const wchar_t *, int);
typedef int    (__stdcall *PFN_Rectangle)(HDC, int, int, int, int);
typedef int    (__stdcall *PFN_Ellipse)(HDC, int, int, int, int);
typedef HBRUSH (__stdcall *PFN_CreateSolidBrush)(int);
typedef int    (__stdcall *PFN_DeleteObject)(HGDIOBJ);
typedef int    (__stdcall *PFN_MoveToEx)(HDC, int, int, void *);
typedef int    (__stdcall *PFN_LineTo)(HDC, int, int);

static void   *g_user32 = NULL;
static void   *g_gdi32  = NULL;
static int     g_loaded = 0;
static int     g_load_ok = 0;
static PFN_MessageBoxW        pMessageBoxW = NULL;
static PFN_CreateWindowExW     pCreateWindowExW = NULL;
static PFN_DestroyWindow       pDestroyWindow = NULL;
static PFN_ShowWindow          pShowWindow = NULL;
static PFN_UpdateWindow        pUpdateWindow = NULL;
static PFN_PostQuitMessage      pPostQuitMessage = NULL;
static PFN_DefWindowProcW       pDefWindowProcW = NULL;
static PFN_SendMessageW         pSendMessageW = NULL;
static PFN_PostMessageW         pPostMessageW = NULL;
static PFN_FindWindowW          pFindWindowW = NULL;
static PFN_SetWindowTextW       pSetWindowTextW = NULL;
static PFN_MoveWindow           pMoveWindow = NULL;
static PFN_SetForegroundWindow   pSetForegroundWindow = NULL;
static PFN_GetDC                pGetDC = NULL;
static PFN_ReleaseDC            pReleaseDC = NULL;
static PFN_TextOutW             pTextOutW = NULL;
static PFN_Rectangle            pRectangle = NULL;
static PFN_Ellipse              pEllipse = NULL;
static PFN_CreateSolidBrush     pCreateSolidBrush = NULL;
static PFN_DeleteObject         pDeleteObject = NULL;
static PFN_MoveToEx             pMoveToEx = NULL;
static PFN_LineTo               pLineTo = NULL;

/* 懒装载 user32 / gdi32 并解析全部函数指针（首次调用时执行一次） */
static int ensure_loaded(void) {
    if (g_loaded) return g_load_ok;
    g_loaded = 1;
    g_user32 = _real_LoadLibraryA("user32.dll");
    g_gdi32  = _real_LoadLibraryA("gdi32.dll");
    if (!g_user32 || !g_gdi32) {
        g_err = WINAPI_ERR_LOAD;
        snprintf(g_errmsg, sizeof g_errmsg, "user32.dll 或 gdi32.dll 不可用");
        g_load_ok = 0;
        return 0;
    }
    pMessageBoxW       = (PFN_MessageBoxW)_real_GetProcAddress(g_user32, "MessageBoxW");
    pCreateWindowExW   = (PFN_CreateWindowExW)_real_GetProcAddress(g_user32, "CreateWindowExW");
    pDestroyWindow     = (PFN_DestroyWindow)_real_GetProcAddress(g_user32, "DestroyWindow");
    pShowWindow        = (PFN_ShowWindow)_real_GetProcAddress(g_user32, "ShowWindow");
    pUpdateWindow      = (PFN_UpdateWindow)_real_GetProcAddress(g_user32, "UpdateWindow");
    pPostQuitMessage   = (PFN_PostQuitMessage)_real_GetProcAddress(g_user32, "PostQuitMessage");
    pDefWindowProcW    = (PFN_DefWindowProcW)_real_GetProcAddress(g_user32, "DefWindowProcW");
    pSendMessageW      = (PFN_SendMessageW)_real_GetProcAddress(g_user32, "SendMessageW");
    pPostMessageW      = (PFN_PostMessageW)_real_GetProcAddress(g_user32, "PostMessageW");
    pFindWindowW       = (PFN_FindWindowW)_real_GetProcAddress(g_user32, "FindWindowW");
    pSetWindowTextW    = (PFN_SetWindowTextW)_real_GetProcAddress(g_user32, "SetWindowTextW");
    pMoveWindow        = (PFN_MoveWindow)_real_GetProcAddress(g_user32, "MoveWindow");
    pSetForegroundWindow = (PFN_SetForegroundWindow)_real_GetProcAddress(g_user32, "SetForegroundWindow");
    pGetDC             = (PFN_GetDC)_real_GetProcAddress(g_gdi32, "GetDC");
    pReleaseDC         = (PFN_ReleaseDC)_real_GetProcAddress(g_gdi32, "ReleaseDC");
    pTextOutW          = (PFN_TextOutW)_real_GetProcAddress(g_gdi32, "TextOutW");
    pRectangle         = (PFN_Rectangle)_real_GetProcAddress(g_gdi32, "Rectangle");
    pEllipse           = (PFN_Ellipse)_real_GetProcAddress(g_gdi32, "Ellipse");
    pCreateSolidBrush  = (PFN_CreateSolidBrush)_real_GetProcAddress(g_gdi32, "CreateSolidBrush");
    pDeleteObject      = (PFN_DeleteObject)_real_GetProcAddress(g_gdi32, "DeleteObject");
    pMoveToEx          = (PFN_MoveToEx)_real_GetProcAddress(g_gdi32, "MoveToEx");
    pLineTo            = (PFN_LineTo)_real_GetProcAddress(g_gdi32, "LineTo");
    g_load_ok = 1;
    return 1;
}

/* UTF-8 -> UTF-16LE（手写，不依赖任何平台 API）。返回 malloc 的以 NUL 结尾缓冲区。 */
static wchar_t *utf8_to_utf16(const char *s, int64_t n) {
    if (n <= 0) { wchar_t *e = (wchar_t *)malloc(2); e[0] = 0; return e; }
    wchar_t *out = (wchar_t *)malloc((size_t)(n + 1) * 2);
    if (!out) return NULL;
    int64_t wi = 0, i = 0;
    while (i < n) {
        unsigned char c = (unsigned char)s[i];
        int64_t cp, len;
        if (c < 0x80)            { cp = c;                 len = 1; }
        else if ((c & 0xE0) == 0xC0) { cp = c & 0x1F;      len = 2; }
        else if ((c & 0xF0) == 0xE0) { cp = c & 0x0F;      len = 3; }
        else if ((c & 0xF8) == 0xF0) { cp = c & 0x07;      len = 4; }
        else                     { cp = 0xFFFD;           len = 1; }
        int k;
        for (k = 1; k < len && i + k < n; k++) {
            if (((unsigned char)s[i + k] & 0xC0) != 0x80) { cp = 0xFFFD; len = k; break; }
            cp = (cp << 6) | ((unsigned char)s[i + k] & 0x3F);
        }
        i += len;
        if (cp < 0 || cp > 0x10FFFF) cp = 0xFFFD;
        if (cp < 0x10000) {
            out[wi++] = (wchar_t)cp;
        } else {
            cp -= 0x10000;
            out[wi++] = (wchar_t)(0xD800 + (cp >> 10));
            out[wi++] = (wchar_t)(0xDC00 + (cp & 0x3FF));
        }
    }
    out[wi] = 0;
    return out;
}

#else
/* ---- 非 Windows 平台：桩实现（编译进 posix 目标） ------------------------ */
#define WINAPI_STUB() do { g_err = WINAPI_ERR_PLATFORM; \
    snprintf(g_errmsg, sizeof g_errmsg, "winapi: 当前平台不支持 Windows API"); } while (0)
#endif

/* ======================================================================== */
/* 以下每个 extern fn 签名跨平台一致；函数体按 _WIN32 分支（真实 / 桩）。       */

/* ---- kernel32（11）------------------------------------------------------ */
HMODULE GetModuleHandleW(DialString lpModuleName) {
#ifdef _WIN32
    wchar_t *w = utf8_to_utf16(dial_string_cstr(lpModuleName), dial_string_length(lpModuleName));
    HMODULE r = _real_GetModuleHandleW(w);
    free(w); g_err = 0; return r;
#else
    WINAPI_STUB(); return (HMODULE)0;
#endif
}

HMODULE LoadLibraryA(DialString lpLibFileName) {
#ifdef _WIN32
    HMODULE r = _real_LoadLibraryA(dial_string_cstr(lpLibFileName));
    g_err = 0; return r;
#else
    WINAPI_STUB(); return (HMODULE)0;
#endif
}

int64_t FreeLibrary(HMODULE hLibModule) {
#ifdef _WIN32
    int r = _real_FreeLibrary(hLibModule); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

void *GetProcAddress(HMODULE hModule, DialString lpProcName) {
#ifdef _WIN32
    void *r = _real_GetProcAddress(hModule, dial_string_cstr(lpProcName));
    g_err = 0; return r;
#else
    WINAPI_STUB(); return (void *)0;
#endif
}

int64_t GetLastError(void) {
#ifdef _WIN32
    unsigned int r = _real_GetLastError(); g_err = 0; return (int64_t)r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t SetLastError(int dwErrCode) {
#ifdef _WIN32
    _real_SetLastError((unsigned int)dwErrCode); g_err = 0; return 0;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t Sleep(int dwMilliseconds) {
#ifdef _WIN32
    _real_Sleep((unsigned int)dwMilliseconds); g_err = 0; return 0;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t ExitProcess(int uExitCode) {
#ifdef _WIN32
    _real_ExitProcess((unsigned int)uExitCode); return 0;  /* 不返回 */
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t GetCurrentProcessId(void) {
#ifdef _WIN32
    unsigned int r = _real_GetCurrentProcessId(); g_err = 0; return (int64_t)r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t GetCurrentThreadId(void) {
#ifdef _WIN32
    unsigned int r = _real_GetCurrentThreadId(); g_err = 0; return (int64_t)r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t GetTickCount(void) {
#ifdef _WIN32
    unsigned int r = _real_GetTickCount(); g_err = 0; return (int64_t)r;
#else
    WINAPI_STUB(); return -1;
#endif
}

/* ---- user32（13）------------------------------------------------------- */
int64_t MessageBoxW(HWND hWnd, DialString text, DialString caption, int uType) {
#ifdef _WIN32
    if (!ensure_loaded() || !pMessageBoxW) { g_err = WINAPI_ERR_LOAD; return -1; }
    wchar_t *wt = utf8_to_utf16(dial_string_cstr(text), dial_string_length(text));
    wchar_t *wc = utf8_to_utf16(dial_string_cstr(caption), dial_string_length(caption));
    int r = pMessageBoxW((HWND)hWnd, wt, wc, (unsigned int)uType);
    free(wt); free(wc); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

HWND CreateWindowExW(int dwExStyle, DialString lpClassName, DialString lpWindowName,
                     int dwStyle, int x, int y, int nWidth, int nHeight,
                     HWND hWndParent, HMENU hMenu, HINSTANCE hInstance, void *lpParam) {
#ifdef _WIN32
    if (!ensure_loaded() || !pCreateWindowExW) { g_err = WINAPI_ERR_LOAD; return (HWND)0; }
    wchar_t *wc = utf8_to_utf16(dial_string_cstr(lpClassName), dial_string_length(lpClassName));
    wchar_t *wt = utf8_to_utf16(dial_string_cstr(lpWindowName), dial_string_length(lpWindowName));
    HWND r = pCreateWindowExW((unsigned int)dwExStyle, wc, wt, (unsigned int)dwStyle,
                              (int)x, (int)y, (int)nWidth, (int)nHeight,
                              (HWND)hWndParent, (HMENU)hMenu, (HINSTANCE)hInstance, lpParam);
    free(wc); free(wt); g_err = 0; return r;
#else
    WINAPI_STUB(); return (HWND)0;
#endif
}

int64_t DestroyWindow(HWND hWnd) {
#ifdef _WIN32
    if (!ensure_loaded() || !pDestroyWindow) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pDestroyWindow((HWND)hWnd); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t ShowWindow(HWND hWnd, int nCmdShow) {
#ifdef _WIN32
    if (!ensure_loaded() || !pShowWindow) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pShowWindow((HWND)hWnd, (int)nCmdShow); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t UpdateWindow(HWND hWnd) {
#ifdef _WIN32
    if (!ensure_loaded() || !pUpdateWindow) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pUpdateWindow((HWND)hWnd); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t PostQuitMessage(int nExitCode) {
#ifdef _WIN32
    if (!ensure_loaded() || !pPostQuitMessage) { g_err = WINAPI_ERR_LOAD; return -1; }
    pPostQuitMessage((int)nExitCode); g_err = 0; return 0;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t DefWindowProcW(HWND hWnd, int Msg, int wParam, int lParam) {
#ifdef _WIN32
    if (!ensure_loaded() || !pDefWindowProcW) { g_err = WINAPI_ERR_LOAD; return -1; }
    int64_t r = pDefWindowProcW((HWND)hWnd, (int64_t)Msg, (int64_t)wParam, (int64_t)lParam);
    g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t SendMessageW(HWND hWnd, int Msg, int wParam, int lParam) {
#ifdef _WIN32
    if (!ensure_loaded() || !pSendMessageW) { g_err = WINAPI_ERR_LOAD; return -1; }
    int64_t r = pSendMessageW((HWND)hWnd, (int64_t)Msg, (int64_t)wParam, (int64_t)lParam);
    g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t PostMessageW(HWND hWnd, int Msg, int wParam, int lParam) {
#ifdef _WIN32
    if (!ensure_loaded() || !pPostMessageW) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pPostMessageW((HWND)hWnd, (int64_t)Msg, (int64_t)wParam, (int64_t)lParam);
    g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

HWND FindWindowW(DialString lpClassName, DialString lpWindowName) {
#ifdef _WIN32
    if (!ensure_loaded() || !pFindWindowW) { g_err = WINAPI_ERR_LOAD; return (HWND)0; }
    wchar_t *wc = utf8_to_utf16(dial_string_cstr(lpClassName), dial_string_length(lpClassName));
    wchar_t *wt = utf8_to_utf16(dial_string_cstr(lpWindowName), dial_string_length(lpWindowName));
    HWND r = pFindWindowW(wc, wt);
    free(wc); free(wt); g_err = 0; return r;
#else
    WINAPI_STUB(); return (HWND)0;
#endif
}

int64_t SetWindowTextW(HWND hWnd, DialString lpString) {
#ifdef _WIN32
    if (!ensure_loaded() || !pSetWindowTextW) { g_err = WINAPI_ERR_LOAD; return -1; }
    wchar_t *ws = utf8_to_utf16(dial_string_cstr(lpString), dial_string_length(lpString));
    int r = pSetWindowTextW((HWND)hWnd, ws);
    free(ws); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t MoveWindow(HWND hWnd, int x, int y, int nWidth, int nHeight, int bRepaint) {
#ifdef _WIN32
    if (!ensure_loaded() || !pMoveWindow) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pMoveWindow((HWND)hWnd, (int)x, (int)y, (int)nWidth, (int)nHeight, (int)bRepaint);
    g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t SetForegroundWindow(HWND hWnd) {
#ifdef _WIN32
    if (!ensure_loaded() || !pSetForegroundWindow) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pSetForegroundWindow((HWND)hWnd); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

/* ---- gdi32（9）--------------------------------------------------------- */
HDC GetDC(HWND hWnd) {
#ifdef _WIN32
    if (!ensure_loaded() || !pGetDC) { g_err = WINAPI_ERR_LOAD; return (HDC)0; }
    HDC r = pGetDC((HWND)hWnd); g_err = 0; return r;
#else
    WINAPI_STUB(); return (HDC)0;
#endif
}

int64_t ReleaseDC(HWND hWnd, HDC hDC) {
#ifdef _WIN32
    if (!ensure_loaded() || !pReleaseDC) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pReleaseDC((HWND)hWnd, (HDC)hDC); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t TextOutW(HDC hDC, int x, int y, DialString lpString, int c) {
#ifdef _WIN32
    if (!ensure_loaded() || !pTextOutW) { g_err = WINAPI_ERR_LOAD; return -1; }
    wchar_t *ws = utf8_to_utf16(dial_string_cstr(lpString), dial_string_length(lpString));
    int r = pTextOutW((HDC)hDC, (int)x, (int)y, ws, (int)c);
    free(ws); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t Rectangle(HDC hDC, int left, int top, int right, int bottom) {
#ifdef _WIN32
    if (!ensure_loaded() || !pRectangle) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pRectangle((HDC)hDC, (int)left, (int)top, (int)right, (int)bottom);
    g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t Ellipse(HDC hDC, int left, int top, int right, int bottom) {
#ifdef _WIN32
    if (!ensure_loaded() || !pEllipse) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pEllipse((HDC)hDC, (int)left, (int)top, (int)right, (int)bottom);
    g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

HBRUSH CreateSolidBrush(int crColor) {
#ifdef _WIN32
    if (!ensure_loaded() || !pCreateSolidBrush) { g_err = WINAPI_ERR_LOAD; return (HBRUSH)0; }
    HBRUSH r = pCreateSolidBrush((int)crColor); g_err = 0; return r;
#else
    WINAPI_STUB(); return (HBRUSH)0;
#endif
}

int64_t DeleteObject(HGDIOBJ hObject) {
#ifdef _WIN32
    if (!ensure_loaded() || !pDeleteObject) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pDeleteObject((HGDIOBJ)hObject); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t MoveToEx(HDC hDC, int x, int y, void *lpPoint) {
#ifdef _WIN32
    if (!ensure_loaded() || !pMoveToEx) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pMoveToEx((HDC)hDC, (int)x, (int)y, lpPoint); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}

int64_t LineTo(HDC hDC, int x, int y) {
#ifdef _WIN32
    if (!ensure_loaded() || !pLineTo) { g_err = WINAPI_ERR_LOAD; return -1; }
    int r = pLineTo((HDC)hDC, (int)x, (int)y); g_err = 0; return r;
#else
    WINAPI_STUB(); return -1;
#endif
}
