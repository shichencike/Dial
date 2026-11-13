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
 * 只做纯值契约层：extern fn + Fail-Fast 参数校验 + 能力探测 + 确定性降级码。
 *   - 不 dlopen 任何 GL/EGL/Vulkan/系统库，不 #include 任何 windows.h / X11 / GL 头；
 *   - 不做真实 GPU 合成、不弹原生对话框、不注册真实系统托盘 / 全局快捷键；
 *   - headless（无 /dev/dri、无桌面会话）下所有能力探测恒为「未装载」，
 *     所有动作恒返确定性降级码 GUI_PLATFORM_DEGRADE(-2)。
 * 真实后端（挂 core.c 的 window_flush，只上传 CPU framebuffer 不改像素）与
 * 原生弹窗/托盘/快捷键实机，留到下一批。硬件加速与原生弹窗**未实机验证**。
 *
 * 为什么「纯值」是必要的：这套 GUI 的宪法是「只追加 extern fn、零反向回调、
 * 逐像素确定」。真实 GPU/平台能力必须由 Dial 侧主动查询可用码、再决定是否
 * 调用；C 侧绝不反向回调 Dial，也绝不因缺库让可执行文件加载失败。因此先
 * 落地一层「可查询、可校验、可预测降级」的纯值契约，是接入真实后端前
 * 唯一不破坏确定性的形态。
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
