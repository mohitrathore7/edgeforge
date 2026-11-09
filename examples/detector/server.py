"""Demo inference service used to exercise EdgeForge.

It is a stand-in for a real model server: it loads a label file at start-up,
exposes /healthz for the EdgeForge health probe and /predict which returns a
deterministic "detection" for the request body. Swap it for ONNX Runtime,
TensorRT or TFLite in a real deployment — EdgeForge only cares about the
process and its health endpoint.
"""
import hashlib
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(os.environ.get("PORT", "8081"))
MODEL_PATH = os.environ.get("MODEL_PATH", "model/labels.txt")
VERSION = os.environ.get("EDGEFORGE_VERSION", "dev")

try:
    with open(MODEL_PATH) as f:
        LABELS = [line.strip() for line in f if line.strip()]
except OSError as e:
    print(f"failed to load model from {MODEL_PATH}: {e}", file=sys.stderr, flush=True)
    sys.exit(2)


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, payload):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/healthz":
            self._send(200, {"status": "ok", "version": VERSION, "labels": len(LABELS)})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/predict":
            return self._send(404, {"error": "not found"})
        data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        h = hashlib.sha256(data).digest()
        label = LABELS[h[0] % len(LABELS)]
        self._send(200, {"label": label, "score": round(0.5 + h[1] / 512, 3), "version": VERSION})

    def log_message(self, fmt, *args):
        pass


if __name__ == "__main__":
    print(f"detector v{VERSION}: {len(LABELS)} labels, listening on :{PORT}", flush=True)
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
