#!/usr/bin/env bash
# Sets up the proof of concept on a cluster and waits for a Build's image (docs/poc.md):
#   scripts/poc.sh <context> [build...]       # default build: whoami
# Environment:
#   IMAGE_PREFIX, TAG     the images from scripts/build-images.sh (zeabur-builder, dev)
#   K3D_CLUSTER           when set, imports the images into this k3d cluster first
#   HOST_USERS            buildkitd's hostUsers (false: a user namespace per buildkitd)
#   IDLE                  seconds before an idle buildkitd scales to zero (600)
#   NAMESPACES            build namespaces given push credentials (builder-poc)
#   TIMEOUT               seconds to wait for each Build (900)
set -euo pipefail

context="${1:?usage: poc.sh <context> [build...]}"
shift
builds=("$@")
[[ ${#builds[@]} -gt 0 ]] || builds=(whoami)
prefix="${IMAGE_PREFIX:-zeabur-builder}"
tag="${TAG:-dev}"
host_users="${HOST_USERS:-false}"
idle="${IDLE:-600}"
read -r -a namespaces <<<"${NAMESPACES:-builder-poc}"
timeout="${TIMEOUT:-900}"
root="$(cd "$(dirname "$0")/.." && pwd)"
manifests="$root/deploy/poc"
registry=registry.builder-registry.svc.cluster.local:5000

k() { kubectl --context "$context" "$@"; }
render() {
  sed -e "s|__IMAGE_PREFIX__|$prefix|g" -e "s|__TAG__|$tag|g" \
    -e "s|__HOST_USERS__|$host_users|g" -e "s|__IDLE__|$idle|g" "$1"
}
ensure_namespace() { k create namespace "$1" --dry-run=client -o yaml | k apply -f - >/dev/null; }

if [[ -n ${K3D_CLUSTER:-} ]]; then
  k3d image import --cluster "$K3D_CLUSTER" \
    "$prefix/builder:$tag" "$prefix/builder-job:$tag" "$prefix/builder-railpack:$tag"
fi

k apply --server-side -f "$root/deploy/crds.json"
k wait --for condition=Established --timeout 60s \
  crd/builds.builder.zeabur.com crd/buildruns.builder.zeabur.com \
  crd/clusterbuilders.builder.zeabur.com
for ns in builder-system builder-registry "${namespaces[@]}"; do ensure_namespace "$ns"; done

# One registry user per build namespace, allowed to push only below `<namespace>/`; the
# controller hands each namespace's builds the Secret registry-push-<namespace>.
htpasswd=""
repositories='{"**": {"anonymousPolicy": ["read"]}}'
for ns in "${namespaces[@]}"; do
  # A rerun keeps the password: Zot sees a new htpasswd only once kubelet syncs the Secret.
  password=$(k -n builder-system get secret "registry-push-$ns" --ignore-not-found \
    -o jsonpath='{.data.\.dockerconfigjson}' | base64 -d | jq -r '.auths[].password' 2>/dev/null || true)
  [[ -n $password ]] || password=$(head -c 24 /dev/urandom | base64 | tr -d '/+=')
  htpasswd+=$(docker run --rm docker.io/library/httpd:2.4.65-alpine htpasswd -nbB "$ns" "$password")
  htpasswd+=$'\n'
  repositories=$(jq --arg ns "$ns" '. + {($ns + "/**"): {
    "policies": [{"users": [$ns], "actions": ["read", "create", "update", "delete"]}],
    "anonymousPolicy": ["read"]}}' <<<"$repositories")
  k -n builder-system create secret docker-registry "registry-push-$ns" --docker-server="$registry" \
    --docker-username="$ns" --docker-password="$password" --dry-run=client -o yaml | k apply -f - >/dev/null
done
config=$(jq -n --argjson repositories "$repositories" '{
  storage: {rootDirectory: "/var/lib/registry"},
  http: {address: "0.0.0.0", port: "5000", auth: {htpasswd: {path: "/etc/zot/auth/htpasswd"}},
    accessControl: {repositories: $repositories}},
  log: {level: "info"}}')
k -n builder-registry create configmap zot --from-literal=config.json="$config" \
  --dry-run=client -o yaml | k apply -f - >/dev/null
k -n builder-registry create secret generic zot-htpasswd --from-literal=htpasswd="$htpasswd" \
  --dry-run=client -o yaml | k apply -f - >/dev/null
k apply -f "$manifests/registry.yaml"
# A changed config needs a restart; a changed htpasswd is reloaded by Zot.
k -n builder-registry patch deployment registry --type merge -p \
  "{\"spec\":{\"template\":{\"metadata\":{\"annotations\":{\"zot-config\":\"$(sha256sum <<<"$config" | cut -c1-16)\"}}}}}"

k apply -f "$manifests/buildkitd.yaml"
render "$manifests/controller.yaml" | k apply -f -
# The controller restarts so that a new image with the same tag is picked up.
k -n builder-system rollout restart deployment/zeabur-builder
k -n builder-system rollout status deployment/zeabur-builder --timeout 120s
k -n builder-registry rollout status deployment/registry --timeout 120s

start=$(date +%s)
k apply -f "$manifests/builds.yaml" >/dev/null
for build in "${builds[@]}"; do
  echo "waiting for builder-poc/$build"
  deadline=$((start + timeout))
  while :; do
    image=$(k -n builder-poc get build "$build" -o jsonpath='{.status.latestImage}')
    if [[ -n $image ]]; then
      echo "$build: $image after $(($(date +%s) - start))s"
      break
    fi
    failed=$(k -n builder-poc get buildrun -l "builder.zeabur.com/build=$build" \
      -o jsonpath='{range .items[?(@.status.phase=="Failed")]}{.metadata.name}{" "}{.status.reason}{": "}{.status.message}{"\n"}{end}')
    if [[ -n $failed ]]; then
      echo "$build failed: $failed" >&2
      exit 1
    fi
    if (($(date +%s) > deadline)); then
      echo "$build: no image after ${timeout}s" >&2
      k -n builder-poc get build,buildrun,job,pod >&2
      exit 1
    fi
    sleep 5
  done
done
k -n builder-poc get build,buildrun
