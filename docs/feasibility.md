# Feasibility

What we checked before building the plan in [plan.md](plan.md), and what we found. The
research was done on 2026-10-21 and 2026-10-22 against the upstream sources linked below. A
claim marked **not verified** was not checked against a source or a running system.

## Verdict

**Feasible, with two changes to the plan and one upstream dependency.**

- Every mechanism the plan relies on exists today and works as the plan assumes: BuildKit's
  client, frontends, secrets, registry cache and metadata; Railpack's frontend; Kubernetes CEL
  validation, ValidatingAdmissionPolicy, user namespaces and termination messages; GitHub App
  tokens and Checks; Keel and Flux receivers.
- **Change 1, language (ADR-9 in the plan):** the controller is Rust, on the same template as
  Zeabur Identity. The build Job runs the stock `buildctl` from `moby/buildkit` with a small
  Rust wrapper, not a Go program linking the BuildKit client. No production-quality Rust
  BuildKit client exists, and writing one would cost three to five engineer-weeks plus upkeep.
  `buildctl` exposes everything the Job needs.
- **Change 2, Railpack options:** Railpack's frontend reads only `build-arg:`-prefixed options.
  The plan's `secrets-hash` and `cache-key` must be passed as `build-arg:secrets-hash` and
  `build-arg:cache-key`; bare names are ignored silently.
- **Upstream dependency, zbplan (M4 only):** zbplan cannot speak mTLS to buildkitd, and it adds
  build variables to `RUN` steps with a random per-process key, so no two Jobs share a cache.
  Both need small upstream options in zbplan. Neither blocks M0 to M3.

The M0 skeleton in this repository compiles, passes its tests, and its custom resource
definitions were applied to Kubernetes v1.36.1 (kind), where every admission rule rejected and
accepted the objects it should.

## By section

### §2 Architecture: one controller, one buildkitd pool, one Job per build

Sound. `RUN` executes in buildkitd's worker; the Job is a client that uploads the context
through the BuildKit session and needs no privileges. This matches how `buildctl` works: the
`--local` directories are served from the client over the session ([buildctl build][buildctl]).

### §3 API

- CEL rules on CRDs are GA since Kubernetes 1.29 and transition rules (`oldSelf`) are
  supported ([CRD validation rules][crd-cel]). A transition rule on a field does not run when
  the field is removed, so field-level immutability needs `!has(oldSelf.f) || has(self.f)`;
  `optionalOldSelf` is available since 1.33. The skeleton puts `self == oldSelf` on the whole
  `BuildRun` spec, which also catches removals; verified on v1.36.1 (removing `spec.trigger` is
  rejected).
- The API server rejects definitions whose estimated CEL cost is unbounded. Every string, list
  and map in the skeleton has a bound, enforced by a unit test (`every_value_is_bounded`).
- ValidatingAdmissionPolicy is GA since 1.30 ([VAP][vap]); the namespace output-path rule fits
  it because it needs `params` (the default registry) and `namespaceObject` (the label
  exemption).
- Printer columns cannot compute a duration or shorten a SHA (JSONPath only). The skeleton
  shows `Started` and `Finished` instead of `Duration`, and the full revision with
  `-o wide` (priority 1). See corrections.

### §4 Controller

Feasible in Rust with kube 4.2: `Controller` with owned-object watches, `reconciler` requeue,
field indexes via a reflector store, leader election through `kube-leader-election` 0.44 or
`kube-lease-manager` 0.12. **Not verified** in code beyond compiling the CRD types; that is M0
controller work.

### §5 Source providers

- **GitHub App:** installation tokens can be narrowed to listed repositories and permissions
  and last one hour ([installation tokens][gh-token]); check runs can be created only by GitHub
  Apps ([check runs][gh-checks]); GitHub does not redeliver failed webhooks automatically and
  expects a response within 10 seconds ([failed deliveries][gh-redeliver],
  [best practices][gh-best]). This confirms ADR-3 and the polling safety net. Rust: `octocrab`
  0.54.5 covers App JWTs, scoped installation tokens and Checks.
- **Plain Git:** `gix` 0.89 implements `ls-refs` over HTTP and shells out to `ssh` for SSH, so
  the controller image needs `ssh` for SSH remotes, not `git`. Basic auth and anonymous need
  nothing extra.
- **URL normalisation (§5.4):** implemented and table-tested (`zeabur-builder-core::repo`).
  Git's own rules for the scp-like form were followed ([git-clone URLs][git-urls]); remote
  helper syntax (`ext::`, `fd::`) is rejected, which the plan did not mention.

