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
`status.resolvedRef` says which branch that was. The commit found last wins: a build of an older
commit that finishes later does not replace `status.latestImage`.

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
  registry as an OCI artifact, by digest ([docs/archive-source.md](docs/archive-source.md)).
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

The chart installs the CRDs, the controller, the buildkitd config and egress policy, the
`dockerfile` and `railpack` ClusterBuilders, and the admission policy that limits the controller
([Controller permissions](#controller-permissions)). A released chart pins its three images by digest
(see [Releases](#releases)):

```sh
helm install builder oci://ghcr.io/zeabur-oss/charts/zeabur-builder --version <version> \
  -n builder-system --create-namespace --set registry.host=<registry>
```

`deploy/chart/values.yaml` documents every value. The registry values:

- `registry.host`: where images go, as `<host>/<namespace>/<build>`.
- `registry.tokenKey`: the Secret whose key signs tokens the registry trusts. See
  [Registry tokens](#registry-tokens).
- `registry.ca` or `registry.caFrom`: the CA of a registry with a private certificate.
- `registry.inCluster`: the registry's namespace, Pod labels and port, when it runs in the
  cluster, so that the egress policy lets buildkitd reach it.

With the [Zeabur Catalog](https://github.com/zeabur-oss/catalog)'s Zot, which takes pushes only
with a token signed by `zot-token-signer`:

```sh
helm install builder oci://ghcr.io/zeabur-oss/charts/zeabur-builder --version <version> \
  -n builder-system --create-namespace --set registry.host=zot.zot.svc:5000 \
  --set registry.caFrom.namespace=zot --set registry.caFrom.secret=zot-tls \
  --set registry.inCluster.namespace=zot \
  --set registry.inCluster.podLabels.app\\.kubernetes\\.io/name=zot \
  --set registry.tokenKey.namespace=zot --set registry.tokenKey.secret=zot-token-signer
```

The controller itself must reach the registry too, to delete repositories: allow it in any
NetworkPolicy of the registry's namespace.

Without `registry.tokenKey`, pushes to `registry.host` are anonymous: only for a test registry
that takes them.

Helm does not upgrade CRDs. Before `helm upgrade`, apply the new chart's `crds/crds.yaml`.

On a test cluster, `scripts/deploy-example.sh` installs the chart from this checkout with a test
registry and sample Builds ([CONTRIBUTING.md](CONTRIBUTING.md#tests-against-a-cluster)).

### Registry tokens

With `registry.tokenKey`, the registry trusts one Ed25519 key, and only the controller reads it.
The controller signs a token for each use; the token names each repository exactly and expires:

- **A run** gets push and pull on its image repository, `<namespace>/<build>`, and pull on its
  archive repository, `<namespace>/<build>/source`. The token lasts the Job's deadline plus ten
  minutes. A `Build` whose image is on `registry.host` but not at its own repository fails.
  Images on other registries use a push Secret ([External registries](#external-registries)).
- **An upload** to an archive source: a new value of the annotation
  `builder.zeabur.com/upload-token-request` on the `Build` gets the Secret `<build>-upload`, with
  a token that pushes to `<namespace>/<build>/source` for 15 minutes. `archive.digest` may stay
  unset until the upload is done; the `Build` then waits with `SourceReady` `WaitingForUpload`.

With kubectl and [ORAS](https://oras.land), as anyone who may edit the `Build`:

```sh
kubectl -n team-a annotate build/api --overwrite builder.zeabur.com/upload-token-request=u1
kubectl -n team-a wait build/api --for=jsonpath='{.status.uploadToken.request}'=u1
kubectl -n team-a get secret api-upload -o jsonpath='{.data.\.dockerconfigjson}' |
  base64 -d > auth.json
oras push --registry-config auth.json zot.zot.svc:5000/team-a/api/source:upload-1 \
  --artifact-type application/vnd.zeabur.source.v1+tar \
  source.tar.gz:application/vnd.oci.image.layer.v1.tar+gzip
```

Then set `archive.digest` to the digest `oras push` prints. ORAS must reach `zot.zot.svc:5000`
and trust the CA in the Secret `zot/zot-tls`. From outside the cluster, through
`kubectl -n zot port-forward svc/zot 5000:5000`, give the Secret's `token` key to
`localhost:5000` and keep TLS:

```sh
printf '{"auths":{"localhost:5000":{"registrytoken":"%s"}}}' \
  "$(kubectl -n team-a get secret api-upload -o jsonpath='{.data.token}' | base64 -d)" > auth.json
kubectl -n zot get secret zot-tls -o jsonpath='{.data.ca\.crt}' | base64 -d > ca.crt
oras push --registry-config auth.json --plain-http=false --ca-file ca.crt \
  localhost:5000/team-a/api/source:upload-1 ...
```

An admin can push an image without a `Build` with a token from the controller's Pod:

```sh
TOKEN=$(kubectl -n builder-system exec deploy/zeabur-builder -- \
  /zeabur-builder registry-token --repository zeabur_admin/tools --push)
printf '{"auths":{"zot.zot.svc:5000":{"registrytoken":"%s"}}}' "$TOKEN" > auth.json
oras push --registry-config auth.json zot.zot.svc:5000/zeabur_admin/tools:v1 ./file
```

Keep such images out of the `<namespace>/<name>` layout, for example with a first segment that
holds `_` or `.`, which no namespace name has. Otherwise the cleanup below deletes them.

**Cleanup.** When a `Build` is deleted, by itself or with its namespace, the controller deletes
`<namespace>/<build>/source` and `<namespace>/<build>`, with every tag. It acts on the delete
event, and a sweep of the registry catalog at start and every ten minutes catches what it
missed. So images go within one sweep after the deletion, and a namespace never waits for the
controller: there is no finalizer. A new `Build` first deletes what an earlier `Build` of its
name left, before any token names those repositories, so the new owner never sees the old
images or sources. If the controller is not installed, images stay until it is installed again;
its first sweep deletes them. While it runs, an admin deletes an image by hand with a token from
`registry-token --delete` and `DELETE /v2/<repository>/manifests/<digest>`.

Earlier versions pushed each build's full cache to the tag `buildcache` of the image repository,
where anyone who may pull could read it. The first sweep after each start deletes that tag in
every image repository; the registry's garbage collection then removes the layers.

Deleting every tag of an image repository by hand also deletes the repositories below it in Zot,
such as its archives in `<repository>/source`.

**Key rotation.** zot reads the public key once at start. Delete the Secret (with the catalog's
Zot, `kubectl -n zot delete secret zot-token-signer`), wait for cert-manager to issue a new one,
then restart the registry (`kubectl -n zot rollout restart statefulset/zot`). Tokens signed with
the old key stop working at once; running builds fail and can be rebuilt.

**Permissions.** With `registry.tokenKey`, the chart adds a Role in the key's namespace: `get` on
that one Secret. The controller also writes the `<build>-upload` Secrets with its existing
Secret permissions.

**Known limits.**

- A token stays valid until it expires, even after its `Build` is deleted. A run's token can push
  again after the cleanup; the next sweep deletes what it pushed.
- A token names repositories, not namespace UIDs. If a namespace is deleted and someone creates
  a namespace of the same name, a run token of the old namespace can push to the new one's
  repositories until it expires: at most 70 minutes with the default Job deadline (60 minutes
  plus 10). Wait that long before you give a deleted namespace's name to another team.
- Anyone in the cluster who can reach the registry can pull every image, as the nodes do. Keep
  secrets out of images and give them to the workload as Secrets.

### External registries

An image outside `registry.host` (`spec.output.image: ghcr.io/acme/api`) is pushed with a push
Secret in the `Build`'s own namespace. Create a `kubernetes.io/dockerconfigjson` Secret with the
label `builder.zeabur.com/push-credentials: "true"` and name it in `spec.output.pushSecretRef`:

```sh
kubectl -n team-a create secret docker-registry ghcr-push \
  --docker-server=ghcr.io --docker-username=acme --docker-password=<token>
kubectl -n team-a label secret ghcr-push builder.zeabur.com/push-credentials=true
```

```yaml
spec:
  output:
    image: ghcr.io/acme/api
    pushSecretRef: {name: ghcr-push}
```

A run waits with `RegistryCredentialsMissing` while the Secret is missing or has no label. Who may
push where is up to the external registry's account. With `registry.tokenKey`, the entry for
`registry.host` in the Secret is replaced by the run's token.

### Controller permissions

Treat the controller as a cluster administrator. Its ServiceAccount can create Jobs in every
namespace, read and write Secrets in every namespace, and create StatefulSets in `builder-system`,
where Pods may be privileged. Whoever takes over the controller or its token can do the same.

The chart's ValidatingAdmissionPolicies (`<release>-zeabur-builder-jobs` and
`<release>-zeabur-builder-buildkitd`) limit what that ServiceAccount may create:

- A Job only in the shape of a build Job: the images `jobImage`, `railpackImage` and
  `admissionPolicy.extraImages`; no privilege, added capabilities, host network, PID, IPC, port
  or path; the namespace's `default` ServiceAccount without a token; and never in a `kube-*`
  namespace, the controller's namespace, the registry's namespaces or
  `admissionPolicy.extraDeniedNamespaces`.
- A StatefulSet only in the controller's namespace and only with `buildkitdImage`, without host
  namespaces, ports or paths.

The policies stop a stolen token from running a privileged Pod, from using another
ServiceAccount, and from building in a system namespace. They do not stop it from reading the
Secrets of a namespace where builds may run: a build Job can mount any of them. They are plain
Kubernetes objects:

```sh
kubectl get validatingadmissionpolicy,validatingadmissionpolicybinding
kubectl edit validatingadmissionpolicy builder-zeabur-builder-jobs
```

A `helm upgrade` writes them again from the chart values. A ClusterBuilder of your own with
another planner image needs that image in `admissionPolicy.extraImages`.

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

On a k3s 1.36 cluster, with `scripts/deploy-example.sh`, `scripts/example-isolation.sh`,
`scripts/example-admission.sh` and `scripts/example-archive.sh` (CI runs them,
`scripts/ci.sh kubernetes`):

- Dockerfile and Railpack builds of public repositories, started by polling.
- Failures for a missing ref and a missing Dockerfile, in `SourceReady` and the `BuildRun`.
- One buildkitd per namespace, started on demand and scaled to zero when idle.
- Isolation between namespaces: another namespace's client certificate, registry path and Pods
  are refused, and so is a push without a token. `RUN` reaches the internet and the registry; it cannot reach Services, the
  Kubernetes API or `169.254.169.254`.
- An archive source built from its digest; the same digest in a `Build` of another namespace or
  of another name fails at the source.
- The admission policy: the controller's ServiceAccount creates build Jobs and buildkitd, and a
  privileged Job, a Job in `kube-system`, with another image, a host path, the host network or
  another ServiceAccount, or another StatefulSet is refused (`scripts/example-admission.sh`).

In unit tests, and against a local SSH server for SSH: private repositories with
`credentialsRef`, push webhooks for every provider, the GitHub App, Keel notifications and
`builder.zeabur.com/rebuild`.

Not built yet:

- Prometheus metrics.

Open issues:

- The controller's ClusterRole has `get`, `create`, `update` and `delete` on Secrets in every
  namespace, to read the Secrets Builds name and write its copies. It needs only the Secrets
  labelled `builder.zeabur.com/git-credentials` or `builder.zeabur.com/push-credentials` and its
  own copies.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues as described in
[SECURITY.md](SECURITY.md), which also describes the trust model.

## License

[Apache-2.0](LICENSE).
