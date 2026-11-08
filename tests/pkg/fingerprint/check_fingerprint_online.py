"""可选在线指纹校验（辅助验收，默认不跑、不进 CI）。

用户裁决的验收方式是「两者结合」：本地录制比对为主（进 CI、进回归），
本脚本为辅——把 dialc 客户端指向公网指纹回显服务，**只打印**服务返回的
JA3 / JA4 供人工核对。

设计约束（为什么不默认跑）：
  - 公网服务返回值随服务端版本变化，可能限流或下线；
  - 需要公网出口，破坏「离线可复现」与 CI 确定性；
  - 因此：必须显式 --online 且提供 DIAL_FP_ONLINE_URL 才执行；未开启时
    直接退出 0（W 组用 wskip 标注，不视为失败）。

用法：
    DIAL_FP_ONLINE_URL=https://<fingerprint-echo>/json \
      python3 check_fingerprint_online.py --online --base http://127.0.0.1:PORT
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request


def main():
    ap = argparse.ArgumentParser(description="可选在线指纹校验")
    ap.add_argument("--online", action="store_true", help="显式开启（默认关闭）")
    ap.add_argument("--url", default=os.environ.get("DIAL_FP_ONLINE_URL", ""),
                    help="指纹回显服务 URL（或环境变量 DIAL_FP_ONLINE_URL）")
    ap.add_argument("--timeout", type=float, default=8.0)
    args = ap.parse_args()

    if not args.online:
        print("online_check = skipped (需 --online 显式开启；默认不跑、不进 CI)")
        return 0
    if not args.url:
        print("online_check = skipped (未提供 DIAL_FP_ONLINE_URL)")
        return 0

    try:
        with urllib.request.urlopen(args.url, timeout=args.timeout) as r:
            body = r.read().decode("utf-8", "replace")
    except Exception as e:
        # 在线校验失败不影响主验收
        print("online_check = unavailable (%s)" % e)
        return 0

    try:
        data = json.loads(body)
    except json.JSONDecodeError:
        print("online_check = non_json_response")
        return 0

    # 只打印，不写文件、不参与断言
    for key in ("ja3", "ja3_hash", "ja4", "akamai", "http2"):
        if key in data:
            print("online_%s = %s" % (key, data[key]))
    print("online_check = done (仅供人工核对，不参与断言)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
