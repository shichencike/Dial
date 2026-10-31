#!/usr/bin/env python3
"""通过 GitHub REST API（Git Data API）推送本地提交。

为什么不用 `git push`：本沙箱到 `github.com:443` 的链路不可靠（TCP 握手超时），
但 `api.github.com` 稳定可达。因此改走 API：上传 blob -> 建 tree -> 建 commit -> 更新 ref。

**目标：逐个复刻本地提交的 SHA**（作者/邮箱/时间/时区/消息全部按原始值提交），
因此推送完成后本地与远端是同一批对象，`git status` 依然干净、历史完全一致。

用法：
    GITHUB_TOKEN=xxx python3 tools/api_push.py                # 推送 main
    GITHUB_TOKEN=xxx python3 tools/api_push.py --dry-run      # 只比对，不写入
"""

import base64
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timedelta, timezone

API = "https://api.github.com"
REPO = "shichencike/Dial"
BRANCH = "main"
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

TOKEN = os.environ.get("GITHUB_TOKEN", "").strip()
DRY = "--dry-run" in sys.argv


def git(*args, binary=False):
    out = subprocess.run(["git", "-C", ROOT, *args], check=True,
                         stdout=subprocess.PIPE).stdout
    return out if binary else out.decode("utf-8", "replace")


def api(method, path, payload=None, expect=(200, 201)):
    url = path if path.startswith("http") else API + path
    body = json.dumps(payload).encode() if payload is not None else None
    cmd = ["curl", "-sS", "--max-time", "120", "-X", method, url,
           "-H", "Authorization: Bearer " + TOKEN,
           "-H", "Accept: application/vnd.github+json",
           "-H", "X-GitHub-Api-Version: 2022-11-28",
           "-H", "User-Agent: dialc-release-bot"]
    if body is not None:
        # 必须显式用 `-d @-`，否则 curl 不会把 stdin 当作请求体（会得到空 body 的 422）
        cmd += ["--data-binary", "@-"]
    req = subprocess.run(cmd, input=body, stdout=subprocess.PIPE)
    try:
        d = json.loads(req.stdout.decode("utf-8", "replace"))
    except Exception:
        raise SystemExit("非 JSON 响应: %r" % req.stdout[:400])
    if isinstance(d, dict) and d.get("message") and "sha" not in d:
        raise SystemExit("API %s %s 失败: %s" % (method, path, d.get("message")))
    if req.returncode != 0:
        raise SystemExit("API %s %s HTTP %d" % (method, path, req.returncode))
    return d


def iso(raw_epoch_tz):
    """git raw 日期（'1793393278 +0800'）-> ISO8601（GitHub API 需要）。"""
    ts, tz = raw_epoch_tz.split()
    sign = 1 if tz[0] == "+" else -1
    off = timedelta(hours=int(tz[1:3]), minutes=int(tz[3:5])) * sign
    dt = datetime.fromtimestamp(int(ts), timezone(off))
    return dt.isoformat()


def fmt(tree_line):
    """`git ls-tree` 一行 -> (mode, type, sha, path)"""
    meta, path = tree_line.split("\t", 1)
    mode, typ, sha = meta.split()
    return mode, typ, sha, path


def main():
    if not TOKEN:
        raise SystemExit("缺少 GITHUB_TOKEN 环境变量")

    head = git("rev-parse", "HEAD").strip()
    remote = api("GET", "/repos/%s/git/ref/heads/%s" % (REPO, BRANCH))
    remote_sha = remote["object"]["sha"]
    print("远端 %s = %s" % (BRANCH, remote_sha[:10]))

    # 远端有、本地没有的提交 -> 只能快进
    if remote_sha not in git("rev-list", head):
        raise SystemExit("远端与本地分叉（非快进），需人工处理")

    commits = git("rev-list", "--reverse", "%s..%s" % (remote_sha, head)).split()
    if not commits:
        print("没有需要推送的提交 ✅")
        return 0
    print("待推送 %d 个提交\n" % len(commits))

    blob_cache = set()
    prev_tree = git("rev-parse", "%s^{tree}" % remote_sha).strip()

    for c in commits:
        parent = git("rev-parse", "%s^" % c).strip()
        tree = git("rev-parse", "%s^{tree}" % c).strip()
        # 消息必须**逐字节**取自 commit 对象：GitHub 的 API 是**原样存储**，
        # 既不会补结尾换行、也不会去掉。因此这里不能用 `git log --format=%B`
        # （它会多补一个换行），也不能 rstrip —— 否则算出的 commit SHA 与本地不一致。
        raw = git("cat-file", "commit", c, binary=True)
        message = raw.split(b"\n\n", 1)[1].decode("utf-8")
        author = git("log", "-1", "--format=%an", c).strip()
        aemail = git("log", "-1", "--format=%ae", c).strip()
        adate = iso(git("log", "-1", "--format=%ad", "--date=raw", c).strip())
        cname = git("log", "-1", "--format=%cn", c).strip()
        cemail = git("log", "-1", "--format=%ce", c).strip()
        cdate = iso(git("log", "-1", "--format=%cd", "--date=raw", c).strip())

        # 相对父提交变化的路径（新增/修改/删除）
        changed = git("diff-tree", "--no-commit-id", "--name-only", "-r", c).split("\n")
        changed = [p for p in changed if p]
        entries = []
        for path in changed:
            line = git("ls-tree", c, "--", path).strip()
            if not line:
                # 该路径在此提交中已被删除：GitHub 的 tree API 用 `sha: null` 表示删除
                # （不能填全零 SHA，那样会被判成非法 blob）。
                entries.append({"path": path, "mode": "100644", "type": "blob", "sha": None})
                continue
            mode, typ, sha, p = fmt(line)
            if sha not in blob_cache:
                data = git("cat-file", "blob", sha, binary=True)
                r = api("POST", "/repos/%s/git/blobs" % REPO, {
                    "content": base64.b64encode(data).decode(),
                    "encoding": "base64"})
                if r["sha"] != sha:
                    raise SystemExit("blob SHA 不符：本地 %s 远端 %s" % (sha, r["sha"]))
                blob_cache.add(sha)
            entries.append({"path": path, "mode": mode, "type": typ, "sha": sha})

        if DRY:
            print("  [dry-run] %s  tree=%s  变更 %d 个文件" % (c[:10], tree[:10], len(entries)))
            prev_tree = tree
            continue

        r = api("POST", "/repos/%s/git/trees" % REPO,
                {"base_tree": prev_tree, "tree": entries})
        if r["sha"] != tree:
            raise SystemExit("tree SHA 不符：本地 %s 远端 %s" % (tree, r["sha"]))

        r = api("POST", "/repos/%s/git/commits" % REPO, {
            "message": message,
            "tree": tree,
            "parents": [parent],
            "author": {"name": author, "email": aemail, "date": adate},
            "committer": {"name": cname, "email": cemail, "date": cdate},
        })
        if r["sha"] != c:
            raise SystemExit("commit SHA 不符：本地 %s 远端 %s\n  远端消息: %r"
                             % (c, r["sha"], r.get("commit", {}).get("message")))
        print("  ✅ %s  %s" % (c[:10], message.strip().split("\n")[0][:60]))
        prev_tree = tree

    if DRY:
        print("\n[dry-run] 未写入任何内容")
        return 0

    api("PATCH", "/repos/%s/git/refs/heads/%s" % (REPO, BRANCH),
        {"sha": head, "force": False})
    print("\n✅ 已更新 %s -> %s" % (BRANCH, head[:10]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
