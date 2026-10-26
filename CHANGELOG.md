# Changelog

All notable changes to Zeabur Builder. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- The `zeabur-builder` Helm chart installs everything a cluster needs: the CRDs, the controller,
  the `dockerfile` and `railpack` ClusterBuilders, buildkitd's configuration and its egress
  policy. Registry settings are chart values.
- Images for linux/amd64 and linux/arm64, published by digest and signed with Sigstore; the chart
  pins them by digest.
- A ValidatingAdmissionPolicy in the chart limits the controller's ServiceAccount to build Jobs
  and buildkitd StatefulSets (README, Controller permissions). `buildkitdImage` sets buildkitd's
  image.
- `spec.output.pushSecretRef`: a push Secret in the `Build`'s namespace for an image on another
  registry.
- `registry.tokenKey.namespace` and `registry.tokenKey.secret` are required. The chart fails at
  install and render time without them, not after an upload waits for a token that never comes.
  The controller requires `ZEABUR_BUILDER_REGISTRY_TOKEN_KEY` and exits at start without it.
  The default registry takes pushes only with the controller's tokens; anonymous pushes to
  `registry.host` are refused.
