#!/bin/bash
# 钉钉自定义机器人通知（加签模式）
# 用法: notify-dingtalk.sh <标题> <文本>
# 配置: 同目录 notify.env (DINGTALK_WEBHOOK_URL / DINGTALK_SECRET)

set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$DIR/notify.env"

export DINGTALK_TITLE="${1:-快照轮转}"
export DINGTALK_TEXT="${2:-无内容}"

python3 - <<'EOF'
import json, os, base64, hashlib, hmac, time, urllib.request, urllib.parse

webhook = os.environ['DINGTALK_WEBHOOK_URL']
secret = os.environ['DINGTALK_SECRET']
title = os.environ['DINGTALK_TITLE']
text = os.environ['DINGTALK_TEXT']

ts = str(int(time.time() * 1000))
sign = base64.b64encode(
    hmac.new(secret.encode(), f"{ts}\n{secret}".encode(), hashlib.sha256).digest()
).decode()
sign_url = urllib.parse.quote_plus(sign)

body = json.dumps({
    "msgtype": "markdown",
    "markdown": {"title": title, "text": text},
}, ensure_ascii=False).encode()

url = f"{webhook}&timestamp={ts}&sign={sign_url}"
req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
try:
    resp = urllib.request.urlopen(req, timeout=15)
    print(resp.read().decode())
except Exception as e:
    print(f"dingtalk send error: {e}")
    exit(1)
EOF
