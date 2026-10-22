#!/usr/bin/env python3
"""Negative-case self-test for the audit scripts.

The scripts in this directory are the *evidence* that compression is safe.
A check that never fires is not evidence. This file feeds each script a
block that the reviewed code once damaged, and asserts that the script
reports it. A script that stays silent here is a script that gives false
green — which is exactly what the review found.

Usage: python3 tools/lrc_audit_selftest.py
"""
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
DIFF = os.path.join(HERE, "lrc_diff_fidelity.py")
FID = os.path.join(HERE, "lrc_fidelity.py")


def msg(role, content, tool_call_id=None):
    m = {"role": role, "content": content}
    if tool_call_id:
        m["tool_call_id"] = tool_call_id
    return m


def body(messages):
    return {"model": "m", "messages": messages, "stream": False}


def write_capture(rows, path):
    with open(path, "w") as fh:
        for r in rows:
            fh.write(json.dumps(r) + "\n")


def run(script, path):
    out = subprocess.run(
        [sys.executable, script, path], capture_output=True, text=True
    )
    return out.stdout + out.stderr


def case_json_dedup():
    """C1: a repeated JSON document must stay parseable."""
    doc = json.dumps({"id": "k", "name": "dsh", "padding": "x" * 400})
    block = f"{doc}\n{doc}\n"
    damaged = f"{doc} [x2]\n"          # what the old deduper produced
    # The baseline must itself be intact JSON, otherwise the audit cannot
    # tell "was damaged" from "was never JSON" — and the case would pass
    # vacuously.
    assert json.loads(doc)
    rows = [
        {"body": body([msg("user", "go"), msg("tool", block, "c1")])},
        {"body": body([msg("user", "go"), msg("tool", damaged, "c1")])},
    ]
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as fh:
        path = fh.name
    write_capture(rows, path)
    out = run(DIFF, path)
    os.unlink(path)
    return "lost-deletion" in out or "json" in out.lower(), out


def case_dash_deletion():
    """C2: a removed dashed rule must not be eaten by the progress filter."""
    # A real patch carries a hunk header; without it the block is
    # indistinguishable from a markdown list and the audit cannot judge it.
    orig = "@@ -1,3 +1,3 @@\nline one\n-  ---------------\n+  ===============\nline two\n"
    damaged = "@@ -1,3 +1,3 @@\nline one\n+  ===============\nline two\n"
    rows = [
        {"body": body([msg("user", "go"), msg("tool", orig, "c1")])},
        {"body": body([msg("user", "go"), msg("tool", damaged, "c1")])},
    ]
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as fh:
        path = fh.name
    write_capture(rows, path)
    out = run(DIFF, path)
    os.unlink(path)
    return "lost-deletion" in out, out


def case_error_body():
    """C3: a real error body plus its `--> file:line` must survive."""
    orig = (
        "compiling\n"
        "error[E0432]: unresolved import `missing::thing`\n"
        "  --> src/lib.rs:12:5\n"
        "  = note: expected `i32`, found `String`\n"
    )
    damaged = "compiling\n"
    rows = [
        {"body": body([msg("user", "go"), msg("tool", orig, "c1")])},
        {"body": body([msg("user", "go"), msg("tool", damaged, "c1")])},
    ]
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as fh:
        path = fh.name
    write_capture(rows, path)
    out = run(DIFF, path)
    os.unlink(path)
    return "lost-error" in out, out


def case_single_body_json():
    """The single-body audit must flag a JSON block damaged by an elision."""
    damaged = '{"id":"k","padding":"x"}\n[omni-elided 3 lines]\n'
    rows = [{"body": body([msg("user", "go"), msg("tool", damaged, "c1")])}]
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as fh:
        path = fh.name
    write_capture(rows, path)
    out = run(FID, path)
    os.unlink(path)
    return "json-damaged-by-elision" in out, out


CASES = [
    ("C1 json dedup breaks parseability", case_json_dedup),
    ("C2 dash deletion line dropped", case_dash_deletion),
    ("C3 error body lost", case_error_body),
    ("C1 single-body json check", case_single_body_json),
]


def main():
    failures = 0
    for name, fn in CASES:
        try:
            detected, out = fn()
        except Exception as exc:  # noqa: BLE001
            print(f"  ERROR {name}: {exc}")
            failures += 1
            continue
        print(f"  {'OK  ' if detected else 'MISS'} {name}")
        if not detected:
            failures += 1
            print("    --- script output ---")
            for line in out.splitlines()[:12]:
                print("    " + line)
    if failures:
        print(f"\n{failures} case(s) NOT detected — the audit script gives false green")
        return 1
    print("\nall negative cases detected — the audit scripts have teeth")
    return 0


if __name__ == "__main__":
    sys.exit(main())
