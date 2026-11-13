/*
 * dial_gui_raster.c —— 自研 2D 光栅化器 + 字体层（gui.dialpkg）
 *
 * 这是整个包里**唯一**碰像素的翻译单元，也是唯一 include stb_truetype.h
 * 实现段的地方（单头文件库，不能在多个 TU 里展开，否则重复符号）。
 *
 * 抗锯齿不用 XRender / GDI 的现成路径，而是自己算**覆盖面积**：
 * 每个像素取 aa×aa 个均匀子采样点，统计落在几何图元内的比例 cov/(aa*aa)，
 * 再按 cov 把前景色以 source-over 混合进 ARGB32 缓冲。好处是三平台
 * 逐像素一致、不依赖任何显示服务器、get_pixel / painter_crc32 读得到结果。
 * XRender 的职责在窗口合成链（Picture + SetPictureFilter），见 dial_gui_x11.c。
 *
 * 零第三方运行依赖：内置 8x8 点阵是静态常量表；TrueType 解析用 vendor 的
 * stb_truetype.h v1.26（只加载本机可信字体文件，绝不解析网络字体）。
 */
#include "dial_gui.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

#define STB_TRUETYPE_IMPLEMENTATION
#include "stb_truetype.h"

#include "gui_font8.h"

/* ---------------------------------------------------------------- 缓冲管理 */

GuiPainterImpl *gui_painter_new(int w, int h, int owned) {
    if (w <= 0 || h <= 0 || w > 16384 || h > 16384) return NULL;
    GuiPainterImpl *p = (GuiPainterImpl *)calloc(1, sizeof(GuiPainterImpl));
    if (!p) return NULL;
    p->pixels = (uint32_t *)calloc((size_t)w * (size_t)h, sizeof(uint32_t));
    if (!p->pixels) { free(p); return NULL; }
    p->w = w; p->h = h;
    p->aa = 4;
    p->color = gui_rgba(255, 255, 255, 255);
    p->owned = owned;
    /* 第 18 层：高 DPI 放大因子默认 1 = 逐像素等价旧路径。calloc 归零是 0，
     * 必须显式置 1，否则 scale 语义会被静默破坏（参考第 16 层 set_kind 教训）。*/
    p->scale = 1;
    return p;
}

void gui_painter_dispose(GuiPainterImpl *p) {
    if (!p) return;
    if (!p->ext_pixels) free(p->pixels);
    p->pixels = NULL;
    free(p);
}

int gui_painter_resize(GuiPainterImpl *p, int w, int h) {
    if (!p || w <= 0 || h <= 0 || w > 16384 || h > 16384) return 0;
    /* 窗口绑定的画布指向 XShm / DIBSection，必须由后端重建，不能在这里换普通内存。 */
    if (p->ext_pixels) return 0;
    if (w == p->w && h == p->h) return 1;
    uint32_t *np = (uint32_t *)calloc((size_t)w * (size_t)h, sizeof(uint32_t));
    if (!np) return 0;
    free(p->pixels);
    p->pixels = np;
    p->w = w; p->h = h;
    p->has_clip = 0;
    return 1;
}

/* ---------------------------------------------------------------- 混合 / 裁剪 */

static int in_clip(const GuiPainterImpl *p, int x, int y) {
    if (x < 0 || y < 0 || x >= p->w || y >= p->h) return 0;
    if (p->has_clip &&
        (x < p->clip_x || y < p->clip_y ||
         x >= p->clip_x + p->clip_w || y >= p->clip_y + p->clip_h)) return 0;
    return 1;
}

/*
 * source-over，dst 为非预乘 ARGB32。
 * cov/total 是几何覆盖率（0..1 的定点表示），与颜色自带 alpha 相乘。
 * 推导：SA=a*cov/total(0..255)，OA_NUM = SA*255 + DA*(255-SA)，
 *       out_rgb = (src*SA*255 + dst*DA*(255-SA)) / OA_NUM。
 */
static void blend_pixel(GuiPainterImpl *p, int x, int y, gui_color_t c, int cov, int total) {
    if (cov <= 0 || total <= 0 || !in_clip(p, x, y)) return;
    int SA = (int)gui_a(c) * cov / total;
    if (SA <= 0) return;
    if (SA > 255) SA = 255;
    uint32_t *d = &p->pixels[(size_t)y * (size_t)p->w + (size_t)x];
    int DA = (int)gui_a(*d);
    int INV = 255 - SA;
    int OA_NUM = SA * 255 + DA * INV;
    if (OA_NUM <= 0) { *d = 0; return; }
    int oa = OA_NUM / 255;
    if (oa > 255) oa = 255;
    int sr = gui_r(c), sg = gui_g(c), sb = gui_b(c);
    int dr = gui_r(*d), dg = gui_g(*d), db = gui_b(*d);
    int or_ = (sr * SA * 255 + dr * DA * INV) / OA_NUM;
    int og  = (sg * SA * 255 + dg * DA * INV) / OA_NUM;
    int ob  = (sb * SA * 255 + db * DA * INV) / OA_NUM;
    *d = gui_rgba((uint8_t)oa, (uint8_t)or_, (uint8_t)og, (uint8_t)ob);
}

