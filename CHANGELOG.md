# Changelog

All notable changes to Zeabur Builder. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- A ValidatingAdmissionPolicy in the chart limits the controller's ServiceAccount to build Jobs
  and buildkitd StatefulSets (README, Controller permissions). `buildkitdImage` sets buildkitd's
  image.
- `spec.output.pushSecretRef`: a push Secret in the `Build`'s namespace for an image on another
  registry.

### Removed

- `registry.authPrefix` and the `registry-push-<namespace>` Secrets. The default registry takes
  pushes with the controller's tokens (`registry.tokenKey`).
- The registry build cache (`<image>:buildcache`, `mode=max`). The first sweep after each start
  deletes the `buildcache` tags that earlier builds pushed.

## [0.0.1]

The first release.

- The `zeabur-builder` Helm chart installs everything a cluster needs: the CRDs, the controller,
  the `dockerfile` and `railpack` ClusterBuilders, buildkitd's configuration and its egress
  policy. Registry settings are chart values.
- Images for linux/amd64 and linux/arm64, published by digest and signed with Sigstore; the chart
  pins them by digest.
