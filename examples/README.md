# Examples

| Directory | Purpose |
|-----------|---------|
| `detector/` | v1.0.0 of a small demo inference service with an HTTP health endpoint, 256 MiB / 0.5 CPU limits |
| `detector-broken/` | v1.1.0 that ships without its model file and exits on start — used to show automatic rollback |

`server.py` is a stand-in for a real model server (ONNX Runtime, TensorRT, TFLite…).
EdgeForge only manages the process, its resources and its health endpoint.

Run `scripts/demo.sh` from the repository root to walk through install → update → failed update → rollback.