### §6 Builder and Job

- `moby/buildkit:v0.34.1` (the Alpine variant) ships `buildctl`, `git` and `openssh` (verified
  in its [Dockerfile][bk-dockerfile]), so one image serves `fetch` and `build`.
- `buildctl build` (v0.34.1, [source][buildctl]) has every flag the Job needs: `--frontend`,
  `--local`, `--opt`, `--secret id=…,env=…`, `--output type=image,name=…,push=true`,
  `--import-cache`/`--export-cache type=registry`, `--metadata-file` (the digest is
  `containerimage.digest`), `--progress rawjson`, and `--tlsdir` for mTLS (`ca.pem`, `cert.pem`,
  `key.pem`). Registry credentials are read from `DOCKER_CONFIG` on the client side and served
  to buildkitd through the session, so they never enter a `RUN` step (inferred from the session
  auth provider; **not verified** by experiment).
- `--progress rawjson` prints each `SolveStatus` as one JSON line, with `cached` and `completed`
  per vertex, so cache statistics need no BuildKit client library
  (`zeabur-builder-core::progress`, tested on a recorded stream shape).
- Attestations: `--opt attest:provenance=mode=max` records build-argument values; Railpack
  passes no secrets as build arguments, but a Dockerfile that does would leak them. Use
  `mode=min` unless a `Build` opts in. SBOM: `--opt attest:sbom=`.
- Termination messages are capped at 4096 bytes by the kubelet ([termination messages][term]).
  `zeabur-builder-core::result` encodes within the cap (message cut to 1 KiB, lists dropped
  first) and rejects anything longer.
- The kubelet expands `$(VAR)` in `command` and `args`. The plan did not cover this; the Job
  builder doubles `$` in every user-controlled value and passes those values to shell scripts
  only as positional parameters (tested).

### §6.2 Railpack

Railpack v0.43.0 (2026-10-19, MIT; weekly releases):

- `railpack prepare <dir> --plan-out … --info-out … --env NAME=VALUE`; exit code 75 means a
  transient failure worth retrying, 1 a planning failure; the info file is written on failure
  too ([production guide][railpack-prod]).
- Frontend `ghcr.io/railwayapp/railpack-frontend:<version>` via `gateway.v0`; the plan is read
  from the `dockerfile` local, file `railpack-plan.json` by default ([frontend][railpack-fe]).
  Matching planner and frontend versions is recommended, not enforced.
- Options: `build-arg:secrets-hash`, `build-arg:cache-key`, `build-arg:github-token`. Secrets
  are passed as BuildKit secrets with `id=<NAME>`, and each name must be in the plan (from
  `--env`).

### §6.2–6.5 zbplan (resolves §16 items 1–3)

zbplan @ `20dd521` (2026-10-08), latest tag v0.3.0, MPL-2.0, Go
([repository][zbplan]):

1. **Output:** the Dockerfile on stdout, logs on stderr; exit 0 on success, 1 on any failure
   ("dockerfile failed to build after N attempts; last dockerfile: …"). The returned Dockerfile
   is the model's text, without variable plumbing. Attempts are available only through the Go
   library (`Result{Dockerfile, Attempts, FromUser}`); token usage is not exposed anywhere, so
   §10.2's token metrics need an upstream change.
2. **Build variables:** `pkg/buildenv` rewrites `RUN` headers to mount variables as secrets.
   Secret IDs are `HMAC(cacheKey, name\0value)` with `cacheKey` drawn randomly once per process
   and unexported; there is no way to pass a stable key. Every Job therefore misses the cache of
   every `RUN` that sees a variable. The CLI passes no variables, so zbplan's own trial builds
   are unaffected. **Needed upstream:** a caller-supplied key, and a way to run the rewrite
   alone (for example `zbplan prepare-env`), so the plan container can prepare the pinned
   Dockerfile for each build with a per-`Build` key.
3. **LLM configuration:** `ZBPLAN_MODEL=claude|openai`; `ZBPLAN_ANTHROPIC_API_KEY`,
   `_MODEL`, `_BASE_URL`; `ZBPLAN_OPENAI_API_KEY`, `_MODEL`, `_BASE_URL` (Chat Completions with
   tool calling, so an OpenAI-compatible gateway such as Zeabur AI Hub works). Running the
   unmodified zbplan binary in a planner image is ordinary MPL-2.0 distribution: ship its
   licence and point to its source. Linking it as a library is not needed.
