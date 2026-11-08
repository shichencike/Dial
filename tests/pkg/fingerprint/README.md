# 浏览器一致性画像 · 基准与录制比对工具

本目录为 `http.dialpkg` 的「协议层浏览器一致性」提供**唯一比对基准**与离线回放工具。
目标画像：**Chrome 稳定版 Windows/x64**；一致性目标：**JA3 / JA4 全字段一致 + HTTP 行为逐项一致**。

## 文件

| 文件 | 作用 |
| --- | --- |
| `chrome_windows_x64.json` | **仓库内唯一基准**。含 `clienthello` 解析结果、内嵌原始 `clienthello_record_hex`、`http_behavior`（H1 头序与 H2 帧行为）、`exemptions`（做不到项显式清单）、`meta`（录制口径与来源） |
| `fp_norm.py` | GREASE 归一化 + JA3 / JA4 / JA4_r / JA4_o 计算的**唯一口径**（被 Python 工具与 C 侧 `dial_fingerprint.c` 共同遵循） |
| `clienthello.py` | 纯 Python TLS ClientHello 解析（无第三方依赖），输出结构化字段与指纹 |
| `capture_ch.py` | 录制器：`--chrome` 录 Chrome 基准，`--listen` 录 dialc 候选 |
| `emit_clienthello.c` | 候选 ClientHello 发射器：直接调 `dial_tls_connect` 让 http 包真实发一个 CH（连接 127.0.0.1、SNI 用域名、ALPN 双 offer），供 `--listen` 录制 |
| `emit_http.c` | 候选 HTTP 行为发射器（期 2）：`--h1` 调 `dial_http_get()`、`--h2` 调 `dial_http2_connect()+request()`，两者都先切 `chrome` 画像 |
| `check_hpack.c` | HPACK / Huffman 编码侧往返自校验（期 2）：用包内既有解码器验证静态表索引与 Huffman 编码正确，含全 256 字节覆盖 |
| `check_profile_thread.c` | 期 3 自校：主线程设 `chrome`、子线程不设，断言两线程各读到 `chrome` / `default`，证明画像为线程局部、互不干扰 |
| `record_http.py` | 候选 HTTP 行为录制器：`--h1` 录头序、`--h2` 录 H2 帧序与 SETTINGS。**期 2 起 H2 段真实解析 HPACK**（从 `dial_hpack_tables.h` 读 Huffman 码表 + 内置静态表），伪头顺序 / static_index / huffman 均由解码结果得出，不再是占位常量 |
| `compare_fp.py` | 逐字段比对，输出 `name = pass` / `MISMATCH` / `exemption ... declared` |
| `check_fingerprint_online.py` | 可选在线校验，**默认不跑、不进 CI**，只打印供人工核对 |

## 归一化口径（技术诚实）

**GREASE**（RFC 8701）：16 个值 `0x0a0a / 0x1a1a / … / 0xfafa`，判定式
`(v >> 8) == (v & 0xff) && (v & 0x0f) == 0x0a`。在 cipher / extension / groups /
sig_algs / supported_versions 中一律**剥离值**，但**保留点位**单独断言（GREASE
点位是稳定的浏览器特征，而哈希里看不到它）。patch 内用固定常量表，不调 RNG。

**JA4**（FoxIO）为跨连接稳定的**主判据**：
- a 段 `t13d1516h2` = TCP + TLS1.3 + 有 SNI + 15 cipher + 16 ext + 首 ALPN 首尾字符；
- b 段 = cipher 去 GREASE 后排序、SHA256 前 12 位；
- c 段 = 扩展去 GREASE / SNI(0000) / ALPN(0010) 后排序，接 `_` 与 sig_algs 原始顺序，SHA256 前 12 位。

因排序，Chrome 110+ 的**扩展随机置换不影响 JA4**。实测本基准的 b/c 段
`8daaf6152771_d8a2da3f94cd` 与业界公开的 Chrome JA4 值逐字符一致，证明口径正确。

**JA3**（Salesforce）也剥离 GREASE，但 extension **保持发送顺序**，故顺序敏感，
只能与「基准录制那一次」对齐。**不承诺**与所有 Chrome 连接一致。

**不追求逐字节一致**：ClientHello 含 32 字节 random、key_share 临时公钥、GREASE
随机值、ECH 密文——两次真实 Chrome 握手也不逐字节相同。

## 基准怎么来的（可复现步骤）

1. 本机装 Google Chrome（本基准用 `Google Chrome 144.0.7559.132`）。
2. `python3 capture_ch.py --chrome --out /tmp/ch.bin --json /tmp/ch.json`
   —— 用 headless Chrome + `--host-resolver-rules=MAP fp.local 127.0.0.1` 走
   **域名路径**（发 SNI，JA4 a 段才是 `d` 而非 `i`），只取首个 TLS record，不完成握手。
