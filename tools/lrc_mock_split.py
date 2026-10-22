#!/usr/bin/env python3
"""Mock upstream that files the forwarded body by compression mode.

The gateway forwards to this mock, so the captured body is exactly what the
upstream would receive. The `x-omniroute-compression` request header is
echoed by the gateway? No — the gateway does not forward its own headers, so
the mode is inferred from the body itself: a compressed body carries an
`[omni-elided` marker, an uncompressed one does not.

Usage: tools/lrc_mock_split.py <out_dir> [port]
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

OUT_DIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/lrc-split"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 1234
os.makedirs(OUT_DIR, exist_ok=True)


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
        compressed = "omni-elided" in text or "args-elided" in text
        name = "compressed" if compressed else "plain"
        path = os.path.join(OUT_DIR, f"{name}.jsonl")
        with open(path, "a") as fh:
            fh.write(json.dumps({
                "ts": time.time(),
                "bytes": len(raw),
                "tokens": tokens(text),
                "body": body,
            }) + "\n")
        out = json.dumps({
            "id": "chatcmpl-mock", "object": "chat.completion", "created": int(time.time()),
            "model": "mock-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": tokens(text), "completion_tokens": 1,
                      "total_tokens": tokens(text) + 1},
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


if __name__ == "__main__":
    open(os.path.join(OUT_DIR, "plain.jsonl"), "w").close()
    open(os.path.join(OUT_DIR, "compressed.jsonl"), "w").close()
    HTTPServer(("127.0.0.1", PORT), H).serve_forever()