/* ---------------------------------------------------------------- 覆盖面积 AA 内核 */

typedef int (*gui_inside_fn)(double sx, double sy, const void *ctx);

typedef struct { double x, y, w, h; } rect_ctx;
typedef struct { double cx, cy, r2, half_t; } circle_ctx;
typedef struct { double cx, cy, rx, ry; } ellipse_ctx;
typedef struct { double x0, y0, x1, y1, half_t; } line_ctx;
typedef struct { double y0, y1, xtl, xtr, xbl, xbr; } trap_ctx;

static int inside_rect(double sx, double sy, const void *v) {
    const rect_ctx *c = (const rect_ctx *)v;
    return (sx >= c->x && sx < c->x + c->w && sy >= c->y && sy < c->y + c->h);
}
static int inside_circle(double sx, double sy, const void *v) {
    const circle_ctx *c = (const circle_ctx *)v;
    double dx = sx - c->cx, dy = sy - c->cy;
    return dx * dx + dy * dy <= c->r2;
}
static int inside_ring(double sx, double sy, const void *v) {
    const circle_ctx *c = (const circle_ctx *)v;
    double dx = sx - c->cx, dy = sy - c->cy;
    double d = sqrt(dx * dx + dy * dy);
    return fabs(d - sqrt(c->r2)) <= c->half_t;
}
static int inside_ellipse(double sx, double sy, const void *v) {
    const ellipse_ctx *c = (const ellipse_ctx *)v;
    double dx = (sx - c->cx) / c->rx, dy = (sy - c->cy) / c->ry;
    return dx * dx + dy * dy <= 1.0;
}
static int inside_line(double sx, double sy, const void *v) {
    const line_ctx *c = (const line_ctx *)v;
    double vx = c->x1 - c->x0, vy = c->y1 - c->y0;
    double len2 = vx * vx + vy * vy;
    double t = len2 > 0 ? ((sx - c->x0) * vx + (sy - c->y0) * vy) / len2 : 0.0;
    if (t < 0) t = 0; else if (t > 1) t = 1;
    double px = c->x0 + t * vx, py = c->y0 + t * vy;
    double dx = sx - px, dy = sy - py;
    return dx * dx + dy * dy <= c->half_t * c->half_t;
}
static int inside_trapezoid(double sx, double sy, const void *v) {
    const trap_ctx *c = (const trap_ctx *)v;
    if (sy < c->y0 || sy >= c->y1) return 0;
    double span = c->y1 - c->y0;
    double t = span > 0 ? (sy - c->y0) / span : 0.0;
    double left  = c->xtl + (c->xbl - c->xtl) * t;
    double right = c->xtr + (c->xbr - c->xtr) * t;
    if (left > right) { double tmp = left; left = right; right = tmp; }
    return sx >= left && sx < right;
}

/* bbox 与画布 / 裁剪区求交，逐像素做 aa×aa 子采样。 */
static void paint_aa(GuiPainterImpl *p, int x0, int y0, int x1, int y1,
                     gui_inside_fn inside, const void *ctx, gui_color_t c) {
    if (!p || !p->pixels) return;
    int bx0 = x0, by0 = y0, bx1 = x1, by1 = y1;
    if (bx0 < 0) bx0 = 0;
    if (by0 < 0) by0 = 0;
    if (bx1 > p->w) bx1 = p->w;
    if (by1 > p->h) by1 = p->h;
    if (p->has_clip) {
        if (bx0 < p->clip_x) bx0 = p->clip_x;
        if (by0 < p->clip_y) by0 = p->clip_y;
        if (bx1 > p->clip_x + p->clip_w) bx1 = p->clip_x + p->clip_w;
        if (by1 > p->clip_y + p->clip_h) by1 = p->clip_y + p->clip_h;
    }
    if (bx1 <= bx0 || by1 <= by0) return;
    int aa = p->aa < 1 ? 1 : (p->aa > 8 ? 8 : p->aa);
    int total = aa * aa;
    for (int py = by0; py < by1; ++py) {
        for (int px = bx0; px < bx1; ++px) {
            int cov = 0;
            if (aa == 1) {
                cov = inside(px + 0.5, py + 0.5, ctx) ? 1 : 0;
            } else {
                for (int j = 0; j < aa; ++j) {
                    double sy = py + (j + 0.5) / aa;
                    for (int i = 0; i < aa; ++i) {
                        double sx = px + (i + 0.5) / aa;
                        if (inside(sx, sy, ctx)) ++cov;
                    }
                }
            }
            blend_pixel(p, px, py, c, cov, total);
        }
    }
}

