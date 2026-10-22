#!/usr/bin/env python3
"""Local mock OpenAI-compatible upstream used for the LRC acceptance run.

It records every forwarded request body to a JSONL file and answers with a
completion whose `usage.prompt_tokens` equals the *measured* body size in
tokens (utf16/4 — the same convention the gateway uses), so the gateway-level
effect of compression is observable end-to-end without depending on a real
upstream's quota.
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/lrc-mock-bodies.jsonl"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 20999


def tokens(text):
    return (len(text.encode("utf-16-le")) // 2 + 3) // 4


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n)
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        text = raw.decode("utf-8", "ignore")
        with open(OUT, "a") as fh:
            fh.write(json.dumps({
                "ts": time.time(),
                "path": self.path,
                "bytes": len(raw),
                "tokens": tokens(text),
                "body": body,
            }) + "\n")
        if self.path.endswith("/chat/completions"):
            payload = {
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "created": int(time.time()),
                "model": body.get("model", "mock"),
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": tokens(text), "completion_tokens": 1,
                          "total_tokens": tokens(text) + 1},
            }
        else:
            payload = {"id": "resp-mock", "object": "response", "output": [],
                       "usage": {"input_tokens": tokens(text), "output_tokens": 1}}
        out = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def do_GET(self):
        out = json.dumps({"data": [{"id": "mock-model", "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


if __name__ == "__main__":
    open(OUT, "w").close()
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), H)
    print(f"mock upstream on http://127.0.0.1:{PORT} → {OUT}", flush=True)
    srv.serve_forever()
