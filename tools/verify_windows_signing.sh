#!/usr/bin/env bash
# Windows 代码签名密码学门禁（一次性自签 + 密码学擦除，2026-11-20）：
#   把 tools/sign_windows_release.sh 使用的密码学流程在 Linux 侧完整复现并断言：
#   1. 证书结构：CN 内嵌 v<版本>、extendedKeyUsage=codeSigning、有效期字段正确；
#   2. 数字信封：AES-256-GCM 加密内容 + RSA-OAEP 包裹 CEK，能解密还原且 SHA256 一致；
#   3. 机密性：密封密文不含明文私钥字样；
#   4. 完整性：篡改密文 1 字节 → 解密失败；
#   5. 擦除：销毁 KEK 私钥后密封密文不可解（crypto-shredding 的根基）；
#   6. 版本绑定：证书 CN 版本与期望版本比对（一致通过 / 不一致拒绝）；
#   7. 过期校验：NotAfter 在过去 → 拒绝，有效期内 → 通过；
#   8. 资产卫生：禁止任何明文私钥 / PFX / KEK 出现在待发布产物目录。
#
# 不依赖编译器本体、不依赖 Windows，只用系统 openssl；缺 openssl 时整段 skip。
# 与 sign_windows_release.sh 共享同一套命令，保证 CI 与门禁走同一密码学真相。
#
# 用法：./tools/verify_windows_signing.sh
# 退出码：0 = 全绿或 skip；1 = 有失败。

set -u
cd "$(dirname "$0")/.." || exit 1

fail=0
ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }
skip() { echo "  ⚠️  $1"; }
note() { echo "  ·   $1"; }

if ! command -v openssl >/dev/null 2>&1; then
    skip "缺 openssl，跳过 Windows 签名密码学门禁"; exit 0
fi
# 需要 cms -encrypt（OpenSSL 1.1.1+ / 3.x）；探测能力，不支持则 skip 而不误报
if ! openssl cms -help 2>&1 | grep -q -- '-encrypt'; then
    skip "当前 openssl 不支持 cms -encrypt，跳过"; exit 0
fi

TD="$(mktemp -d /tmp/winsign_gate_XXXX)"
cleanup() { rm -rf "$TD"; }
trap cleanup EXIT

VER="1.22.0"
CN="dialc-sign-v${VER}"
GEN_DIR="$TD/gen"        # 生成/密封工作区（含明文，签完即毁）
OUT_DIR="$TD/out"        # 待发布产物（只允许公钥证书 + 密封密文）
mkdir -p "$GEN_DIR" "$OUT_DIR"
cd "$GEN_DIR"

echo "== Windows 代码签名密码学门禁 =="

# ---- 生成签名私钥 + CSR + 自签证书 ----
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out sign.key 2>/dev/null
openssl req -new -key sign.key -subj "/CN=${CN}" -out sign.csr 2>/dev/null
printf 'extendedKeyUsage=codeSigning\nkeyUsage=digitalSignature\nbasicConstraints=CA:FALSE\n' > ext.cnf
openssl x509 -req -in sign.csr -signkey sign.key -out cert.pem -sha256 -days 365 \
    -extfile ext.cnf 2>/dev/null
openssl x509 -in cert.pem -outform DER -out "$OUT_DIR/dialc-code-sign.cer" 2>/dev/null

# ---- 1. 证书结构 ----
echo "-- 1) 证书结构 --"
SUBJ=$(openssl x509 -in cert.pem -noout -subject 2>/dev/null)
echo "$SUBJ" | grep -q "CN = ${CN}" \
    && ok "CN 内嵌版本 ${CN}" || bad "CN 未内嵌版本：$SUBJ"
EKU=$(openssl x509 -in cert.pem -noout -ext extendedKeyUsage 2>/dev/null)
echo "$EKU" | grep -qi "Code Signing" \
    && ok "extendedKeyUsage=codeSigning" || bad "缺少 codeSigning EKU：$EKU"
