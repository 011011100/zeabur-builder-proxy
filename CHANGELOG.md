# Changelog

All notable changes to Zeabur Builder. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [0.0.1]

The first release.

- The `zeabur-builder` Helm chart installs everything a cluster needs: the CRDs, the controller,
  the `dockerfile` and `railpack` ClusterBuilders, buildkitd's configuration and its egress
  policy. Registry settings are chart values.
- Images for linux/amd64 and linux/arm64, published by digest and signed with Sigstore; the chart
  pins them by digest.
