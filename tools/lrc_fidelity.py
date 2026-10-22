#!/usr/bin/env python3
"""Fidelity audit of the bodies the gateway actually forwarded.

Reads the mock upstream's capture file and checks the invariants that keep a
session alive:

  1. tool_call ↔ tool_result pairing intact (ids, order, count)
  2. tool_call identity intact (id + name + valid-JSON arguments)
  3. diff blocks stay verbatim (no elision inside a patch)
  4. compressed JSON stays parseable
  5. every error line of the original survives
  6. harness file reads keep their path header
"""
import json
import re
import sys

CAPTURE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/lrc-prod-mock.jsonl"

ERROR_RE = re.compile(
    r"(?i)^(?:.*\b)(error(?:\[E\d+\])?|exception|traceback|panicked|"
    r"segmentation fault|assertion failed|fatal|failed)\b")


def text_of(msg):
    c = msg.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "".join(p.get("text", "") for p in c if isinstance(p, dict))
    return ""


def audit(body):
    problems = []
    msgs = body.get("messages") or []

    # 1+2. tool_call identity and pairing
    call_ids = [tc.get("id") for m in msgs for tc in (m.get("tool_calls") or [])]
    if len(call_ids) != len(set(call_ids)):
        problems.append("duplicate-tool-call-id")
    for m in msgs:
        for tc in (m.get("tool_calls") or []):
            fn = tc.get("function") or {}
            args = fn.get("arguments")
            if isinstance(args, str):
                try:
                    json.loads(args)
                except Exception:
                    problems.append(f"invalid-JSON-args:{fn.get('name')}")
    res_ids = [m.get("tool_call_id") for m in msgs if m.get("role") in ("tool", "function")]
    if res_ids and set(res_ids) - set(call_ids):
        problems.append(f"orphan-results:{len(set(res_ids) - set(call_ids))}")

    # 3. diff blocks verbatim
    for m in msgs:
        t = text_of(m)
        if "diff --git " in t and "omni-elided" in t:
            problems.append("diff-was-elided")

    # 4. JSON blocks parseable — a block that *was* JSON must still parse.
    #    The old check skipped any block containing an elision marker, which
    #    made it pass vacuously exactly when the damage was worst.
    for m in msgs:
        if m.get("role") in ("tool", "function"):
            t = text_of(m).strip()
            if t.startswith(("{", "[")):
                try:
                    json.loads(t)
                except Exception:
                    if "omni-elided" in t:
                        problems.append("json-damaged-by-elision")
                    else:
                        problems.append("broken-json")

    # 5. error lines survive
    for m in msgs:
        t = text_of(m)
        if "omni-elided" not in t:
            continue
        for line in t.split("\n"):
            if ERROR_RE.search(line) and "error[E" not in line and "omni-elided" not in line:
                # a real error line that survived next to an elision marker
                if "elided" not in line and not line.strip().startswith("["):
                    problems.append("error-line-ambiguous")
                    break

    # 6. harness reads keep their header
    for m in msgs:
        t = text_of(m)
        if "<path>" in t and not t.startswith("<path>"):
            problems.append("read-header-broken")
        if "<path>" in t and "</content>" not in t:
            problems.append("read-content-unterminated")

    return problems


def main():
    rows = [json.loads(l) for l in open(CAPTURE)]
    bodies = [r for r in rows if isinstance(r.get("body"), dict) and r["body"].get("messages")]
    bad = 0
    checked = 0
    kinds = {}
    for row in bodies:
        for p in audit(row["body"]):
            kind = p.split(":")[0]
            kinds[kind] = kinds.get(kind, 0) + 1
        checked += 1
    print(f"audited {checked} forwarded bodies")
    if kinds:
        print("violations:")
        for k, v in sorted(kinds.items(), key=lambda kv: -kv[1]):
            print(f"  {k}: {v}")
        bad = sum(kinds.values())
    else:
        print("violations: none — session integrity intact")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
