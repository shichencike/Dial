/*
 * dial_gui.h —— gui.dialpkg 内部公共头（不对外，不属于 .diali 契约）
 *
 * 分层：
 *   dial_gui_core.c    FFI 包装：句柄生命周期 / 错误槽 / 事件槽 / 调后端 vtable
 *   dial_gui_raster.c  自研 2D 光栅化器 + 内置点阵 + stb_truetype 字形
 *   dial_gui_widget.c  控件框架层：Widget 树 / 布局 / 命中 / packed 事件块（纯 C）
 *   dial_gui_theme.c   主题层：14 个可覆写颜色角色 / 默认值 / reset（纯 C）
 *   dial_gui_look.c    成品控件外观层：主题表驱动的四态 + 八种控件绘制 / 事件推进（纯 C）
 *   dial_gui_input.c   输入层：TextBox 编辑 / 光标·选区 / 焦点链 / 键码推进（纯 C）
 *   dial_gui_signal.c  第 6 层：信号槽 / 控件事件订阅——可订阅纯值信号块 + 每树环形队列（纯 C）
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

/* ---- 统一功能键码（两平台同值；dial_gui_event_key）-------------------- *
 * 起点 0x110000 > Unicode 最大码点 0x10FFFF，绝不与可打印字符的码点冲突。
 * X11 KeySym / Win32 VK 都先映射到这里再进事件槽，Dial 侧代码平台无关。
 * 表外键统一收敛为 GUI_KEY_UNKNOWN（不承诺逐键同值，但绝不返回平台私有值）。*/
#define GUI_KEY_BASE       0x110000
#define GUI_KEY_BACKSPACE  (GUI_KEY_BASE +  1)   /* 0x110001 */
#define GUI_KEY_TAB        (GUI_KEY_BASE +  2)   /* 0x110002 */
#define GUI_KEY_ENTER      (GUI_KEY_BASE +  3)   /* 0x110003 */
#define GUI_KEY_ESCAPE     (GUI_KEY_BASE +  4)   /* 0x110004 */
#define GUI_KEY_LEFT       (GUI_KEY_BASE +  5)   /* 0x110005 */
#define GUI_KEY_UP         (GUI_KEY_BASE +  6)   /* 0x110006 */
#define GUI_KEY_RIGHT      (GUI_KEY_BASE +  7)   /* 0x110007 */
#define GUI_KEY_DOWN       (GUI_KEY_BASE +  8)   /* 0x110008 */
#define GUI_KEY_HOME       (GUI_KEY_BASE +  9)   /* 0x110009 */
#define GUI_KEY_END        (GUI_KEY_BASE + 10)   /* 0x11000A */
#define GUI_KEY_PAGEUP     (GUI_KEY_BASE + 11)   /* 0x11000B */
#define GUI_KEY_PAGEDOWN   (GUI_KEY_BASE + 12)   /* 0x11000C */
#define GUI_KEY_DELETE     (GUI_KEY_BASE + 13)   /* 0x11000D */
#define GUI_KEY_SHIFT      (GUI_KEY_BASE + 14)   /* 0x11000E */
#define GUI_KEY_CONTROL    (GUI_KEY_BASE + 15)   /* 0x11000F */
#define GUI_KEY_ALT        (GUI_KEY_BASE + 16)   /* 0x110010 */
#define GUI_KEY_F1         (GUI_KEY_BASE + 17)   /* 0x110011 */
#define GUI_KEY_F12        (GUI_KEY_BASE + 28)   /* 0x11001C */
#define GUI_KEY_UNKNOWN    (GUI_KEY_BASE + 63)   /* 0x11003F */

/* 修饰键位掩码（dial_gui_event_mods / widget_key 的 mods 参数）。*/
#define GUI_MOD_SHIFT      1
#define GUI_MOD_CTRL       2
#define GUI_MOD_ALT        4

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
/* 第 6 层：环形队列完整定义私有在 dial_gui_signal.c，头里只留前向声明。 */
typedef struct GuiSignalQueue GuiSignalQueue;
/* 第 7 层：菜单 / 对话框覆盖层（纯 C，三平台共用，无平台符号）。 */
typedef struct GuiMenuItem    GuiMenuItem;
typedef struct GuiMenu        GuiMenu;
typedef struct GuiScroll       GuiScroll;
typedef struct GuiDialog      GuiDialog;
/* 第 10 层：右键上下文菜单（借用第 7 层 GuiMenu，纯内存对象）。*/
typedef struct GuiContext     GuiContext;
/* 第 11 层：悬浮提示 Tooltip（纯内存对象）。*/
typedef struct GuiTooltip     GuiTooltip;
/* 第 12 层：状态栏 StatusBar（纯内存对象）。*/
typedef struct GuiStatusBar   GuiStatusBar;
/* 第 13 层：通知气泡 Toast（队列覆盖层，纯内存对象）。*/
typedef struct GuiToast       GuiToast;
/* 第 15 层：快捷键 / 命令系统（纯内存表，应用拥有，不挂进窗口槽位）。*/
typedef struct GuiHotkey      GuiHotkey;

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

/* ---- 第 6 层：信号槽 / 控件事件订阅 ------------------------------------- *
 * 信号码是**位值**（可组合成订阅掩码），编号跨版本稳定，只准末尾追加新位。
 * 每个信号只由「语义状态真的发生变化」的那一次交互 / setter 触发一次，
 * a/b 的含义见 gui.diali 的信号码表与 README §38.12。 */
#define GUI_SIG_CLICKED        (1LL << 0)   /* 1   BUTTON RELEASE 激活 */
#define GUI_SIG_TOGGLED        (1LL << 1)   /* 2   CHECKBOX 翻转 / RADIO 选中 / set_checked */
#define GUI_SIG_TEXT_CHANGED   (1LL << 2)   /* 4   TEXTBOX 文本被编辑 / set_text */
#define GUI_SIG_VALUE_CHANGED  (1LL << 3)   /* 8   SLIDER 设值 / set_value */
#define GUI_SIG_FOCUS_CHANGED  (1LL << 4)   /* 16  焦点变化 / 点击聚焦 / Tab */
#define GUI_SIG_SUBMITTED      (1LL << 5)   /* 32  TEXTBOX Enter 提交 / 对话框按钮点击（a=结果码）*/
#define GUI_SIG_SELECTED       (1LL << 6)   /* 64  菜单项被点击选中（a = item id，tag/userdata = 菜单）*/
#define GUI_SIG_COUNT          7
#define GUI_SIG_ALL            ((1LL << GUI_SIG_COUNT) - 1)   /* 127 */
#define GUI_SIGNAL_CAP         64           /* 每树环形队列容量 */

/*
 * 第 6 层信号纯值块（C 侧 owned，Dial 侧只读）。
 *
 * 与上面 104 字节 GuiWidgetEvent 的**关键区别**：本块 **100% 纯值、零指针、
 * 零句柄**——连 widget_id / win_title 那样的借用 const char* 都没有，控件句柄
 * 也不存，只存 tag / userdata 两个 int64 纯值键（Dial 侧自建 map 定位控件）。
 * 因此控件被摘除、文本被 set_text 覆盖、甚至整棵树被释放之后，已入队的信号
 * 仍是一块自洽的整数快照，读数绝不悬垂、绝不需要 string_from_cstr 复制。
 *
 * 布局：9 × int64 = 72 字节，全部字段按 8 字节紧挨。由下面的 _Static_assert
 * 与 Dial demo 的 sizeof/offsetof 编译期内建双向锁死。
 */
typedef struct __attribute__((packed)) GuiWidgetSignal {
    int64_t seq;        /*  0  单调递增序号（从 0 起，clear 不重置）*/
    int64_t signal;     /*  8  GUI_SIG_* 位值（本次只一个位）*/
    int64_t tag;        /* 16  发出控件的 tag（纯值键）*/
    int64_t userdata;   /* 24  发出控件的 userdata（纯值键）*/
    int64_t kind;       /* 32  控件种类 GUI_WIDGET_* */
    int64_t a;          /* 40  主值：新值 / 1=获得焦点 / 点击恒 1 */
    int64_t b;          /* 48  副值：旧值 / 0=失去焦点 / 焦点控件 tag */
    int64_t x;          /* 56  事件坐标 x（键盘类信号为 0）*/
    int64_t y;          /* 64  事件坐标 y（键盘类信号为 0）*/
} GuiWidgetSignal;

