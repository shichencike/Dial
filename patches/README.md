# patches —— mbedTLS 3.6.2 外置补丁（ClientHello 重排钩子）

## 为什么不直接改 `third_party/`

`third_party/README.md` 的铁律：**不要手工修改 `third_party/` 里的任何文件**——
一旦手改，就再也无法从上游复现同一份产物。

但浏览器一致性画像要求 ClientHello 的 cipher 顺序、扩展顺序、GREASE 点位、
哑扩展 id 集合与 Chrome 基准对齐。精读 mbedTLS 3.6.2 的
`library/ssl_client.c` / `ssl_tls13_client.c` 后确认，公开配置 API 做不到：

| 公开 API | 实际能做什么 | 为什么不够 |
| --- | --- | --- |
| `mbedtls_ssl_conf_ciphersuites()` | 改 cipher 列表顺序 | 写 CH 时会跳过未知 id，GREASE(`0x0a0a`) 进不去；`ssl_write_client_hello_cipher_suites()` 末尾还**强制追加** SCSV(`0x00ff`)，而 Chrome 144 不发 SCSV |
| `mbedtls_ssl_conf_groups()` | 改 `supported_groups` | 未知 group 被过滤，PQC 组 `11ec` 无法声明；也插不进 GREASE |
| `mbedtls_ssl_conf_sig_algs()` | 改 `signature_algorithms` 顺序 | 只影响内容，改不了**扩展写入顺序** |
| `mbedtls_ssl_conf_alpn_protocols()` | 改 ALPN | ✅ 唯一够用的，双 offer 在 `dial_tls.c` 调用侧配置 |
| 自定义扩展 / 扩展顺序 / ECH / 证书压缩 | — | mbedTLS 3.6.2 **没有**公开 API；扩展顺序在 `ssl_write_client_hello_body()` 里硬编码 |

所以采用**外置 patch**：上游源码在 `third_party/` 里一个字节都不动，差异以
patch 文件存在这里，构建期复制到临时目录再打补丁。

> 期 1 曾计划调用上面三个 conf API，精读后**取消**：它们会过滤 GREASE 与
> `11ec`，无法表达基准；且集中式 reshape 已统一控制发出的字节，mbedTLS
> 内部默认列表足以覆盖服务器实际会选择的 cipher / group / sig_alg。加这三个
> API 反而会制造「内部状态 ≠ 对外宣称」的不一致。唯一必须同步内部状态的是
> ALPN（服务端可能选 `h2`，mbedTLS 接收侧要认得），故只做双 offer 配置。

## patch 改了什么

`mbedtls-3.6.2-clienthello.patch` 只碰 `library/ssl_client.c` 两处：

1. 头部加一行 `#include "dial_clienthello_fp.h"`；
2. `ssl_write_client_hello_body()` 末尾、`*out_len` 回写之后、`return 0` 之前，
   插入一次钩子调用：

```c
size_t fp_len = 0;
if (dial_fp_reshape_clienthello(buf, *out_len,
                                (size_t) (end - buf), &fp_len) == 0) {
    *out_len = fp_len;
}
```

真正的编排逻辑在**包内** `packages/http/src/dial_clienthello_fp.c`，不属于上游。

### 为什么这个注入点安全

上层 `mbedtls_ssl_write_client_hello()` 在 body 返回后用 `msg_len`（即
`*out_len`）依次做：

```c
mbedtls_ssl_add_hs_hdr_to_checksum(ssl, MBEDTLS_SSL_HS_CLIENT_HELLO, msg_len);
ssl->handshake->update_checksum(ssl, buf, msg_len - binders_len);
mbedtls_ssl_finish_handshake_msg(ssl, buf_len, msg_len);
```

即 handshake header 的 3 字节长度、record 长度、transcript hash **都由新长度
推导**，没有在 body 之前就写死。故在 body 函数末尾整体重建并回写长度，
transcript 自动一致，不需要碰任何校验和代码。

