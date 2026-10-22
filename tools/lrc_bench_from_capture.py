#!/usr/bin/env python3
"""Benchmark LRC on bodies the gateway *actually received* from a real client.

Reconstructing a client's transcript from a session log is error-prone: a
tool result that arrives late ends up as a `tool` message with no
immediately preceding assistant, and the upstream rejects the request
("Messages with role 'tool' must be a response to a preceding message with
'tool_calls'"). That is an artefact of the reconstruction, not a gateway
defect — and it makes a replay benchmark measure the wrong thing.

This harness instead captures the bodies the gateway received, by pointing a
provider at a local mock that records them, and replays *those* bodies
against the real upstream with and without compression. Every byte is
exactly what a live client sent.

Usage:
  tools/lrc_bench_from_capture.py <capture.jsonl> <gateway> <api-key> <model> [profiles]
"""
import json
import statistics
import sys
import time
import urllib.error
import urllib.request

CAPTURE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/lrc-split/plain.jsonl"
BASE = sys.argv[2] if len(sys.argv) > 2 else "http://127.0.0.1:20128"
KEY = sys.argv[3] if len(sys.argv) > 3 else ""
MODEL = sys.argv[4] if len(sys.argv) > 4 else "opencode-go/deepseek-v4.1-flash"
PROFILES = (sys.argv[5] if len(sys.argv) > 5 else "conservative,balanced,aggressive,extreme").split(",")


def call(body, mode, timeout=300):
    headers = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}
    if mode:
        headers["x-omniroute-compression"] = mode
    req = urllib.request.Request(
        f"{BASE}/v1/chat/completions", data=json.dumps(body).encode(),
        headers=headers, method="POST")
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            payload = json.loads(r.read())
            ms = (time.time() - t0) * 1000
            u = payload.get("usage") or {}
            pt = u.get("prompt_tokens") or 0
            cached = (u.get("prompt_tokens_details") or {}).get("cached_tokens") or 0
            return {"ok": True, "pt": pt, "cached": cached, "ms": ms}
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            msg = json.loads(raw).get("error", {}).get("message", "")
        except Exception:
            msg = raw[:200]
        return {"ok": False, "status": e.code, "error": msg[:180], "ms": (time.time() - t0) * 1000}
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "error": str(e)[:160], "ms": (time.time() - t0) * 1000}


def main():
    rows = [json.loads(l) for l in open(CAPTURE) if l.strip()]
    bodies = []
    for r in rows:
        b = r.get("body")
        if not isinstance(b, dict) or not isinstance(b.get("messages"), list):
            continue
        if len(b["messages"]) < 20:
            continue
        bodies.append(b)
    # keep the largest distinct bodies
    seen = set()
    uniq = []
    for b in sorted(bodies, key=lambda x: len(json.dumps(x)), reverse=True):
        h = hash(json.dumps(b))
        if h in seen:
            continue
        seen.add(h)
        uniq.append(b)
    print(f"captured bodies: {len(bodies)} (distinct: {len(uniq)})")
    results = []
    for bi, b in enumerate(uniq[:3]):
        body = {"model": MODEL, "messages": b["messages"], "stream": False,
                "max_tokens": 4096}
        if b.get("tools"):
            body["tools"] = b["tools"]
        base = call(body, None)
        if not base["ok"]:
            print(f"  body{bi}: baseline failed: {base.get('error')}")
            continue
        rec = {"bytes": len(json.dumps(body)), "base": base, "prof": {}}
        print(f"  body{bi}: msgs={len(b['messages'])} baseline pt={base['pt']} "
              f"cached={base['cached']} {base['ms']:.0f}ms")
        for p in PROFILES:
            # two calls: the first warms the provider's prefix cache, the
            # second is the steady state that the client actually pays for.
            call(body, f"lrc:{p}")
            r = call(body, f"lrc:{p}")
            rec["prof"][p] = r
            if r["ok"]:
                print(f"      {p:12s} pt={r['pt']:>7d} cached={r['cached']:>7d} "
                      f"save={100*(base['pt']-r['pt'])/max(base['pt'],1):5.1f}% "
                      f"paid={r['pt']-r['cached']:>7d} {r['ms']:6.0f}ms")
            else:
                print(f"      {p:12s} FAILED: {r.get('error')}")
            time.sleep(1.0)
        results.append(rec)
    if not results:
        print("no usable samples")
        return
    print(f"\n=== N = {len(results)} real captured bodies, model={MODEL} ===")
    bp = [r["base"]["pt"] for r in results]
    print(f"{'profile':14s} {'pt save median':>15s} {'pt save mean':>14s} {'paid med':>10s}")
    for p in PROFILES:
        s = [100 * (r["base"]["pt"] - r["prof"][p]["pt"]) / max(r["base"]["pt"], 1)
             for r in results if r["prof"].get(p, {}).get("ok")]
        paid = [r["prof"][p]["pt"] - r["prof"][p]["cached"]
                for r in results if r["prof"].get(p, {}).get("ok")]
        if not s:
            continue
        print(f"{p:14s} {statistics.median(s):14.1f}% {sum(s)/len(s):13.1f}% "
              f"{statistics.median(paid):10.0f}")


if __name__ == "__main__":
    main()
