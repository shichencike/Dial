/*
 * dial_gui_hotkey.c —— gui.dialpkg 第 15 层：快捷键 / 命令系统（纯 C，三平台共用）
 *
 * 职责：把「按键组合 (key, mods) → 命令 id」的映射与「命令启用态」收进一张
 *      纯内存表。窗口/菜单/上下文菜单拿到事件后只需 dispatch(key, mods)，
 *      命中且命令启用时返回命令 id，调用方自行决定执行什么——C 侧绝不
 *      保存函数指针、绝不反向调用 Dial，命令语义完全归应用。
 *
 * 硬边界（与第 9~14 层一致）：
 *   - 只 include dial_gui.h + libc，绝不碰 X11 / Win32 任何 API；三份 .a 的
 *     `nm -u` 不会因本层新增任何平台符号。
 *   - 纯值：不读时钟、不捕获鼠标、不查系统键盘布局；按键码由调用方从
 *     dial_gui_event_key / event_mods 主动送入，三平台行为逐字节一致。
 *   - 表容量与命令 id 上界都是编译期常量，无动态扩容，故内存上界确定。
 *
 * 所有权：表是 C 侧自有对象，dial_gui_hotkey_free 释放；与窗口覆盖层同属
 *         「应用拥有」，不挂进 GuiWindowImpl，不引入新的借用槽位。
 */
#include "dial_gui.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define HK_CAP        64    /* 单表最大绑定数 */
#define HK_CMD_MAX    256   /* 命令 id 上界：[0, HK_CMD_MAX) */
#define HK_MODS_ALL   (GUI_MOD_SHIFT | GUI_MOD_CTRL | GUI_MOD_ALT)
#define HK_VIEW_ROWS  8     /* 快捷键表面板一次最多显示的行数 */
#define HK_PANEL_W    200   /* 面板固定宽度（像素）*/

/* 内置 8×8 点阵只读 f->kind=0，其余字段全零即可用于 gui_font_width / draw。*/
static GuiFontImpl g_builtin_font;

/* 简写配色：一律从主题表取，theme_set 一次改色整表立即生效。*/
#define C(role) gui_theme_color(GUI_THEME_ROLE_##role)

static int hk_valid(int64_t key, int64_t mods, int64_t cmd) {
    if (key < 0) return 0;
    if (mods < 0 || (mods & ~(int64_t)HK_MODS_ALL) != 0) return 0;
    if (cmd < 0 || cmd >= HK_CMD_MAX) return 0;
    return 1;
}

static int hk_valid_combo(int64_t key, int64_t mods) {
    if (key < 0) return 0;
    if (mods < 0 || (mods & ~(int64_t)HK_MODS_ALL) != 0) return 0;
    return 1;
}

static int hk_find(const GuiHotkey *h, int64_t key, int64_t mods) {
    for (int i = 0; i < h->count; ++i) {
        if (h->binds[i].key == key && h->binds[i].mods == mods) return i;
    }
    return -1;
}

static void hk_arg_err(const char *what) {
    gui_core_set_err(GUI_ERR_ARG, what);
}

GuiHotkey *dial_gui_hotkey_new(void) {
    GuiHotkey *h = (GuiHotkey *)calloc(1, sizeof(GuiHotkey));
    if (!h) {
        gui_core_set_err(GUI_ERR_ALLOC, "快捷键表内存分配失败");
        return NULL;
    }
    /* 命令默认全部启用；清空表时复位到同一初始态。*/
    memset(h->enabled, 1, sizeof(h->enabled));
    return h;
}

int64_t dial_gui_hotkey_free(GuiHotkey *h) {
    if (!h) return -1;
    free(h);
    return 0;
}

int64_t dial_gui_hotkey_bind(GuiHotkey *h, int64_t key, int64_t mods, int64_t cmd) {
    if (!h) { hk_arg_err("快捷键绑定失败（空表）"); return -1; }
    if (!hk_valid(key, mods, cmd)) {
        hk_arg_err("快捷键绑定失败（非法键码 / 修饰键掩码 / 命令 id）");
        return -1;
    }
    if (hk_find(h, key, mods) >= 0) {
        hk_arg_err("快捷键绑定冲突：同一按键组合已存在绑定（先 unbind 或改用 rebind）");
        return -1;
    }
    if (h->count >= HK_CAP) {
        hk_arg_err("快捷键绑定失败：表已满");
        return -1;
    }
    h->binds[h->count].key = key;
    h->binds[h->count].mods = mods;
    h->binds[h->count].cmd = cmd;
    ++h->count;
    return 0;
}

int64_t dial_gui_hotkey_rebind(GuiHotkey *h, int64_t key, int64_t mods, int64_t cmd) {
    if (!h) { hk_arg_err("快捷键重绑失败（空表）"); return -1; }
    if (!hk_valid(key, mods, cmd)) {
        hk_arg_err("快捷键重绑失败（非法键码 / 修饰键掩码 / 命令 id）");
        return -1;
    }
    int at = hk_find(h, key, mods);
    if (at >= 0) {
        h->binds[at].cmd = cmd;
        return 0;
    }
    if (h->count >= HK_CAP) {
        hk_arg_err("快捷键重绑失败：表已满");
        return -1;
    }
    h->binds[h->count].key = key;
    h->binds[h->count].mods = mods;
    h->binds[h->count].cmd = cmd;
    ++h->count;
    return 0;
}

