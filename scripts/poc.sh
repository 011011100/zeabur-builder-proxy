#!/usr/bin/env bash
# Sets up the proof of concept on a cluster and waits for a Build's image (docs/poc.md):
#   scripts/poc.sh <context> [build...]       # default build: whoami
# Environment:
#   IMAGE_PREFIX, TAG     the images from scripts/build-images.sh (zeabur-builder, dev)
#   K3D_CLUSTER           when set, imports the images into this k3d cluster first
#   HOST_USERS            buildkitd's hostUsers (false: a user namespace per instance)
#   BUILDKITD_REPLICAS    the size of the buildkitd pool (1)
#   TIMEOUT               seconds to wait for each Build (900)
set -euo pipefail

context="${1:?usage: poc.sh <context> [build...]}"
shift
builds=("$@")
[[ ${#builds[@]} -gt 0 ]] || builds=(whoami)
prefix="${IMAGE_PREFIX:-zeabur-builder}"
tag="${TAG:-dev}"
host_users="${HOST_USERS:-false}"
replicas="${BUILDKITD_REPLICAS:-1}"
timeout="${TIMEOUT:-900}"
root="$(cd "$(dirname "$0")/.." && pwd)"
manifests="$root/deploy/poc"

k() { kubectl --context "$context" "$@"; }
render() {
  sed -e "s|__IMAGE_PREFIX__|$prefix|g" -e "s|__TAG__|$tag|g" \
    -e "s|__HOST_USERS__|$host_users|g" -e "s|__REPLICAS__|$replicas|g" "$1"
}

if [[ -n ${K3D_CLUSTER:-} ]]; then
  k3d image import --cluster "$K3D_CLUSTER" \
    "$prefix/builder:$tag" "$prefix/builder-job:$tag" "$prefix/builder-railpack:$tag"
fi

k apply --server-side -f "$root/deploy/crds.json"
k wait --for condition=Established --timeout 60s \
  crd/builds.builder.zeabur.com crd/buildruns.builder.zeabur.com \
  crd/clusterbuilders.builder.zeabur.com
k apply -f "$manifests/registry.yaml"
k create namespace builder-system --dry-run=client -o yaml | k apply -f -
render "$manifests/controller.yaml" | k apply -f -
# The controller restarts so that a new image with the same tag is picked up.
k -n builder-system rollout restart deployment/zeabur-builder
k -n builder-system rollout status deployment/zeabur-builder --timeout 120s
until k -n builder-system get secret buildkitd-tls >/dev/null 2>&1; do sleep 1; done
render "$manifests/buildkitd.yaml" | k apply -f -
k -n builder-system rollout status statefulset/buildkitd --timeout 300s
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
