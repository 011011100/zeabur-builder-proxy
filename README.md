# Zeabur Builder

**Push a commit, get an image digest.** A Kubernetes operator that builds container images from
Git with BuildKit, and hands the result to Flux, Keel or Argo CD for rollout.

> **Status: design and M0 skeleton.** Nothing here builds an image yet. The custom resources,
> the build Job and the routing logic exist and are tested; the controller does not. See the
> [plan](docs/plan.md) (Traditional Chinese) and the [feasibility study](docs/feasibility.md).

## What it does

```yaml
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: api, namespace: team-a}
spec:
  source: {git: {url: https://github.com/acme/api, ref: main}}
  builderRef: railpack
```

- **Three ways to build.** Your Dockerfile, [Railpack](https://railpack.com) for repositories
  without one, or [zbplan](https://github.com/zeabur/zbplan), which writes a Dockerfile with an
  LLM once and reuses it.
- **GitHub App or any Git server.** Push events start builds and report back as checks; plain Git
  repositories are polled.
- **Tags rollout tools can sort.** `main-1761058800-3e1a9c2`: one per build, newest last, plus the
  digest in `status.latestImage`.
- **Warm caches.** A shared pool of buildkitd instances, and builds of one repository keep landing
  on the same one, so `RUN --mount=type=cache` survives between builds.
- **Small.** One controller, one buildkitd StatefulSet, one Job per build. No Tekton, no admission
  webhook server.

It does not run tests, deploy, or keep build logs; your pipeline, rollout tool and log collector
already do those.

## How it works

1. A push (or a poll) wakes the `Build`. The controller resolves the ref to a commit itself.
2. It creates a `BuildRun` for that commit and a Job for the run.
3. The Job fetches the commit, plans if the builder needs it, and runs `buildctl` against the
   buildkitd chosen for the repository. Your `RUN` steps execute in buildkitd; the Job needs no
   privileges.
4. The image is pushed; its digest and tags land in the `BuildRun` and the `Build`.

```sh
kubectl get buildruns
NAME        BUILD   PHASE       REASON   STARTED   FINISHED   AGE
api-x7k2p   api     Succeeded            2m        30s        2m
```

## Try the skeleton

```sh
nix develop
cargo nextest run --workspace
cargo run -q -p zeabur-builder -- crds | kubectl apply -f -   # on a disposable cluster
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues as described in
[SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE).
