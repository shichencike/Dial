/*
 * dial_gui_theme.c —— gui.dialpkg 主题层（纯 C，三平台共用）
 *
 * 职责：把「内置浅色默认主题」从编译期写死的常量升级为**运行时可覆写的
 * 颜色表**，让 Dial 侧能按角色改配色，而不必改动任何一个既有 extern fn
 * 的签名（gui_widget_draw(root, p) 一字不动，abi = 6 不变）。
 *
 * 硬边界（与前四层完全一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；
 *     三份 .a 的 `nm -u` 不会因本层新增任何平台符号。
 *   - 绝不 C → Dial 反向回调：不保存函数指针、不认识 Dial 对象、不碰 ARC。
 *     颜色表是进程级静态数据（GUI 单线程事件循环，无需加锁），与 look.c
 *     里 g_builtin_font 静态零分配对象同口径。
 *   - 每个 demo 都是独立进程，静态表天然从默认值起步，因此 theme 改动
 *     绝不会污染其它用例的 CRC 基准；theme_reset() 可随时逐值复位。
 *
 * Fail-Fast（§20）：role 越界 / 颜色值越界都明确返回 -1 并写错误槽，
 * 绝不静默截断，绝不把非法值悄悄当成某个合法角色。
 *
 * 关键防漂移：g_default[] 逐字取自 dial_gui.h 的 GUI_THEME_* 宏，默认配色
 * 与引入 setter 之前完全一致，因此 look_crc_normal = 3884455222 不漂移。
 */
#include "dial_gui.h"

/* 默认值唯一真相源：dial_gui.h 的 GUI_THEME_* 宏（不在本文件复写字面量）。*/
static const gui_color_t g_default[GUI_THEME_ROLE_COUNT] = {
    /*  0 BG         */ GUI_THEME_BG,
    /*  1 BORDER     */ GUI_THEME_BORDER,
    /*  2 TEXT       */ GUI_THEME_TEXT,
    /*  3 TEXT_DIS   */ GUI_THEME_TEXT_DIS,
    /*  4 BTN        */ GUI_THEME_BTN,
    /*  5 BTN_HOVER  */ GUI_THEME_BTN_HOVER,
    /*  6 BTN_DOWN   */ GUI_THEME_BTN_DOWN,
    /*  7 BTN_DIS    */ GUI_THEME_BTN_DIS,
    /*  8 CHECK_BG   */ GUI_THEME_CHECK_BG,
    /*  9 MARK       */ GUI_THEME_MARK,
    /* 10 SEL        */ GUI_THEME_SEL,
    /* 11 TRACK      */ GUI_THEME_TRACK,
    /* 12 FILL       */ GUI_THEME_FILL,
    /* 13 KNOB       */ GUI_THEME_KNOB,
};

static gui_color_t g_theme[GUI_THEME_ROLE_COUNT];
static int         g_inited = 0;

/* 静态零初始化 → 首次访问时铺默认值；单线程 GUI 事件循环，无需加锁。*/
static void ensure_init(void) {
    if (g_inited) return;
    for (int i = 0; i < GUI_THEME_ROLE_COUNT; i++) g_theme[i] = g_default[i];
    g_inited = 1;
}

/* 绘制内部用：role 由调用方用 GUI_THEME_ROLE_* 常量传入；越界只做防御性
 * 兜底（返回 BG 默认值），绝不返回未初始化内存。 */
gui_color_t gui_theme_color(int role) {
    ensure_init();
    if (role < 0 || role >= GUI_THEME_ROLE_COUNT) return g_default[0];
    return g_theme[role];
}

int gui_theme_set(int role, gui_color_t c) {
    if (role < 0 || role >= GUI_THEME_ROLE_COUNT) return -1;
    ensure_init();
    g_theme[role] = c;
    return 0;
}

/* 返回 0..0xFFFFFFFF 的色值；非法 role 返回 -1（绝不静默当成 0）。*/
int64_t gui_theme_get(int role) {
    if (role < 0 || role >= GUI_THEME_ROLE_COUNT) return -1;
    ensure_init();
    return (int64_t)g_theme[role];
}

void gui_theme_reset(void) {
    for (int i = 0; i < GUI_THEME_ROLE_COUNT; i++) g_theme[i] = g_default[i];
    g_inited = 1;
}

int gui_theme_role_count(void) { return GUI_THEME_ROLE_COUNT; }
