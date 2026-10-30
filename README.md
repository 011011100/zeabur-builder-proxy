<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/cover-dark.avif">
  <img alt="Zeabur Builder: push a commit, get an image digest; a Build for a Git repository and the status field that holds the image it built" src="docs/images/cover-light.avif">
</picture>

# Zeabur Builder

This repository's `mingxing-proxy` branch is a maintained **MingXing proxy variant**, based on
upstream v0.1.1 (`8fd8b6e9b6480cb2ea3fb28d250b4b71da663b8a`), not an official Zeabur release.
It adds unauthenticated HTTP proxy support to Git ref resolution, GitHub API requests, fetch
Jobs, Railpack planning and BuildKit builds. TLS certificate verification remains enabled.
The chart's `proxy.url` accepts `http://IPv4:port`; `proxy.noProxy` retains the internal cluster
addresses. Egress exceptions are limited to that proxy's `/32` and TCP port.

The `MingXing proxy build` workflow runs unit/security tests and disposable K3s proxy builds,
then exports digest-checked offline controller/job images and an installation bundle. The
bundle's `install.py --check` is read-only; `--apply` is restricted to the intended new server
and creates a private rollback backup. Existing GitHub App credentials and business workloads
are preserved. Official Builder upgrades do not incorporate this variant automatically.

**Push a commit, get an image digest.** A Kubernetes operator that builds container images from
Git with BuildKit and hands the result to your rollout tool.

## What it does

A `Build` names a Git repository and a builder:

```yaml
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: whoami, namespace: team-a}
spec:
  source: {git: {url: https://github.com/traefik/whoami}}
  builderRef: dockerfile
```

For every new commit on the ref, the controller builds an image, pushes it to
`<registry.host>/<namespace>/<name>` and writes it to `status.latestImage` as
`<repository>:<tag>@<digest>`. Leave out `ref` to follow the repository's default branch;
`status.resolvedRef` says which branch that was.

- **Two builders.** `dockerfile` builds your Dockerfile. `railpack` plans the build with
  [Railpack](https://railpack.com) for repositories without one; its images run as uid 1000, so
  they start under Pod Security `restricted`.
- **Tags rollout tools can sort.** `main-1761058800-3e1a9c2` (branch, Unix time, commit), one per
  build, newest last. `spec.output.tagTemplate` and `extraTags` change them.
- **One buildkitd per namespace.** Each namespace that builds gets its own buildkitd, cache,
  certificate authority and registry path. The cache stays on buildkitd's volume between builds,
  so `RUN --mount=type=cache` keeps its contents; it never goes to the registry. An idle
  buildkitd scales to zero after 10 minutes.
- **Build variables as secrets.** `spec.env` reaches the build as BuildKit secrets, from a value
  or a Secret key.
- **Uploaded folders.** `spec.source.archive` builds a folder that a client pushed to the
  registry as an OCI artifact, by digest ([archive sources](docs/builds/archive-source.mdx)).
- **Small.** One controller, a buildkitd per building namespace, one Job per build, three custom
  resources. The Job runs `buildctl` and needs no privileges.

Zeabur Builder builds images and stops there. Tests, deployment and log storage belong to your
pipeline, rollout tool and log collector.

## Documentation

- [Install](docs/get-started/install.mdx) and [your first Build](docs/get-started/first-build.mdx)
- [Triggers](docs/builds/triggers.mdx), [private repositories](docs/builds/private-repos.mdx), [archive sources](docs/builds/archive-source.mdx), [rollout](docs/builds/rollout.mdx) and [external registries](docs/builds/external-registries.mdx)
- [Registry tokens](docs/operations/registry-tokens.mdx), [controller permissions](docs/operations/controller-permissions.mdx) and [releases](docs/operations/releases.mdx)
- [How it works](docs/reference/how-it-works.mdx), [status](docs/reference/status.mdx) and [security](docs/reference/security.mdx)

## Status

Zeabur Builder has no release yet. The API is `v1alpha1`. See [status](docs/reference/status.mdx) for what runs on `main`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues as described in
[SECURITY.md](SECURITY.md), which also describes the trust model.

## License

[Apache-2.0](LICENSE).
