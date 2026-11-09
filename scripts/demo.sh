#!/usr/bin/env bash
# End-to-end demo: install -> OTA update -> broken update (auto rollback) -> manual rollback.
# Runs entirely in a temp directory; needs cargo and python3.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --release --quiet
BIN=target/release
WORK=$(mktemp -d)
API=127.0.0.1:9123
export EDGEFORGE_API=http://$API

cleanup() { kill "${DAEMON:-}" 2>/dev/null || true; wait 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }

step "Packing artifacts"
$BIN/edgeforge pack examples/detector -o "$WORK/detector-1.0.0.tar"
$BIN/edgeforge pack examples/detector --version 1.0.1 -o "$WORK/detector-1.0.1.tar"
$BIN/edgeforge pack examples/detector-broken -o "$WORK/detector-1.1.0.tar"
$BIN/edgeforge verify "$WORK/detector-1.0.0.tar"

step "Starting edgeforged (state in $WORK/state)"
$BIN/edgeforged --root "$WORK/state" --listen "$API" --no-cgroups --tick-ms 500 \
  > "$WORK/daemon.log" 2>&1 &
DAEMON=$!
for _ in $(seq 50); do curl -sf "http://$API/healthz" >/dev/null && break; sleep 0.1; done

step "Install + start v1.0.0"
$BIN/edgeforge install "$WORK/detector-1.0.0.tar" --start > /dev/null
sleep 3
$BIN/edgeforge list
curl -s -X POST --data 'frame-0001' http://127.0.0.1:8081/predict; echo

step "OTA update to v1.0.1 (slot A -> B, health-gated)"
$BIN/edgeforge update detector "$WORK/detector-1.0.1.tar" > /dev/null
sleep 1
$BIN/edgeforge list

step "Manual rollback to v1.0.0 (B -> A)"
$BIN/edgeforge rollback detector > /dev/null
$BIN/edgeforge list

step "OTA update to broken v1.1.0 (expected: automatic rollback)"
$BIN/edgeforge update detector "$WORK/detector-1.1.0.tar" || true
sleep 2
$BIN/edgeforge list

step "Prometheus metrics"
curl -s "http://$API/metrics" | grep -E '^edgeforge_(app_up|app_info|updates_total|rollbacks_total|artifact_verifications_total)'

step "Daemon log"
cat "$WORK/daemon.log"
