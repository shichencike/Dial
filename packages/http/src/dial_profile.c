/*
 * dial_profile.c —— 两张画像的常量表（唯一真源，见 dial_profile.h）。
 *
 * 数据来源：tests/pkg/fingerprint/chrome_windows_x64.json 的 http_behavior 段。
 * TLS 段为 Chrome 144 实测，HTTP 段按 Chrome 稳定版 Win/x64 导航 GET 的已知常量
 * 写死（见基准文件 http_behavior.source）。改动这里必须同步改基准与 README。
 */
#include "dial_profile.h"

#include <string.h>

/* ---- default：保守画像，沿用原有 6 头顺序与 UA，既有回归零影响 ---- */
static const dh_profile_header k_default_headers[] = {
    { "Host",            NULL,                 NULL,            DH_DYN_HOST },
    { "User-Agent",      "user-agent",        NULL,            DH_DYN_UA   },
    { "Accept",          "accept",            "*/*",           DH_DYN_NONE },
    { "Accept-Language", "accept-language",   "en-US,en;q=0.9", DH_DYN_NONE },
    { "Accept-Encoding", "accept-encoding",   "gzip, deflate", DH_DYN_NONE },
    { "Connection",      NULL,                 "keep-alive",    DH_DYN_NONE },
};

static const dh_profile k_default_profile = {
    "default",
    "dial-http/1.1",
    NULL,                 /* 不发 Sec-CH-UA */
    k_default_headers,
    (int) (sizeof(k_default_headers) / sizeof(k_default_headers[0])),
    NULL, NULL, 0,       /* 空 SETTINGS */
    0                    /* 不发连接级 WINDOW_UPDATE */
};

/* ---- chrome：逐字对齐基准 header_order / header_values / ua / sec_ch_ua ---- */
static const dh_profile_header k_chrome_headers[] = {
    { "Host",                     NULL,                          NULL, DH_DYN_HOST },
    { "Connection",               NULL,                          "keep-alive", DH_DYN_NONE },
    { "Upgrade-Insecure-Requests","upgrade-insecure-requests",   "1",   DH_DYN_NONE },
    { "User-Agent",               "user-agent",                  NULL, DH_DYN_UA },
    { "Accept",                   "accept",
      "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,"
      "image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",    DH_DYN_NONE },
    { "Sec-Fetch-Site",           "sec-fetch-site",              "none",     DH_DYN_NONE },
    { "Sec-Fetch-Mode",           "sec-fetch-mode",              "navigate", DH_DYN_NONE },
    { "Sec-Fetch-User",           "sec-fetch-user",              "?1",       DH_DYN_NONE },
    { "Sec-Fetch-Dest",           "sec-fetch-dest",              "document", DH_DYN_NONE },
    { "Sec-CH-UA",                "sec-ch-ua",                   NULL, DH_DYN_SEC_CH_UA },
    { "Sec-CH-UA-Mobile",         "sec-ch-ua-mobile",            "?0",       DH_DYN_NONE },
    { "Sec-CH-UA-Platform",       "sec-ch-ua-platform",          "\"Windows\"", DH_DYN_NONE },
    { "Accept-Encoding",          "accept-encoding",             "gzip, deflate", DH_DYN_NONE },
    { "Accept-Language",          "accept-language",             "en-US,en;q=0.9", DH_DYN_NONE },
};

/* Chrome 连接前导：SETTINGS 六参数（RFC 7540 §6.5.2）+ 连接级 WINDOW_UPDATE。
 * id/value 与基准 http_behavior.h2.settings 逐项一致，顺序也一致。 */
static const uint16_t k_chrome_settings_id[6]  = { 1, 2, 3, 4, 5, 6 };
static const uint32_t k_chrome_settings_val[6] = {
    65536,      /* HEADER_TABLE_SIZE      */
    0,          /* ENABLE_PUSH            */
    1000,       /* MAX_CONCURRENT_STREAMS */
    6291456,    /* INITIAL_WINDOW_SIZE    */
    16384,      /* MAX_FRAME_SIZE         */
    262144      /* MAX_HEADER_LIST_SIZE   */
};

static const dh_profile k_chrome_profile = {
    "chrome",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
    "\"Chromium\";v=\"144\", \"Google Chrome\";v=\"144\", \"Not_A Brand\";v=\"24\"",
    k_chrome_headers,
    (int) (sizeof(k_chrome_headers) / sizeof(k_chrome_headers[0])),
    k_chrome_settings_id, k_chrome_settings_val, 6,
    15663105u   /* 基准 h2.connection_window_update_increment */
};

/* 当前画像：**线程局部**（期 3 升级，原为进程级全局）。
 * 进程级全局意味着多线程各自发不同画像的请求会互相踩；改成线程局部后，
 * 每条线程独立持有自己的画像，既贴合「画像作为连接行为的一等配置」定位，
 * 也不影响既有单线程用例。每个线程的副本以 &k_default_profile 静态初始化，
 * 故未调用过 dh_profile_set 的线程天然落在 default。 */
static _Thread_local const dh_profile *g_current = &k_default_profile;

const dh_profile *dh_profile_current(void) { return g_current; }

void dh_profile_set(const char *name) {
    if (name && strcmp(name, "chrome") == 0) g_current = &k_chrome_profile;
    else g_current = &k_default_profile;
}

const char *dh_profile_dynamic(const dh_profile *p, int dyn_kind,
                               const char *hostport) {
    if (!p) return "";
    switch (dyn_kind) {
        case DH_DYN_HOST:      return hostport ? hostport : "";
        case DH_DYN_UA:        return p->ua ? p->ua : "";
        case DH_DYN_SEC_CH_UA: return p->sec_ch_ua ? p->sec_ch_ua : "";
        default:               return "";
    }
}