3. 把 `/tmp/ch.json` 与 `/tmp/ch.bin` 的 hex 写入 `chrome_windows_x64.json` 的
   `clienthello` 与 `clienthello_record_hex`，并复核 `http_behavior` 与 `exemptions`。
4. `python3 compare_fp.py --baseline chrome_windows_x64.json --self` 必须全 pass。

**Linux CI 不重新录制 Chrome**：BoringSSL 跨平台同源，ClientHello 的 cipher /
扩展 / GREASE / groups / sig_algs / ALPN 在 Linux 与 Windows 上一致，故 TLS 段可
直接复用为 Win/x64 基准。基准是一次性录制后提交的**静态文件**；CI 只做
「dialc 客户端候选 vs 基准文件」的离线回放比对。

**HTTP 段来源**：`Sec-CH-UA-Platform: "Windows"`、UA 串等平台字段按 Win/x64 写死
（无法在 Linux 录到），其余 H1 头序与 H2 SETTINGS 六参数为 Chrome 稳定版已知常量。
`http_behavior.source` 已如实标注。

## 关于 ECH（实测纠偏）

Chrome 144 在**所有** `--disable-features=EncryptedClientHello` 组合下仍发 `fe0d`
扩展（ECH GREASE 占位，218 字节），且因之在外层 ClientHello 里省略 `0000` SNI——
实测域名模式下两者并存。这是 2026 版 Chrome 防协议僵化的默认行为，flag 关不掉。

因此基准**保留 `fe0d` 扩展 id**，dialc 侧用**哑扩展占位**对齐扩展集合（JA4 只看
扩展 id，不看 payload），**不实现 HPKE 加密**。基准 `meta.ech` 记
`present_as_grease`，豁免清单 `ech_hpke` 已声明。

## 验收命令

```bash
# 1) 归一化 / 解析器自校验
python3 fp_norm.py && python3 clienthello.py --self-check

# 2) 基准自比：离线重放内嵌 record hex，全部字段与指纹逐字符一致
python3 compare_fp.py --baseline chrome_windows_x64.json --self     # 34 项 pass，EXIT=0

# 3) 候选 vs 基准（期 1 TLS 段；前两行在仓库根执行，后两行在本目录执行）
#    3a) 编译发射器，链接刚构建的 libhttp.a
zig cc -target x86_64-linux-gnu tests/pkg/fingerprint/emit_clienthello.c \
    -I packages/http/src packages/http/lib/x86_64-linux-gnu/libhttp.a -o /tmp/emit_ch
#    3b) 起录制器（--listen 是开关，端口用 --port 0 让 OS 分配）
python3 capture_ch.py --listen --port 0 --port-file /tmp/fp_port \
    --out /tmp/cand.bin --json /tmp/cand.json &
#    3c) 端口文件就绪后让 http 包发一个 CH（SNI=localhost，录制端不完成握手）
/tmp/emit_ch "$(cat /tmp/fp_port)" localhost

# 4) 期 2：HPACK 自校验 + HTTP 行为录制（以下在仓库根执行）
zig cc -target x86_64-linux-gnu tests/pkg/fingerprint/check_hpack.c \
    -I packages/http/src packages/http/lib/x86_64-linux-gnu/libhttp.a -o /tmp/check_hpack
/tmp/check_hpack                                  # hpack_selfcheck = pass
zig cc -target x86_64-linux-gnu tests/pkg/fingerprint/emit_http.c \
    -I packages/http/src -I stdlib -I runtime \
    packages/http/lib/x86_64-linux-gnu/libhttp.a -o /tmp/emit_http
#    4a) H1：录制器起服务，发射器发一个真实 GET（chrome 画像）
python3 tests/pkg/fingerprint/record_http.py --h1 --port 0 \
    --port-file /tmp/fp_h1port --out /tmp/behavior_h1.json &
# 等端口文件就绪后：/tmp/emit_http --h1 "$(cat /tmp/fp_h1port)"
#    4b) H2：录制器起服务，发射器发一个真实 h2c 请求
python3 tests/pkg/fingerprint/record_http.py --h2 --port 0 \
    --port-file /tmp/fp_h2port --out /tmp/behavior_h2.json &
# 等端口文件就绪后：/tmp/emit_http --h2 "$(cat /tmp/fp_h2port)"
#    4c) 合并 h1/h2 两段 → behavior.json
python3 -c "import json; a=json.load(open('/tmp/behavior_h1.json')); \
    a.update(json.load(open('/tmp/behavior_h2.json'))); \
    json.dump(a, open('/tmp/behavior.json','w'), ensure_ascii=False, indent=2)"
python3 tests/pkg/fingerprint/compare_fp.py \
    --baseline tests/pkg/fingerprint/chrome_windows_x64.json \
    --candidate /tmp/cand.json --behavior /tmp/behavior.json

# 5) 可选在线校验（默认跳过）
python3 check_fingerprint_online.py            # -> online_check = skipped

# 6) 期 3：画像线程局部 + 诊断 getter（以下在仓库根执行）
zig cc -target x86_64-linux-gnu tests/pkg/fingerprint/check_profile_thread.c \
    -I packages/http/src -I stdlib -I runtime \
    packages/http/lib/x86_64-linux-gnu/libhttp.a -o /tmp/check_profile_thread -lpthread
/tmp/check_profile_thread                       # profile_thread_local = pass
#   设 chrome 后回读：/tmp/emit_http --h1 <port> 的 stderr 打印
#   dial_http_profile() = chrome
```

