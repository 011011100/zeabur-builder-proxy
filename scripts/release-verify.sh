#!/usr/bin/env bash
# Checks a downloaded draft release as someone installing it would, with no registry login and
# nothing rebuilt: the signed checksums, every signature and provenance attestation, each image
# index, and the chart's pinned images. Run inside `nix develop`; `gh` must be signed in (the
# attestations are read from GitHub's API).
#   scripts/release-verify.sh v0.0.2 downloaded \
#     https://github.com/zeabur-oss/builder/.github/workflows/release.yml@refs/tags/v0.0.2
set -euo pipefail

if [[ $# != 3 ]]; then
  echo "usage: release-verify.sh <vVERSION> <directory> <certificate-identity>" >&2
  exit 2
fi
version="$1"
directory="$(cd "$2" && pwd)"
identity="$3"
: "${IN_NIX_SHELL:?run inside nix develop}"
if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ || -z "$identity" ]]; then
  echo "expected a version like v0.0.2 and the exact signing identity" >&2
  exit 2
fi
plain="${version#v}"
repository="${GITHUB_REPOSITORY:-zeabur-oss/builder}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
# A fresh, empty registry configuration: a publisher's login must not hide a private package.
export DOCKER_CONFIG="$work/docker" HELM_REGISTRY_CONFIG="$work/docker/config.json"
mkdir -p "$DOCKER_CONFIG"
certificate=(--certificate-identity "$identity" --certificate-oidc-issuer https://token.actions.githubusercontent.com)
attestation=(--repo "$repository" --cert-identity "$identity")
chart_file="zeabur-builder-$plain.tgz"

# 1. The checksums: signed by the release workflow at this tag, and covering every asset.
cosign verify-blob "${certificate[@]}" --bundle "$directory/SHA256SUMS.sigstore.json" "$directory/SHA256SUMS"
listed="$(awk '{ print $2 }' "$directory/SHA256SUMS" | sort)"
if [[ "$listed" != "$(printf '%s\n' digests.txt "$chart_file" | sort)" ]]; then
  echo "SHA256SUMS does not list exactly the release assets: $listed" >&2
  exit 1
fi
(cd "$directory" && sha256sum --check --strict SHA256SUMS)
gh attestation verify "$directory/$chart_file" "${attestation[@]}" >/dev/null

# 2. Each image and the chart: the digests that the signed digests.txt records, signed and
# attested. Exactly one line of each kind.
recorded() { # $@ = the kind's fields
  local refs
  refs="$(awk -v kind="$*" '{ ref = $NF; $NF = ""; sub(/ +$/, "") } $0 == kind { print ref }' "$directory/digests.txt")"
  [[ "$refs" =~ ^[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]] || { echo "digests.txt: no single digest for $*" >&2; return 1; }
  echo "$refs"
}
values="$(helm show values "$directory/$chart_file")"
for name in builder builder-job builder-railpack; do
  index="$(recorded "$name" index)"
  amd64="$(recorded "$name" linux/amd64)"
  arm64="$(recorded "$name" linux/arm64)"
  for ref in "$index" "$amd64" "$arm64"; do
    cosign verify "${certificate[@]}" "$ref" >/dev/null
    echo "signature verified: $ref"
  done
  platforms="$(skopeo inspect --raw "docker://$index" |
    jq -r '[.manifests[] | "\(.platform.os)/\(.platform.architecture) \(.digest)"] | sort | join(",")')"
  if [[ "$platforms" != "linux/amd64 ${amd64#*@},linux/arm64 ${arm64#*@}" ]]; then
    echo "$name: the index holds $platforms, not the two recorded images" >&2
    exit 1
  fi
  gh attestation verify "oci://$index" "${attestation[@]}" >/dev/null
  echo "provenance verified: $index"
  # 3. The chart installs this index by default.
  case "$name" in
    builder) key=image ;;
    builder-job) key=jobImage ;;
    builder-railpack) key=railpackImage ;;
  esac
  pinned="$(KEY="$key" yq '.[strenv(KEY)].repository + "@" + .[strenv(KEY)].digest' <<<"$values")"
  [[ "$pinned" == "$index" ]] || { echo "the chart's $key is $pinned, not $index" >&2; exit 1; }
done
chart="$(recorded chart)"
cosign verify "${certificate[@]}" "$chart" >/dev/null
gh attestation verify "oci://$chart" "${attestation[@]}" >/dev/null
echo "signature and provenance verified: $chart"

# 4. The chart in the registry is the checksummed asset.
mkdir "$work/chart"
helm pull "oci://$chart" --destination "$work/chart"
cmp "$work/chart/zeabur-builder@sha256-${chart##*@sha256:}.tgz" "$directory/$chart_file"
helm template builder "$directory/$chart_file" --set registry.host=registry.example:5000 >/dev/null
echo "release $version verified"
