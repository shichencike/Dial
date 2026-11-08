"""浏览器一致性指纹逐字段比对器（主验收，进 CI / 进回归）。

输入：
  --baseline  基准 JSON（chrome_windows_x64.json）
  --candidate 候选 ClientHello 二进制，或已解析的 JSON（clienthello.py 输出）
  --behavior  可选：候选 HTTP 行为 JSON（含 h1 / h2 段）
  --self      基准自比（验收基准自身可复现，期 0 用）

输出：逐行 `name = value`，贴合 W 组 shell grep 断言惯例：
  - 一致：`ja4 = pass`、`cipher_seq_norm = pass` ...
  - 不一致：`xxx = MISMATCH expected=<...> got=<...>`（退出码 1）
  - 豁免：`exemption = <name> = declared`（显式声明，不许含糊）

比对口径（唯一真源 fp_norm.py）：
  - GREASE **值**归一化（随机，不入哈希）、**点位**单独断言。
  - JA4 排序后哈希，跨连接稳定 → **主判据**。
  - JA3 / extension 顺序敏感 → 与「基准录制那一次」对齐，不作跨连接承诺。
  - HTTP 行为段逐字段比对，缺失的候选段打印 `skip = <name> = not_provided`。

    python3 compare_fp.py --baseline chrome_windows_x64.json --self
    python3 compare_fp.py --baseline chrome_windows_x64.json \
        --candidate cand.bin [--behavior behavior.json]
"""

from __future__ import annotations

import argparse
import json
import sys

import clienthello
import fp_norm


def _load_candidate(path: str) -> dict:
    if path.endswith(".json"):
        return json.load(open(path, encoding="utf-8"))
    return clienthello.parse_clienthello(open(path, "rb").read())


def _norm_list(seq):
    """把候选里的 hex 字符串序列归一化：GREASE 值替换为标记。"""
    out = []
    for x in seq:
        v = int(x, 16) if isinstance(x, str) and not x.startswith("GREASE") else None
        out.append("GREASE" if (v is not None and fp_norm.is_grease(v)) else x.lower())
    return out


def emit(name: str, expected, got) -> bool:
    ok = expected == got
    if ok:
        print("%s = pass" % name)
    else:
        print("%s = MISMATCH expected=%s got=%s" % (name, expected, got))
    return ok


