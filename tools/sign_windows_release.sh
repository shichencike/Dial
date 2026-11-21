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
# Git Bash（Windows runner）默认会把以 `/` 开头的参数当 POSIX 路径改写成
# `C:/Program Files/Git/...`。本脚本大量使用此类参数：openssl 的 `-subj /CN=...`、
# signtool 的 `/f /p /fd`、cipher 的 `/w:`。不关闭转换会令这些命令收到被篡改的
# 参数而静默失败（正是首次在 Windows CI 跑到签名步时 exit 1 的根因）。
# MSYS2_ARG_CONV_EXCL='*' 覆盖 MSYS2，MSYS_NO_PATHCONV=1 覆盖旧版 Git Bash。
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*'
# ERR 陷阱：任何未捕获的失败都打印行号，避免 2>/dev/null 把错误吞成无声退出。
trap 'rc=$?; echo "❌ 签名脚本失败：第 ${LINENO} 行，退出码 ${rc}" >&2' ERR
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
# OUT_DIR / EXE 都可能是相对路径（CI 传 `dist-sign` / `target/.../dialc.exe`）。
# 下面会 cd 到临时工作区 $WORK，相对路径届时会错误地落到 $WORK 内。故在进入
# 临时目录前先把两者规范为绝对路径——这是首轮 Windows CI 在 "写 .cer" 处
# exit 1 的真正根因（本地测试用绝对 /tmp 路径恰好掩盖了它）。
EXE="$(cd "$(dirname "$EXE")" && pwd)/$(basename "$EXE")"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
# 生成工作区在临时目录，与待发布产物隔离（明文永不进 OUT_DIR）
WORK="$(mktemp -d /tmp/dialsign_work_XXXXXX)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
cd "$WORK"
# 探测 openssl 是否可用；把版本打到 CI 日志，便于定位 runner 差异。
echo "   openssl: $(openssl version 2>&1 || echo 不可用)"

# Git Bash 里 `pwd` 返回 MSYS 路径（/d/a/...），但 Git-for-Windows 自带的 openssl
# 是**原生 Windows 程序**，不认 MSYS 路径——给它传绝对 -out 路径会 "cannot open
# file" 而 exit 1（本机 Linux 用 /workspace/... 原生可认，恰好掩盖此差异）。
# 用 cygpath -m 转成 D:/... 混合路径；非 Windows（无 cygpath）原样返回。
native_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -m "$1" 2>/dev/null || echo "$1"
    else
        echo "$1"
    fi
}

CN="dialc-sign-v${VER}"
echo "== 一次性签名 v${VER} (CN=${CN}) =="

# ---- 1. 签名私钥 + CSR + 自签证书 ----
echo "-- 1) 生成签名私钥 / 证书 --"
# 不吞 stderr：任何 openssl 失败都要在 CI 日志里可见（原本 2>/dev/null 令首轮
# Windows 失败只剩一行 exit 1，无法定位）。genpkey 的 RSA 进度点会打到日志，
# 换来对首次 Windows 运行的完全可诊断性——这是值得的。
if ! openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out sign.key; then
    echo "❌ 签名私钥 genpkey 失败（openssl=$(openssl version 2>&1)）" >&2
    exit 1
fi
BITS="$(openssl pkey -in sign.key -text -noout 2>/dev/null | head -1)"
echo "$BITS" | grep -q "2048 bit" \
    || { echo "❌ 签名私钥位宽不是 2048: $BITS" >&2; exit 1; }
openssl req -new -key sign.key -subj "/CN=${CN}" -out sign.csr 2>/dev/null \
    || { echo "❌ 生成 CSR 失败" >&2; exit 1; }
printf 'extendedKeyUsage=codeSigning\nkeyUsage=digitalSignature\nbasicConstraints=CA:FALSE\n' > ext.cnf
openssl x509 -req -in sign.csr -signkey sign.key -out cert.pem -sha256 -days 365 \
    -extfile ext.cnf 2>/dev/null
# 核验证书字段（Fail-Fast）。不同 OpenSSL 版本/配置对 -subject / -ext 的默认
# 输出格式不一致（"CN = x" / "CN=x" / RFC2253；EKU "Code Signing" / "codeSigning"），
# 故只做大小写不敏感的**固定串**匹配：CN 值 `dialc-sign-v<版本>` 出现在 subject 中
# 即可，EKU 用宽松正则 code.?sign。实际值打进错误，便于定位 runner 差异。
SUBJ="$(openssl x509 -in cert.pem -noout -subject 2>/dev/null)"
if ! echo "$SUBJ" | grep -qiF "$CN"; then
    echo "❌ 证书 CN 未内嵌版本：实际=[$SUBJ]，期望含=[$CN]" >&2
    exit 1
fi
EKU="$(openssl x509 -in cert.pem -noout -ext extendedKeyUsage 2>/dev/null)"
if ! echo "$EKU" | grep -qiE 'code.?sign'; then
    echo "❌ 证书缺 codeSigning EKU：实际=[$EKU]" >&2
    exit 1
fi
openssl x509 -in cert.pem -outform DER -out "$(native_path "$OUT_DIR/dialc-code-sign.cer")" 2>/dev/null \
    || { echo "❌ 写公钥证书 .cer 失败" >&2; exit 1; }
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
SEAL_NATIVE="$(native_path "$SEAL")"
if openssl cms -encrypt -binary -aes-256-gcm -in sign.key -out "$SEAL_NATIVE" \
        -outform DER -recip kek.crt \
        -keyopt rsa_padding_mode:oaep -keyopt rsa_oaep_md:sha256 >/dev/null 2>&1 \
   && [ -s "$SEAL" ]; then
    echo "   ✅ 数字信封 AES-256-GCM + RSA-OAEP"
else
    rm -f "$SEAL"
    openssl cms -encrypt -binary -aes-256-cbc -in sign.key -out "$SEAL_NATIVE" \
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
# Windows NTFS 空闲空间清理（尽力而为，SSD 不保证物理擦除）。
# 注意：cipher /w 会覆写**整个卷**的空闲空间，在 CI runner 的大容量磁盘上可能
# 耗时数十分钟甚至挂死（本发布流程中 tag CI 第 12 步卡死 >18 分钟即根因在此）。
# 明文销毁的根本保证是上方 shred_file 逐文件覆写 + KEK 私钥销毁（密文不可解），
# cipher /w 只是额外的 NTFS 卫生措施，非安全关键，故默认跳过；仅当显式设置
# DIAL_CIPHER_WIPE=1 时才执行（如需在专用慢速环境做深度清理）。
if [ "${DIAL_CIPHER_WIPE:-0}" = "1" ] \
   && { command -v cipher.exe >/dev/null 2>&1 || [ -f /c/Windows/System32/cipher.exe ]; }; then
    CIPHER=$(command -v cipher.exe 2>/dev/null || echo /c/Windows/System32/cipher.exe)
    "$CIPHER" /w:"$(cygpath -w "$WORK" 2>/dev/null || echo "$WORK")" >/dev/null 2>&1 || true
    echo "   ✅ cipher /w 清理 NTFS 空闲空间（尽力而为）"
else
    echo "   ⏭️  跳过 cipher /w（默认；如需深度清理设 DIAL_CIPHER_WIPE=1）"
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
