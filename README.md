<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/cover-dark.avif">
  <img alt="Zeabur Builder: push a commit, get an image digest; a Build for a Git repository and the status field that holds the image it built" src="docs/images/cover-light.avif">
</picture>

# Zeabur Builder

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
  certificate authority and registry path. The cache stays between builds, so
  `RUN --mount=type=cache` keeps its contents. An idle buildkitd scales to zero after 10 minutes.
- **Build variables as secrets.** `spec.env` reaches the build as BuildKit secrets, from a value
  or a Secret key.
- **Small.** One controller, a buildkitd per building namespace, one Job per build, three custom
  resources. The Job runs `buildctl` and needs no privileges.

Zeabur Builder builds images and stops there. Tests, deployment and log storage belong to your
pipeline, rollout tool and log collector.

## How it works

1. The controller resolves the `Build`'s ref to a commit over Git's smart HTTP or SSH protocol.
2. For a new commit it creates a `BuildRun`, and for the run a Job.
3. The Job fetches the commit, runs the planner if the builder has one, and runs `buildctl`
   against its namespace's buildkitd, which the controller starts on demand. `RUN` steps execute
   in buildkitd, cut off from Pods, Services and nodes.
4. The image is pushed; its digest and tags land in the `BuildRun` and the `Build`.

`kubectl get builds` shows each Build's builder, source, revision and readiness (`-o wide` adds
the image). `kubectl get buildruns` shows each run's phase, reason, start and finish. Build logs
are the Job's Pod logs.

## Triggers

- **Polling.** The controller asks the Git server for the ref every minute
  (`spec.triggers.pollInterval`).
- **Push webhooks.** GitHub, GitLab, Gitea, Forgejo, Bitbucket and
  [Standard Webhooks](https://www.standardwebhooks.com/) deliveries wake the `Build`s that follow
  the pushed repository and branch. The controller checks the signature and resolves the ref
  itself; a delivery never supplies the commit.
- **GitHub App.** Builds of private GitHub repositories use a short-lived installation token,
  read-only for the Job. Each commit gets a commit status. The App's push events wake the
  `Build`s without a per-Build secret, and such Builds poll every 10 minutes when webhooks are on.
- **By hand.** A new value in the annotation `builder.zeabur.com/rebuild` rebuilds the current
  commit. `spec.suspend` stops automatic builds.

`spec.concurrency` decides what a new commit does to a build in progress: `Replace` (default)
cancels it, `Queue` lets it finish.

### Webhooks

Enable the receiver in the chart (`webhooks.enabled`, then `webhooks.ingress` or
`webhooks.httpRoute`), and give each `Build` a secret:

```yaml
spec:
  triggers:
    webhook: {secretRef: {name: api-webhook, key: secret}}
```

Point the repository's webhook at `https://<host>/hooks/<provider>` with the same secret.
`<provider>` is `github`, `gitlab`, `gitea`, `forgejo`, `bitbucket` or `generic`. Polling stays on.

### GitHub App

A cluster admin creates the App once and stores it in the controller's namespace:

```sh
zeabur-builder github-app create --webhook-url https://<host>/hooks/github-app \
  > github-app.json   # opens the manifest flow on 127.0.0.1
kubectl -n builder-system apply -f github-app.json
kubectl -n builder-system create configmap github-app-installations \
  --from-literal=12345678=team-a,team-b   # installation id: namespaces allowed to use it
```

A `Build` then names the installation:

```yaml
spec:
  source:
    git: {url: https://github.com/acme/private-api}
    githubApp: {installationId: 12345678}
```

Other private repositories use `spec.source.credentialsRef`: a `kubernetes.io/basic-auth` Secret
with a token, or a `kubernetes.io/ssh-auth` Secret with a deploy key and `known_hosts`, labelled
`builder.zeabur.com/git-credentials: "true"`.

## Rollout

Rollout tools that watch a registry pick up the sortable tags: Flux image automation, Argo CD
Image Updater and Keel's polling. To skip the wait, a `Build` can tell Keel after each successful
build:

```yaml
spec:
  notify:
    - {type: keel, url: http://keel.keel.svc:9300/v1/webhooks/native}
```

The controller sends `{"name": <repository>, "tag": <tag>}`, tries three times, and records the
answer as an Event on the `Build`.

## Install

Requirements: Kubernetes 1.33 or later for buildkitd's user namespace (older clusters add
`--buildkitd-host-users=true` to `extraArgs`), a CNI that enforces NetworkPolicy, a registry, and
disk space on the nodes for each namespace's build cache (20 GiB by default). buildkitd runs
privileged, so its namespace must allow the Pod Security level `privileged`.