/* ---------------------------------------------------------------- 绘图原语 */

void gui_raster_clear(GuiPainterImpl *p, gui_color_t color) {
    if (!p || !p->pixels) return;
    /* 遵守裁剪（契约：只影响裁剪区内）。纯色填充无需子采样，直接全覆盖混合。 */
    gui_raster_fill_rect(p, 0, 0, p->w, p->h, color);
}

void gui_raster_set_pixel(GuiPainterImpl *p, int x, int y, gui_color_t c) {
    if (!p || !p->pixels || x < 0 || y < 0 || x >= p->w || y >= p->h) return;
    p->pixels[(size_t)y * (size_t)p->w + (size_t)x] = c;
}

gui_color_t gui_raster_get_pixel(const GuiPainterImpl *p, int x, int y) {
    if (!p || !p->pixels || x < 0 || y < 0 || x >= p->w || y >= p->h) return 0;
    return p->pixels[(size_t)y * (size_t)p->w + (size_t)x];
}

void gui_raster_fill_rect(GuiPainterImpl *p, int x, int y, int w, int h, gui_color_t c) {
    if (w <= 0 || h <= 0) return;
    rect_ctx rc = { (double)x, (double)y, (double)w, (double)h };
    paint_aa(p, x, y, x + w, y + h, inside_rect, &rc, c);
}

void gui_raster_stroke_rect(GuiPainterImpl *p, int x, int y, int w, int h, int t, gui_color_t c) {
    if (w <= 0 || h <= 0) return;
    if (t < 1) t = 1;
    if (t * 2 >= w || t * 2 >= h) { gui_raster_fill_rect(p, x, y, w, h, c); return; }
    gui_raster_fill_rect(p, x, y, w, t, c);
    gui_raster_fill_rect(p, x, y + h - t, w, t, c);
    gui_raster_fill_rect(p, x, y + t, t, h - t * 2, c);
    gui_raster_fill_rect(p, x + w - t, y + t, t, h - t * 2, c);
}

void gui_raster_line(GuiPainterImpl *p, int x0, int y0, int x1, int y1, int t, gui_color_t c) {
    if (t < 1) t = 1;
    line_ctx lc = { (double)x0, (double)y0, (double)x1, (double)y1, (double)t / 2.0 };
    int bx0 = x0 < x1 ? x0 : x1, bx1 = x0 < x1 ? x1 : x0;
    int by0 = y0 < y1 ? y0 : y1, by1 = y0 < y1 ? y1 : y0;
    int pad = t / 2 + 2;
    paint_aa(p, bx0 - pad, by0 - pad, bx1 + pad + 1, by1 + pad + 1, inside_line, &lc, c);
}

void gui_raster_fill_circle(GuiPainterImpl *p, int cx, int cy, int r, gui_color_t c) {
    if (r <= 0) return;
    circle_ctx cc = { (double)cx, (double)cy, (double)r * (double)r, 0.0 };
    paint_aa(p, cx - r - 1, cy - r - 1, cx + r + 2, cy + r + 2, inside_circle, &cc, c);
}

void gui_raster_stroke_circle(GuiPainterImpl *p, int cx, int cy, int r, int t, gui_color_t c) {
    if (r <= 0) return;
    if (t < 1) t = 1;
    circle_ctx cc = { (double)cx, (double)cy, (double)r * (double)r, (double)t / 2.0 };
    int pad = t / 2 + 2;
    paint_aa(p, cx - r - pad, cy - r - pad, cx + r + pad + 1, cy + r + pad + 1,
             inside_ring, &cc, c);
}

void gui_raster_fill_ellipse(GuiPainterImpl *p, int cx, int cy, int rx, int ry, gui_color_t c) {
    if (rx <= 0 || ry <= 0) return;
    ellipse_ctx ec = { (double)cx, (double)cy, (double)rx, (double)ry };
    paint_aa(p, cx - rx - 1, cy - ry - 1, cx + rx + 2, cy + ry + 2, inside_ellipse, &ec, c);
}

