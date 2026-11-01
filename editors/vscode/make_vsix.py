#!/usr/bin/env python3
"""把 editors/vscode 打包成合规的 .vsix 安装包。

vsce 不可用时的回退方案：手写 OPC（Open Packaging Conventions）zip——
VS Code 的 `code --install-extension dial-*.vsix` 侧载接受这种包，只要根目录含
`extension.vsixmanifest` 与 `[Content_Types].xml` 即可，无需联网、无需 npm。

用法：
    python3 editors/vscode/make_vsix.py [输出路径]
默认输出到 editors/vscode/dial-<version>.vsix
"""
import json
import os
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))


def read_package() -> dict:
    with open(os.path.join(HERE, "package.json"), "r", encoding="utf-8") as f:
        return json.load(f)


def build_manifest(pkg: dict) -> str:
    desc = pkg.get("description", "Dial language support")
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011">
    <Metadata>
        <Identity Language="en-US" Id="{pkg.get('name', 'dial')}" Version="{pkg.get('version', '0.0.0')}" Publisher="{pkg.get('publisher', 'dial-lang')}" />
        <DisplayName>{pkg.get('displayName', 'Dial')}</DisplayName>
        <Description xml:space="preserve">{desc}</Description>
        <Tags>dial,language,compiler,lsp</Tags>
        <License>MIT</License>
    </Metadata>
    <Installation>
        <InstallationTarget Id="Microsoft.VisualStudio.Code" />
    </Installation>
    <Dependencies />
    <Assets>
        <Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="package.json" Addressable="true" />
    </Assets>
</PackageManifest>
"""


def build_content_types() -> str:
    # 列出包里会出现的扩展名；VS Code 用这份表校验 MIME。
    return """<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="vsixmanifest" ContentType="text/xml" />
  <Default Extension="json" ContentType="application/json" />
  <Default Extension="js" ContentType="application/javascript" />
  <Default Extension="md" ContentType="text/markdown" />
</Types>
"""


def main():
    pkg = read_package()
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        HERE, f"dial-{pkg.get('version', '0.0.0')}.vsix"
    )

    manifest = build_manifest(pkg)
    content_types = build_content_types()

    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        # 1) 两个 OPC 必需文件放根目录
        z.writestr("extension.vsixmanifest", manifest)
        z.writestr("[Content_Types].xml", content_types)
        # 2) 递归打包 editors/vscode 下的全部真实文件（排除构建脚本与已生成的 .vsix）
        exclude = {"build.sh", "make_vsix.py"}
        for root, _dirs, files in os.walk(HERE):
            for name in sorted(files):
                full = os.path.join(root, name)
                if full == out or name.endswith(".vsix") or name in exclude:
                    continue
                rel = os.path.relpath(full, HERE)
                with open(full, "rb") as fh:
                    z.writestr(rel, fh.read())
    size = os.path.getsize(out)
    print(f"✅ 已生成 {out} ({size} 字节)")
    print(f"   版本 {pkg.get('version')} | publisher={pkg.get('publisher')}")


if __name__ == "__main__":
    main()