The chart installs the CRDs, the controller, the buildkitd config and egress policy, and the
`dockerfile` and `railpack` ClusterBuilders. A released chart pins its three images by digest
(see [Releases](#releases)):

```sh
helm install builder oci://ghcr.io/zeabur-oss/charts/zeabur-builder --version <version> \
  -n builder-system --create-namespace --set registry.host=<registry>
```

`deploy/chart/values.yaml` documents every value. The registry values:

- `registry.host`: where images go, as `<host>/<namespace>/<build>`.
- `registry.authPrefix`: for each namespace that builds, create a
  `kubernetes.io/dockerconfigjson` Secret named `registry-push-<namespace>` in `builder-system`,
  allowed to push below `<registry>/<namespace>/`. Set it to `""` for a registry that takes
  anonymous pushes.
- `registry.ca` or `registry.caFrom`: the CA of a registry with a private certificate.
- `registry.inCluster`: the registry's namespace, Pod labels and port, when it runs in the
  cluster, so that the egress policy lets buildkitd reach it.

With the [Zeabur Catalog](https://github.com/zeabur-oss/catalog)'s Zot, which takes anonymous
pushes over TLS:

```sh
helm install builder oci://ghcr.io/zeabur-oss/charts/zeabur-builder --version <version> \
  -n builder-system --create-namespace --set registry.host=zot.zot.svc:5000 \
  --set registry.authPrefix= --set registry.caFrom.namespace=zot \
  --set registry.caFrom.secret=zot-tls --set registry.inCluster.namespace=zot \
  --set registry.inCluster.podLabels.app\\.kubernetes\\.io/name=zot
```

Helm does not upgrade CRDs. Before `helm upgrade`, apply the new chart's `crds/crds.yaml`.

On a test cluster, `scripts/deploy-example.sh` installs the chart from this checkout with a test
registry and sample Builds ([CONTRIBUTING.md](CONTRIBUTING.md#tests-against-a-cluster)).

### Releases

A tag `v<version>` builds the images for linux/amd64 and linux/arm64, smokes them on a k3s cluster,
publishes `ghcr.io/zeabur-oss/builder`, `builder-job` and `builder-railpack` by digest, pins those
digests in the chart, pushes the chart to `oci://ghcr.io/zeabur-oss/charts/zeabur-builder` and signs
all of them keyless with Sigstore. A maintainer then publishes the draft release. To check an
image before you install it:

```sh
cosign verify ghcr.io/zeabur-oss/builder@sha256:<digest> \
  --certificate-identity https://github.com/zeabur-oss/builder/.github/workflows/release.yml@refs/tags/v<version> \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

## Status

Zeabur Builder has no release yet. The API is `v1alpha1`.

On a k3s 1.36 cluster, with `scripts/deploy-example.sh` and `scripts/example-isolation.sh` (CI runs
both, `scripts/ci.sh kubernetes`):

- Dockerfile and Railpack builds of public repositories, started by polling.
- Failures for a missing ref and a missing Dockerfile, in `SourceReady` and the `BuildRun`.
- One buildkitd per namespace, started on demand and scaled to zero when idle.
- Isolation between namespaces: another namespace's client certificate, registry path and Pods
  are refused. `RUN` reaches the internet and the registry; it cannot reach Services, the
  Kubernetes API or `169.254.169.254`.

In unit tests, and against a local SSH server for SSH: private repositories with
`credentialsRef`, push webhooks for every provider, the GitHub App, Keel notifications and
`builder.zeabur.com/rebuild`.

Not built yet:

- The `zbplan` builder type: the Job runs [zbplan](https://github.com/zeabur/zbplan), which
  writes a Dockerfile with an LLM, but plan pinning and approval (`spec.plan`) are not
  implemented and no zbplan image is built.
- `spec.triggers.onBaseImageChange`.
- Prometheus metrics.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues as described in
[SECURITY.md](SECURITY.md), which also describes the trust model.

## License

[Apache-2.0](LICENSE).
