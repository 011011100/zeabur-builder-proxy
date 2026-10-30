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
# Reuse the original built payloads, changing only the old archive's incompatible digest-only
# index entries. Record both indexes and checksums; this diagnostic is not an installable release.
python3 - "${bundle[0]}" "$evidence" <<'PY'
import hashlib, io, json, os, pathlib, sys, tarfile
root, evidence = map(pathlib.Path, sys.argv[1:])
archive = root / 'images/components-amd64.oci.tar'
before = hashlib.sha256(archive.read_bytes()).hexdigest()
temporary = archive.with_suffix('.partial')
with tarfile.open(archive, 'r:') as source, tarfile.open(temporary, 'w:') as target:
    for member in source.getmembers():
        if not member.isfile():
            raise ValueError('Diagnostic fixture must contain regular OCI files only')
        data = source.extractfile(member).read()
        if member.name == 'index.json':
            index = json.loads(data)
            original = json.loads(data)
            index['manifests'] = [entry for entry in index['manifests']
                                  if '@' not in entry['annotations']['org.opencontainers.image.ref.name']]
            data = json.dumps(index).encode()
            member.size = len(data)
        target.addfile(member, io.BytesIO(data))
os.replace(temporary, archive)
(evidence / 'index-repair.json').write_text(json.dumps({
    'before_sha256': before, 'after_sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
    'original_index': original, 'tag_only_index': index,
}, indent=2) + '\n')
PY
if ! python3 scripts/check-proxy-import.py "${bundle[0]}"; then
  # An isolated containerd namespace distinguishes archive decoding from watcher/CRI faults.
  # This is diagnostic only: it cannot make the k8s.io watcher verification pass.
  docker cp "${bundle[0]}/images/components-amd64.oci.tar" "$node:/tmp/proxy-format-probe.tar"
  docker exec "$node" /bin/ctr --address /run/k3s/containerd/containerd.sock -n offline-format-probe images import \
    --local --all-platforms --no-unpack /tmp/proxy-format-probe.tar \
    > "$evidence/format-probe.log" 2>&1 || true
  docker exec "$node" /bin/ctr --address /run/k3s/containerd/containerd.sock -n offline-format-probe images list \
    > "$evidence/format-probe-images.log" 2>&1 || true
  cat "$evidence/format-probe.log"
  cat "$evidence/failure.node.log"
  exit 1
fi
