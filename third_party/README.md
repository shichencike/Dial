# third_party —— 随仓库分发的第三方源码

本目录下的代码**不是本项目写的**，是从上游原样取来的第三方源码。
为的是**离线可复现**：构建 `http.dialpkg` / `crypto.dialpkg` 时不再需要外网，
上游仓库变动也不会悄悄改掉我们的产物。

| 目录 | 上游 | 版本 | 许可 | 为什么进仓 |
| --- | --- | --- | --- | --- |
| `mbedtls/` | https://github.com/Mbed-TLS/mbedtls | 3.6.2 | Apache-2.0 OR GPL-2.0-or-later（取 Apache-2.0） | **两个包共用**：`http.dialpkg` 的 TLS 传输层（TLS 1.3 + 证书校验 fail-closed + HTTP/2 `h2` 的 ALPN 协商）；`crypto.dialpkg` 的五种摘要（MD5 / SHA-1 / SHA-256 / SHA-512 / SHA3-256）、HMAC-SHA256 与 Base64 编解码 |
| `zlib/` | https://github.com/madler/zlib | 1.3.1 | zlib License | HTTP `Content-Encoding: gzip / deflate` 的**解压**（inflate） |

## 铁律：绝不手搓密码学底层

TLS 与摘要类密码学**一律不自己实现**。自己写密码学是安全事故的经典入口
（侧信道、padding oracle、随机数质量、边界条件），本项目一律走成熟实现：
需要密码学时用 `mbedtls`，需要压缩时用 `zlib`。

`crypto.dialpkg` 是这条铁律的直接落地：`packages/crypto/src/dial_crypto.c`
只做 `DialString` 句柄 ↔ 裸字节的 FFI 包装与错误诊断，五种摘要走
`mbedtls_md()`、HMAC-SHA256 走 `mbedtls_md_hmac()`、Base64 走
`mbedtls_base64_encode/decode()`。**不自己拼 ipad/opad，不自己写摘要**。

自研的部分只限**协议编解码与传输**：HTTP/1.1 报文解析、Header 拼装、
Socket 读写、HTTP/2 分帧与 HPACK 头压缩——这些不含密码学，且必须由我们自己
掌控（线级细节可控，才能为后续的连接指纹定制留出空间）。HPACK 的静态表与
Huffman 码表由 `tools/gen_hpack_tables.py` 从 Python `hpack` 库常量生成，
不是手写的密码学或压缩算法实现。

## 许可与再分发

- `mbedtls/LICENSE`：上游原文（Apache-2.0 / GPL-2.0 双许可，我们取 Apache-2.0）。
- `zlib/LICENSE`：上游原文（zlib License）。
- 两份许可文件都**原样保留**，未做任何删改；上游的版权声明同样保留在源文件头。

四个 C11 包（`http` / `text` / `json` / `crypto`）各自在 `dial.toml` 里声明
`license = "MIT OR Apache-2.0"`，与上述两份许可兼容（Apache-2.0 与 MIT 兼容）。
`text` / `json` 零第三方依赖，但沿用同一许可口径。

## 裁剪说明（不是「原样」）

`zlib/` 只保留了 **HTTP 解压真正用到的核心文件**：

- 保留：`adler32.c` `crc32.c` `deflate.c` `infback.c` `inffast.c` `inflate.c`
  `inftrees.c` `trees.c` `uncompr.c` `zutil.c` `compress.c` 及配套头文件；
- **排除**：`gzclose.c` `gzlib.c` `gzread.c` `gzwrite.c` —— 这是 zlib 的
  **gzip 文件** API（读写 `.gz` 文件），依赖 POSIX `lseek`，与 HTTP 的
  **内存解压**无关；带上它反而让 Windows / musl 目标编不过。

`mbedtls/` 保留了 `library/` `include/` `3rdparty/`（everest、p256-m）
与 `LICENSE`；上游的 `tests/` `programs/` `docs/` `scripts/` 等**未收录**
（本项目不需要它们，且它们会带来大量无关文件）。

**`crypto.dialpkg` 为什么归档 mbedTLS 全量**：本仓库 `mbedtls_config.h` 打开了
`MBEDTLS_PSA_CRYPTO_C` 与 `MBEDTLS_RIPEMD160_C`，`md.c` 会引用 `psa_*` /
`ripemd160` 符号；只挑 `md5.c` / `sha*.c` / `base64.c` 会在链接期缺符号。
静态归档由链接器**按需抽取**，最终可执行文件不会因此变大，只是
`.dialpkg` 包体积略增（约 6 MB）。`http.dialpkg` 也采用同一口径。

## 如何更新版本

1. 从上游下载对应 tag 的 tarball；
2. 按上面的「裁剪说明」替换对应目录；
3. 更新本文件里的版本号；
4. 重新构建三平台静态库并跑全量回归。

**不要**手工修改 `third_party/` 里的任何文件——一旦手改，就再也无法
从上游复现同一份产物了。