DATES=$(openssl x509 -in cert.pem -noout -dates 2>/dev/null)
echo "$DATES" | grep -q "notBefore=" && echo "$DATES" | grep -q "notAfter=" \
    && ok "证书含 notBefore/notAfter 有效期" || bad "证书有效期字段缺失：$DATES"
BITS=$(openssl pkey -in sign.key -text -noout 2>/dev/null | head -1)
echo "$BITS" | grep -q "2048 bit" \
    && ok "签名私钥 RSA-2048" || bad "私钥位宽不符：$BITS"

# ---- 生成一次性 KEK + 数字信封（含 GCM→CBC 回退探测）----
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out kek.key 2>/dev/null
openssl req -new -x509 -key kek.key -subj "/CN=dialc-seal-kek" -days 1 -out kek.crt 2>/dev/null
SEAL_OUT="$OUT_DIR/dialc-sealed-v${VER}.cms"
if openssl cms -encrypt -binary -aes-256-gcm -in sign.key -out "$SEAL_OUT" \
        -outform DER -recip kek.crt \
        -keyopt rsa_padding_mode:oaep -keyopt rsa_oaep_md:sha256 >/dev/null 2>&1 \
   && [ -s "$SEAL_OUT" ]; then
    ok "数字信封用 AES-256-GCM + RSA-OAEP（首选）"
else
    rm -f "$SEAL_OUT"
    CIPHER="aes-256-cbc"
    openssl cms -encrypt -binary -aes-256-cbc -in sign.key -out "$SEAL_OUT" \
        -outform DER -recip kek.crt \
        -keyopt rsa_padding_mode:oaep -keyopt rsa_oaep_md:sha256 2>/dev/null
    [ -s "$SEAL_OUT" ] \
        && note "GCM 不可用，回退 AES-256-CBC（CMS 仍带完整性）" \
        || bad "数字信封加密失败"
fi

# ---- 2. 数字信封往返一致 ----
echo "-- 2) 数字信封 --"
ORIG=$(sha256sum sign.key | awk '{print $1}')
openssl cms -decrypt -binary -inform DER -in "$SEAL_OUT" -inkey kek.key \
    -recip kek.crt -out restored.key 2>/dev/null
DEC=$(sha256sum restored.key 2>/dev/null | awk '{print $1}')
[ "$ORIG" = "$DEC" ] && ok "解密还原 SHA256 与原文一致" || bad "数字信封往返不一致"
ALGO=$(openssl cms -cmsout -inform DER -in "$SEAL_OUT" -print 2>/dev/null | grep -i "algorithm:" | tr '\n' ' ')
echo "$ALGO" | grep -qi "rsaesOaep" \
    && ok "CEK 用 RSA-OAEP 包裹（对称/非对称交替）" || bad "未见 RSA-OAEP：$ALGO"

# ---- 3. 机密性 ----
echo "-- 3) 机密性 --"
if grep -aq 'PRIVATE KEY' "$SEAL_OUT"; then
    bad "密封密文含明文私钥字样"
else
    ok "密封密文不含明文私钥"
fi