_Static_assert(sizeof(GuiWidgetSignal) == 72,
               "GuiWidgetSignal 必须为 72 字节（Dial 侧 packed extern struct 同布局）");
_Static_assert(offsetof(GuiWidgetSignal, seq) == 0,   "seq 偏移须为 0");
_Static_assert(offsetof(GuiWidgetSignal, signal) == 8, "signal 偏移须为 8");
_Static_assert(offsetof(GuiWidgetSignal, tag) == 16,  "tag 偏移须为 16");
_Static_assert(offsetof(GuiWidgetSignal, kind) == 32, "kind 偏移须为 32");
_Static_assert(offsetof(GuiWidgetSignal, a) == 40,    "a 偏移须为 40");
_Static_assert(offsetof(GuiWidgetSignal, b) == 48,    "b 偏移须为 48");
_Static_assert(offsetof(GuiWidgetSignal, x) == 56,    "x 偏移须为 56");
_Static_assert(offsetof(GuiWidgetSignal, y) == 64,    "y 偏移须为 64");

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
    /* 第 18 层：高 DPI 整数放大因子（默认 1 = 逐像素等价旧路径）。
     * 注意 calloc 归零 ≠ 1，必须在 gui_painter_new 里显式置 1。*/
    int       scale;
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
    /* ---- 第 7 层：菜单 / 对话框覆盖层（窗口拥有，free 时一并释放）-------- *
     * 覆盖层不参与控件树布局，因此控件几何 / painter CRC 逐字节不漂移；
     * 菜单项 / 对话框按钮的选中事件复用 root 树的信号队列（须先 set_root）。 */
    GuiMenu      *menubar;     /* 顶栏菜单（左→右排布，恒展开）*/
    GuiMenu      *popup;       /* 当前展开的弹出菜单（可空；点击外部关闭）*/
    int           popup_x, popup_y;   /* 弹出菜单锚点（窗口内坐标）*/
    int           menubar_h;   /* 顶栏高度（固定，默认 22）*/
    GuiDialog    *dialog;      /* 当前模态对话框覆盖层（可空）*/
    /* ---- 第 9~13 层：借用型覆盖层（窗口只持指针，不拥有、不释放）-------- *
     * 与上面第 7 层「窗口拥有」不同：这些对象是应用 `*_new()` 出来的纯内存
     * 对象，应用还要继续 `scroll_to` / `tick` / `push` 推进它们，故窗口绝不
     * 接管其生死；`clear_*` 只清槽位。空槽位一律跳过绘制——未挂载任何
     * 覆盖层时 `gui_window_draw` 的像素逐字节不变（既有 CRC 断言不破）。*/
    GuiScroll    *scroll;      /* 第 9 层：滚动视图（可空）*/
    GuiContext   *context;     /* 第 10 层：右键上下文菜单（可空）*/
    GuiTooltip   *tooltip;     /* 第 11 层：悬浮提示（可空）*/
    GuiStatusBar *statusbar;   /* 第 12 层：状态栏（可空）*/
    GuiToast     *toast;       /* 第 13 层：通知气泡（可空）*/
};