void gui_raster_fill_trapezoid(GuiPainterImpl *p, int y0, int y1,
                               double x_tl, double x_tr, double x_bl, double x_br,
                               gui_color_t c) {
    if (y1 <= y0) return;
    trap_ctx tc = { (double)y0, (double)y1, x_tl, x_tr, x_bl, x_br };
    double lo = x_tl < x_tr ? x_tl : x_tr;
    if (x_bl < lo) lo = x_bl;
    if (x_br < lo) lo = x_br;
    double hi = x_tl > x_tr ? x_tl : x_tr;
    if (x_bl > hi) hi = x_bl;
    if (x_br > hi) hi = x_br;
    paint_aa(p, (int)floor(lo) - 1, y0, (int)ceil(hi) + 2, y1, inside_trapezoid, &tc, c);
}

void gui_raster_blit(GuiPainterImpl *p, int dx, int dy, int w, int h,
                     const unsigned char *src, int src_len, gui_color_t tint) {
    (void)tint;  /* 契约未暴露染色：按源像素原样 alpha 混合 */
    if (!p || !p->pixels || !src || w <= 0 || h <= 0) return;
    if (src_len < w * h * 4) return;
    for (int j = 0; j < h; ++j) {
        for (int i = 0; i < w; ++i) {
            const unsigned char *q = src + ((size_t)j * (size_t)w + (size_t)i) * 4;
            /* 源按 ARGB32 字节序：A,R,G,B */
            gui_color_t c = gui_rgba(q[0], q[1], q[2], q[3]);
            blend_pixel(p, dx + i, dy + j, c, (int)gui_a(c), 255);
        }
    }
}

uint32_t gui_raster_crc32(const GuiPainterImpl *p) {
    if (!p || !p->pixels || p->w <= 0 || p->h <= 0) return 0;
    const unsigned char *buf = (const unsigned char *)p->pixels;
    size_t n = (size_t)p->w * (size_t)p->h * 4u;
    uint32_t crc = 0xFFFFFFFFu;
    for (size_t i = 0; i < n; ++i) {
        crc ^= buf[i];
        for (int k = 0; k < 8; ++k)
            crc = (crc >> 1) ^ (0xEDB88320u & (0u - (crc & 1u)));
    }
    return crc ^ 0xFFFFFFFFu;
}

/* ---------------------------------------------------------------- UTF-8 */

/* 取下一个码点；非法字节一律替换成 U+FFFD，绝不越界读。 */
static int utf8_next(const unsigned char *s, int len, int *i, uint32_t *cp) {
    if (*i >= len) return 0;
    unsigned char c = s[(*i)];
    if (c < 0x80) { *cp = c; (*i)++; return 1; }
    int n; uint32_t v;
    if ((c & 0xE0) == 0xC0)      { n = 2; v = c & 0x1Fu; }
    else if ((c & 0xF0) == 0xE0) { n = 3; v = c & 0x0Fu; }
    else if ((c & 0xF8) == 0xF0) { n = 4; v = c & 0x07u; }
    else { (*i)++; *cp = 0xFFFDu; return 1; }
    if (*i + n > len) { (*i)++; *cp = 0xFFFDu; return 1; }
    for (int k = 1; k < n; ++k) {
        unsigned char cc = s[*i + k];
        if ((cc & 0xC0) != 0x80) { (*i)++; *cp = 0xFFFDu; return 1; }
        v = (v << 6) | (cc & 0x3Fu);
    }
    *i += n; *cp = v;
    return 1;
}

/* ---------------------------------------------------------------- 字体 */

GuiFontImpl *gui_font_default(void) {
    GuiFontImpl *f = (GuiFontImpl *)calloc(1, sizeof(GuiFontImpl));
    if (f) f->kind = 0;
    return f;
}

GuiFontImpl *gui_font_load(const char *path, char *err, int errcap) {
    if (!path || !*path) {
        if (err && errcap > 0) snprintf(err, (size_t)errcap, "字体路径为空");
        return NULL;
    }
    FILE *fp = fopen(path, "rb");
    if (!fp) {
        if (err && errcap > 0) snprintf(err, (size_t)errcap, "无法打开字体文件：%s", path);
        return NULL;
    }
    if (fseek(fp, 0, SEEK_END) != 0) { fclose(fp); return NULL; }
    long sz = ftell(fp);
    if (sz <= 0 || sz > 64L * 1024L * 1024L) {
        fclose(fp);
        if (err && errcap > 0) snprintf(err, (size_t)errcap, "字体文件大小异常：%ld 字节", sz);
        return NULL;
    }
    rewind(fp);
    unsigned char *buf = (unsigned char *)malloc((size_t)sz);
    if (!buf) { fclose(fp); return NULL; }
    size_t got = fread(buf, 1, (size_t)sz, fp);
    fclose(fp);
    if (got != (size_t)sz) { free(buf); return NULL; }

    stbtt_fontinfo *info = (stbtt_fontinfo *)calloc(1, sizeof(stbtt_fontinfo));
    if (!info) { free(buf); return NULL; }
    if (!stbtt_InitFont(info, buf, stbtt_GetFontOffsetForIndex(buf, 0))) {
        free(info); free(buf);
        if (err && errcap > 0) snprintf(err, (size_t)errcap, "不是有效的 TrueType/OpenType 字体：%s", path);
        return NULL;
    }
    GuiFontImpl *f = (GuiFontImpl *)calloc(1, sizeof(GuiFontImpl));
    if (!f) { free(info); free(buf); return NULL; }
    f->kind = 1;
    f->ttf = buf;
    f->ttf_len = (int)sz;
    f->stb = info;
    return f;
}