> 注：指纹录制 / 比对 / HPACK 自校 / 线程局部自校是**离线回放**工具链，不进
> `tests/run_tests.sh` 的 A→Z 主线（需录制真实 Chrome、且含随机字节，无法离线
> 复现逐字节一致）。但**画像的生效底线**已通过期 4 接入主线：W 组**步骤 11**
> 跑 `tests/pkg/use_http_profile.dial`，校验 `set_profile` + `dial_http_profile`
> 回读一致，且 chrome 画像真实发出的 UA（含 `Chrome/144.0.0.0`）/ `Sec-CH-UA` /
> `Sec-Fetch-*` 被服务端收到、切回 `default` 后回落 `dial-http/1.1`。离线逐字段
> 比对仍手动跑本目录脚本，结论写入各期「实测结果」小节。

## 期 1 实测结果（TLS 段）

外置 patch（`patches/mbedtls-3.6.2-clienthello.patch`）+ 包内集中式 reshape
（`packages/http/src/dial_clienthello_fp.c`）落地后，用 `emit_clienthello` 录到
**307 字节** ClientHello：

| 项 | 实测值 |
| --- | --- |
| JA4 | `t13d1516h2_8daaf6152771_d8a2da3f94cd` |
| JA3 MD5 | `0bb7b38b4e71c92b23a4f3dbed3b0489` |
| cipher / 扩展数 | 16 / 18（去 GREASE 后 15 / 16） |
| SCSV(`0x00ff`) | 无（基准也不发；mbedTLS 默认会追加，reshape 已去掉） |
| ALPN | `h2`, `http/1.1`（双 offer，首项 h2） |
| `has_sni` | true |

`compare_fp.py`（期 1 当时）：TLS 段 **24 项 pass、0 MISMATCH**；HTTP 行为段 2 项
`skip = ... = not_provided`（期 2 已全部转 pass，见下）；9 项 `exemption ... declared`。

ALPN 双 offer 归期 1 而非期 2：`ja4_a` 的 `h2` 与 `alpn` 字段都是硬断言，
单 offer 必然留下 MISMATCH。同步在 `dial_http.c` 加了 fail-closed——服务端若
真协商出 `h2`，HTTP/1.1 层明确报错，绝不拿 H1 解析层去读 H2 帧。

## 期 2 实测结果（HTTP 行为段，H1/H2 全量对齐）

`dial_profile.c` 抽成 H1/H2 共用的单一数据源（头序/头值/UA/Sec-CH-UA/
SETTINGS/WINDOW_UPDATE 只写一次），`dial_hpack.c` 请求侧升级为静态表索引 +
Huffman，`dial_http2.c` 连接前导补齐六参数 SETTINGS + 连接级 WINDOW_UPDATE 后：

| 项 | 实测值 |
| --- | --- |
| H1 头序 | 14 项，逐项等于基准 `header_order`（Host 起、Accept-Language 止） |
| H1 头值 / UA / Sec-CH-UA | 10 组头值、Win/x64 UA、`Chromium/Google Chrome/Not_A Brand` 串全部逐字一致 |
| H2 帧序 | `preface → SETTINGS → WINDOW_UPDATE → HEADERS`，与基准一致 |
| H2 SETTINGS | 六参数 `1:65536 2:0 3:1000 4:6291456 5:16384 6:262144` 全中 |
| H2 WINDOW_UPDATE | 连接级 increment `15663105` |
| H2 伪头序 / HPACK | `:method→:scheme→:authority→:path`；`static_index=true`、`huffman=true`（录制器真解 HPACK 得出，非占位） |