4. **mTLS (new finding):** zbplan connects with `client.New(ctx, addr)` and no TLS options
   (`internal/plantools/builder.go`), so it cannot reach a buildkitd that requires client
   certificates. **Needed upstream:** TLS flags matching `buildctl` (`--tlsdir`). A local
   plaintext proxy in the plan container would work but widens what the Pod can reach.

Both upstream changes are small and zbplan is a Zeabur project; they gate M4 only.

### §7 buildkitd pool (resolves §16 items 7 and 10)

- **Readiness probe:** an exec probe running `buildctl --addr unix:///run/buildkit/buildkitd.sock
  debug workers` works with mTLS enabled on TCP, because it uses the local socket. A gRPC
  health probe cannot present a client certificate. The upstream Kubernetes examples
  ([examples][bk-k8s]) use the same exec approach.
- **GC:** BuildKit v0.34 configures GC with `reservedSpace`, `maxUsedSpace`, `minFreeSpace` and
  `keepDuration`; `keepBytes` is the legacy name ([buildkitd.toml][bk-toml]). The plan's "80%
  of the PVC" becomes `maxUsedSpace = "80%"`.
- **User namespaces:** `hostUsers: false` is beta and on by default since Kubernetes 1.33 and GA
  in 1.36. It needs Linux ≥ 6.3, containerd ≥ 2.0, runc ≥ 1.2, and a
  filesystem with idmapped-mount support for volumes (not NFS) ([user namespaces][userns]).
  Rootless BuildKit cannot run inside a user namespace; rootful BuildKit with `hostUsers: false`
  can ([CERN][cern]), and upstream ships `statefulset.userns.yaml`. The target cluster's
  versions (item 10) are a deployment question; the chart keeps the `privileged` fallback.
- **Rendezvous routing:** implemented and tested (`zeabur-builder-core::route`): 1000 keys over
  four instances land within ±28% of even, and adding an instance moves keys only to it.
- **Shared daemon:** every tenant's registry token and secrets pass through the same
  buildkitd. Acceptable under the plan's trust model (internal teams); it is the main reason the
  model cannot be relaxed to untrusted tenants without per-tenant pools.

### §8 Registry

No surprises. `oci-client` 0.18 (Rust) supports `artifactType`, `subject` and the referrers
API, enough for plan artifacts (§3.4) without `oras-go`.

### §9 Rollout integrations (resolves §16 items 5 and 6)

- **Keel:** `POST /v1/webhooks/native` on port 9300 with `{"name": "<image>", "tag": "<tag>"}`
  ([Keel][keel]).
- **Flux:** a `Receiver` of type `generic-hmac` verifies `X-Signature: sha256=<hex>` and
  reconciles the listed `ImageRepository` (`image.toolkit.fluxcd.io/v1`) ([receivers][flux-rcv]).
  `ImagePolicy` with `filterTags.pattern` and `extract: '$ts'` plus `policy.numerical` selects
  the newest `<branch>-<unixtime>-<sha>` tag ([image policies][flux-ip]). Flux Operator's
  `ResourceSetInputProvider` has an `OCIArtifactTag` type for the Gitless flow
  ([RSIP][flux-rsip]).

### §10 Observability

`prometheus-client` 0.25.1 covers counters, gauges and histograms with typed label sets. The
label rules in §10.2 are kept. zbplan exposes no token counts (item 1).

### §16 items 4, 8 and 9

- **4 Railpack option names:** `build-arg:secrets-hash` and `build-arg:cache-key` (see above).
- **8 go-github / ghinstallation:** replaced by `octocrab` 0.54.5 (Rust); App JWT, installation
  tokens with `repositories` and `permissions`, and Checks are covered. **Not verified:** GHES
  base-URL handling in octocrab beyond its builder option.
- **9 BuildKit client attestation and session auth:** moot with `buildctl`; attestations are
  `--opt attest:…`, and auth is the client's Docker config.

## Novelty

No open-source project combines `Build`/`BuildRun` resources, a shared buildkitd pool with
cache-affinity routing, and pluggable planners:

- Shipwright (v0.21.10) has the closest API but still requires Tekton
  ([Shipwright][shipwright]).
- kpack builds only Cloud Native Buildpacks ([kpack][kpack]).
- Kaniko was archived in June 2025 ([Kaniko][kaniko]).
- SeatGeek's buildkit-operator provisions buildkitd daemons and nothing above them
  ([buildkit-operator][seatgeek]).

The design is the plan's; the routing (rendezvous hashing) is textbook, the combination is new.

## Risks

