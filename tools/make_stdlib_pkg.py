#!/usr/bin/env python3
# ===========================================================================
# make_stdlib_pkg.py —— 组装官方标准库包 stdlib.dialpkg
#
# 输入：一个 .diali 接口文件 + 三份预编译静态库（每平台一份 .a）。
# 输出：zip 容器 stdlib.dialpkg，内含
#        manifest.txt   (JSON: name/version/abi/iface/license/targets/sha256)
#        stdlib.diali   (接口签名)
#        lib/<triple>/libdial_std.a   (三平台)
#
# 用法：
#   python3 tools/make_stdlib_pkg.py \
#       --diali stdlib/stdlib.diali \
#       --out  dist-stdlib/stdlib.dialpkg \
#       --version 1.1.0 --abi 2 \
#       x86_64-linux-gnu:dist-stdlib/x86_64-linux-gnu/libdial_std.a \
#       x86_64-pc-windows-gnu:dist-stdlib/x86_64-pc-windows-gnu/libdial_std.a \
#       aarch64-unknown-linux-musl:dist-stdlib/aarch64-unknown-linux-musl/libdial_std.a
# ===========================================================================
import argparse
import hashlib
import json
import os
import sys
import tempfile
import zipfile

PKG_NAME = "stdlib"
IFACE_NAME = "stdlib.diali"
LICENSE = "MIT OR Apache-2.0"


def sha256_of(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser(description="组装 stdlib.dialpkg")
    ap.add_argument("--diali", required=True, help="接口签名文件 .diali")
    ap.add_argument("--out", required=True, help="输出 .dialpkg 路径")
    ap.add_argument("--version", default="1.1.0", help="包版本号（仅展示/核对）")
    ap.add_argument("--abi", type=int, default=2, help="ABI 版本号（兼容性判据）")
    ap.add_argument(
        "libs",
        nargs="+",
        metavar="triple:path",
        help="每平台静态库：<target-triple>:<libdial_std.a 路径>",
    )
    args = ap.parse_args()

    if not os.path.isfile(args.diali):
        print(f"❌ 找不到接口文件: {args.diali}", file=sys.stderr)
        return 1

    targets: dict[str, dict[str, str]] = {}
    sha256: dict[str, str] = {}
    staged: list[tuple[str, str]] = []  # (zip_path, real_path)

    # 接口文件
    iface_rel = IFACE_NAME
    sha256[iface_rel] = sha256_of(args.diali)
    staged.append((iface_rel, args.diali))

    # 三平台静态库
    for spec in args.libs:
        if ":" not in spec:
            print(f"❌ 参数格式应为 <triple>:<path>，收到: {spec}", file=sys.stderr)
            return 1
        triple, libpath = spec.split(":", 1)
        if not os.path.isfile(libpath):
            print(f"❌ 找不到平台 {triple} 的静态库: {libpath}", file=sys.stderr)
            return 1
        in_pkg = f"lib/{triple}/libdial_std.a"
        targets[triple] = {"lib": in_pkg}
        sha256[in_pkg] = sha256_of(libpath)
        staged.append((in_pkg, libpath))

    if not targets:
        print("❌ 至少需要一个平台的静态库", file=sys.stderr)
        return 1

    manifest = {
        "name": PKG_NAME,
        "version": args.version,
        "abi": args.abi,
        "iface": IFACE_NAME,
        "license": LICENSE,
        "targets": targets,
        "sha256": sha256,
    }

    out_dir = os.path.dirname(os.path.abspath(args.out))
    os.makedirs(out_dir, exist_ok=True)

    # 先把 manifest 落到临时文件以便归档
    with tempfile.TemporaryDirectory() as td:
        manifest_path = os.path.join(td, "manifest.txt")
        with open(manifest_path, "w", encoding="utf-8") as f:
            json.dump(manifest, f, indent=2, ensure_ascii=False)
            f.write("\n")

        # 打包：zip 内所有路径用正斜杠，保持确定性顺序
        with zipfile.ZipFile(args.out, "w", zipfile.ZIP_DEFLATED) as zf:
            zf.write(manifest_path, "manifest.txt")
            for zip_path, real_path in staged:
                zf.write(real_path, zip_path)

    print(f"✅ 已生成 {args.out}")
    print(f"   包名:     {PKG_NAME}")
    print(f"   版本:     {args.version}")
    print(f"   ABI:      {args.abi}")
    print(f"   许可:     {LICENSE}")
    print(f"   支持平台: {', '.join(sorted(targets))}")
    print(f"   内含文件:")
    print(f"     manifest.txt")
    print(f"     {IFACE_NAME}")
    for triple in sorted(targets):
        print(f"     lib/{triple}/libdial_std.a")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
