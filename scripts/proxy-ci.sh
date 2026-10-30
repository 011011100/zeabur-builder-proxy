#!/usr/bin/env bash
# Compile and test the custom proxy build on a disposable GitHub Linux runner, never the Mac.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
mkdir -p target/proxy-artifacts

# Retain an actionable formatting patch when a contributor has no local Rust toolchain.
cargo fmt --all
git diff --binary > target/proxy-artifacts/format.patch
if [[ -s target/proxy-artifacts/format.patch ]]; then
  echo "Rust formatting differs; apply the uploaded format.patch and rerun." >&2
  exit 1
fi
cargo lint
cargo nextest run --workspace --all-features --locked --no-tests=warn
cargo deny --locked check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items --locked
nixfmt --check flake.nix
./scripts/check-levels.sh
./scripts/check-chart.sh
shellcheck scripts/*.sh
actionlint
python3 -m unittest discover -s deploy/proxy -p 'test_*.py'
python3 -m unittest discover -s tests/proxy -p 'test_*.py'

version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "zeabur-builder") | .version')
source_commit=$(git rev-parse HEAD)
tag="$version-${source_commit:0:12}"
./scripts/build-images.sh zeabur-builder "$tag"
# Existing upstream end-to-end checks run in a disposable k3d cluster and cover unchanged
# Dockerfile/Railpack builds, admission restrictions, cross-namespace isolation and archives.
export KUBECONFIG="$root/target/ci/kubeconfig"
trap 'k3d cluster delete zeabur-builder >/dev/null 2>&1 || true' EXIT
ZEABUR_CI_SHELL=1 ZEABUR_CI_KEEP=1 ZEABUR_CI_PROXY_TESTS=1 ZEABUR_CI_IMAGE_TAG="$tag" \
  CI_ARTIFACTS="$root/target/proxy-artifacts/cluster" ./scripts/ci.sh kubernetes
cp -R target/ci/proxy-smoke target/proxy-artifacts/cluster/proxy-smoke

prefix="ghcr.io/${GITHUB_REPOSITORY_OWNER,,}/zeabur-builder-proxy"
mkdir -p target/proxy-oci products
for image in builder builder-job; do
  ref="$prefix/$image:$tag"
  docker tag "zeabur-builder/$image:$tag" "$ref"
  skopeo copy "docker-daemon:$ref" "oci:target/proxy-oci/$image:$tag"
done
helm package deploy/chart --destination products
yq eval -o=json deploy/chart/values.yaml > products/default-values.json
python3 scripts/package-proxy.py \
  --images target/proxy-oci \
  --controller-ref "$prefix/builder:$tag" \
  --job-ref "$prefix/builder-job:$tag" \
  --chart "products/zeabur-builder-$version.tgz" \
  --defaults products/default-values.json \
  --version "$version" --commit "$source_commit" \
  --output target/proxy-artifacts
python3 scripts/check-proxy-import.py "target/proxy-artifacts/zeabur-builder-proxy-$version-linux-amd64"
