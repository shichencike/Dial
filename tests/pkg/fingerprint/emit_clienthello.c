/*
 * emit_clienthello.c —— 浏览器一致性画像的「候选 ClientHello 发射器」。
 *
 * 用途：让 http.dialpkg 的 TLS 层（dial_tls_connect → mbedTLS + 外置 patch 钩子）
 * 真实地发出一个 ClientHello，供 capture_ch.py --listen 录制，再用
 * compare_fp.py 与 Chrome 基准逐字段比对。
 *
 * 为什么需要它：dialc 层的 GET 会走完整请求/响应，而录制器只读首个 TLS
 * record 就断开（不完成握手）。这里直接调 C 层的 dial_tls_connect：
 *   - 连接目标固定 127.0.0.1:<port>（IPv4，避开 localhost 解析到 ::1 的坑）；
 *   - SNI 主机名单独传参（默认 "localhost"），保证发 0000 server_name 扩展，
 *     对齐 JA4 a 段的 `d`（有 SNI）而非 `i`；
 *   - ALPN 用与包内一致的双 offer {"h2","http/1.1"}；
 *   - 录制端读完首包就 close，握手必然失败 —— 这**无关紧要**，ClientHello
 *     已经在线上了。故本程序无论握手成败都退出 0。
 *
 * 它只负责「发」，不做证书校验绕过的任何事：dial_tls_connect 内部仍是
 * MBEDTLS_SSL_VERIFY_REQUIRED / fail-closed，只是我们不等握手完成。
 *
 * 编译（Linux 本机验证/回归用，三个交叉目标产物由 build.sh 负责）：
 *   zig cc -target x86_64-linux-gnu tests/pkg/fingerprint/emit_clienthello.c \
 *       -I packages/http/src \
 *       packages/http/lib/x86_64-linux-gnu/libhttp.a -o /tmp/emit_ch
 *
 * 用法：/tmp/emit_ch <port> [sni_host]
 */
#include "dial_tls.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    /* 录制端读完 ClientHello 立即 close，后续 close_notify 可能触发 SIGPIPE。 */
    signal(SIGPIPE, SIG_IGN);

    if (argc < 2) {
        fprintf(stderr, "用法: %s <port> [sni_host]\n", argv[0]);
        return 2;
    }
    int port = atoi(argv[1]);
    const char *sni_host = (argc > 2 && argv[2][0]) ? argv[2] : "localhost";
    if (port <= 0 || port > 65535) {
        fprintf(stderr, "端口非法: %s\n", argv[1]);
        return 2;
    }

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { perror("socket"); return 2; }

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons((uint16_t) port);
    sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);   /* 127.0.0.1 */
    if (connect(fd, (struct sockaddr *) &sa, sizeof(sa)) != 0) {
        perror("connect");
        close(fd);
        return 2;
    }

    const char *alpn[] = { "h2", "http/1.1", NULL };
    uint32_t verify = (uint32_t) -1;
    char errbuf[256];
    errbuf[0] = '\0';

    DialTls *t = dial_tls_connect(fd, sni_host, alpn,
                                  NULL,            /* 走系统根证书库 */
                                  &verify, errbuf, sizeof(errbuf));
    /* 握手成功与否都不影响录制：ClientHello 已在 dial_tls_connect 内部发出。 */
    if (t) {
        dial_tls_free(t);
    } else {
        fprintf(stderr, "握手未完成（预期，录制端不回 ServerHello）：%s\n",
                errbuf[0] ? errbuf : "连接被关闭");
    }
    close(fd);
    return 0;
}