`compare_fp.py --behavior`：TLS + HTTP 行为 **34 项全 pass、0 MISMATCH、0 skip**，
9 项豁免显式声明。`check_hpack.c` 自校验 23 项全绿：13 个 Huffman 往返样例 +
全 256 字节覆盖 + HPACK 编解码往返（indexed / indexed-name / new-name 三路径）
+ 首字节 indexed 与 H 位判据。`record_http.py` 的 Huffman 解码器用 RFC 7541
Appendix C 官方样例（`www.example.com` / `no-cache`）验证。

三个技术裁决：

1. **profile 单一数据源**：`dial_http.c` / `dial_http2.c` 都读 `dial_profile.c`，
   避免两层各硬编码一份导致头值漂移。H1 用原文头名（大小写敏感），H2 用小写头名；
   `Host` 由 H2 的 `:authority` 取代、`Connection` 按 RFC 7540 §8.1.2.2 在 H2 不发
   （hop-by-hop 头，发了对端判协议错）。
2. **default 保守 / chrome 完整**：`default` 画像仍发原有的空 SETTINGS / 无
   WINDOW_UPDATE / 6 头，`chrome` 画像才发全套。既有 W 组回归零影响，浏览器画像
   由 `dial_http_set_profile("chrome")` 显式开启。
3. **HPACK 编码全局升级**（不分画像）：请求侧一律静态表索引 / indexed-name /
   new-name + Huffman。这是 RFC 7541 合规编码，客户端不维护发送侧动态表。
   W 组 h2 用例（Python `h2`/`hpack` 服务端真实握手）负责验证互操作性不受损。

## 期 2 修复：HPACK 解码上限须等于本端声明的 `HEADER_TABLE_SIZE`

`chrome` 画像声明 `SETTINGS HEADER_TABLE_SIZE=65536`，对端据此在响应首块发
动态表大小更新（`3f e1 ff 03` = 65536）。原解码上下文上限写死 `4096`，会报
「动态表大小超过上限」→ stream 失败。按 RFC 7541 §4.2，**解码上限应等于本端自己
声明的值**（服务端 SETTINGS 里的 `HEADER_TABLE_SIZE` 约束的是本端**编码侧**，
不该覆盖解码上下文）。修复点：

- `dial_http2.c`：`dh_hpack_ctx_init(&s->hpack, prof->header_table_size)` 用
  画像值初始化解码上限；删掉 SETTINGS 处理里 `if (id==0x1) s->hpack.max_table_size
  = val` 那行（它把解码上限按对端值打回 4096）。
- `dial_hpack.c`：错误文案与注释同步改为「超过本端声明上限」。

`default` 画像上限仍是 4096（兼容既有行为）；`chrome` 画像落地后才能在真实
Python `h2` 服务端完成完整握手（回归用 default 画像所以未触发）。验证：
`/tmp/emit_http --h2 <port> chrome` 对真实 h2c 服务端返回 `h2_status = 200`。

## 期 3：profile 一等公民（线程局部 + `.diali` 诊断 getter）

期 2 画像是**进程级全局** `g_current`，多线程各自发不同画像的请求会互相踩。
期 3 把画像升级为**线程局部**（`_Thread_local`），并补一个回读 getter：

- `dial_profile.c`：`static _Thread_local const dh_profile *g_current`，
  每线程以 `&k_default_profile` 静态初始化，未调用 `dh_profile_set` 的线程
  天然落在 default。
- `dial_http.c`：新增 `DialString dial_http_profile(void)`，回读当前线程生效的
  画像名（`"default"` / `"chrome"`），供诊断与测试断言。
- `http.diali`：补 `extern fn dial_http_profile(): string`，契约里注明画像为
  线程局部、多线程各自独立。
- `check_profile_thread.c`（新增）：主线程设 `chrome`、子线程不设，断言两线程
  各读到 `chrome` / `default`——若画像仍是全局，子线程会被污染成 `chrome` 而失败。

画像作为「连接行为的一等配置」由此成立：每条线程独立持有自己的画像，既不影响
既有单线程用例，也贴合 Dial 程序用 `spawn` 起多线程各发不同画像请求的场景
（`dial_http_set_profile` 在 connect/request 之前调用即对该线程后续连接生效）。

`emit_http.c` 同步加 getter 回读：设 `chrome` 后 `dial_http_profile()` 即回 `chrome`。

## 做不到 / 不做（基准 `exemptions` 逐项声明，不许含糊）

ECH 的 HPKE 加密、证书压缩解压、真·多组 key_share、`br/zstd` 解压、ALPS 应用层
协商、session/PSK/OCSP/SCT 状态逻辑、cookie jar / 重定向 / 缓存条件请求、
HTTP/2 PRIORITY 依赖树、逐字节完全一致。新边界统一为：**只做协议层浏览器
一致性，不伪造凭据、不绕验证码、不做批量自动化**。