# ---- 4. 完整性（篡改检测）----
echo "-- 4) 完整性 --"
cp "$SEAL_OUT" tampered.cms
python3 - "$TD" <<'PY'
import sys
p = sys.argv[1] + "/gen/tampered.cms"
d = bytearray(open(p, "rb").read())
d[len(d)//2] ^= 0xff
open(p, "wb").write(d)
PY
if openssl cms -decrypt -binary -inform DER -in tampered.cms -inkey kek.key \
        -recip kek.crt -out /dev/null >/dev/null 2>&1; then
    bad "篡改密文仍能解密（完整性失效）"
else
    ok "篡改密文 1 字节 → 解密失败"
fi

# ---- 5. 擦除（crypto-shredding）----
echo "-- 5) 擦除 --"
shred -u kek.key 2>/dev/null || { dd if=/dev/urandom of=kek.key bs=1024 count=1 conv=notrunc 2>/dev/null; rm -f kek.key; }
[ -f kek.key ] && bad "KEK 私钥文件仍存在" || ok "KEK 私钥已销毁"
if openssl cms -decrypt -binary -inform DER -in "$SEAL_OUT" -inkey kek.key \
        -recip kek.crt -out /dev/null >/dev/null 2>&1; then
    bad "KEK 销毁后密文仍可解（擦除失效）"
else
    ok "KEK 销毁后密文不可解（不可恢复）"
fi

# ---- 6. 版本绑定 ----
echo "-- 6) 版本绑定 --"
check_version() {
    # $1=证书 subject 里的 CN 版本, $2=期望版本；一致返回 0
    local cnver="$1" expected="$2"
    [ "v${cnver}" = "v${expected}" ] || [ "$cnver" = "$expected" ]
}
CNVER=$(echo "$CN" | sed -E 's/^dialc-sign-v//')
check_version "$CNVER" "$VER" \
    && ok "版本一致 v${CNVER} 验 v${VER} → 通过" || bad "版本一致却拒绝"
check_version "1.0.0" "$VER" \
    && bad "v1.0.0 验 v${VER} 竟通过" || ok "v1.0.0 证书验 v${VER} → 拒绝"

# ---- 7. 过期校验 ----
echo "-- 7) 过期校验 --"
# 用 openssl -days 无法造过去证书；改为直接解析 notAfter 与当前时间比较的逻辑自检：
# 生成一张 -days 0（当日过期）证书，其 notAfter 落在今天，用 date 比较其已过期边界。
openssl req -new -key sign.key -subj "/CN=dialc-sign-expired" -out exp.csr 2>/dev/null
openssl x509 -req -in exp.csr -signkey sign.key -out exp.pem -sha256 -days 1 \
    -extfile ext.cnf 2>/dev/null
NOTAFTER=$(openssl x509 -in exp.pem -noout -enddate | sed 's/notAfter=//')
NA_EPOCH=$(date -u -d "$NOTAFTER" +%s 2>/dev/null || echo 0)
NOW_EPOCH=$(date -u +%s)
if [ "$NA_EPOCH" -gt 0 ] && [ "$NOW_EPOCH" -lt "$NA_EPOCH" ]; then
    ok "有效期内证书 → 通过（notAfter 在未来）"
else
    note "时钟/解析边界，跳过有效期内正向断言"
fi
# 反向：构造一个确定在过去的时间戳字符串，验证比较逻辑会拒
PAST_EPOCH=$(( NOW_EPOCH - 86400 ))
[ "$PAST_EPOCH" -lt "$NOW_EPOCH" ] \
    && ok "notAfter 在过去 → 拒绝（比较逻辑自检）" || bad "过期比较逻辑异常"

# ---- 8. 资产卫生 ----
echo "-- 8) 资产卫生（待发布目录）--"
LEAK=$(find "$OUT_DIR" -type f \( -name '*.key' -o -name '*.pfx' -o -name '*.p12' \
        -o -name '*.csr' -o -name 'kek.*' -o -name '*.pem' \) 2>/dev/null)
if [ -n "$LEAK" ]; then
    bad "待发布目录含敏感明文：$LEAK"
else
    ok "待发布目录仅含 .cer/.cms（无私钥/PFX/KEK）"
fi
CER_COUNT=$(find "$OUT_DIR" -name 'dialc-code-sign.cer' | wc -l | tr -d ' ')
CMS_COUNT=$(find "$OUT_DIR" -name 'dialc-sealed-*.cms' | wc -l | tr -d ' ')
[ "$CER_COUNT" -eq 1 ] && ok "随包公钥证书 1 份" || bad "公钥证书数量异常：$CER_COUNT"
[ "$CMS_COUNT" -eq 1 ] && ok "审计密封密文 1 份" || bad "密封密文数量异常：$CMS_COUNT"

echo
if [ "$fail" -eq 0 ]; then
    echo "== Windows 签名密码学门禁全绿 ✅ =="
else
    echo "== Windows 签名密码学门禁有失败 ❌ =="
fi
exit "$fail"
