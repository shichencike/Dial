"""纯 Python TLS ClientHello 结构化解析器（无第三方依赖）。

用途：把 capture_ch.py 录到的 TLS record 字节解析成结构化字段，调用
fp_norm.py 计算 JA3 / JA4 / JA4_r / JA4_o，作为「Chrome 基准」与
「dialc 客户端候选」共用的解析口径。

不依赖 scapy / dpkt / tls 库：TLS ClientHello 的二进制布局是 RFC 8446
固定的，直接按字节解析即可，这也保证离线可复现。

    python3 clienthello.py --hex <record.hex>       # 输出 JSON
    python3 clienthello.py --bin <record.bin>
    python3 clienthello.py --self-check             # 内置最小自校验
"""

from __future__ import annotations

import argparse
import json
import struct
import sys

import fp_norm

# 扩展名（仅用于诊断展示，不参与指纹计算）
EXT_NAMES = {
    0x0000: "server_name",
    0x0005: "status_request",
    0x000A: "supported_groups",
    0x000B: "ec_point_formats",
    0x000D: "signature_algorithms",
    0x0010: "application_layer_protocol_negotiation",
    0x0012: "signed_certificate_timestamp",
    0x0017: "extended_master_secret",
    0x001B: "compress_certificate",
    0x0023: "session_ticket",
    0x002B: "supported_versions",
    0x002D: "psk_key_exchange_modes",
    0x0033: "key_share",
    0x44CD: "application_settings(ALPS)",
    0xFE0D: "encrypted_client_hello",
    0xFF01: "renegotiation_info",
}


def _u16(buf, off):
    return struct.unpack_from(">H", buf, off)[0]