int64_t dial_gui_hotkey_unbind(GuiHotkey *h, int64_t key, int64_t mods) {
    if (!h) { hk_arg_err("快捷键解绑失败（空表）"); return -1; }
    if (!hk_valid_combo(key, mods)) {
        hk_arg_err("快捷键解绑失败（非法键码 / 修饰键掩码）");
        return -1;
    }
    int at = hk_find(h, key, mods);
    if (at < 0) return -1;
    /* 前移保持插入顺序稳定（只读回 key_at / mods_at / cmd_at 可据此断言）。*/
    for (int i = at; i + 1 < h->count; ++i) h->binds[i] = h->binds[i + 1];
    --h->count;
    memset(&h->binds[h->count], 0, sizeof(h->binds[h->count]));
    return 0;
}

int64_t dial_gui_hotkey_lookup(GuiHotkey *h, int64_t key, int64_t mods) {
    if (!h || !hk_valid_combo(key, mods)) return -1;
    int at = hk_find(h, key, mods);
    return at >= 0 ? h->binds[at].cmd : -1;
}

int64_t dial_gui_hotkey_dispatch(GuiHotkey *h, int64_t key, int64_t mods) {
    if (!h || !hk_valid_combo(key, mods)) return -1;
    int at = hk_find(h, key, mods);
    if (at < 0) return -1;
    int64_t cmd = h->binds[at].cmd;
    return h->enabled[cmd] ? cmd : -1;
}

int64_t dial_gui_hotkey_set_enabled(GuiHotkey *h, int64_t cmd, int64_t on) {
    if (!h || cmd < 0 || cmd >= HK_CMD_MAX) {
        hk_arg_err("命令启用态设置失败（空表或命令 id 越界）");
        return -1;
    }
    h->enabled[cmd] = on ? 1 : 0;
    return 0;
}

int64_t dial_gui_hotkey_is_enabled(GuiHotkey *h, int64_t cmd) {
    if (!h || cmd < 0 || cmd >= HK_CMD_MAX) return -1;
    return h->enabled[cmd] ? 1 : 0;
}

int64_t dial_gui_hotkey_clear(GuiHotkey *h) {
    if (!h) { hk_arg_err("快捷键表清空失败（空表）"); return -1; }
    memset(h->binds, 0, sizeof(h->binds));
    h->count = 0;
    /* 清空绑定同时把所有命令复位为启用；tag 属用户数据，保持不变。*/
    memset(h->enabled, 1, sizeof(h->enabled));
    return 0;
}

int64_t dial_gui_hotkey_count(GuiHotkey *h) {
    return h ? h->count : -1;
}

int64_t dial_gui_hotkey_capacity(GuiHotkey *h) {
    return h ? HK_CAP : -1;
}

int64_t dial_gui_hotkey_set_tag(GuiHotkey *h, int64_t tag) {
    if (!h) return -1;
    h->tag = tag;
    return 0;
}

int64_t dial_gui_hotkey_tag(GuiHotkey *h) {
    return h ? h->tag : -1;
}

int64_t dial_gui_hotkey_key_at(GuiHotkey *h, int64_t idx) {
    if (!h || idx < 0 || idx >= h->count) return -1;
    return h->binds[idx].key;
}

int64_t dial_gui_hotkey_mods_at(GuiHotkey *h, int64_t idx) {
    if (!h || idx < 0 || idx >= h->count) return -1;
    return h->binds[idx].mods;
}

int64_t dial_gui_hotkey_cmd_at(GuiHotkey *h, int64_t idx) {
    if (!h || idx < 0 || idx >= h->count) return -1;
    return h->binds[idx].cmd;
}

/* 把一条绑定渲染成 ASCII 行，例如 "Ctrl+Shift+S -> 7"。*/
static void hk_render_line(char *buf, size_t cap, int64_t key, int64_t mods, int64_t cmd) {
    char keybuf[32];
    if (key >= 32 && key < 127) {
        snprintf(keybuf, sizeof(keybuf), "%c", (char)key);
    } else {
        snprintf(keybuf, sizeof(keybuf), "0x%llX", (unsigned long long)key);
    }
    snprintf(buf, cap, "%s%s%s%s -> %lld",
             (mods & GUI_MOD_CTRL)  ? "Ctrl+"  : "",
             (mods & GUI_MOD_SHIFT) ? "Shift+" : "",
             (mods & GUI_MOD_ALT)   ? "Alt+"   : "",
             keybuf, (long long)cmd);
}

int64_t dial_gui_hotkey_draw(GuiPainterImpl *p, GuiHotkey *h, int64_t x, int64_t y) {
    if (!p || !h) return -1;
    if (h->count == 0) return 0;   /* 空表零像素影响，与覆盖层同口径 */
    int row_h = GUI_LOOK_FONT_SIZE + 2;
    int panel_h = HK_VIEW_ROWS * row_h;
    /* 先擦固定面板再画当前绑定：绑定数减少时旧行被背景覆盖，
     * 绘制结果因此只由当前表内容决定（重绘幂等、差异可观测）。*/
    gui_raster_fill_rect(p, (int)x, (int)y, HK_PANEL_W, panel_h, C(BG));
    gui_raster_stroke_rect(p, (int)x, (int)y, HK_PANEL_W - 1, panel_h - 1, 1, C(BORDER));
    int rows = h->count < HK_VIEW_ROWS ? h->count : HK_VIEW_ROWS;
    char line[96];
    for (int i = 0; i < rows; ++i) {
        hk_render_line(line, sizeof(line), h->binds[i].key, h->binds[i].mods,
                       h->binds[i].cmd);
        gui_font_draw(p, (int)(x + 4), (int)(y + 4 + i * row_h),
                      line, (int)strlen(line), &g_builtin_font,
                      GUI_LOOK_FONT_SIZE, C(TEXT));
    }
    return 0;
}