### 严格不动的部分

密钥生成、HKDF / AEAD / 签名、RNG、`transcript hash`、
`handshake->sent_extensions` mask、`pre_shared_key` 必须是最后一条扩展的约束、
PSK binders 逻辑——全部原样。reshape 只写 `uint16_t` 与字节、调整顺序、
插 GREASE 哑值与哑扩展占位，符合「绝不手搓密码学」铁律。

### 失败语义（Fail-Safe，非 Fail-Closed）

`dial_fp_reshape_clienthello()` 在缓冲不足 / 解析失败 / 非 TLS 1.3 握手 /
含 PSK binders / **HelloRetryRequest 之后带 `cookie(002c)` 的第二个
ClientHello** 等情况下返回非 0 且**不修改 buffer**，钩子忽略返回值，
mbedTLS 原生 ClientHello 照常发出。画像改造失败只导致「指纹不对齐」，
**不会拖垮握手**。

cookie 这条例外是功能正确性必需：真实站点常用 cookie-HRR 做防 DoS，
服务器靠 cookie 确认地址所有权；reshape 若把 cookie 丢掉，服务器会再次
HRR 或直接拒绝。放行原生 CH 后，第一个 CH 仍是指纹对齐的，第二个 CH 不对齐
但连接能完成。

## 构建期流程（`packages/http/build.sh`）

1. `sha256sum -c patches/mbedtls-3.6.2.SHA256SUMS` 校验上游全量 311 文件；
2. `cp -a third_party/mbedtls` → `mktemp -d` 副本；
3. `patch -p1 --dry-run` 通过后再实际应用；
4. 编译**副本**里的 `library/*.c` 与包内 `dial_clienthello_fp.c`，
   `-I` 全部指向副本，`ssl_client.c` 额外加 `-I packages/http/src`；
5. `trap` 清理临时目录；
6. 三个平台构建结束后**再跑一次** `sha256sum -c`，确认 `third_party/` 未被触碰。

`crypto.dialpkg` 不引用这个钩子，仍编译原版 `third_party/mbedtls`，不受影响。

## 可复现性 / CI

```bash
bash patches/gen_patch.sh --check   # SUMS 完整性 + patch 可重放 + dry-run
bash patches/gen_patch.sh --write   # 从当前 upstream 重新生成 patch 与 SUMS
git diff --exit-code -- third_party/   # 上游目录必须零改动
```

`gen_patch.sh` 是 patch 的**唯一生成口径**：它带精确锚点断言，上游一旦漂移
（换版本 / 改了钩子附近代码）会直接报错，而不是静默生成错误补丁。
CI 会跑 `--check` 与 `git diff --exit-code -- third_party/`。

## 边界（不做的事）

- ECH(`fe0d`) 只发哑占位，不实现 HPKE 加密；
- `compress_certificate(001b)` 只发空算法列表，不实现 brotli 解压；
- `key_share` 只保留 mbedTLS 真实生成的 X25519 一组，不做真·多组 / PQC 交换；
- `ALPS(44cd)`、`session_ticket`、`SCT`、`OCSP` 只占位，不实现会话状态逻辑；
- 服务器若对哑扩展作出响应，mbedTLS 接收侧可能拒绝——本地 Python ssl / h2
  服务器实测不回，真实环境属已声明边界；
- **不承诺逐字节一致**：ClientHello 含 `random` / 临时公钥 / GREASE 随机值 /
  ECH 密文，两次真 Chrome 也不相同。目标是 JA3/JA4 全字段一致。

## 上游升级步骤

1. 按 `third_party/README.md` 的「裁剪说明」替换 mbedTLS 目录；
2. `bash patches/gen_patch.sh --write`（锚点漂移则人工复核钩子位置）；
3. `bash packages/http/build.sh` 三平台零警告；
4. 跑 W 组步骤 7（TLS 1.3 握手）/ 9（h2）/ 11（指纹比对）与全量 A→Z 回归。