def main():
    ap = argparse.ArgumentParser(description="浏览器一致性指纹逐字段比对")
    ap.add_argument("--baseline", required=True)
    ap.add_argument("--candidate", default="")
    ap.add_argument("--behavior", default="")
    ap.add_argument("--self", action="store_true", help="基准自比（离线重放复现）")
    args = ap.parse_args()

    base = json.load(open(args.baseline, encoding="utf-8"))
    bch = base["clienthello"]

    if args.self:
        # 用基准内嵌的原始 record hex 重新解析 → 与基准字段路径逐项比对
        cand = clienthello.parse_clienthello(bytes.fromhex(base["clienthello_record_hex"]))
        behavior = {"h1": base["http_behavior"]["h1"],
                    "h2": base["http_behavior"]["h2"]}
    else:
        if not args.candidate:
            raise SystemExit("需 --candidate 或 --self")
        cand = _load_candidate(args.candidate)
        behavior = json.load(open(args.behavior, encoding="utf-8")) if args.behavior else {}

    all_ok = True

    # ---- 指纹（主判据 JA4；JA3 顺序敏感只对齐单次采样）----
    bf, cf = bch["fingerprints"], cand["fingerprints"]
    for k in ("ja3_full", "ja3_md5", "ja4", "ja4_a", "ja4_r", "ja4_o"):
        all_ok &= emit(k, bf[k], cf[k])

    # ---- TLS 结构字段 ----
    all_ok &= emit("legacy_version", bch["legacy_version"], cand["legacy_version"])
    all_ok &= emit("session_id_len", bch["session_id_len"], cand["session_id_len"])
    all_ok &= emit("compression_methods", bch["compression_methods"], cand["compression_methods"])
    all_ok &= emit("cipher_seq_norm", bch["cipher_suites_norm"], _norm_list(cand["cipher_suites"]))
    all_ok &= emit("cipher_grease_positions", bch["cipher_grease_positions"], cand["cipher_grease_positions"])
    all_ok &= emit("ext_seq_norm", bch["extension_ids_norm"], _norm_list(cand["extension_ids"]))
    all_ok &= emit("ext_order", bch["extension_ids_norm"], _norm_list(cand["extension_ids"]))
    all_ok &= emit("ext_grease_positions", bch["extension_grease_positions"], cand["extension_grease_positions"])
    all_ok &= emit("extension_count", bch["extension_count"], cand["extension_count"])
    all_ok &= emit("groups", bch["supported_groups_norm"], _norm_list(cand["supported_groups"]))
    all_ok &= emit("sig_algs", bch["signature_algorithms"], cand["signature_algorithms"])
    all_ok &= emit("supported_versions", bch["supported_versions_norm"], _norm_list(cand["supported_versions"]))
    all_ok &= emit("ec_point_formats", bch["ec_point_formats"], cand["ec_point_formats"])
    all_ok &= emit("alpn", bch["alpn"], cand["alpn"])
    all_ok &= emit("alpn_first", bch["alpn"][0] if bch["alpn"] else "",
                   cand["alpn"][0] if cand["alpn"] else "")
    all_ok &= emit("has_sni", bch["has_sni"], cand["has_sni"])

    # SCSV 位置：Chrome 144 不发 SCSV(0x00FF)；mbedTLS 默认会追加，故必须断言其缺失
    b_scsv = [i for i, x in enumerate(bch["cipher_suites"]) if x == "00ff"]
    c_scsv = [i for i, x in enumerate(cand["cipher_suites"]) if x == "00ff"]
    all_ok &= emit("scsv_position", b_scsv, c_scsv)

    # ---- HTTP 行为（候选未提供则显式 skip，不静默通过）----
    bh = base["http_behavior"]
    if behavior.get("h1"):
        h1b, h1c = bh["h1"], behavior["h1"]
        all_ok &= emit("h1_header_order", h1b["header_order"], h1c.get("header_order"))
        # 只比对基准显式列出的键：Accept-Encoding 等豁免键不参与逐值断言，
        # 由 exemptions 统一声明（否则「声明豁免」与「逐值断言」自相矛盾）。
        bvals = h1b["header_values"]
        cvals = h1c.get("header_values") or {}
        all_ok &= emit("h1_header_values", bvals, {k: cvals.get(k) for k in bvals})
        all_ok &= emit("h1_ua", h1b["ua"], h1c.get("ua"))
        all_ok &= emit("h1_sec_ch_ua", h1b["sec_ch_ua"], h1c.get("sec_ch_ua"))
    else:
        print("skip = h1_behavior = not_provided")
    if behavior.get("h2"):
        h2b, h2c = bh["h2"], behavior["h2"]
        all_ok &= emit("h2_frame_order", h2b["frame_order"], h2c.get("frame_order"))
        all_ok &= emit("h2_settings", h2b["settings"], h2c.get("settings"))
        all_ok &= emit("h2_window_update", h2b["connection_window_update_increment"],
                       h2c.get("connection_window_update_increment"))
        all_ok &= emit("h2_pseudo_header_order", h2b["pseudo_header_order"],
                       h2c.get("pseudo_header_order"))
        all_ok &= emit("hpack_huffman", h2b["hpack"]["huffman"],
                       (h2c.get("hpack") or {}).get("huffman"))
        all_ok &= emit("hpack_static_index", h2b["hpack"]["static_index"],
                       (h2c.get("hpack") or {}).get("static_index"))
    else:
        print("skip = h2_behavior = not_provided")

    # ---- 豁免项显式声明 ----
    for e in base["exemptions"]:
        print("exemption = %s = declared" % e["name"])

    print("compare_total = %s" % ("pass" if all_ok else "FAIL"))
    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main())
