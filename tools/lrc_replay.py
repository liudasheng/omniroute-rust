#!/usr/bin/env python3
"""Replay REAL DSH session steps through the gateway and measure the actual
prompt_tokens delta caused by LRC.

For each sampled step we reconstruct the exact request the harness sent
(system + history user/assistant/tool + tools schema) and issue it twice to
the local gateway: once with compression off, once with `lrc:<profile>`.
The provider-reported `usage.prompt_tokens` is the ground truth.
"""
import glob
import json
import os
import subprocess
import sys
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:20130"
KEY = sys.argv[2] if len(sys.argv) > 2 else "test-master-key"
MODEL = sys.argv[3] if len(sys.argv) > 3 else "lmstudio/mock-model"
PROFILES = (sys.argv[4] if len(sys.argv) > 4 else "conservative,balanced,aggressive,extreme").split(",")
N_SESSIONS = int(sys.argv[5]) if len(sys.argv) > 5 else 6
MAX_STEPS = int(sys.argv[6]) if len(sys.argv) > 6 else 3


def reconstruct(path, max_steps):
    """Rebuild up to `max_steps` request snapshots from a DSH session log."""
    msgs = []
    tool_names = {}
    tools = None
    steps = []
    for line in open(path, "r", errors="ignore"):
        line = line.strip()
        if not line:
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        t = o.get("type")
        d = o.get("data") or {}
        if t == "request/header":
            tools = (d.get("header") or {}).get("tools") or tools
        elif t == "system/message":
            m = d.get("message") or {}
            txt = "".join(x.get("text", "") for x in m.get("content") or [] if isinstance(x, dict))
            msgs.append({"role": "system", "content": txt})
        elif t == "user/message":
            txt = "".join(x.get("text", "") for x in d.get("content") or [] if isinstance(x, dict))
            msgs.append({"role": "user", "content": txt})
        elif t == "tool/call":
            tool_names[d.get("callId")] = d.get("name")
        elif t == "assistant/message":
            m = d.get("message") or {}
            parts = m.get("content") or []
            text = "".join(x.get("text", "") for x in parts if isinstance(x, dict) and x.get("type") == "text")
            reasoning = "".join(x.get("text", "") for x in parts if isinstance(x, dict) and x.get("type") == "reasoning")
            calls = []
            for x in parts:
                if isinstance(x, dict) and x.get("type") == "tool-call":
                    calls.append({"id": x.get("id"), "type": "function",
                                  "function": {"name": x.get("name"), "arguments": x.get("arguments") or "{}"}})
            steps.append({"messages": [dict(m) for m in msgs], "tools": tools})
            msgs.append({"role": "assistant", "content": text, **({"reasoning_content": reasoning} if reasoning else {}),
                         **({"tool_calls": calls} if calls else {})})
        elif t == "tool/result":
            m = d.get("message") or {}
            txt = "".join(x.get("text", "") for x in m.get("content") or [] if isinstance(x, dict))
            cid = m.get("toolCallId")
            msgs.append({"role": "tool", "tool_call_id": cid, "content": txt})
    # keep the LAST steps: deep history is where the token mass lives
    if len(steps) > max_steps:
        steps = steps[-max_steps:]
    return steps


def post(body, mode):
    headers = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}
    if mode:
        headers["x-omniroute-compression"] = mode
    req = urllib.request.Request(f"{BASE}/v1/chat/completions",
                                 data=json.dumps(body).encode(), headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=240) as r:
            return r.status, dict(r.headers), json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), {"error": e.read().decode()[:200]}
    except Exception as e:  # noqa: BLE001
        return 0, {}, {"error": str(e)[:200]}


def main():
    # The master switch gates every mode (parity: `enabled=false` is a hard
    # kill). Enable the machinery but keep the *default* mode off, so the
    # baseline is "no compression" and each profile is selected per request.
    req = urllib.request.Request(
        f"{BASE}/v1/compression",
        data=json.dumps({"enabled": True, "default_mode": "off"}).encode(),
        headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"},
        method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            print("config:", json.loads(r.read()).get("default_mode"), "(enabled)")
    except Exception as e:  # noqa: BLE001
        print("config update failed:", str(e)[:160])

    sessions = sorted(glob.glob(os.path.expanduser("~/.dsh/sessions/*/*/session.v4.jsonl.zstd")),
                      key=os.path.getmtime, reverse=True)[:N_SESSIONS]
    rows = []
    for s in sessions:
        raw = subprocess.run(["zstd", "-dc", s], capture_output=True).stdout.decode("utf-8", "ignore")
        tmp = "/tmp/lrc-replay.jsonl"
        with open(tmp, "w") as fh:
            fh.write(raw)
        steps = reconstruct(tmp, MAX_STEPS)
        for st in steps:
            body = {"model": MODEL, "messages": st["messages"], "stream": False}
            if st.get("tools"):
                body["tools"] = st["tools"]
            size = len(json.dumps(body))
            print(f"   sample bytes={size} msgs={len(st['messages'])}")
            base_status, _, base_resp = post(body, "off")
            base_pt = ((base_resp.get("usage") or {}) if isinstance(base_resp, dict) else {}).get("prompt_tokens")
            if not base_pt:
                print(f"   base ERR status={base_status} {str(base_resp)[:220]}")
                continue
            rec = {"bytes": size, "base": base_pt, "profiles": {}}
            for prof in PROFILES:
                status, headers, resp = post(body, f"lrc:{prof}")
                pt = ((resp.get("usage") or {}) if isinstance(resp, dict) else {}).get("prompt_tokens")
                rec["profiles"][prof] = {"status": status, "tokens": pt,
                                         "header": headers.get("x-omniroute-compression", "")[:160]}
                if not pt:
                    print("   ERR", prof, status, str(resp)[:160])
            rows.append(rec)
            print(f"bytes={size:>8d} base={base_pt:>7d} " +
                  " ".join(f"{p}={v['tokens']}({100*(base_pt-v['tokens'])/base_pt if v['tokens'] else 0:5.1f}%)"
                           for p, v in rec["profiles"].items()))
    if not rows:
        print("no samples")
        return
    print(f"\nN = {len(rows)} real steps")
    print(f"{'profile':14s} {'median saving%':>15s} {'min':>8s} {'max':>8s} {'weighted':>9s}")
    for prof in PROFILES:
        sav = sorted(100 * (r["base"] - r["profiles"][prof]["tokens"]) / r["base"]
                     for r in rows if r["profiles"][prof]["tokens"])
        if not sav:
            continue
        wsum = sum(r["base"] - r["profiles"][prof]["tokens"] for r in rows if r["profiles"][prof]["tokens"])
        wtot = sum(r["base"] for r in rows if r["profiles"][prof]["tokens"])
        print(f"{prof:14s} {sav[len(sav)//2]:14.1f}% {sav[0]:7.1f}% {sav[-1]:7.1f}% {100*wsum/wtot:8.1f}%")


if __name__ == "__main__":
    main()