def parse_clienthello(data: bytes) -> dict:
    """解析一段（可能分片后拼好的）TLS record 流，返回结构化 ClientHello。

    只取第一个 handshake 消息为 ClientHello 的 record；Chrome 的 ClientHello
    是单 record，若后续还有 record（罕见）忽略。
    """
    # ---- TLS record 层 ----
    if len(data) < 5:
        raise ValueError("数据过短：不足 TLS record 头（5 字节）")
    if data[0] != 0x16:
        raise ValueError("不是 handshake record（content type != 0x16）")
    rec_len = _u16(data, 3)
    body = data[5:5 + rec_len]
    if len(body) < 4:
        raise ValueError("TLS record 体过短")

    # ---- Handshake 层 ----
    if body[0] != 0x01:
        raise ValueError("不是 ClientHello（handshake type != 0x01）")
    hs_len = (body[1] << 16) | (body[2] << 8) | body[3]
    hs = body[4:4 + hs_len]

    p = 0
    legacy_version = _u16(hs, p); p += 2
    random_hex = hs[p:p + 32].hex(); p += 32
    sid_len = hs[p]; p += 1
    session_id = hs[p:p + sid_len].hex(); p += sid_len
    cs_len = _u16(hs, p); p += 2
    ciphers = [_u16(hs, p + 2 * i) for i in range(cs_len // 2)]; p += cs_len
    comp_len = hs[p]; p += 1
    compression = list(hs[p:p + comp_len]); p += comp_len
    ext_total = _u16(hs, p); p += 2
    ext_start = p

    p = ext_start
    ext_ids = []
    ext_bodies = {}
    while p + 4 <= ext_start + ext_total:
        et = _u16(hs, p); el = _u16(hs, p + 2); p += 4
        eb = hs[p:p + el]; p += el
        # ext_ids 保序（JA3 顺序敏感）；ext_bodies 供哑扩展实现与逐项比对参考
        ext_ids.append(et)
        ext_bodies.setdefault(et, eb)

    def ext_body(et):
        return ext_bodies.get(et, b"")

    # ---- 关键扩展细解 ----
    supported_groups = []
    b = ext_body(0x000A)
    if len(b) >= 2:
        n = _u16(b, 0)
        supported_groups = [_u16(b, 2 + 2 * i) for i in range(n // 2)]

    sig_algs = []
    b = ext_body(0x000D)
    if len(b) >= 2:
        n = _u16(b, 0)
        sig_algs = [_u16(b, 2 + 2 * i) for i in range(n // 2)]

    supported_versions = []
    b = ext_body(0x002B)
    if b:  # supported_versions 在 ClientHello 里是 1 字节长度前缀
        n = b[0]
        supported_versions = [_u16(b, 1 + 2 * i) for i in range(n // 2)]

    alpn = []
    b = ext_body(0x0010)
    if len(b) >= 2:
        n = _u16(b, 0); q = 2
        while q < 2 + n and q < len(b):
            l = b[q]; q += 1
            alpn.append(b[q:q + l].decode("latin1")); q += l

    point_formats = []
    b = ext_body(0x000B)
    if b:
        point_formats = list(b[1:1 + b[0]])

    key_share_groups = []
    b = ext_body(0x0033)
    if len(b) >= 2:
        n = _u16(b, 0); q = 2
        while q + 4 <= 2 + n:
            g = _u16(b, q); kl = _u16(b, q + 2); q += 4
            key_share_groups.append({"group": g, "key_len": kl})
            q += kl

    has_sni = 0x0000 in ext_ids

    # ---- 指纹（唯一归一化口径）----
    fps = fp_norm.fingerprints(
        legacy_version=legacy_version,
        ciphers=ciphers,
        extensions=ext_ids,
        groups=supported_groups,
        point_formats=point_formats,
        supported_versions=supported_versions,
        has_sni=has_sni,
        alpn=alpn,
        sig_algs=sig_algs,
    )

    return {
        "record_len": rec_len,
        "legacy_version": "%04x" % legacy_version,
        "session_id_len": sid_len,
        "compression_methods": ["%02x" % c for c in compression],
        "cipher_suites": ["%04x" % c for c in ciphers],
        "cipher_suites_norm": fp_norm.norm_sequence(ciphers),
        "cipher_grease_positions": fp_norm.grease_positions(ciphers),
        "extension_ids": ["%04x" % e for e in ext_ids],
        "extension_names": [EXT_NAMES.get(e, "GREASE" if fp_norm.is_grease(e) else "unknown(%04x)" % e)
                            for e in ext_ids],
        "extension_ids_norm": fp_norm.norm_sequence(ext_ids),
        "extension_grease_positions": fp_norm.grease_positions(ext_ids),
        "extension_count": len(ext_ids),
        "extension_bodies_hex": {"%04x" % k: v.hex() for k, v in ext_bodies.items()},
        "supported_groups": ["%04x" % g for g in supported_groups],
        "supported_groups_norm": fp_norm.norm_sequence(supported_groups),
        "signature_algorithms": ["%04x" % s for s in sig_algs],
        "signature_algorithms_norm": fp_norm.norm_sequence(sig_algs),
        "supported_versions": ["%04x" % v for v in supported_versions],
        "supported_versions_norm": fp_norm.norm_sequence(supported_versions),
        "key_share_groups": [{"group": "%04x" % k["group"], "key_len": k["key_len"]}
                             for k in key_share_groups],
        "alpn": alpn,
        "ec_point_formats": ["%02x" % f for f in point_formats],
        "has_sni": has_sni,
        "fingerprints": fps,
    }


def read_record(path: str, is_hex: bool) -> bytes:
    raw = open(path, "rb").read()
    if is_hex:
        return bytes.fromhex(raw.decode("ascii").strip())
    return raw


def _self_check():
    """用一段内置的最小 ClientHello 验证解析与指纹计算自洽。"""
    # legacy 0303 / random 32B / sid 0 / 3 ciphers / comp 00 / 1 ext(supported_versions)
    hs = bytes.fromhex("0303") + b"\x00" * 32 + b"\x00"
    cs = bytes.fromhex("0006130113021303")
    comp = bytes.fromhex("0100")
    # supported_versions ext: id 002b, len 0005, body 04(字节长) 0304 0303
    ext = bytes.fromhex("002b00050403040303")
    body = hs + cs + comp + struct.pack(">H", len(ext)) + ext
    ch = b"\x01" + len(body).to_bytes(3, "big") + body
    rec = b"\x16\x03\x01" + struct.pack(">H", len(ch)) + ch
    r = parse_clienthello(rec)
    assert r["legacy_version"] == "0303"
    assert r["cipher_suites"] == ["1301", "1302", "1303"]
    assert r["extension_ids"] == ["002b"]
    assert r["supported_versions"] == ["0304", "0303"]
    # a 段：t + 13(TLS1.3) + i(无 SNI) + 03(cipher) + 01(ext) + 00(无 ALPN)
    assert r["fingerprints"]["ja4_a"] == "t13i030100", r["fingerprints"]["ja4_a"]
    assert len(r["fingerprints"]["ja3_md5"]) == 32
    assert len(r["fingerprints"]["ja4"]) == 36  # a(10) + _ + b(12) + _ + c(12)
    print("clienthello self-check: OK (解析 / JA3 / JA4 自洽)")


def main():
    ap = argparse.ArgumentParser(description="解析 TLS ClientHello 并计算 JA3/JA4")
    ap.add_argument("--hex", help="TLS record 的 hex 文本文件")
    ap.add_argument("--bin", help="TLS record 的二进制文件")
    ap.add_argument("--self-check", action="store_true", help="运行内置自校验")
    args = ap.parse_args()

    if args.self_check:
        _self_check()
        return
    if args.hex:
        data = read_record(args.hex, True)
    elif args.bin:
        data = read_record(args.bin, False)
    else:
        data = sys.stdin.buffer.read()

    print(json.dumps(parse_clienthello(data), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
