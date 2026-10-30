#!/usr/bin/env bash
# Proxy-enabled end-to-end smoke, only in scripts/ci.sh's disposable k3d cluster.
set -euo pipefail

context="${1:?usage: example-proxy.sh k3d-zeabur-builder}"
root="$(cd "$(dirname "$0")/.." && pwd)"
[[ $context == k3d-zeabur-builder && ${KUBECONFIG:-} == "$root/target/ci/kubeconfig" ]] || {
  echo "Proxy smoke refuses a context outside the disposable CI cluster." >&2
  exit 1
}
prefix="${IMAGE_PREFIX:-zeabur-builder}"
tag="${TAG:-ci}"
fixture="$prefix/proxy-smoke:$tag"
work="$root/target/ci/proxy-smoke"
mkdir -p "$work"
k() { kubectl --context "$context" "$@"; }

docker build -q -t "$fixture" "$root/deploy/proxy/ci"
k3d image import --cluster zeabur-builder "$fixture"
k apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata: {name: builder-proxy-smoke, namespace: builder-system}
spec:
  automountServiceAccountToken: false
  enableServiceLinks: false
  securityContext:
    runAsNonRoot: true
    runAsUser: 1000
    runAsGroup: 1000
    fsGroup: 1000
    seccompProfile: {type: RuntimeDefault}
  containers:
    - name: proxy
      image: $fixture
      imagePullPolicy: Never
      ports: [{containerPort: 3128, name: proxy}]
      readinessProbe: {httpGet: {path: /health, port: proxy}, periodSeconds: 2}
      resources: {requests: {cpu: 10m, memory: 32Mi}, limits: {memory: 256Mi}}
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities: {drop: [ALL]}
      volumeMounts: [{name: temporary, mountPath: /tmp}]
  volumes: [{name: temporary, emptyDir: {}}]
EOF
k -n builder-system wait --for condition=Ready pod/builder-proxy-smoke --timeout 120s
k -n builder-system exec builder-proxy-smoke -- python3 /mockproxy.py direct-check
proxy_ip=$(k -n builder-system get pod builder-proxy-smoke -o jsonpath='{.status.podIP}')
python3 - "$proxy_ip" <<'PY'
import ipaddress
import sys
assert ipaddress.IPv4Address(sys.argv[1]) in ipaddress.IPv4Network("10.42.0.0/16"), "proxy is outside blocked Pod CIDR"
PY
helm --kube-context "$context" -n builder-system get values builder -o json > "$work/current-values.json"
no_proxy=$(yq -r '.proxy.noProxy' "$root/deploy/chart/values.yaml")
python3 - "$work" "$proxy_ip" "$fixture" "$no_proxy" <<'PY'
import json
from pathlib import Path
import sys
work, ip, fixture, no_proxy = sys.argv[1:]
current = json.loads((Path(work) / "current-values.json").read_text()) or {}
extra = current.get("admissionPolicy", {}).get("extraImages", [])
values = {"proxy": {"url": "http://" + ip + ":3128", "noProxy": no_proxy},
          "admissionPolicy": {"extraImages": list(dict.fromkeys([*extra, fixture]))}}
(Path(work) / "proxy-values.json").write_text(json.dumps(values))
PY
helm --kube-context "$context" -n builder-system upgrade builder "$root/deploy/chart" \
  --reuse-values -f "$work/proxy-values.json" --wait --timeout 120s
k -n builder-system rollout status deployment/zeabur-builder --timeout 120s
frontend=$(k get clusterbuilder railpack -o jsonpath='{.spec.frontend.source}')
k apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata: {name: proxy-smoke}
---
apiVersion: builder.zeabur.com/v1alpha1
kind: ClusterBuilder
metadata: {name: proxy-smoke-railpack}
spec:
  type: railpack
  planner: {image: $fixture}
  frontend: {type: gateway.v0, source: $frontend}
---
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: proxy-docker, namespace: proxy-smoke}
spec:
  source: {git: {url: http://git.proxy-test.invalid/repo.git, ref: main}}
  builderRef: dockerfile
---
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: proxy-railpack, namespace: proxy-smoke}
spec:
  source: {git: {url: http://git.proxy-test.invalid/repo.git, ref: main}}
  builderRef: proxy-smoke-railpack
EOF

deadline=$(($(date +%s) + ${TIMEOUT:-600}))
pending=(proxy-docker proxy-railpack)
while ((${#pending[@]})); do
  waiting=()
  for name in "${pending[@]}"; do
    image=$(k -n proxy-smoke get build "$name" -o jsonpath='{.status.latestImage}')
    if [[ -n $image ]]; then
      echo "$image" > "$work/$name.image"
      k -n builder-system exec builder-proxy-smoke -- python3 /mockproxy.py inspect "$image"
      continue
    fi
    failed=$(k -n proxy-smoke get buildrun -l "builder.zeabur.com/build=$name" \
      -o jsonpath='{range .items[?(@.status.phase=="Failed")]}{.status.reason}{": "}{.status.message}{"\n"}{end}')
    if [[ -n $failed ]]; then
      echo "$name failed: $failed" >&2
      exit 1
    fi
    waiting+=("$name")
  done
  pending=("${waiting[@]}")
  if (($(date +%s) > deadline)); then
    echo "Proxy builds did not finish before the timeout." >&2
    exit 1
  fi
  ((${#pending[@]} == 0)) || sleep 3
done
k -n builder-system logs builder-proxy-smoke > "$work/proxy.log"
k -n builder-system get networkpolicy buildkitd-egress -o json > "$work/egress.json"
python3 - "$work" "$proxy_ip" <<'PY'
import json
from pathlib import Path
import sys
work, proxy = Path(sys.argv[1]), sys.argv[2]
records = [json.loads(line) for line in (work / "proxy.log").read_text().splitlines() if line.startswith("{")]
git = [entry for entry in records if entry["host"] == "git.proxy-test.invalid"]
assert any(entry["user_agent"].startswith("git/zeabur-builder-") for entry in git), "controller Resolve did not use proxy"
assert any(entry["user_agent"].startswith("git/") and not entry["user_agent"].startswith("git/zeabur-builder-") for entry in git), "Git fetch did not use proxy"
for phase in ("dockerfile", "planner", "railpack"):
    assert any(entry["host"] == "probe.proxy-test.invalid" and "phase=" + phase in entry["path"]
               for entry in records), "missing proxy probe phase: " + phase
policy = json.loads((work / "egress.json").read_text())
assert any(any(peer.get("ipBlock", {}).get("cidr") == proxy + "/32" for peer in rule.get("to", []))
           and rule.get("ports") == [{"port": 3128, "protocol": "TCP"}]
           for rule in policy["spec"]["egress"]), "missing narrow proxy /32 TCP3128 exception"
assert any("10.0.0.0/8" in peer.get("ipBlock", {}).get("except", [])
           for rule in policy["spec"]["egress"] for peer in rule.get("to", [])), "private-range isolation was removed"
print("PROXY_E2E_OK: Resolve, Git fetch, Dockerfile RUN, planner and Railpack secret RUN; final image Env is clean.")
PY
