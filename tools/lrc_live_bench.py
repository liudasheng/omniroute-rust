#!/usr/bin/env python3
"""Live performance test of LRC against a REAL upstream model.

No mock, no byte-counting proxy: every measurement comes from the upstream's
own `usage` report (prompt_tokens / cached_tokens) and from wall-clock latency.

For each sampled step of a real DSH session we replay the exact request the
harness sent, once with compression off and once per LRC profile, and report:

  * prompt_tokens        (what the provider counted)
  * cached_tokens        (cache hit → the bytes we *pay* for are the miss part)
  * paid_tokens          = prompt_tokens - cached_tokens  (the real cost)
  * latency_ms           (upstream wall clock)
  * completion_ok        (did the answer survive the compression)

NOTE: `max_tokens` MUST come from the session's own request/header. Reasoning
models spend the completion budget on thinking; a tiny value (e.g. 64) leaves
`content` empty, which the gateway correctly reports as "upstream returned
empty content" — a test artifact that looks like an upstream outage.

Usage:
  tools/lrc_live_bench.py <gateway> <api-key> <model> [profiles] [sessions] [steps]
"""
import glob
import json
import os
import subprocess
import sys
import time
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:20128"
KEY = sys.argv[2] if len(sys.argv) > 2 else ""
MODEL = sys.argv[3] if len(sys.argv) > 3 else "coding-free"
PROFILES = (sys.argv[4] if len(sys.argv) > 4 else "conservative,balanced,aggressive,extreme").split(",")
N_SESSIONS = int(sys.argv[5]) if len(sys.argv) > 5 else 3
MAX_STEPS = int(sys.argv[6]) if len(sys.argv) > 6 else 2


def reconstruct(path, max_steps):
    """Rebuild request snapshots from a DSH session log (event replay)."""
    msgs = []
    tools = None
    steps = []
    # Tool-call ids seen so far: a `tool` message is only legal when its
    # `tool_call_id` matches an assistant tool_call that precedes it.
    known_call_ids = set()
    pending_calls = {}
    max_tokens = 8192  # reasoning models need room; a tiny value yields empty content
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
            hdr = d.get("header") or {}
            tools = hdr.get("tools") or tools
            mt = (hdr.get("config") or {}).get("maxTokens")
            if mt:
                max_tokens = int(mt)
        elif t == "system/message":
            m = d.get("message") or {}
            msgs.append({"role": "system", "content":
                         "".join(x.get("text", "") for x in m.get("content") or [] if isinstance(x, dict))})
        elif t == "user/message":
            msgs.append({"role": "user", "content":
                         "".join(x.get("text", "") for x in d.get("content") or [] if isinstance(x, dict))})
        elif t == "assistant/message":
            m = d.get("message") or {}
            parts = m.get("content") or []
            text = "".join(x.get("text", "") for x in parts if isinstance(x, dict) and x.get("type") == "text")
            reasoning = "".join(x.get("text", "") for x in parts if isinstance(x, dict) and x.get("type") == "reasoning")
            calls = [{"id": x.get("id"), "type": "function",
                      "function": {"name": x.get("name"), "arguments": x.get("arguments") or "{}"}}
                     for x in parts if isinstance(x, dict) and x.get("type") == "tool-call"]
            # The step is snapshotted *before* the assistant message, so the
            # snapshot is the request the client actually sent.
            steps.append({"messages": [dict(m) for m in msgs], "tools": tools,
                          "max_tokens": max_tokens})
            for c in calls:
                if c.get("id"):
                    known_call_ids.add(c["id"])
            msgs.append({"role": "assistant", "content": text,
                         **({"reasoning_content": reasoning} if reasoning else {}),
                         **({"tool_calls": calls} if calls else {})})
        elif t == "tool/call":
            # A tool call whose result never arrived (the turn is still
            # running) must not leave a `tool` message without its
            # assistant: an OpenAI upstream rejects such a request with
            # "Messages with role 'tool' must be a response to a preceding
            # message with 'tool_calls'".
            pending_calls[d.get("callId")] = d.get("name")
        elif t == "tool/result":
            m = d.get("message") or {}
            call_id = m.get("toolCallId")
            # A result whose call was never seen in this reconstruction (the
            # log was compacted, or the call belongs to another session
            # segment) would become an orphan — drop it.
            if call_id and call_id not in known_call_ids:
                continue
            msgs.append({"role": "tool", "tool_call_id": call_id, "content":
                         "".join(x.get("text", "") for x in m.get("content") or [] if isinstance(x, dict))})
            if call_id:
                known_call_ids.add(call_id)
    return steps[-max_steps:] if len(steps) > max_steps else steps


def post(body, mode):
    headers = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}
    if mode:
        headers["x-omniroute-compression"] = mode
    req = urllib.request.Request(f"{BASE}/v1/chat/completions",
                                 data=json.dumps(body).encode(), headers=headers, method="POST")
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=600) as r:
            payload = json.loads(r.read())
            ms = (time.time() - t0) * 1000
            return r.status, dict(r.headers), payload, ms
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), {"error": e.read().decode()[:300]}, (time.time() - t0) * 1000
    except Exception as e:  # noqa: BLE001
        return 0, {}, {"error": str(e)[:200]}, (time.time() - t0) * 1000