struct GuiAppImpl {
    const GuiBackend *be;
    void             *native;       /* 后端私有数据 */
    /* 最近事件槽（每 App 一份；单线程 GUI 事件循环，无需加锁）*/
    int  ev_kind, ev_x, ev_y, ev_button, ev_key, ev_w, ev_h;
    int  ev_mods;              /* 最近事件的修饰键位掩码（GUI_MOD_*）；
                                * 由后端 pump 在每次事件写入，core 不覆盖 */
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
#define GUI_LAYOUT_GRID  3   /* 第 16 层：行列栅格布局（容器定义 cols/rows，子节点占格）*/

/* ---- 成品控件外观层：种类 + 内置浅色默认主题 -------------------------- *
 * 颜色常量是**默认主题色**（g_default[] 的唯一真相源），运行时可由
 * dial_gui_theme.c 的颜色表按角色覆写；theme_reset() 逐值复位到这些宏。
 * 默认值与引入 theme setter 之前逐字一致，保证三平台逐像素一致、
 * painter_crc32 的历史基准（look_crc_normal = 3884455222）不漂移。 */
#define GUI_WIDGET_BOX       0
#define GUI_WIDGET_LABEL     1
#define GUI_WIDGET_BUTTON    2
#define GUI_WIDGET_CHECKBOX  3
#define GUI_WIDGET_TEXTBOX   4   /* 输入层：单行文本编辑（焦点 / 光标 / 选区）*/
#define GUI_WIDGET_RADIO     5   /* 单选按钮：圆圈 + 选中点 + 右侧文本，同组互斥 */
#define GUI_WIDGET_SLIDER    6   /* 滑块：轨道 + 已填充 + 滑块，RELEASE 按 x 设值 */
#define GUI_WIDGET_PROGRESS  7   /* 进度条：轨道 + 填充，只读不参与激活 */
/* 第 7 层信号 kind 标记（非真实控件，仅用于 GuiWidgetSignal.kind 区分来源）：
 * 菜单项选中 = MENU、对话框按钮点击 = DIALOG。set_kind 仍只校验 0..7。 */
#define GUI_WIDGET_MENU      8
#define GUI_WIDGET_DIALOG    9
/* 第 16 层：组合框 / 列表框（真实控件，set_kind 上界扩到 11）。两者复用
 * GuiWidgetImpl.items 项存储（容量 GUI_COMBO_CAP），绘制/推进在 dial_gui_look.c。*/
#define GUI_WIDGET_COMBO     10  /* 组合框：只读框 + 点击展开下拉，单选取值 */
#define GUI_WIDGET_LIST      11  /* 列表框：常显可选项，单击/多选 + 选中信号 */

/* 组合框 / 列表框项容量（固定，无动态扩容，内存上界确定）。*/
#define GUI_COMBO_CAP        64

/* 第 17 层：TextBox 撤销 / 重做快照栈容量（固定，无动态扩容）。
 * 栈满时丢最老的一条（线性前移），绝不静默截断成「不可撤销」的黑洞。 */
#define GUI_UNDO_CAP         32

/* ---- 第 18 层常量 ------------------------------------------------------ *
 * 全部是纯值 / 固定容量，headless 零 DISPLAY 可测，绝不引入平台符号。 */
/* 高 DPI：整数放大因子合法范围 [1,4]（1 = 逐像素等价旧路径）。*/
#define GUI_SCALE_MIN        1
#define GUI_SCALE_MAX        4
/* 虚拟化：逻辑总行数上界（纯值，不代表内存——可视窗口槽固定 <= 64）。*/
#define GUI_VIRTUAL_MAX_TOTAL   (1000000)
/* IME 预编辑串借用缓冲容量（字节，含结尾 '\0'）。*/
#define GUI_IME_PREEDIT_CAP  256
/* 平台纯值契约层：确定性降级返回码（区别于参数错误 -1）。*/
#define GUI_PLATFORM_DEGRADE (-2)

/* ---- 对话框按钮位（dial_gui_dialog_set_buttons 的 mask）--------------- *
 * 单个对话框可同时挂多个按钮（按位或）；点击后 dial_gui_dialog_result 返回
 * 被点按钮对应的值（1/2/4/8）。 */
#define GUI_DLG_OK     1
#define GUI_DLG_CANCEL 2
#define GUI_DLG_YES    4
#define GUI_DLG_NO     8
#define GUI_DLG_RESULT_NONE  (-1)   /* 尚未点击任何按钮 */

/* TextBox 文本按 UTF-8 字节数计上限；0 表示不限（仍受本上限兜底）。*/
#define GUI_TEXT_MAX_LEN    4096

#define GUI_LOOK_FONT_SIZE  16   /* 内置 8×8 点阵，sc = size/8 = 2，每字符 18×16 */

/* ---- 主题角色（dial_gui_theme_set/get 的 role 取值）-------------------- *
 * 顺序即 g_default[] 的初始化顺序，新增角色只能追加到末尾，绝不插入中间，
 * 这样 role 编号跨版本稳定、旧程序读到的色值语义不变。 */
#define GUI_THEME_ROLE_BG        0
#define GUI_THEME_ROLE_BORDER    1
#define GUI_THEME_ROLE_TEXT      2
#define GUI_THEME_ROLE_TEXT_DIS  3
#define GUI_THEME_ROLE_BTN       4
#define GUI_THEME_ROLE_BTN_HOVER 5
#define GUI_THEME_ROLE_BTN_DOWN  6
#define GUI_THEME_ROLE_BTN_DIS   7
#define GUI_THEME_ROLE_CHECK_BG  8
#define GUI_THEME_ROLE_MARK      9
#define GUI_THEME_ROLE_SEL      10
#define GUI_THEME_ROLE_TRACK    11  /* Slider / Progress 轨道底 */
#define GUI_THEME_ROLE_FILL     12  /* Slider 已填充 / Progress 进度 */
#define GUI_THEME_ROLE_KNOB     13  /* Slider 滑块 */
#define GUI_THEME_ROLE_COUNT    14

#define GUI_THEME_BG        ((gui_color_t)0xFFF4F6F9u)  /* 面板 / BOX 背景 */
#define GUI_THEME_BORDER    ((gui_color_t)0xFFC7CEDAu)  /* 细边框 */
#define GUI_THEME_TEXT      ((gui_color_t)0xFF202733u)  /* 文本 */
#define GUI_THEME_TEXT_DIS  ((gui_color_t)0xFFA9B0BDu)  /* 禁用态文本 */
#define GUI_THEME_BTN       ((gui_color_t)0xFFE7ECF4u)  /* 按钮 普通 */
#define GUI_THEME_BTN_HOVER ((gui_color_t)0xFFD3E0FAu)  /* 按钮 悬停 */
#define GUI_THEME_BTN_DOWN  ((gui_color_t)0xFFB2C8F2u)  /* 按钮 按下 */
#define GUI_THEME_BTN_DIS   ((gui_color_t)0xFFF0F2F5u)  /* 按钮 禁用 */
#define GUI_THEME_CHECK_BG  ((gui_color_t)0xFFFFFFFFu)  /* 勾选框 / Radio / TextBox 底 */
#define GUI_THEME_MARK      ((gui_color_t)0xFF2F6FE0u)  /* 勾 / 焦点边框 / 光标 / 高亮 / 选中点 */
#define GUI_THEME_SEL       ((gui_color_t)0xFFC9DCFBu)  /* TextBox 选区背景 */
#define GUI_THEME_TRACK     ((gui_color_t)0xFFE2E8F0u)  /* 轨道底（新增角色）*/
#define GUI_THEME_FILL      ((gui_color_t)0xFF2F6FE0u)  /* 进度填充（与 MARK 同蓝）*/
#define GUI_THEME_KNOB      ((gui_color_t)0xFFFFFFFFu)  /* 滑块（白底 + MARK 描边）*/

/* 第 16 层：组合框 / 列表框项（C 侧自有副本，容量固定 GUI_COMBO_CAP）。
 * label 是 strdup 副本，替换 / 释放时由 widget.c 的 dispose_subtree 负责 free。*/
typedef struct GuiComboItem {
    int64_t id;        /* 纯值键（SELECTED 信号的 a）*/
    char *label;      /* C 侧 strdup 自有副本（NULL = 空）*/
} GuiComboItem;

/* 第 17 层：TextBox 撤销 / 重做的一条快照（C 侧自有副本，纯值）。
 * text 为 NULL 表示空串；cursor / sel_anchor 是编辑前的 UTF-8 字节偏移。
 * 快照只在键盘编辑（replace_range）成功改文本时入栈；显式 set_text 建立
 * 新基线并清两栈，绝不把程序化赋值记成一次用户编辑。 */
typedef struct GuiUndoSnap {
    char *text;         /* malloc+memcpy 自有副本（NULL = 空串）*/
    int   cursor;
    int   sel_anchor;
} GuiUndoSnap;

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
    /* ---- 输入层（dial_gui_input.c）-------------------------------------- */
    int  focused;               /* 焦点态：一棵树内至多一个（由 input.c 推进） */
    int  cursor;                /* TextBox 光标：UTF-8 字节偏移 [0, strlen(text)] */
    int  sel_anchor;            /* 选区锚点：与 cursor 相等表示无选区 */
    int  max_len;               /* TextBox 输入上限（字节）；0 = 不限 */
    GuiWidgetImpl *focus;       /* 仅根使用：当前焦点控件（树内裸指针，不进 ARC）*/
    /* ---- 主题层 / 成品控件扩展（dial_gui_theme.c + look.c）-------------- *
     * 纯值字段，calloc 已归零；vmax 由 tree_new / add 初始化为 100。 */
    int  value;                 /* SLIDER / PROGRESS 当前值（默认 0）*/
    int  vmin, vmax;            /* 值域 [vmin, vmax]（默认 0..100）*/
    int64_t group;              /* RADIO 组号：非 0 时同组互斥，0 = 各自独立 */
    /* ---- 第 6 层：信号槽 / 控件事件订阅（dial_gui_signal.c）-------------- *
     * sig_mask 每个节点都有（calloc 归零 = 默认不订阅，零开销）；
     * sigq / sig_dropped 仅**根节点**有意义：队列懒分配、丢信号计数可观测。 */
    int64_t sig_mask;           /* 订阅位掩码（GUI_SIG_* 按位或）*/
    int64_t sig_dropped;        /* 本树累计溢出 / 分配失败丢弃数（仅根用）*/
    GuiSignalQueue *sigq;       /* 环形队列（仅根用，首次 emit 时 calloc）*/
    /* ---- 第 16 层：网格布局（dial_gui_widget.c）-------------------------- *
     * 子节点声明自己占哪格（默认 0,0 + 跨度 1,1）；容器声明 cols/rows，
     * layout_apply 按弹性均分余量算单元格矩形（纯几何，零 DISPLAY 依赖）。 */
    int  grid_col, grid_row, grid_cspan, grid_rspan;
    int  grid_cols, grid_rows;  /* 容器网格列数 / 行数（<=0 视为 1×1）*/
    /* ---- 第 16 层：组合框 / 列表框（dial_gui_look.c）--------------------- *
     * items 容量固定 GUI_COMBO_CAP，无动态扩容；label 是 C 侧 strdup 自有副本。
     * 选型态：ComboBox 单选用 selected_idx（默认 -1 未选）；ListBox 多选用
     * list_sel_bits（64 位位图，每位一项，多选 toggle）。 */
    int  combo_open;           /* ComboBox 下拉是否展开 */
    int  selected_idx;         /* ComboBox 当前选中项 idx（-1 未选）*/
    int  item_count;           /* 已添加项数（<= GUI_COMBO_CAP）*/
    int64_t list_sel_bits;     /* ListBox 多选用位图（64 位，每位一项）*/
    /* ---- 第 16 层：右键按下态（dial_gui_look.c，纯值）------------------- */
    int  right_pressed;        /* 右键按下态（widget_handle 推进，纯值）*/
    GuiComboItem items[GUI_COMBO_CAP];  /* 组合框 / 列表框项存储 */
    /* ---- 第 17 层：TextBox 撤销 / 重做（固定容量快照栈）---------------- *
     * 两栈都在控件自身，calloc 归零，dispose_subtree 逐条 free 文本副本。
     * undo_len / redo_len 是当前深度（0..GUI_UNDO_CAP）。 */
    GuiUndoSnap undo_stack[GUI_UNDO_CAP];
    GuiUndoSnap redo_stack[GUI_UNDO_CAP];
    int  undo_len, redo_len;
    /* ---- 第 17 层：每控件主题覆写（未覆写角色回落进程级全局表）-------- *
     * ov_mask 的 bit r 表示角色 r 已被覆写（这样 color=0x00000000 也是合法
     * 覆写值，不会与「未覆写」哨兵混淆）；ov_color[r] 存 0..0xFFFFFFFF。
     * 未覆写时绘制逐像素等价于旧 C(role) 路径，历史 CRC 不漂移。 */
    uint64_t ov_mask;
    int64_t  ov_color[GUI_THEME_ROLE_COUNT];
    /* ---- 第 17 层：Slider 拖动 + Grid 自动换行（纯值）------------------ *
     * drag_active：PRESS 命中 SLIDER 置 1、MOVE 按绝对 x 设值、RELEASE 清 0。
     * grid_autoflow：容器级开关，子节点按加入顺序自动依次占格。 */
    int  drag_active;
    int  grid_autoflow;
    /* ---- 第 18 层：ListBox/Grid 虚拟化 lazy load（纯值数据槽，零反向回调）*
     * 逻辑上可有 virtual_total 行（如 100 万），但 C 侧只持有当前可视窗口
     * [virtual_first, +virtual_count) 的 <= 64 个槽，复用 items[] 存文本。
     * Dial 侧在滚动时把可视区数据「推进」这些槽（纯值，Dial -> C 单向），
     * 绘制/命中按 first 偏移换算，绝不 C -> Dial 反向回调索要数据。
     * virtual_on == 0 时绘制/命中走旧 item_count 路径，逐像素等价。 */
    int  virtual_on;
    int  virtual_total;
    int  virtual_first;
    int  virtual_count;
    /* ---- 第 18 层：IME 预编辑纯值事件块（挂在 widget 树根，非每控件订阅）*
     * 一次 ime_feed 写成一个自洽纯值快照：预编辑串（固定缓冲，借给 Dial
     * string_from_cstr 复制）+ 光标 + 候选框矩形 + 候选数 + 单调 seq。
     * 不进 GUI_SIG_* 位值体系、不扩 GuiWidgetSignal、不改 signal_count()。*/
    char ime_preedit[GUI_IME_PREEDIT_CAP];
    int  ime_caret;
    int  ime_cx, ime_cy, ime_cw, ime_ch;
    int  ime_cand_count;
    int  ime_active;
    int64_t ime_seq;
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

/* ---- 第 16 层：网格布局 / 组合框 / 列表框 / 右键态 --------------------- *
 * 与框架层 / 外观层硬边界一致：只读写 GuiWidgetImpl 上的 C 侧纯值，绝不碰平台
 * API、绝不 C → Dial 反向回调。set_kind 上界由 GUI_WIDGET_PROGRESS(7) 扩到
 * GUI_WIDGET_LIST(11)；8/9 仍留给第 7 层信号 kind 标记，不占真实控件。 */
/* 网格布局：容器声明 cols/rows（<=0 视为 1×1）；子节点声明占格。
 * 全部 : int 一律 int64_t（Dial int 是 64 位，返回 -1 不能被零扩展）。*/
int64_t        dial_gui_widget_set_grid(GuiWidgetImpl *w, int64_t cols, int64_t rows);
int64_t        dial_gui_widget_set_grid_cell(GuiWidgetImpl *w, int64_t col, int64_t row,
                                             int64_t cspan, int64_t rspan);
/* 组合框：只读框 + 点击展开下拉，单选取值（items 容量 GUI_COMBO_CAP）。*/
int64_t        dial_gui_combo_add_item(GuiWidgetImpl *w, int64_t id, DialString label);
int64_t        dial_gui_combo_clear(GuiWidgetImpl *w);
int64_t        dial_gui_combo_count(GuiWidgetImpl *w);
int64_t        dial_gui_combo_item_id(GuiWidgetImpl *w, int idx);
const char    *dial_gui_combo_item_label(GuiWidgetImpl *w, int idx);   /* 借用指针 */
int64_t        dial_gui_combo_select(GuiWidgetImpl *w, int idx);
int64_t        dial_gui_combo_selected(GuiWidgetImpl *w);
int64_t        dial_gui_combo_open(GuiWidgetImpl *w);
int64_t        dial_gui_combo_close(GuiWidgetImpl *w);
int64_t        dial_gui_combo_is_open(GuiWidgetImpl *w);
/* 列表框：常显可选项，单击单选 / 多选 toggle + 选中信号。*/
int64_t        dial_gui_list_add_item(GuiWidgetImpl *w, int64_t id, DialString label);
int64_t        dial_gui_list_clear(GuiWidgetImpl *w);
int64_t        dial_gui_list_count(GuiWidgetImpl *w);
int64_t        dial_gui_list_item_id(GuiWidgetImpl *w, int idx);
const char    *dial_gui_list_item_label(GuiWidgetImpl *w, int idx);    /* 借用指针 */
int64_t        dial_gui_list_select(GuiWidgetImpl *w, int idx);
int64_t        dial_gui_list_select_toggle(GuiWidgetImpl *w, int idx);
int64_t        dial_gui_list_is_selected(GuiWidgetImpl *w, int idx);
int64_t        dial_gui_list_selected_count(GuiWidgetImpl *w);
int64_t        dial_gui_list_selected_first(GuiWidgetImpl *w);
/* 右键按下态（widget_handle 推进的纯值，与左键 pressed 平行）。*/
int64_t        dial_gui_widget_right_pressed(GuiWidgetImpl *w);

/* ---- 第 17 层：TextBox 撤销 / 重做 + Slider 拖动 + 每控件主题覆写 ------- *
 * + Grid 自动换行（全部 : int 一律 int64_t，Dial int 是 64 位）。
 * 与前面各层同一条硬边界：只读写 GuiWidgetImpl 上的 C 侧纯值，绝不碰平台
 * API、绝不 C → Dial 反向回调；未启用时逐像素等价于旧路径。 */
/* TextBox 撤销 / 重做：undo / redo 返回 1 成功、0 栈空、-1 无效句柄。 */
int64_t        dial_gui_widget_undo(GuiWidgetImpl *w);
int64_t        dial_gui_widget_redo(GuiWidgetImpl *w);
int64_t        dial_gui_widget_can_undo(GuiWidgetImpl *w);
int64_t        dial_gui_widget_can_redo(GuiWidgetImpl *w);
int64_t        dial_gui_widget_undo_depth(GuiWidgetImpl *w);
int64_t        dial_gui_widget_redo_depth(GuiWidgetImpl *w);
int64_t        dial_gui_widget_undo_clear(GuiWidgetImpl *w);
int64_t        dial_gui_widget_undo_capacity(GuiWidgetImpl *w);
/* Slider 拖动：PRESS 置 drag_active、MOVE 按绝对 x 设值、RELEASE 清。 */
int64_t        dial_gui_widget_dragging(GuiWidgetImpl *w);
/* 每控件主题覆写：role 越界 / color 高 32 位非 0 一律失败并写错误槽。 */
int64_t        dial_gui_widget_set_color(GuiWidgetImpl *w, int64_t role, int64_t color);
int64_t        dial_gui_widget_color(GuiWidgetImpl *w, int64_t role);
int64_t        dial_gui_widget_has_color(GuiWidgetImpl *w, int64_t role);
int64_t        dial_gui_widget_clear_color(GuiWidgetImpl *w, int64_t role);
int64_t        dial_gui_widget_clear_colors(GuiWidgetImpl *w);
/* Grid 自动换行：容器级开关，子节点按加入顺序自动依次占格。 */
int64_t        dial_gui_widget_set_grid_autoflow(GuiWidgetImpl *w, int64_t on);
int64_t        dial_gui_widget_grid_autoflow(GuiWidgetImpl *w);

/* ---- 第 18 层：高 DPI / 虚拟化 / IME / GPU·原生平台纯值契约 ------------ *
 * 全部 : int 一律 int64_t；string 形参 -> DialString；返回借用 C 串用
 * rawptr<byte>（Dial 侧 string_from_cstr 复制）。四组能力未启用时逐像素
 * 等价于旧路径，故第 1~17 层全部既有 CRC 钉值一字不动。 */
/* 高 DPI（挂 GuiPainter）：scale 默认 1，set 越界 [1,4] 明确拒绝。
 * painter_scaled 用最近邻把画布放大 factor 倍，返回一个新的独立离屏画布
 * （factor==1 时逐字节拷贝，等价旧画布），供离屏 CRC 断言。 */
int64_t        dial_gui_painter_set_scale(GuiPainterImpl *p, int64_t scale);
int64_t        dial_gui_painter_scale(GuiPainterImpl *p);
GuiPainterImpl *dial_gui_painter_scaled(GuiPainterImpl *src, int64_t factor);
/* 虚拟化 lazy load（挂 GuiWidget）：逻辑总行数可极大，C 侧只持有可视窗口槽。 */
int64_t        dial_gui_widget_set_virtual(GuiWidgetImpl *w, int64_t on, int64_t total);
int64_t        dial_gui_widget_virtual_window(GuiWidgetImpl *w, int64_t first, int64_t count);
int64_t        dial_gui_widget_virtual_slot(GuiWidgetImpl *w, int64_t slot,
                                            int64_t id, DialString text);
int64_t        dial_gui_widget_virtual_first(GuiWidgetImpl *w);
int64_t        dial_gui_widget_virtual_count(GuiWidgetImpl *w);
int64_t        dial_gui_widget_virtual_total(GuiWidgetImpl *w);
int64_t        dial_gui_widget_virtual_on(GuiWidgetImpl *w);
/* IME 预编辑纯值事件块（挂 widget 树根；不进 GUI_SIG_* 位值体系）。 */
int64_t        dial_gui_ime_feed(GuiWidgetImpl *w, DialString preedit, int64_t caret,
                                 int64_t cx, int64_t cy, int64_t cw, int64_t ch,
                                 int64_t cand_count);
const char    *dial_gui_ime_preedit(GuiWidgetImpl *w);   /* 借用指针 */
int64_t        dial_gui_ime_caret(GuiWidgetImpl *w);
int64_t        dial_gui_ime_cand_count(GuiWidgetImpl *w);
int64_t        dial_gui_ime_rect(GuiWidgetImpl *w, int64_t which); /* 0..3 = cx/cy/cw/ch */
int64_t        dial_gui_ime_seq(GuiWidgetImpl *w);
int64_t        dial_gui_ime_clear(GuiWidgetImpl *w);
/* GPU 合成 + 原生平台集成「纯值契约层」（dial_gui_platform.c，无句柄、无平台符号）。
 * 本轮只做参数校验 + 能力探测 + 确定性降级码；真实后端留下一批。 */
int64_t        dial_gui_gpu_available(void);
const char    *dial_gui_gpu_backend(void);               /* 借用指针 */
int64_t        dial_gui_gpu_caps(void);
int64_t        dial_gui_gpu_try_composite(GuiPainterImpl *p);
const char    *dial_gui_gpu_degrade_reason(void);        /* 借用指针 */
int64_t        dial_gui_tray_set(int64_t slot, int64_t flags);
int64_t        dial_gui_native_file_dialog(int64_t mode);
int64_t        dial_gui_global_hotkey_register(int64_t id, int64_t mods, int64_t key);
int64_t        dial_gui_platform_degrade_code(void);
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

/* ---- 输入层：dial_gui_input.c（纯 C，三平台共用，无平台符号）---------- *
 * 与前三层的硬边界一致：只 include dial_gui.h + libc，绝不碰 X11 / Win32，
 * 绝不保存函数指针、绝不 C → Dial 反向回调。焦点是根上的 C 侧裸指针，
 * 文本是控件自己的 strdup 副本，键盘一律由 Dial 侧主动喂纯值事件推进。 */
GuiWidgetImpl *gui_widget_focus(GuiWidgetImpl *root);
int            gui_widget_set_focus(GuiWidgetImpl *root, GuiWidgetImpl *w);
/* backward=0 为 Tab，非 0 为 Shift+Tab；返回新焦点（无焦点返回 NULL）。*/
GuiWidgetImpl *gui_widget_focus_next(GuiWidgetImpl *root, int backward);
/* 返回 0 无变化 / 1 文本或光标变化 / 2 焦点变化 / 3 Enter 提交 / <0 参数错。*/
int            gui_widget_key(GuiWidgetImpl *root, int kind, int key, int mods);
/* a/b 是 UTF-8 字节偏移，自动 clamp 到字符边界；a==b 表示清选区。*/
int            gui_widget_select(GuiWidgetImpl *w, int a, int b);
int            gui_widget_set_cursor(GuiWidgetImpl *w, int pos);
/* ---- 第 17 层：TextBox 撤销 / 重做快照栈（dial_gui_input.c）----------- *
 * 快照在 replace_range 成功改文本前入栈；显式 set_text 建立新基线并清两栈。
 * 返回 1 = 成功撤销 / 重做，0 = 栈空（不是错误，绝不写错误槽）。 */
int            gui_widget_undo(GuiWidgetImpl *w);
int            gui_widget_redo(GuiWidgetImpl *w);
void           gui_widget_undo_clear(GuiWidgetImpl *w);

/* ---- 主题层：dial_gui_theme.c（纯 C，三平台共用，无平台符号）---------- *
 * 进程级静态颜色表，GUI 单线程事件循环无需加锁；默认值逐字取自 GUI_THEME_*，
 * reset 后逐像素 CRC 与引入 setter 之前一致。role 取 GUI_THEME_ROLE_*。 */
gui_color_t gui_theme_color(int role);
int         gui_theme_set(int role, gui_color_t c);
int64_t     gui_theme_get(int role);
void        gui_theme_reset(void);
int         gui_theme_role_count(void);

/* ---- 成品控件扩展：值域 / 值 / 单选组（look.c + core.c）---------------- *
 * 与前四层的硬边界一致：只读写 GuiWidgetImpl 上的 C 侧纯值，绝不碰平台 API、
 * 绝不 C → Dial 反向回调。 */
/* RADIO 同组互斥：选中 hit 时清掉同组其它 RADIO 的 checked。 */
void gui_widget_radio_select(GuiWidgetImpl *root, GuiWidgetImpl *hit);
/* SLIDER 按命中点局部 x 比例设值并 clamp 进 [vmin, vmax]；返回 1 有变化。 */
int  gui_widget_slider_set_from_x(GuiWidgetImpl *hit, int local_x);

/* ---- 第 6 层：dial_gui_signal.c（纯 C，三平台共用，无平台符号）---------- *
 * 与前五层硬边界一致：只读写 GuiWidgetImpl / 队列上的 C 侧纯值，绝不碰平台
 * API、绝不保存函数指针、绝不 C → Dial 反向回调。信号块 100% 纯值零指针。 */
/* 埋点：w 产生 signal 变化时调用；未订阅直接返回（零订阅=零开销）。 */
void           gui_signal_emit(GuiWidgetImpl *w, int64_t signal,
                               int64_t a, int64_t b, int x, int y);
/* 第 7 层：非控件来源（菜单 / 对话框）的选中事件，kind/tag/userdata 显式给定。*/
void           gui_signal_emit_ex(GuiWidgetImpl *anchor, int64_t signal,
                                  int64_t a, int64_t b, int x, int y,
                                  int kind, int64_t tag, int64_t userdata);
int64_t        gui_signal_pending(GuiWidgetImpl *root);   /* 待取条数 */
GuiWidgetSignal *gui_signal_peek(GuiWidgetImpl *root);    /* 队首块地址 / NULL */
int            gui_signal_pop(GuiWidgetImpl *root);       /* 1 弹出 / 0 空 */
int            gui_signal_clear(GuiWidgetImpl *root);     /* 清空，seq 不重置 */
int64_t        gui_signal_dropped(GuiWidgetImpl *root);   /* 累计丢弃数 */

/* ---- 第 7 层：菜单 / 对话框（dial_gui_menu.c，纯 C，三平台共用，无平台符号）*
 * 硬边界与前六层一致：只 include dial_gui.h + libc，绝不碰 X11 / Win32、
 * 绝不保存函数指针、绝不 C → Dial 反向回调。菜单项 / 对话框按钮的选中事件
 * 复用 root 控件树的信号队列（GuiWidgetSignal），kind 标记 MENU / DIALOG。
 * 覆盖层不参与控件树布局，因此控件几何 / painter CRC 逐字节不漂移。 */
#define GUI_MENU_BAR_H  22   /* 顶栏 / 弹出菜单行高（像素）*/
/* 弹出菜单 / 上下文菜单的**行高**（像素）：第 7 层 popup 与第 10 层上下文菜单
 * 共用同一常量，故两者行几何逐像素一致。*/
#define GUI_MENU_ROW_H  20

struct GuiMenuItem {
    int64_t id;            /* 菜单项 id（纯值键；SELECTED 信号的 a）*/
    char *label;          /* C 侧 strdup 自有副本（NULL 表示分隔符）*/
    int  sep;             /* 1 = 分隔符（绘制横线，不可命中）*/
    int  disabled;
    GuiMenuItem *next;
};

struct GuiMenu {
    GuiMenuItem *first;
    int  count;           /* 项数（含分隔符）*/
    int64_t tag;
    int64_t userdata;
    int  hover;           /* 悬停项下标（-1 无，仅绘制用）*/
};

struct GuiDialog {
    char *title;
    char *message;
    int  buttons;         /* GUI_DLG_* 位掩码 */
    int  result;          /* 最近结果码；GUI_DLG_RESULT_NONE = 未点 */
    int  open;            /* 是否展开显示（窗口级覆盖）*/
    int  w, h;            /* 对话框尺寸（绘制时按文本算，缓存）*/
    int64_t tag, userdata;
};

struct GuiScroll {
    int  vp_x, vp_y, vp_w, vp_h;   /* 视口矩形（窗口内坐标，含滚动条占用）*/
    int  content_w, content_h;     /* 内容总尺寸 */
    int  off_x, off_y;             /* 当前偏移（已夹紧）*/
    int  x_enabled, y_enabled;     /* 横 / 纵是否启用 */
    int  bar_size;                 /* 滚动条厚度，默认 10 */
    int64_t tag;                   /* 用户 tag（全宽，可 round-trip）*/
};

/*
 * 第 10 层：右键上下文菜单（GuiContext，纯内存对象，无窗口）。
 *
 * **借用**一份第 7 层 GuiMenu（set_menu 不转移所有权、free 也不释放菜单），
 * 菜单项的增删 / 禁用 / tag 全部走第 7 层 API，本层零重复实现。
 * 本层只管：弹出锚点、展开状态、命中 / 悬停 / 选中、绘制。
 */
struct GuiContext {
    GuiMenu *menu;       /* 借用的第 7 层菜单（不拥有）*/
    int  x, y;           /* 弹出锚点（窗口内坐标）*/
    int  open;           /* 是否展开 */
    int  hover;          /* 悬停行号（-1 无；与绘制行序一致）*/
    int64_t tag;         /* 用户 tag（全宽，可 round-trip）*/
};

/*
 * 第 11 层：悬浮提示 Tooltip（GuiTooltip，纯内存对象，无窗口）。
 *
 * 文本是 C 侧 strdup 自有副本（与 GuiWidget.text / GuiMenuItem.label 同口径），
 * 调用方不保留指针。**延迟完全由调用方以毫秒数推进**（hover_delay / tick），
 * 本层**不读时钟、不依赖平台 API**，故三平台行为逐字节一致、headless 可测。
 */
struct GuiTooltip {
    char *text;          /* C 侧 strdup 自有副本（NULL = 无文本）*/
    int  x, y;           /* 提示框左上角（窗口内坐标）*/
    int  w, h;           /* 提示框尺寸（set_size 设；缺省按文本算）*/
    int  delay_ms;       /* 悬停多久后弹出（默认 500）*/
    int  hover_ms;       /* 已悬停累计毫秒（纯值，由 tick 推进）*/
    int  shown;          /* 当前是否显示（hover_ms >= delay_ms 且有文本）*/
    int  fixed;          /* 1 = 固定位置（不跟随鼠标）*/
    int64_t tag;         /* 用户 tag（全宽，可 round-trip）*/
};

/*
 * 第 12 层：状态栏 StatusBar（GuiStatusBar，纯内存对象，无窗口）。
 *
 * 窗口底栏的纯值模型：左文本区 + 右文本区 + 可选进度条槽。两段文本都是
 * C 侧 strdup 自有副本（同 GuiTooltip / GuiWidget.text 口径）。
 * 进度条值域 [vmin, vmax] 由调用方设，set_value 自动夹紧。
 */
struct GuiStatusBar {
    char *left;          /* 左侧文本（C 侧 strdup 自有副本，NULL = 无）*/
    char *right;         /* 右侧文本（C 侧 strdup 自有副本，NULL = 无）*/
    int  h;              /* 状态栏高度（像素，默认 22）*/
    int  has_progress;   /* 是否显示进度条槽位 */
    int  value;          /* 进度条当前值（已夹紧到 [vmin, vmax]）*/
    int  vmin, vmax;     /* 进度条值域（默认 0..100）*/
    int  sep;            /* 1 = 左右文本之间画竖分隔线 */
    int64_t tag;         /* 用户 tag（全宽，可 round-trip）*/
};

/* 第 13 层：通知气泡 Toast —— 队列覆盖层（纯内存对象，无窗口）。
 *
 * 一组「消息」按先进先出排队，每条自带生命周期；生命周期由调用方以毫秒数
 * 推进（tick(dt_ms)），到期自动消隐。本层**不读时钟、不捕获鼠标**，
 * 故三平台行为逐字节一致、可确定性单测。
 */
#define GUI_TOAST_CAP 16   /* 队列容量（满时 push 拒绝最旧的一条）*/

typedef struct GuiToastItem {
    char *text;          /* C 侧 strdup 自有副本（NULL = 已释放的空槽）*/
    int  life_ms;        /* 本条总生命周期（毫秒，<= 0 表示不过期）*/
    int  elapsed_ms;     /* 本条已存活毫秒（由 tick 推进）*/
    int  kind;           /* 0 = 信息 / 1 = 警告 / 2 = 错误 */
    int64_t tag;         /* 单条 tag（全宽，可 round-trip）*/
} GuiToastItem;

struct GuiToast {
    GuiToastItem items[GUI_TOAST_CAP];   /* FIFO 队列（head = 最旧）*/
    int  count;                          /* 当前排队条数 */
    int  x, y;            /* 气泡左上角锚点（窗口内坐标）*/
    int  w, h;            /* 单条气泡尺寸（缺省按文本算）*/
    int  spacing;         /* 相邻气泡间距（像素）*/
    int  fixed_size;      /* 1 = 用固定 w/h，不按文本自适应*/
    int64_t tag;          /* 用户 tag（全宽，可 round-trip）*/
};

/* ---- 第 15 层：快捷键 / 命令系统（纯内存表）--------------------------------
 * 编译期定长，无动态扩容，内存上界确定。命令 id 是 [0, GUI_HOTKEY_CMD_MAX)
 * 内的整数，语义完全归应用；C 侧只管「按键组合 → id」与「id 启用态」。
 * 绝不保存函数指针 / 绝不 C → Dial 反向回调。
 */
#define GUI_HOTKEY_CAP       64
#define GUI_HOTKEY_CMD_MAX   256

typedef struct GuiHotkeyBind {
    int64_t key;    /* 统一键码（可打印字符是 Unicode 码点，功能键见 GUI_KEY_*）*/
    int64_t mods;   /* GUI_MOD_SHIFT / GUI_MOD_CTRL / GUI_MOD_ALT 按位或 */
    int64_t cmd;    /* 命令 id */
} GuiHotkeyBind;

struct GuiHotkey {
    GuiHotkeyBind binds[GUI_HOTKEY_CAP];       /* 按插入顺序，unbind 后前移压实 */
    int  count;
    uint8_t enabled[GUI_HOTKEY_CMD_MAX];       /* 每命令启用态，默认 1 */
    int64_t tag;                               /* 用户 tag（64 位 round-trip）*/
};

/* 菜单项增删 / 访问（纯内存，无窗口；可在无 DISPLAY 环境单元测试）*/
GuiMenu  *gui_menu_new(void);
void      gui_menu_free(GuiMenu *m);
int       gui_menu_add_item(GuiMenu *m, int64_t id, const char *label);
int       gui_menu_add_sep(GuiMenu *m);
int       gui_menu_set_tag(GuiMenu *m, int64_t tag);
int64_t   gui_menu_tag(GuiMenu *m);
int       gui_menu_count(GuiMenu *m);
int64_t   gui_menu_item_id(GuiMenu *m, int idx);
const char *gui_menu_item_label(GuiMenu *m, int idx);   /* 借用指针 */
int       gui_menu_item_set_enabled(GuiMenu *m, int idx, int v);

/* 对话框（纯内存，无窗口）*/
GuiDialog *gui_dialog_new(void);
void       gui_dialog_free(GuiDialog *d);
int        gui_dialog_set_title(GuiDialog *d, const char *title);
int        gui_dialog_set_message(GuiDialog *d, const char *msg);
int        gui_dialog_set_buttons(GuiDialog *d, int mask);
int        gui_dialog_result(GuiDialog *d);
int        gui_dialog_open(GuiDialog *d);

/* 窗口级覆盖层：统一绘制 + 事件分发（菜单 / 对话框优先，否则委派控件树）。
 * 仅在 win->root 已 set_root 时才产生信号（选中进 root 树队列）。*/
int  gui_window_draw(GuiWindowImpl *win);
int  gui_window_handle(GuiWindowImpl *win, int kind, int x, int y, int button);

/* 第 9 层：滚动视图（dial_gui_scroll.c，窗口级辅助覆盖层）。
 * 纯几何 + 绘制：视口裁剪走 dial_gui_set_clip，滚动条走 gui_raster_*，
 * 零平台符号、绝不反向回调；偏移是纯值，由应用主动 scroll_to / wheel / drag 推进。*/
GuiScroll *dial_gui_scroll_new(void);
int64_t    dial_gui_scroll_free(GuiScroll *s);
int64_t    dial_gui_scroll_set_viewport(GuiScroll *s, int x, int y, int w, int h);
int64_t    dial_gui_scroll_set_content(GuiScroll *s, int w, int h);
int64_t    dial_gui_scroll_enable(GuiScroll *s, int which, int on);   /* which: 0 横 / 1 纵 / 2 两者 */
int64_t    dial_gui_scroll_set_tag(GuiScroll *s, int64_t tag);
int64_t    dial_gui_scroll_tag(GuiScroll *s);
int64_t    dial_gui_scroll_offset_x(GuiScroll *s);
int64_t    dial_gui_scroll_offset_y(GuiScroll *s);
int64_t    dial_gui_scroll_scroll_to(GuiScroll *s, int x, int y);    /* 越界夹紧 */
int64_t    dial_gui_scroll_wheel(GuiScroll *s, int dy);              /* 滚轮：dy>0 下滚，返回新 off_y */
int64_t    dial_gui_scroll_hit(GuiScroll *s, int x, int y, int which); /* 0 未命中 / 1 滑块 / 2 轨道 */
int64_t    dial_gui_scroll_drag(GuiScroll *s, int x, int y, int which); /* 拖拽推进，返回新 off_y */
int64_t    dial_gui_scroll_set_bar_size(GuiScroll *s, int sz);
int64_t    dial_gui_scroll_draw_bar(GuiPainterImpl *p, GuiScroll *s, int which);

/* 覆盖层几何 / 命中 / 绘制（dial_gui_menu.c 内部，供 gui_window_* 调用）*/
void gui_menu_draw_bar(GuiPainterImpl *p, GuiMenu *m, int w);
void gui_menu_draw_popup(GuiPainterImpl *p, GuiMenu *m, int px, int py);
void gui_dialog_draw(GuiPainterImpl *p, GuiDialog *d, int win_w, int win_h);
/* 弹出菜单宽度（第 7 层 popup 与第 10 层上下文菜单共用同一口径）。*/
int  gui_menu_popup_w(GuiMenu *m);

/* 第 10 层：右键上下文菜单（dial_gui_context.c，纯 C，三平台共用，无平台符号）。
 * 纯几何 + 纯值状态：借用第 7 层 GuiMenu，锚点 / 展开 / 命中 / 悬停 / 选中
 * 全部由应用主动推进，绝不自动捕获鼠标事件、绝不反向回调。*/
GuiContext *dial_gui_context_new(void);
int64_t     dial_gui_context_free(GuiContext *c);
int64_t     dial_gui_context_set_menu(GuiContext *c, GuiMenu *m);
GuiMenu    *dial_gui_context_menu(GuiContext *c);
int64_t     dial_gui_context_set_tag(GuiContext *c, int64_t tag);
int64_t     dial_gui_context_tag(GuiContext *c);
int64_t     dial_gui_context_open(GuiContext *c, int x, int y);
int64_t     dial_gui_context_close(GuiContext *c);
int64_t     dial_gui_context_is_open(GuiContext *c);
int64_t     dial_gui_context_anchor_x(GuiContext *c);
int64_t     dial_gui_context_anchor_y(GuiContext *c);
int64_t     dial_gui_context_hit(GuiContext *c, int x, int y);
int64_t     dial_gui_context_hover(GuiContext *c, int x, int y);
int64_t     dial_gui_context_select(GuiContext *c, int x, int y);
int64_t     dial_gui_context_draw(GuiPainterImpl *p, GuiContext *c);

/* 第 11 层：悬浮提示 Tooltip（dial_gui_tooltip.c，纯 C，三平台共用，无平台符号）。
 * 纯值状态机：悬停毫秒由应用侧 tick(hover_ms) 推进，本层不读时钟、不捕获鼠标，
 * 绝不反向回调；文本 / 尺寸 / 延迟全部可配。*/
GuiTooltip *dial_gui_tooltip_new(void);
int64_t     dial_gui_tooltip_free(GuiTooltip *t);
/* 内部（C 层，文本已拷成 NUL 结尾）：FFI 包装 dial_gui_tooltip_set_text 在 core.c。*/
int64_t     gui_tooltip_set_text(GuiTooltip *t, const char *text);
const char *dial_gui_tooltip_text(GuiTooltip *t);          /* 借用指针 */int64_t     dial_gui_tooltip_set_delay(GuiTooltip *t, int ms);
int64_t     dial_gui_tooltip_delay(GuiTooltip *t);
int64_t     dial_gui_tooltip_set_pos(GuiTooltip *t, int x, int y);
int64_t     dial_gui_tooltip_pos_x(GuiTooltip *t);
int64_t     dial_gui_tooltip_pos_y(GuiTooltip *t);
int64_t     dial_gui_tooltip_set_size(GuiTooltip *t, int w, int h);
int64_t     dial_gui_tooltip_size_w(GuiTooltip *t);
int64_t     dial_gui_tooltip_size_h(GuiTooltip *t);
int64_t     dial_gui_tooltip_tick(GuiTooltip *t, int dt_ms);  /* 推进悬停毫秒，返回是否显示 */
int64_t     dial_gui_tooltip_reset(GuiTooltip *t);            /* 归零悬停毫秒（移出即调用）*/
int64_t     dial_gui_tooltip_shown(GuiTooltip *t);             /* 当前是否显示 */
int64_t     dial_gui_tooltip_hover_ms(GuiTooltip *t);          /* 已悬停累计毫秒 */
int64_t     dial_gui_tooltip_set_tag(GuiTooltip *t, int64_t tag);
int64_t     dial_gui_tooltip_tag(GuiTooltip *t);
int64_t     dial_gui_tooltip_draw(GuiPainterImpl *p, GuiTooltip *t);

/* 第 12 层：状态栏 StatusBar（dial_gui_statusbar.c，纯 C，三平台共用，无平台符号）。
 * 纯值模型：左右两段文本 + 进度条槽位；值域由调用方设，set_value 自动夹紧。*/
GuiStatusBar *dial_gui_statusbar_new(void);
int64_t       dial_gui_statusbar_free(GuiStatusBar *sb);
/* 内部（C 层，文本已拷成 NUL 结尾）：FFI 包装在 core.c。*/
int64_t       gui_statusbar_set_left(GuiStatusBar *sb, const char *text);
int64_t       gui_statusbar_set_right(GuiStatusBar *sb, const char *text);
const char   *dial_gui_statusbar_left(GuiStatusBar *sb);      /* 借用指针 */
const char   *dial_gui_statusbar_right(GuiStatusBar *sb);     /* 借用指针 */
int64_t       dial_gui_statusbar_set_height(GuiStatusBar *sb, int h);
int64_t       dial_gui_statusbar_height(GuiStatusBar *sb);
int64_t       dial_gui_statusbar_set_sep(GuiStatusBar *sb, int on);
int64_t       dial_gui_statusbar_sep(GuiStatusBar *sb);
int64_t       dial_gui_statusbar_enable_progress(GuiStatusBar *sb, int on);
int64_t       dial_gui_statusbar_has_progress(GuiStatusBar *sb);
int64_t       dial_gui_statusbar_set_range(GuiStatusBar *sb, int vmin, int vmax);
int64_t       dial_gui_statusbar_value_min(GuiStatusBar *sb);
int64_t       dial_gui_statusbar_value_max(GuiStatusBar *sb);
int64_t       dial_gui_statusbar_set_value(GuiStatusBar *sb, int v);   /* 越界自动夹紧 */
int64_t       dial_gui_statusbar_value(GuiStatusBar *sb);
int64_t       dial_gui_statusbar_set_tag(GuiStatusBar *sb, int64_t tag);
int64_t       dial_gui_statusbar_tag(GuiStatusBar *sb);
int64_t       dial_gui_statusbar_draw(GuiPainterImpl *p, GuiStatusBar *sb, int win_w);

/* 第 13 层：通知气泡 Toast（dial_gui_toast.c，纯 C，三平台共用，无平台符号）。
 * 队列覆盖层：多条消息 FIFO 排队，生命周期由调用方 tick(dt_ms) 推进、到期自动
 * 消隐；本层不读时钟、不捕获鼠标，绝不反向回调。*/
GuiToast *dial_gui_toast_new(void);
int64_t   dial_gui_toast_free(GuiToast *t);
/* 内部（C 层，文本已拷成 NUL 结尾）：FFI 包装在 core.c。*/
int64_t   gui_toast_push(GuiToast *t, const char *text, int life_ms, int kind);
int64_t   dial_gui_toast_count(GuiToast *t);
int64_t   dial_gui_toast_clear(GuiToast *t);
int64_t   dial_gui_toast_tick(GuiToast *t, int dt_ms);        /* 推进并消隐到期条目 */
const char *dial_gui_toast_item_text(GuiToast *t, int idx);   /* 借用指针 */
int64_t   dial_gui_toast_item_kind(GuiToast *t, int idx);
int64_t   dial_gui_toast_item_elapsed(GuiToast *t, int idx);
int64_t   dial_gui_toast_item_life(GuiToast *t, int idx);
int64_t   dial_gui_toast_item_tag(GuiToast *t, int idx);
int64_t   dial_gui_toast_item_set_tag(GuiToast *t, int idx, int64_t tag);
int64_t   dial_gui_toast_set_pos(GuiToast *t, int x, int y);
int64_t   dial_gui_toast_pos_x(GuiToast *t);
int64_t   dial_gui_toast_pos_y(GuiToast *t);
int64_t   dial_gui_toast_set_size(GuiToast *t, int w, int h);
int64_t   dial_gui_toast_size_w(GuiToast *t);
int64_t   dial_gui_toast_size_h(GuiToast *t);
int64_t   dial_gui_toast_set_spacing(GuiToast *t, int sp);
int64_t   dial_gui_toast_spacing(GuiToast *t);
int64_t   dial_gui_toast_set_tag(GuiToast *t, int64_t tag);
int64_t   dial_gui_toast_tag(GuiToast *t);
int64_t   dial_gui_toast_draw(GuiPainterImpl *p, GuiToast *t);

/* ---- 第 14 层：窗口级覆盖层自动集成（借用语义，15 个追加 extern fn）------
 * 每层三件套：set_* 挂载（不转移所有权）/ <layer> 读回槽位（未挂载返回空
 * 句柄）/ clear_* 卸载（只清指针，绝不释放对象）。挂载后 `gui_window_draw`
 * 会按固定顺序自动叠加这 5 层；空槽位零像素影响。C 侧实现在 dial_gui_core.c。
 */
int64_t         dial_gui_window_set_scroll(GuiWindowImpl *win, GuiScroll *s);
GuiScroll      *dial_gui_window_scroll(GuiWindowImpl *win);
int64_t         dial_gui_window_clear_scroll(GuiWindowImpl *win);
int64_t         dial_gui_window_set_context(GuiWindowImpl *win, GuiContext *c);
GuiContext     *dial_gui_window_context(GuiWindowImpl *win);
int64_t         dial_gui_window_clear_context(GuiWindowImpl *win);
int64_t         dial_gui_window_set_tooltip(GuiWindowImpl *win, GuiTooltip *t);
GuiTooltip     *dial_gui_window_tooltip(GuiWindowImpl *win);
int64_t         dial_gui_window_clear_tooltip(GuiWindowImpl *win);
int64_t         dial_gui_window_set_statusbar(GuiWindowImpl *win, GuiStatusBar *sb);
GuiStatusBar   *dial_gui_window_statusbar(GuiWindowImpl *win);
int64_t         dial_gui_window_clear_statusbar(GuiWindowImpl *win);
int64_t         dial_gui_window_set_toast(GuiWindowImpl *win, GuiToast *t);
GuiToast       *dial_gui_window_toast(GuiWindowImpl *win);
int64_t         dial_gui_window_clear_toast(GuiWindowImpl *win);

/* ---- 第 15 层：快捷键 / 命令系统（18 个追加 extern fn，dial_gui_hotkey.c）----
 * 纯内存表：bind 冲突 Fail-Fast，rebind 覆盖式，unbind 压实顺序；
 * lookup 读绑定（不看启用态），dispatch 只发启用且命中的命令。
 * 所有 : int 一律 int64_t（Dial int 是 64 位，返回 -1 不能被零扩展）。
 */
GuiHotkey      *dial_gui_hotkey_new(void);
int64_t         dial_gui_hotkey_free(GuiHotkey *h);
int64_t         dial_gui_hotkey_bind(GuiHotkey *h, int64_t key, int64_t mods, int64_t cmd);
int64_t         dial_gui_hotkey_rebind(GuiHotkey *h, int64_t key, int64_t mods, int64_t cmd);
int64_t         dial_gui_hotkey_unbind(GuiHotkey *h, int64_t key, int64_t mods);
int64_t         dial_gui_hotkey_lookup(GuiHotkey *h, int64_t key, int64_t mods);
int64_t         dial_gui_hotkey_dispatch(GuiHotkey *h, int64_t key, int64_t mods);
int64_t         dial_gui_hotkey_set_enabled(GuiHotkey *h, int64_t cmd, int64_t on);
int64_t         dial_gui_hotkey_is_enabled(GuiHotkey *h, int64_t cmd);
int64_t         dial_gui_hotkey_clear(GuiHotkey *h);
int64_t         dial_gui_hotkey_count(GuiHotkey *h);
int64_t         dial_gui_hotkey_capacity(GuiHotkey *h);
int64_t         dial_gui_hotkey_set_tag(GuiHotkey *h, int64_t tag);
int64_t         dial_gui_hotkey_tag(GuiHotkey *h);
int64_t         dial_gui_hotkey_key_at(GuiHotkey *h, int64_t idx);
int64_t         dial_gui_hotkey_mods_at(GuiHotkey *h, int64_t idx);
int64_t         dial_gui_hotkey_cmd_at(GuiHotkey *h, int64_t idx);
int64_t         dial_gui_hotkey_draw(GuiPainterImpl *p, GuiHotkey *h, int64_t x, int64_t y);

#endif /* DIAL_GUI_H */
