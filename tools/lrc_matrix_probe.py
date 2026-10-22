#!/usr/bin/env python3
"""Matrix probe: same body, several providers and compression modes.

Answers the question the manual curl could not: does the upstream reject the
*compressed* body, or was the failure specific to one provider?

Usage: tools/lrc_matrix_probe.py <body.json> [n_messages]
"""
import json
import os
import sys
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:20128"
BODY = sys.argv[1] if len(sys.argv) > 1 else "/tmp/cmp.json"
N = int(sys.argv[2]) if len(sys.argv) > 2 else 300

KEY = json.load(open(os.path.expanduser("~/.omniroute-rust/api-keys.json")))[0]["key"]


def call(model, mode, messages):
    headers = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}
    if mode:
        headers["x-omniroute-compression"] = mode
    body = {"model": model, "messages": messages, "stream": False, "max_tokens": 8192}
    req = urllib.request.Request(
        f"{BASE}/v1/chat/completions", data=json.dumps(body).encode(),
        headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=240) as r:
            d = json.loads(r.read())
            u = d.get("usage") or {}
            return f"OK   pt={u.get('prompt_tokens')} cached={(u.get('prompt_tokens_details') or {}).get('cached_tokens')}"
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            msg = json.loads(raw).get("error", {}).get("message", "")
        except Exception:
            msg = raw[:200]
        return f"HTTP {e.code}: {msg[:150]}"
    except Exception as e:  # noqa: BLE001
        return f"ERR {str(e)[:120]}"


def main():
    msgs = json.load(open(BODY))["messages"][:N]
    print(f"messages={len(msgs)}")
    for model in ("lmstudio/mock-model", "opencode-go/deepseek-v4.1-flash"):
        for mode in (None, "lrc:conservative", "lrc:balanced", "lrc:aggressive", "lrc:extreme"):
            label = mode or "off"
            print(f"  {model:32s} {label:16s} {call(model, mode, msgs)}")
            import time
            time.sleep(1.0)


if __name__ == "__main__":
    main()
