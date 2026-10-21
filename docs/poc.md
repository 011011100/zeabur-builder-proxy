# Proof of concept

The Dockerfile path with a polling trigger, and the Railpack builder, on one Kubernetes cluster:
the controller resolves a `Build`'s ref over Git smart HTTP, creates a `BuildRun` for each new
commit and a Job for each run; the Job fetches the commit and runs `buildctl` against an in-cluster
buildkitd picked by rendezvous routing, pushes to an in-cluster registry, and the digest ends up in
`BuildRun.status` and `Build.status.latestImage`.

## Status

| Part | State |
| --- | --- |
| Controller, Job wrapper, images | Built; 51 unit tests pass |
| Manifests, `scripts/poc.sh` | Written; applied once on the test cluster |
| Dockerfile build end to end | **Not verified yet** (see [Test cluster](#test-cluster)) |
| Railpack build end to end | **Not verified yet** |

## Requirements

- A cluster with Kubernetes 1.33 or newer for `hostUsers: false` (user namespaces), or
  `HOST_USERS=true` on older ones.
- **Disk:** at least 8 GB free on the node beyond what it already uses. buildkitd's cache, the
  base images it pulls and the registry all share the node's disk; kubelet evicts every Pod on
  the node when free space drops under its threshold.
- Network access from the cluster to the Git host, Docker Hub and ghcr.io.
- `nix develop` (kubectl, k3d) and Docker.

## Reproduce

```sh
nix develop
scripts/build-images.sh zeabur-builder dev     # three images for this machine's architecture
# k3d: import them, then set up and wait for the whoami Build
K3D_CLUSTER=<cluster> scripts/poc.sh <context> whoami
# the Railpack sample
scripts/poc.sh <context> hello-python
```

`scripts/poc.sh` applies `deploy/crds.json` and the manifests in `deploy/poc/`, then waits for each
named Build's `status.latestImage` and fails on the first failed `BuildRun`. Its environment
variables are listed at the top of the script.

To see the result:

```sh
kubectl -n builder-poc get build -o wide        # Ready, Revision, Image
kubectl -n builder-poc get buildrun             # Phase, Reason, Started, Finished
kubectl -n builder-poc logs job/<buildrun>      # buildctl's progress
```

The namespaces are `builder-system` (controller and buildkitd), `builder-registry` and
`builder-poc` (sample Builds). To remove everything:

```sh
kubectl delete namespace builder-poc builder-system builder-registry
kubectl delete clusterrole,clusterrolebinding zeabur-builder
kubectl delete clusterbuilder dockerfile railpack
kubectl delete -f deploy/crds.json
```

## Sample Builds

| Build | Source | Expected |
| --- | --- | --- |
| `whoami` | `traefik/whoami@master`, its Dockerfile | Succeeded |
| `hello-python` | `dockersamples/helloworld-demo-python@main`, no Dockerfile | Succeeded through Railpack |
| `no-such-ref` | a branch that does not exist | `SourceReady=False`, `RefNotFound`, no BuildRun |
| `no-such-dockerfile` | a Dockerfile path that does not exist | BuildRun Failed |

## Decisions

- **`hostUsers: false` by default.** Rootful `moby/buildkit:v0.34.1` with `privileged: true` in a
  user namespace ran a `RUN` step on the test cluster (k3s v1.36.5, containerd 2.3.4, kernel 7.0,
  arm64), with root in the container mapped to an unprivileged host range. `HOST_USERS=true` falls
  back to a plainly privileged container.
- **Provenance:** jobspec passes no `attest:provenance` option, so buildctl's default applies. The
  plan's intended setting is `mode=min`; it is not set yet.
- **Registry:** `registry:2.8.3` over plain HTTP in `builder-registry`, without authentication or
  persistence (`emptyDir`). buildkitd marks it insecure in its config, so jobspec has no special
  case. Each run gets a `{run}-registry` Secret with an empty `auths` unless
  `--registry-auth-secret` names one.
- **mTLS to buildkitd:** the controller creates a CA, a wildcard server certificate for the
  headless Service and one client certificate on first start, and copies the client Secret into
  build namespaces.
- **buildkitd cache:** one PVC per instance; GC keeps the cache under 80% of the volume and frees
  space whenever less than 20% of the disk is free, ahead of kubelet's eviction threshold.

## Test cluster

The single-node k3d cluster on the `zeabur-test` VM (2 CPU, 4 GB, 62 GB disk). Image builds took
3 min 31 s on an aarch64 Mac (Docker, Nix container, cold Cargo cache).

The first run did not reach a build. Loading the images and pulling buildkitd brought the VM disk
under kubelet's eviction threshold (DiskPressure at about 3 GB free). Kubelet tainted the node and
evicted every Pod on it, other workloads included, and its image GC removed the imported images.
The PoC workloads are scaled to zero until the node has at least 8 GB free.

## Known gaps

- End-to-end results, digests and timings are still missing (above).
- Git: public HTTPS repositories only; `credentialsRef` reports `CredentialsMissing`.
- Triggers: polling per Build only; no webhooks, no repository-level poller, no rebuild annotation.
- Controller: a single replica without leader election; no quotas or admission limits.
- Certificates: no rotation, and the CA key is discarded.
- RBAC: Secrets cluster-wide, for copying the client certificate and push credentials.
- Registry: plain HTTP without authentication; images are lost when its Pod restarts.
- Not in scope: zbplan (M4), the GitHub App, rollout integrations, multi-tenant hardening.
