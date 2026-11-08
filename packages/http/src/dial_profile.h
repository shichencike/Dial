#ifndef DIAL_PROFILE_H
#define DIAL_PROFILE_H

/*
 * dial_profile.h —— 浏览器一致性画像的**单一数据源**（包内共享，不进 .diali）。
 *
 * 为什么独立成文件：HTTP/1.1 与 HTTP/2 都要发同一套浏览器请求头。若各写一份
 * 硬编码，头序 / 头值 / UA 只要有一边漏改，两层行为就会漂移，比对基准也就失去
 * 意义。这里一张表同时给出：
 *   - h1_name：HTTP/1.1 原文头名（大小写敏感，逐字对齐 Chrome 基准录制）；
 *   - h2_name：HTTP/2 小写头名（RFC 7540 / HPACK 要求）；NULL 表示 H2 不发
 *             （Host 由 :authority 取代；Connection 是 RFC 7540 §8.1.2.2 明令
 *             禁止的 hop-by-hop 头，发了对端会直接判协议错）；
 *   - value / dyn_kind：值只写一次，动态项（Host、UA、Sec-CH-UA）由调用方填。
 *
 * 连接前导（H2 SETTINGS 六参数 + 连接级 WINDOW_UPDATE）也收在这里，理由同上：
 * 只有 Chrome 画像发这些，default 画像保持原有的空 SETTINGS 行为，既有回归零影响。
 *
 * 边界：只做协议层行为一致性，不伪造凭据、不绕验证码、不做批量自动化。
 */
#include <stdint.h>

enum {
    DH_DYN_NONE = 0,      /* value 是静态字面量 */
    DH_DYN_HOST = 1,      /* 值取 URL 的 host[:port]（H1 Host / H2 :authority） */
    DH_DYN_UA = 2,        /* 值取画像 ua */
    DH_DYN_SEC_CH_UA = 3  /* 值取画像 sec_ch_ua */
};

typedef struct {
    const char *h1_name;
    const char *h2_name;   /* NULL = HTTP/2 不发此项 */
    const char *value;     /* NULL = 动态值，看 dyn_kind */
    int         dyn_kind;
} dh_profile_header;

typedef struct {
    const char *name;
    const char *ua;
    const char *sec_ch_ua;         /* NULL = 该画像不发 Sec-CH-UA */
    const dh_profile_header *headers;
    int nheaders;
    const uint16_t *settings_id;  /* HTTP/2 连接前导 SETTINGS；NULL/0 = 发空 SETTINGS */
    const uint32_t *settings_val;
    int nsettings;
    uint32_t window_update_increment;  /* 0 = 不发连接级 WINDOW_UPDATE */
} dh_profile;

/* 当前画像（进程级；期 3 将改为线程局部并补 .diali getter）。 */
const dh_profile *dh_profile_current(void);

/* 按名字切换；未知名字一律回退 default（不静默保留一个半吊子状态）。 */
void dh_profile_set(const char *name);

/* 取动态头值：hostport 由调用方传入。 */
const char *dh_profile_dynamic(const dh_profile *p, int dyn_kind,
                               const char *hostport);

#endif /* DIAL_PROFILE_H */
