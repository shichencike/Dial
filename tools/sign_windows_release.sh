#!/usr/bin/env bash
# 一次性自签证书 + 密码学擦除：给 Windows dialc.exe 做 Authenticode 签名。
#
# 流程（一次一密，每个发布版本独立）：
#   1. 生成 RSA-2048 签名私钥 + CSR + 自签证书（CN=dialc-sign-v<版本>，codeSigning EKU，365 天）
#   2. 用该私钥对 dialc.exe 做 Authenticode 签名（signtool，故意不加 RFC3161 时间戳 →
#      签名随证书过期而失效，杜绝旧版证书长期可用）
#   3. 生成一次性 KEK（RSA-2048）→ 用 CMS（AES-256-GCM 内容 + RSA-OAEP 包裹 CEK）
#      把签名私钥封成数字信封（对称/非对称相互交替）
#   4. 覆写并删除全部明文：签名私钥 / CSR / 证书 PEM / PFX / KEK 私钥
#      —— KEK 私钥被毁 ⇒ 密封密文不可解（crypto-shredding 的根本保证）
#   5. 自检：待发布目录只留公钥证书 .cer + 审计密封密文 .cms，禁止任何明文入产物
#
# 输出到 <输出目录>：
#   dialc-code-sign.cer       公钥证书（随 zip 分发 + Release 资产）
#   dialc-sealed-v<版本>.cms  密封密文（仅 Release 审计资产，不放 zip）
# 通过 GITHUB_ENV 导出的变量：DIAL_SIGN_CER（.cer 路径），供 package_release.sh 复制入包。
#
# 用法：bash tools/sign_windows_release.sh <dialc.exe 路径> <版本> <输出目录>
# 退出码：0 = 成功；非 0 = 失败（Fail-Fast，CI 应立即中止）。
# 注意：本脚本不手搓密码学，只用系统 openssl + Windows SDK signtool。

set -euo pipefail
cd "$(dirname "$0")/.." || exit 1

