#!/usr/bin/env bash
# Reproduce an existing offline bundle failure without rebuilding any application images.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
export KUBECONFIG="$root/target/ci/kubeconfig"
mkdir -p target/ci
trap 'k3d cluster delete zeabur-builder >/dev/null 2>&1 || true' EXIT
k3d cluster create zeabur-builder --image rancher/k3s:v1.36.5-k3s1 --no-lb --wait --timeout 300s
bundle=(target/proxy-artifacts/zeabur-builder-proxy-*-linux-amd64)
[[ ${#bundle[@]} == 1 && -d ${bundle[0]} ]]
node=k3d-zeabur-builder-server-0
evidence=target/proxy-artifacts/cluster/offline-import
mkdir -p "$evidence"
if ! python3 scripts/check-proxy-import.py "${bundle[0]}"; then
  # An isolated containerd namespace distinguishes archive decoding from watcher/CRI faults.
  # This is diagnostic only: it cannot make the k8s.io watcher verification pass.
  docker cp "${bundle[0]}/images/components-amd64.oci.tar" "$node:/tmp/proxy-format-probe.tar"
  docker exec "$node" k3s ctr -n offline-format-probe images import \
    --local --all-platforms --no-unpack /tmp/proxy-format-probe.tar \
    > "$evidence/format-probe.log" 2>&1 || true
  docker exec "$node" k3s ctr -n offline-format-probe images list \
    > "$evidence/format-probe-images.log" 2>&1 || true
  cat "$evidence/format-probe.log"
  cat "$evidence/failure.node.log"
  exit 1
fi