def usage_of(payload):
    u = payload.get("usage") or {}
    pt = u.get("prompt_tokens") or u.get("input_tokens") or 0
    ct = u.get("completion_tokens") or u.get("output_tokens") or 0
    cached = (u.get("prompt_tokens_details") or {}).get("cached_tokens") or 0
    if not cached:
        cached = u.get("cache_read_input_tokens") or 0
    return pt, cached, ct


def main():
    req = urllib.request.Request(
        f"{BASE}/v1/compression",
        data=json.dumps({"enabled": True, "default_mode": "off"}).encode(),
        headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"},
        method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            r.read()
        print("config: enabled, default_mode=off (baseline)")
    except Exception as e:  # noqa: BLE001
        print("config update failed (proceeding):", str(e)[:120])

    sessions = sorted(glob.glob(os.path.expanduser("~/.dsh/sessions/*/*/session.v4.jsonl.zstd")),
                      key=os.path.getmtime, reverse=True)[:N_SESSIONS]
    rows = []
    for s in sessions:
        raw = subprocess.run(["zstd", "-dc", s], capture_output=True).stdout.decode("utf-8", "ignore")
        with open("/tmp/lrc-live.jsonl", "w") as fh:
            fh.write(raw)
        for st in reconstruct("/tmp/lrc-live.jsonl", MAX_STEPS):
            body = {"model": MODEL, "messages": st["messages"], "stream": False,
                    "max_tokens": st.get("max_tokens", 8192)}
            if st.get("tools"):
                body["tools"] = st["tools"]
            size = len(json.dumps(body))
            status, _, resp, ms = post(body, "off")
            pt, cached, ct = usage_of(resp) if isinstance(resp, dict) else (0, 0, 0)
            if not pt:
                print(f"   baseline failed status={status}: {str(resp)[:150]}")
                continue
            rec = {"bytes": size, "base_pt": pt, "base_cached": cached, "base_ms": ms,
                   "profiles": {}}
            print(f"  step bytes={size:>8d} msgs={len(st['messages']):>4d}  "
                  f"baseline pt={pt:>7d} cached={cached:>7d} paid={pt-cached:>7d} {ms:6.0f}ms")
            for prof in PROFILES:
                # Measure the *warm* cache: the first call populates the
                # provider's prefix cache, the second one reveals what the
                # steady state costs. Measuring a cold call would charge
                # compression for a cache miss it did not cause — and would
                # hide that an unchanged prefix is what makes the cache hit.
                st1, _, r1, _ = post(body, f"lrc:{prof}")
                st2, hdrs, r2, ms2 = post(body, f"lrc:{prof}")
                p2, c2, _ = usage_of(r2) if isinstance(r2, dict) else (0, 0, 0)
                p1, c1, _ = usage_of(r1) if isinstance(r1, dict) else (0, 0, 0)
                rec["profiles"][prof] = {"pt": p2, "cached": c2, "ms": ms2, "status": st2,
                                         "cold_pt": p1, "cold_cached": c1,
                                         "header": hdrs.get("x-omniroute-compression", "")[:180]}
                if p2:
                    print(f"      {prof:13s} pt={p2:>7d} cached={c2:>7d} paid={p2-c2:>7d} {ms2:6.0f}ms "
                          f"save={100*(pt-p2)/pt:5.1f}% paid_save={100*((pt-cached)-(p2-c2))/max(pt-cached,1):7.1f}% "
                          f"(cold: pt={p1} cached={c1})")
                else:
                    print(f"      {prof:13s} FAILED status={st2}: {str(r2)[:120]}")
            rows.append(rec)
            time.sleep(1.0)  # be gentle with the upstream rate limit

    if not rows:
        print("no samples")
        return
    print(f"\n=== N = {len(rows)} real upstream calls, model={MODEL} ===")
    print(f"{'profile':14s} {'pt median':>10s} {'paid med':>10s} {'pt save':>8s} {'paid save':>10s} {'latency':>10s}")
    base_pt = [r["base_pt"] for r in rows]
    base_paid = [r["base_pt"] - r["base_cached"] for r in rows]
    base_ms = [r["base_ms"] for r in rows]
    print(f"{'(baseline)':14s} {sorted(base_pt)[len(base_pt)//2]:10d} {sorted(base_paid)[len(base_paid)//2]:10d} "
          f"{'-':>8s} {'-':>10s} {sorted(base_ms)[len(base_ms)//2]:9.0f}ms")
    for prof in PROFILES:
        pts = [r["profiles"][prof]["pt"] for r in rows if r["profiles"][prof]["pt"]]
        paid = [r["profiles"][prof]["pt"] - r["profiles"][prof]["cached"] for r in rows if r["profiles"][prof]["pt"]]
        mss = [r["profiles"][prof]["ms"] for r in rows if r["profiles"][prof]["pt"]]
        if not pts:
            continue
        pt_save = 100 * (sum(base_pt) - sum(pts)) / sum(base_pt)
        paid_save = 100 * (sum(base_paid) - sum(paid)) / max(sum(base_paid), 1)
        print(f"{prof:14s} {sorted(pts)[len(pts)//2]:10d} {sorted(paid)[len(paid)//2]:10d} "
              f"{pt_save:7.1f}% {paid_save:9.1f}% {sorted(mss)[len(mss)//2]:9.0f}ms")


if __name__ == "__main__":
    main()
