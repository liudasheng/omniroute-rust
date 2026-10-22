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


def deletion_lines(text):
    """Removed lines of a patch: `-` prefixed, excluding the `---` header."""
    out = set()
    for line in text.split("\n"):
        if line.startswith("-") and not line.startswith("---"):
            out.add(line.rstrip())
    return out


def error_bodies(text):
    """Diagnostic *bodies* — a line that IS an error, not one that mentions
    the word. The previous regex matched any line containing "error", so a
    code comment or "0 failed" counted as a diagnostic and the audit could
    not tell a real loss from noise."""
    strong = re.compile(
        r"^\s*(?:[\w./\\-]+:)?\s*\d*:?\s*"
        r"(?:error(?:\[E\d+\])?|fatal|panic(?:ked)?|traceback|exception|"
        r"segmentation fault|assertion failed|failed)\b",
        re.I,
    )
    out = set()
    for line in text.split("\n"):
        t = line.strip()
        if not t or len(t) > 400:
            continue
        if strong.match(t) or t.startswith("-->") or STACK_RE.match(t):
            out.add(t)
    return out


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
            # A block that was a JSON document must still be one: the deduper
            # used to append " [x2]" to a repeated document, which turned
            # every copy into unparseable text.
            for label, t in (("orig", ot), ("comp", ct)):
                stripped = t.strip()
                if not stripped.startswith(("{", "[")):
                    continue
                try:
                    json.loads(stripped)
                except Exception:
                    if label == "orig":
                        continue  # the original was not JSON to begin with
                    problems.append(f"json-damaged[{i}]:{stripped[:60]}")
            for e in error_lines(ot):
                if e not in ct:
                    problems.append(f"lost-error[{i}]:{e[:60]}")
            # A removed line of a patch must never vanish: a silently dropped
            # deletion corrupts the change the agent is about to apply.
            for d in deletion_lines(ot):
                if d not in ct:
                    problems.append(f"lost-deletion[{i}]:{d[:50]}")
            for e in error_bodies(ot):
                if e not in ct:
                    problems.append(f"lost-error-body[{i}]:{e[:60]}")
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
        # The largest body in the group is the baseline (the uncompressed
        # one). Every *other* member is compared against it — including the
        # ones before it in the sorted order. Iterating `group[1:]` skipped
        # the first element while the baseline is the *last* one, so a
        # two-member group compared the baseline with itself and reported
        # "0 compared" — a silent false green.
        group = sorted(group, key=lambda b: len(json.dumps(b)))
        base = group[-1]
        for other in group:
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
