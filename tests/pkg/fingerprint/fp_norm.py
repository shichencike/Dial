"""GREASE 归一化与 JA3 / JA4 指纹计算。

本模块是浏览器一致性画像的**唯一归一化口径**，被 clienthello.py、
compare_fp.py 与 C 侧 dial_fingerprint.c 共用的规则基准（C 侧按本文件的
算法说明实现，不依赖 Python）。

口径依据：
  - RFC 8701（GREASE）：16 个保留值 0x0a0a / 0x1a1a / ... / 0xfafa，
    判定式 (v >> 8) == (v & 0xff) && (v & 0x0f) == 0x0a。
  - FoxIO JA4 规范：GREASE 值在**任何位置都忽略**；cipher 与 extension
    集合排序后再哈希（因此 Chrome 110+ 的扩展随机置换不影响指纹）。
    计数段仍计入 SCSV(0x00FF) 等非 GREASE 值，但不计 GREASE。
  - Salesforce JA3：同样剥离 GREASE 后取 MD5；但 extension 保持**发送顺序**，
    故 JA3 顺序敏感，只能与单次采样对齐。

技术诚实：ClientHello 含 random / key_share 临时公钥 / GREASE 随机值 / ECH
密文，两次真实 Chrome 握手也不逐字节相同。本模块比对的是**归一化后的结构
化字段与指纹**，不宣称逐字节一致。
"""

from __future__ import annotations

import hashlib

# RFC 8701 的 16 个 GREASE 值。判定式比枚举更稳妥（未来扩位也成立）。
GREASE_VALUES = tuple(0x0A0A + 0x1010 * i for i in range(16))


def is_grease(value: int) -> bool:
    """判断一个 16 位值是否为 GREASE 保留值。"""
    return (value >> 8) == (value & 0xFF) and (value & 0x0F) == 0x0A


def grease_filter(values):
    """剥掉 GREASE 值，返回剩余值（保持原顺序）。"""
    return [v for v in values if not is_grease(v)]


def grease_positions(values):
    """返回 GREASE 出现的**点位序列**（0 基下标），供独立断言。

    GREASE 值本身随机、且 JA3/JA4 哈希里都看不到，但点位（例如「cipher
    第 0 位是 GREASE」「extensions 首尾各一个 GREASE」）是稳定的浏览器特征，
    必须单独比对，否则「全字段一致」会漏掉这一层。
    """
    return [i for i, v in enumerate(values) if is_grease(v)]


def norm_sequence(values):
    """把 GREASE 值替换成字面量标记 "GREASE"，其余转成 4 位小写十六进制。

    产出「值序列」，与 grease_positions() 的「点位序列」配套使用。
    """
    return ["GREASE" if is_grease(v) else "%04x" % v for v in values]


def values_to_hex(values):
    """去 GREASE 后转 4 位小写 hex 列表（保持顺序）。"""
    return ["%04x" % v for v in grease_filter(values)]


# ---------------------------------------------------------------------------
# JA3（Salesforce 口径）
# ---------------------------------------------------------------------------

def ja3_full(legacy_version: int, ciphers, extensions, groups, point_formats) -> str:
    """构造 JA3 完整串（未哈希）。

    格式：SSLVersion,Ciphers,Extensions,EllipticCurves,EllipticCurvePointFormats
    各字段内以 "-" 连接十进制值，字段间以 "," 连接。GREASE 一律剥离。
    """
    def field(values):
        return "-".join(str(v) for v in grease_filter(values))

    return ",".join([
        str(legacy_version),
        field(ciphers),
        field(extensions),
        field(groups),
        field(point_formats),
    ])


def ja3_md5(legacy_version: int, ciphers, extensions, groups, point_formats) -> str:
    full = ja3_full(legacy_version, ciphers, extensions, groups, point_formats)
    return hashlib.md5(full.encode("ascii")).hexdigest()


# ---------------------------------------------------------------------------
# JA4（FoxIO 口径）
# ---------------------------------------------------------------------------

# supported_versions 里的真实 TLS 版本 → JA4 a 段版本码
_VERSION_CODE = {
    0x0304: "13",   # TLS 1.3
    0x0303: "12",   # TLS 1.2
    0x0302: "11",
    0x0301: "10",
    0x0300: "s3",
}