| Risk | Effect | Mitigation |
|---|---|---|
| `buildctl` output or flags change | Wrapper misreads progress or digest | Pin the BuildKit image; golden-test the arguments; the wrapper ignores lines it cannot parse |
| zbplan upstream changes not made | No mTLS and no shared cache for zbplan | Ship M0–M3 without zbplan; the changes are small and in-house |
| Kubelet `$(VAR)` expansion | A URL or option containing `$(…)` changes | Escaped and tested in the Job builder |
| Shared buildkitd sees all secrets | One compromised build reads another team's tokens | Trust model in §1.3; per-team pools would be a later option |
| Cache mounts live on one instance | A fallback route rebuilds dependencies | Counted in `buildrun_route_fallback_total`; size slots generously |
| `fetch` by SHA | Servers that refuse `want <sha>` cannot be fetched | GitHub, GitLab, Gitea and `git daemon` allow it (**not verified** for each); fall back to fetching the ref and checking the SHA |
| Rust BuildKit ecosystem | No client library to grow into | The wrapper keeps the protocol in `buildctl`; revisit if a maintained client appears |

## Corrections to the plan

These are applied and marked in [plan.md](plan.md):

1. §4, §6.4: controller in Rust; Job is `buildctl` plus a Rust wrapper (ADR-9).
2. §6.5: `build-arg:secrets-hash`, `build-arg:cache-key`.
3. §7.1: `maxUsedSpace` replaces `keepBytes`; exec probe on the local socket.
4. §3.3: printer columns `Started`/`Finished` instead of `Duration`; revision under `-o wide`.
5. §3: durations are strings (`10m`); conditions follow `metav1.Condition`.
6. §6.1: user values are `$`-escaped against kubelet expansion; `GIT_ALLOW_PROTOCOL` limits
   `fetch` to `https`, `http`, `ssh` and `git`; the image `source` annotation is the
   repository key, never the URL, which may carry a token.
7. §6.4: build variables reach the build container as `ZB_SECRET_<NAME>`, so a variable named
   `PATH` or `BUILDKIT_HOST` cannot change how `buildctl` runs.
8. §6.3, §6.5: zbplan needs the two upstream options above.

## What M0 contains

- `zeabur-builder-core`: repository keys, tags, routing, termination-message encoding and
  progress counting. No I/O.
- `zeabur-builder-api`: the three custom resources with CEL rules, printer columns and
  `deploy/crds.json`.
- `zeabur-builder-jobspec`: the Job as a pure function, with golden files for the three
  builders.
- `zeabur-builder crds`: prints the definitions.

Not yet: the reconcilers, the `zeabur-builder-job` wrapper binary and its image, the
buildkitd StatefulSet and certificates, and the chart.

[buildctl]: https://github.com/moby/buildkit/blob/v0.34.1/cmd/buildctl/build.go
[bk-dockerfile]: https://github.com/moby/buildkit/blob/v0.34.1/Dockerfile
[bk-k8s]: https://github.com/moby/buildkit/tree/v0.34.1/examples/kubernetes
[bk-toml]: https://github.com/moby/buildkit/blob/v0.34.1/docs/buildkitd.toml.md
[crd-cel]: https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definitions/#validation-rules
[vap]: https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/
[userns]: https://kubernetes.io/docs/concepts/workloads/pods/user-namespaces/
[term]: https://kubernetes.io/docs/tasks/debug/debug-application/determine-reason-pod-failure/
[cern]: https://kubernetes.web.cern.ch/blog/2025/06/19/rootless-container-builds-on-kubernetes/
[gh-token]: https://docs.github.com/en/rest/apps/apps#create-an-installation-access-token-for-an-app
[gh-checks]: https://docs.github.com/en/rest/checks/runs
[gh-redeliver]: https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries
[gh-best]: https://docs.github.com/en/webhooks/using-webhooks/best-practices-for-using-webhooks
[git-urls]: https://git-scm.com/docs/git-clone#_git_urls
[railpack-prod]: https://railpack.com/guides/running-railpack-in-production/
[railpack-fe]: https://railpack.com/reference/frontend
[zbplan]: https://github.com/zeabur/zbplan
[keel]: https://keel.sh/docs/#native-webhooks
[flux-rcv]: https://fluxcd.io/flux/components/notification/receivers/
[flux-ip]: https://fluxcd.io/flux/components/image/imagepolicies/
[flux-rsip]: https://fluxoperator.dev/docs/crd/resourcesetinputprovider/
[shipwright]: https://github.com/shipwright-io/build
[kpack]: https://github.com/buildpacks-community/kpack
[kaniko]: https://github.com/GoogleContainerTools/kaniko
[seatgeek]: https://github.com/seatgeek/buildkit-operator