if [ $# -lt 3 ]; then
    echo "用法: bash tools/sign_windows_release.sh <exe> <version> <outdir>" >&2
    exit 2
fi
EXE="$1"
VER="$2"
OUT_DIR="$3"

for tool in openssl; do
    command -v "$tool" >/dev/null 2>&1 || { echo "❌ 缺 $tool，无法签名" >&2; exit 1; }
done
[ -f "$EXE" ] || { echo "❌ 待签名文件不存在：$EXE" >&2; exit 1; }
mkdir -p "$OUT_DIR"
# 生成工作区在临时目录，与待发布产物隔离（明文永不进 OUT_DIR）
WORK="$(mktemp -d /tmp/dialsign_work_XXXX)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
cd "$WORK"

CN="dialc-sign-v${VER}"
echo "== 一次性签名 v${VER} (CN=${CN}) =="

# ---- 1. 签名私钥 + CSR + 自签证书 ----
echo "-- 1) 生成签名私钥 / 证书 --"
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out sign.key 2>/dev/null
openssl pkey -in sign.key -text -noout 2>/dev/null | head -1 | grep -q "2048 bit" \
    || { echo "❌ 签名私钥位宽不是 2048" >&2; exit 1; }
openssl req -new -key sign.key -subj "/CN=${CN}" -out sign.csr 2>/dev/null
printf 'extendedKeyUsage=codeSigning\nkeyUsage=digitalSignature\nbasicConstraints=CA:FALSE\n' > ext.cnf
openssl x509 -req -in sign.csr -signkey sign.key -out cert.pem -sha256 -days 365 \
    -extfile ext.cnf 2>/dev/null
# 核验证书字段（Fail-Fast）
openssl x509 -in cert.pem -noout -subject 2>/dev/null | grep -q "CN = ${CN}" \
    || { echo "❌ 证书 CN 未内嵌版本" >&2; exit 1; }
openssl x509 -in cert.pem -noout -ext extendedKeyUsage 2>/dev/null | grep -qi "Code Signing" \
    || { echo "❌ 证书缺 codeSigning EKU" >&2; exit 1; }
openssl x509 -in cert.pem -outform DER -out "$OUT_DIR/dialc-code-sign.cer" 2>/dev/null
echo "   ✅ 证书就绪（仅公钥 .cer 进产物）"

# ---- 2. Authenticode 签名（有 signtool 才签；本地无则跳过，供门禁复现密码学部分）----
echo "-- 2) Authenticode 签名 dialc.exe --"
PFXPASS="$(openssl rand -hex 24)"
if openssl pkcs12 -export -out sign.pfx -inkey sign.key -in cert.pem \
        -passout "pass:${PFXPASS}" >/dev/null 2>&1; then :; else
    echo "❌ PFX 导出失败" >&2; exit 1;
fi
SIGNED=0
SIGNTOOL=""
for cand in \
    "/c/Program Files (x86)/Windows Kits/10/bin/10.0.26100.0/x64/signtool.exe" \
    "/c/Program Files (x86)/Windows Kits/10/bin/10.0.22621.0/x64/signtool.exe"; do
    [ -f "$cand" ] && { SIGNTOOL="$cand"; break; }
done
if [ -z "$SIGNTOOL" ]; then
    # glob 兜底：SDK 版本随 runner 镜像浮动
    SIGNTOOL=$(ls -1 /c/Program\ Files\ \(x86\)/Windows\ Kits/10/bin/*/x64/signtool.exe 2>/dev/null \
        | sort -V | tail -1 || true)
fi
if [ -n "$SIGNTOOL" ] && command -v "$SIGNTOOL" >/dev/null 2>&1; then
    echo "   signtool: $SIGNTOOL"
    "$SIGNTOOL" sign /f "$(cygpath -w "$WORK/sign.pfx" 2>/dev/null || echo "$WORK/sign.pfx")" \
        /p "$PFXPASS" /fd SHA256 \
        "$(cygpath -w "$EXE" 2>/dev/null || echo "$EXE")" >/dev/null 2>&1 \
        || { echo "❌ signtool 签名失败" >&2; exit 1; }
    SIGNED=1
    echo "   ✅ Authenticode 签名完成（无时间戳，随证书过期失效）"
else
    note="（本地/无 signtool 环境：跳过 PE 签名，仅生成密码学材料供门禁复现）"
    echo "   ⚠️  未找到 signtool，$note"
fi

# ---- 3. 数字信封：AES-256-GCM 加密签名私钥 + RSA-OAEP 包裹 CEK ----
echo "-- 3) 数字信封密封签名私钥 --"
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out kek.key 2>/dev/null
openssl req -new -x509 -key kek.key -subj "/CN=dialc-seal-kek" -days 1 -out kek.crt 2>/dev/null
SEAL="$OUT_DIR/dialc-sealed-v${VER}.cms"
if openssl cms -encrypt -binary -aes-256-gcm -in sign.key -out "$SEAL" \
        -outform DER -recip kek.crt \
        -keyopt rsa_padding_mode:oaep -keyopt rsa_oaep_md:sha256 >/dev/null 2>&1 \
   && [ -s "$SEAL" ]; then
    echo "   ✅ 数字信封 AES-256-GCM + RSA-OAEP"
else
    rm -f "$SEAL"
    openssl cms -encrypt -binary -aes-256-cbc -in sign.key -out "$SEAL" \
        -outform DER -recip kek.crt \
        -keyopt rsa_padding_mode:oaep -keyopt rsa_oaep_md:sha256 2>/dev/null \
        || { echo "❌ 数字信封加密失败" >&2; exit 1; }
    echo "   ✅ 数字信封 AES-256-CBC + RSA-OAEP（GCM 不可用时的回退）"
fi

# ---- 4. 擦除全部明文（crypto-shredding）----
echo "-- 4) 擦除明文密钥 --"
shred_file() {
    local f="$1"
    [ -e "$f" ] || return 0
    if command -v shred >/dev/null 2>&1; then
        shred -u "$f" 2>/dev/null || { dd if=/dev/urandom of="$f" bs=1024 count=1 conv=notrunc 2>/dev/null; rm -f "$f"; }
    else
        # Windows runner 可能无 shred：随机覆写后删除（尽力而为；不可恢复性由 KEK 销毁保证）
        dd if=/dev/urandom of="$f" bs=1024 count=1 conv=notrunc 2>/dev/null || true
        rm -f "$f"
    fi
}
for f in sign.key sign.csr cert.pem sign.pfx ext.cnf kek.key kek.crt; do shred_file "$f"; done
# Windows NTFS 空闲空间清理（尽力而为，SSD 不保证物理擦除）
if command -v cipher.exe >/dev/null 2>&1 || [ -f /c/Windows/System32/cipher.exe ]; then
    CIPHER=$(command -v cipher.exe 2>/dev/null || echo /c/Windows/System32/cipher.exe)
    "$CIPHER" /w:"$(cygpath -w "$WORK" 2>/dev/null || echo "$WORK")" >/dev/null 2>&1 || true
    echo "   ✅ cipher /w 清理 NTFS 空闲空间（尽力而为）"
fi
# 断言明文确实不在工作区
LEAK=$(find "$WORK" -type f 2>/dev/null | head -1)
[ -z "$LEAK" ] && echo "   ✅ 明文密钥已全部销毁" || { echo "❌ 明文残留：$LEAK" >&2; exit 1; }

# ---- 5. 资产卫生自检 ----
echo "-- 5) 产物自检 --"
BAD=$(find "$OUT_DIR" -type f \( -name '*.key' -o -name '*.pfx' -o -name '*.p12' \
        -o -name '*.csr' -o -name '*.pem' -o -name 'kek.*' \) 2>/dev/null)
[ -z "$BAD" ] || { echo "❌ 待发布目录含敏感明文：$BAD" >&2; exit 1; }
[ -s "$OUT_DIR/dialc-code-sign.cer" ] || { echo "❌ 公钥证书缺失" >&2; exit 1; }
[ -s "$SEAL" ] || { echo "❌ 密封密文缺失" >&2; exit 1; }
# 密文不含明文私钥字样
if grep -aq 'PRIVATE KEY' "$SEAL"; then echo "❌ 密文含明文私钥" >&2; exit 1; fi
echo "   ✅ 产物仅含 .cer 公钥证书与 .cms 密封密文"

# 通过 GITHUB_ENV 告知后续打包步骤 .cer 位置（用绝对路径，避免跨 step 工作目录差异）
if [ -n "${GITHUB_ENV:-}" ]; then
    CER_ABS="$(cd "$OUT_DIR" && pwd)/dialc-code-sign.cer"
    echo "DIAL_SIGN_CER=$CER_ABS" >> "$GITHUB_ENV"
fi

echo "== 一次性签名完成 ✅ (signed_pe=${SIGNED}) =="
exit 0