void gui_font_free(GuiFontImpl *f) {
    if (!f) return;
    free(f->stb);
    free(f->ttf);
    free(f);
}

int gui_font_draw(GuiPainterImpl *p, int x, int y, const char *text, int len,
                  GuiFontImpl *f, int size, gui_color_t c) {
    if (!p || !p->pixels || !text || !f || size <= 0) return -1;
    if (len < 0) len = (int)strlen(text);
    int pen = x;
    int i = 0;
    uint32_t cp;
    const unsigned char *s = (const unsigned char *)text;

    if (f->kind == 0) {
        /* 内置点阵：整数倍最近邻放大，ASCII 之外的码点统一画 '?'。 */
        int sc = size / 8;
        if (sc < 1) sc = 1;
        if (sc > 8) sc = 8;
        while (utf8_next(s, len, &i, &cp)) {
            int ch = (cp >= 0x20 && cp <= 0x7E) ? (int)cp : (int)'?';
            const unsigned char *glyph = GUI_FONT8[ch - 0x20];
            for (int row = 0; row < 8; ++row) {
                unsigned char bits = glyph[row];
                for (int col = 0; col < 8; ++col) {
                    if (bits & (1 << (7 - col)))
                        blend_pixel(p, pen + col * sc, y + row * sc, c, 255, 255);
                }
            }
            pen += 9 * sc;
        }
        return pen;
    }

    /* TrueType：stb 出灰度覆盖位图，逐像素按覆盖率混合（与几何 AA 同一套 blend）。 */
    stbtt_fontinfo *info = (stbtt_fontinfo *)f->stb;
    if (!info) return -1;
    float scale = stbtt_ScaleForPixelHeight(info, (float)size);
    int ascent = 0;
    stbtt_GetFontVMetrics(info, &ascent, NULL, NULL);
    int baseline = y + (int)(ascent * scale + 0.5f);
    while (utf8_next(s, len, &i, &cp)) {
        int bw = 0, bh = 0, xoff = 0, yoff = 0;
        unsigned char *bm = stbtt_GetCodepointBitmap(info, 0, scale, (int)cp,
                                                     &bw, &bh, &xoff, &yoff);
        if (bm) {
            for (int j = 0; j < bh; ++j)
                for (int k = 0; k < bw; ++k) {
                    int cov = bm[(size_t)j * (size_t)bw + (size_t)k];
                    if (cov) blend_pixel(p, pen + xoff + k, baseline + yoff + j, c, cov, 255);
                }
            free(bm);
        }
        int adv = 0, lsb = 0;
        stbtt_GetCodepointHMetrics(info, (int)cp, &adv, &lsb);
        pen += (int)(adv * scale + 0.5f);
    }
    return pen;
}

int gui_font_width(const char *text, int len, GuiFontImpl *f, int size) {
    if (!text || !f || size <= 0) return 0;
    if (len < 0) len = (int)strlen(text);
    int pen = 0, i = 0;
    uint32_t cp;
    const unsigned char *s = (const unsigned char *)text;
    if (f->kind == 0) {
        int sc = size / 8;
        if (sc < 1) sc = 1;
        if (sc > 8) sc = 8;
        while (utf8_next(s, len, &i, &cp)) pen += 9 * sc;
        return pen;
    }
    stbtt_fontinfo *info = (stbtt_fontinfo *)f->stb;
    if (!info) return 0;
    float scale = stbtt_ScaleForPixelHeight(info, (float)size);
    while (utf8_next(s, len, &i, &cp)) {
        int adv = 0, lsb = 0;
        stbtt_GetCodepointHMetrics(info, (int)cp, &adv, &lsb);
        pen += (int)(adv * scale + 0.5f);
    }
    return pen;
}