def ja4_a(supported_versions, has_sni: bool, ciphers, extensions, alpn) -> str:
    """JA4 a 段：{t/q}{version}{d/i}{cipher数:02d}{ext数:02d}{alpn首尾}。"""
    real_versions = grease_filter(supported_versions)
    version = _VERSION_CODE.get(max(real_versions), "00") if real_versions else "00"
    sni = "d" if has_sni else "i"
    ncipher = len(grease_filter(ciphers))
    next_ = len(grease_filter(extensions))
    if alpn:
        first = alpn[0]
        alpn_code = (first[0] + first[-1]) if first else "00"
    else:
        alpn_code = "00"
    return "t%s%s%02d%02d%s" % (version, sni, ncipher, next_, alpn_code)


def _sha256_12(text: str) -> str:
    return hashlib.sha256(text.encode("ascii")).hexdigest()[:12]


def ja4_b(ciphers) -> str:
    """JA4 b 段：cipher 去 GREASE 后按 hex 排序、逗号连接、SHA256 前 12 位。"""
    hexes = sorted(values_to_hex(ciphers))
    return _sha256_12(",".join(hexes))


def ja4_c(extensions, sig_algs) -> str:
    """JA4 c 段：扩展去 GREASE + 去 SNI(0000) + 去 ALPN(0010) 后排序，
    用 "_" 接上 signature_algorithms 的**原始顺序**（不排序），SHA256 前 12 位。
    """
    skip = {0x0000, 0x0010}
    ext_hexes = sorted(
        "%04x" % v for v in grease_filter(extensions) if v not in skip
    )
    sig_hexes = values_to_hex(sig_algs)
    return _sha256_12(",".join(ext_hexes) + "_" + ",".join(sig_hexes))


def ja4(supported_versions, has_sni, ciphers, extensions, alpn, sig_algs) -> str:
    return "%s_%s_%s" % (
        ja4_a(supported_versions, has_sni, ciphers, extensions, alpn),
        ja4_b(ciphers),
        ja4_c(extensions, sig_algs),
    )


def ja4_r(supported_versions, has_sni, ciphers, extensions, alpn, sig_algs) -> str:
    """JA4_r：a 段 + 未哈希的排序原始列表（诊断用，人眼可读）。"""
    a = ja4_a(supported_versions, has_sni, ciphers, extensions, alpn)
    b = ",".join(sorted(values_to_hex(ciphers)))
    skip = {0x0000, 0x0010}
    c = ",".join(sorted("%04x" % v for v in grease_filter(extensions) if v not in skip))
    c += "_" + ",".join(values_to_hex(sig_algs))
    return "%s_%s_%s" % (a, b, c)


def ja4_o(supported_versions, has_sni, ciphers, extensions, alpn, sig_algs) -> str:
    """JA4_o：a 段 + 原始顺序（去 GREASE，保留 SNI/ALPN）的列表（诊断用）。

    扩展顺序每次随机置换，此值只用于人工核对与「单次采样」对齐，不作稳定性承诺。
    """
    a = ja4_a(supported_versions, has_sni, ciphers, extensions, alpn)
    b = ",".join(values_to_hex(ciphers))
    c = ",".join(values_to_hex(extensions))
    c += "_" + ",".join(values_to_hex(sig_algs))
    return "%s_%s_%s" % (a, b, c)


def fingerprints(legacy_version, ciphers, extensions, groups, point_formats,
                 supported_versions, has_sni, alpn, sig_algs) -> dict:
    """一次性算出全部指纹字段，供基准固化与候选比对。"""
    return {
        "ja3_full": ja3_full(legacy_version, ciphers, extensions, groups, point_formats),
        "ja3_md5": ja3_md5(legacy_version, ciphers, extensions, groups, point_formats),
        "ja4": ja4(supported_versions, has_sni, ciphers, extensions, alpn, sig_algs),
        "ja4_a": ja4_a(supported_versions, has_sni, ciphers, extensions, alpn),
        "ja4_r": ja4_r(supported_versions, has_sni, ciphers, extensions, alpn, sig_algs),
        "ja4_o": ja4_o(supported_versions, has_sni, ciphers, extensions, alpn, sig_algs),
    }


if __name__ == "__main__":
    # 最小自校验：GREASE 值集合与判定式一致
    assert len(GREASE_VALUES) == 16
    assert all(is_grease(v) for v in GREASE_VALUES)
    assert not is_grease(0x1301) and not is_grease(0x00FF)
    assert grease_positions([0x0A0A, 0x1301, 0x1A1A]) == [0, 2]
    print("fp_norm self-check: OK (GREASE 16 值, 判定式, 点位序列)")
