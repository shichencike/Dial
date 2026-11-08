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
| `record_http.py` | 候选 HTTP 行为录制器：`--h1` 录头序、`--h2` 录 H2 帧序与 SETTINGS |
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

# 3) 候选 vs 基准（期 1 起用 dialc 客户端录到的包）
python3 capture_ch.py --listen 0 --port-file /tmp/fp_port --out /tmp/cand.bin
python3 compare_fp.py --baseline chrome_windows_x64.json --candidate /tmp/cand.bin \
    --behavior /tmp/behavior.json

# 4) 可选在线校验（默认跳过）
python3 check_fingerprint_online.py            # -> online_check = skipped
```

## 做不到 / 不做（基准 `exemptions` 逐项声明，不许含糊）

ECH 的 HPKE 加密、证书压缩解压、真·多组 key_share、`br/zstd` 解压、ALPS 应用层
协商、session/PSK/OCSP/SCT 状态逻辑、cookie jar / 重定向 / 缓存条件请求、
HTTP/2 PRIORITY 依赖树、逐字节完全一致。新边界统一为：**只做协议层浏览器
一致性，不伪造凭据、不绕验证码、不做批量自动化**。
