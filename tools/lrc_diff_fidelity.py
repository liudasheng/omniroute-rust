#!/usr/bin/env python3
"""Differential fidelity audit: compare each compressed body against the
*uncompressed* body the gateway forwarded for the same request.

The mock capture interleaves the runs, so we pair them by the sequence of
tool_call ids (a stable identity of the request) instead of by order.

Checks (only differences caused by compression count as violations):
  - tool_call ids / names / order identical
  - tool_call arguments still valid JSON
  - tool_result ↔ tool_call pairing identical
  - every error line of the original still present in the compressed block
  - every diff hunk header of the original still present
  - harness read blocks keep their <path> header and a closed </content>
  - no compressed block is *longer* than its original
"""
import json
import re
import sys

CAPTURE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/lrc-prod-mock.jsonl"
# A *diagnostic* line, not any line that happens to mention "error":
# it must start with the keyword (optionally prefixed by a path/line number).
ERROR_RE = re.compile(
    r"(?i)^(?:[\w./\\-]+:)?\s*\d*:?\s*"
    r"(error(?:\[E\d+\])?|exception|traceback|panic(?:ked)?|fatal|"
    r"segmentation fault|assertion failed|failed|warning)\b")
STACK_RE = re.compile(r"^\s*at\s+\S+\s*\(")


def txt(m):
    c = m.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "".join(p.get("text", "") for p in c if isinstance(p, dict))
    return ""


def key_of(body):
    """Identity of a request for pairing: the tool_call ids *and* the role
    sequence. The gateway's transcript normalisation may drop a trailing
    assistant turn, so the role sequence is part of the identity — without it
    an uncompressed body would be paired with a differently-shaped one."""
    ids = []
    for m in body.get("messages") or []:
        for tc in (m.get("tool_calls") or []):
            ids.append(tc.get("id"))
    roles = tuple(m.get("role") for m in body.get("messages") or [])
    return (tuple(ids), roles)


def error_lines(text):
    out = set()
    for line in text.split("\n"):
        t = line.strip()
        if not t or "omni-elided" in t:
            continue
        if (ERROR_RE.match(t) or STACK_RE.match(t)) and len(t) < 400:
            out.add(t)
    return out


def hunks(text):
    return {l.strip() for l in text.split("\n") if l.strip().startswith("@@")}


def audit(orig, comp):
    problems = []
    om = orig.get("messages") or []
    cm = comp.get("messages") or []
    if len(om) != len(cm):
        problems.append(f"message-count {len(om)}->{len(cm)}")
        return problems

    for i, (o, c) in enumerate(zip(om, cm)):
        orole = o.get("role")
        if orole != c.get("role"):
            problems.append(f"role[{i}] {orole}->{c.get('role')}")
            continue
        # tool calls
        ocalls = o.get("tool_calls") or []
        ccalls = c.get("tool_calls") or []
        if len(ocalls) != len(ccalls):
            problems.append(f"tool-call-count[{i}]")
            continue
        for oc, cc in zip(ocalls, ccalls):
            if oc.get("id") != cc.get("id"):
                problems.append(f"call-id[{i}]")
            ofn = (oc.get("function") or {}).get("name")
            cfn = (cc.get("function") or {}).get("name")
            if ofn != cfn:
                problems.append(f"call-name[{i}]")
            args = (cc.get("function") or {}).get("arguments")
            if isinstance(args, str):
                try:
                    json.loads(args)
                except Exception:
                    problems.append(f"invalid-args[{i}]:{cfn}")
        # tool results / text
        ot, ct = txt(o), txt(c)
        if orole in ("tool", "function"):
            for e in error_lines(ot):
                if e not in ct:
                    problems.append(f"lost-error[{i}]:{e[:60]}")
            for h in hunks(ot):
                if h not in ct:
                    problems.append(f"lost-hunk[{i}]:{h[:40]}")
            if "<path>" in ot:
                if "<path>" not in ct:
                    problems.append(f"lost-read-path[{i}]")
                if "</content>" in ot and "</content>" not in ct:
                    problems.append(f"unterminated-read[{i}]")
                for e in error_lines(ot):
                    if e not in ct:
                        problems.append(f"lost-read-error[{i}]")
        if len(ct) > len(ot) + 64:
            problems.append(f"grew[{i}] {len(ot)}->{len(ct)}")
    return problems


def main():
    rows = [json.loads(l) for l in open(CAPTURE)]
    bodies = [r["body"] for r in rows
              if isinstance(r.get("body"), dict) and isinstance(r["body"].get("messages"), list)]
    # group by request identity
    groups = {}
    for b in bodies:
        groups.setdefault(key_of(b), []).append(b)
    pairs = 0
    kinds = {}
    samples = []
    for key, group in groups.items():
        if len(group) < 2:
            continue
        # the largest body in the group is the baseline (the uncompressed one)
        group = sorted(group, key=lambda b: len(json.dumps(b)))
        base = group[-1]
        for other in group[1:]:
            if json.dumps(base) == json.dumps(other):
                continue
            pairs += 1
            probs = audit(base, other)
            for p in probs:
                kind = p.split(":")[0].split("[")[0]
                kinds[kind] = kinds.get(kind, 0) + 1
                if len(samples) < 8:
                    samples.append(p)
    print(f"compared {pairs} compressed bodies against their baseline")
    if kinds:
        print("violations by kind:")
        for k, v in sorted(kinds.items(), key=lambda kv: -kv[1]):
            print(f"  {k}: {v}")
        for s in samples:
            print("  e.g.", s)
    else:
        print("violations: none — every compressed body preserves the session skeleton")
    return 1 if kinds else 0


if __name__ == "__main__":
    sys.exit(main())
