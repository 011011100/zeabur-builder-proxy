# Proof of concept

The Dockerfile path with a polling trigger, and the Railpack builder, on one Kubernetes cluster:
the controller resolves a `Build`'s ref over Git smart HTTP, creates a `BuildRun` for each new
commit and a Job for each run; the Job fetches the commit and runs `buildctl` against an in-cluster
buildkitd picked by rendezvous routing, pushes to an in-cluster registry, and the digest ends up in
`BuildRun.status` and `Build.status.latestImage`.

## Status

| Part | State |
| --- | --- |
| Dockerfile build, polling trigger | Verified on the test cluster |
| Railpack build (M1) | Verified on the test cluster |
| Failures: missing ref, missing Dockerfile | Verified: `SourceReady=False RefNotFound`; `BuildRun` Failed `BuildFailed` |
| Unit tests | 52 pass |

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
  user namespace ran every build above on the test cluster; buildkitd's `/proc/self/uid_map` was
  `0 3195928576 65536`, root in the container mapped to an unprivileged host range.
  `HOST_USERS=true` falls back to a plainly privileged container.
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

The single-node k3d cluster on the `zeabur-test` VM: k3s v1.36.5, containerd 2.3.4, kernel 7.0,
arm64, 2 CPU, 4 GB. Images were built on an aarch64 Mac (3 min 31 s with a cold Cargo cache) and
imported with `k3d image import`. `scripts/poc.sh` started all four sample Builds at once, so the
builds shared buildkitd and the two CPUs.

### Results

| Build | Revision | Image |
| --- | --- | --- |
| `whoami` | `1ce75d0` | `…/builder-poc/whoami:master-1792608027-1ce75d0@sha256:687e297d70a419afb58641107ba75f3235dcb7abe33422ea8a93d259d5ffd73f` |
| `hello-python` | `62c0894` | `…/builder-poc/hello-python:main-1792608027-62c0894@sha256:3894b8d8dd5d57341368a0fa25fd224beea3cc831ca89ab21eecd8683e777337` |

`…` is `registry.builder-registry.svc.cluster.local:5000`. The registry lists both tags and the
`buildcache` tag; the controller published a `BuildSucceeded` event for each run.

### Timings

Cold is an empty buildkitd cache and registry. Warm is the same commits again after deleting and
recreating the Builds, with the cache from the cold run. End to end is from the `BuildRun`'s
creation to its `completionTime`.

| Build | Cache | Fetch | Plan | Build and push | End to end | Steps cached |
| --- | --- | --- | --- | --- | --- | --- |
| `whoami` | cold | 1.0 s | – | 34.3 s | 67 s¹ | 0 of 18 |
| `whoami` | warm | 1.0 s | – | 18.7 s | 23 s | 8 of 18 |
| `hello-python` | cold | 1.0 s | 7.0 s | 55.9 s | 67 s | 0 of 24 |
| `hello-python` | warm | 1.0 s | 5.0 s | 3.4 s | 14 s | 14 of 24 |
| `no-such-dockerfile` | – | 1.0 s | – | 0.1 s | 5 s | – |

¹ Includes a 30-second retry: runs started together raced to create the buildkitd client Secret.
That is fixed; the warm run had no retry.

The warm `whoami` image has the same digest as the cold one. The warm `hello-python` image has a
different digest (`sha256:fae44b3f…`); why has not been looked into.

### Disk

At its peak the proof of concept used about 4.5 GB on the node: buildkitd's cache 3.0 GB, the
registry 0.76 GB, the images in k3d about 0.4 GB and their copies in the VM's Docker 0.58 GB.
`buildctl prune --all` freed 3.2 GB. The node must keep well above kubelet's eviction threshold:
an earlier attempt, on a disk with about 5 GB free, put the node under DiskPressure and evicted
every Pod on it.

## Known gaps

- Ready stays `Unknown` (`NotBuilt`) until a run finishes; a missing ref shows only in
  `SourceReady`.
- Only one buildkitd instance was tried; routing across several was not exercised end to end.
- Git: public HTTPS repositories only; `credentialsRef` reports `CredentialsMissing`.
- Triggers: polling per Build only; no webhooks, no repository-level poller, no rebuild annotation.
- Controller: a single replica without leader election; no quotas or admission limits.
- Certificates: no rotation, and the CA key is discarded.
- RBAC: Secrets cluster-wide, for copying the client certificate and push credentials.
- Registry: plain HTTP without authentication; images are lost when its Pod restarts.
- Not in scope: zbplan (M4), the GitHub App, rollout integrations, multi-tenant hardening.
