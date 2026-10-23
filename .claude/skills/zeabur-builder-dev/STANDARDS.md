# Engineering standards

Four checks for every change. Each item is a fail condition a reviewer can point at. The
"Leave alone" lists stop over-correction: a finding must not land on them.

## 1. Comments, doc comments and commit messages

Prose follows ASD-STE100 Issue 9 (Simplified Technical English), adapted to code. Code spans,
identifiers, Kubernetes kinds and fields, numbers with units and RFC designators are technical
nouns: each counts as one word and is never reworded.

- Classify each sentence. A step the reader does is **procedural**: imperative, at most 20
  words. Everything else is **descriptive**: at most 25 words; a paragraph has at most six
  sentences and one topic.
- One instruction per sentence. Put sequential steps in a numbered list.
- Condition first: "If the Lease is expired, take it."
- Active voice with a named actor: "The API server rejects the write." Use the passive only when
  the actor is unknown.
- Simple tenses: present, past, future, imperative. Write "watches", not "is watching" or "has
  watched".
- A verb for an action: "Delete the Pod", not "perform the deletion of the Pod".
- An `-ing` word only as a technical noun (`Pending`, "rolling update") or in "during",
  "missing", "remaining".
- Full sentences: keep articles and verbs, write "does not" for "doesn't", and use periods, not
  semicolons.
- Noun clusters of at most three words: "the cache of role bindings for workspace members".
- One term per concept and one meaning per term: the term the code and docs already use.
- A single verb when one exists: "configure", not "set up".
- `NOTE:` states a fact. Write an instruction as its own imperative sentence.
- `WARNING:` only on a destructive operation (data loss, a security boundary, an outage): the
  condition or command first, then the concrete result.
- Keep the spelling the repository already uses; do not mix British and American forms.
- Commit subject: the Conventional Commits prefix, then one imperative instruction of at most 20
  words. Body: descriptive sentences of at most 25 words that say why.

Leave alone: code, literals, log keys and metric names; quoted upstream text, licence headers and
generated files; tags and labels (`# Safety`, `SAFETY:`, `TODO(name):`, `@param`); RFC 2119
MUST/SHOULD in a contract; comments the change does not touch. These rules govern how a comment
reads, not whether to write one.

```rust
// Before
// Basically we retry here cuz the apiserver sometimes 409s us.
/// This is used for applying deployments after the user has approved things.
// Can't use ? here since the error type doesn't impl From.

// After
// Retry once on `409 Conflict`. Another writer changed the object after the read.
/// Applies the approved plan to the `Deployment`.
///
/// Before you call this function, get approval for the plan.
// Do not use `?` here. `KubeError` does not implement `From<ApplyError>`.
```

```text
Before: fixed webhooks getting lost when the pod restarts
After:  fix(controller): keep webhook wakes in Build status

        The receiver kept the last wake in memory. A restart or another replica lost it.
```

## 2. Tests

A test earns its place when it fails because a promise broke, and only then. Ask four questions
(Khorikov): does it catch a real regression, survive a refactor that keeps the behaviour, run
fast, and read clearly?

Delete or rewrite a test that:
- restates the implementation: an expected value copied from the code, a mocked sequence of
  internal calls, a table, `match` or `switch` copied into the test (a change-detector);
- asserts internal interactions (which helper, a harmless call order) where output or state is
  observable. Keep a call count or order that is itself the requirement;
- reaches private code through a test-only export when the public interface shows the result;
- tests the language, a framework, a library or generated code, with no behaviour of ours;
- tests trivial code: getters, constants, `Display`, plain derives;
- pins exact log text, a full error sentence or map order that no caller reads;
- is a large snapshot or golden file that people regenerate without reading;
- passes when the result is wrong (no assertion, or only "did not fail").

Merge tests that share setup and action and differ only in input into one table of named cases (an array of cases).
Split a test that checks unrelated behaviours; several assertions about one outcome are one
behaviour.

Move a rule tested at several levels to the lowest level that can see it. Keep a higher test only
for what that level cannot see: wiring, a real API server, a real browser. A test that uses the
network, disk, a sleep or a cluster belongs in the suite for its size; do not delete it for being
slow.

Before you remove a test, name the promise it protects and the test that still protects it. A
mutation report that shows no unique kill is a reason to look, not proof.

A fake API server shows our logic. What only kube-apiserver decides (RBAC, server-side apply,
`resourceVersion` conflicts, admission, schema validation) needs a test against a real one.

Leave alone: security tests repeated at levels that each see a different failure; published
contracts (API fields, CLI output, CRD schema, rendered manifests); mocks at real out-of-process
boundaries (SMTP, an OIDC provider, a Git host); regression tests of fixed bugs; small reviewed
golden files; property and fuzz tests; coverage or test count on its own.

```rust
// Before: a change-detector
client.expect_create().with(eq(build_deployment(&spec))).times(1);
// After: assert what the API server holds
let deployment = api.get("web").await?;
assert_eq!(deployment.spec.and_then(|s| s.replicas), Some(2));
```

## 3. Design: build for a caller that exists

Flag a structure when no present caller can be named:
- a trait or interface with one production implementation and no real boundary behind it;
- a generic parameter or dynamic dispatch that serves one concrete type;
- a parameter, option, flag, config key or hook that no production code sets;
- a wrapper or layer that forwards the same arguments and result and adds no invariant
  (a pass-through);
- a configuration knob for a value the code can compute;
- a registry, plugin system, DSL or rules engine with one use;
- shared code that grows a mode flag per caller: inline it, delete each caller's unused branches,
  then extract what is really shared (Metz, "The Wrong Abstraction");
- an abstraction drawn from one or two call sites that do not match yet: wait for the third. A
  duplicated security check is the exception: give it one implementation now;
- a second copy of what Kubernetes provides: a permission model, an object store, a scheduler
  or retry queue, a query language;
- an internal API with versions or adapters and one consumer that ships with it;
- a large design with no working simple path first (Gall).

A finding names the cost, the missing caller, and the simpler design that keeps the behaviour.

Leave alone: a seam at a real out-of-process boundary; layers that enforce security or
correctness (plan, review, apply; authorization checks); published interfaces (API, CRDs,
documented Helm values and flags: removing one is a deprecation, not a cleanup); Rust newtypes and error enums; the second copy of a block; a deep module that hides much
behind a small interface; essential Kubernetes semantics (conflicts, finalizers, owner
references).

```rust
// Before: one implementation, no boundary, every method forwards
trait ProjectStore { async fn get(&self, name: &str) -> Result<Project>; }
struct KubeProjectStore(Api<Project>);
impl ProjectStore for KubeProjectStore {
    async fn get(&self, name: &str) -> Result<Project> { Ok(self.0.get(name).await?) }
}
// After
let project = projects.get(name).await?;
```

## 4. Scale: no unbounded cost on day one

Fail a design when its cost per request, item, tenant, tab or replica grows with no bound, or
when a second replica breaks it. A finding names the growth variable, the resource and the
missing bound.

**Kubernetes API**
- Detect change with a watch (kube-rs `watcher`, `reflector`, `Controller`), not a timer that
  sends GET or LIST. Keep one store per resource type and scope per process, not one per request.
- A store serves only the credential and scope that filled it. Label filters and cache sync are
  not authorization.
- A LIST in a request path sets `limit` and follows `continue`, or reads a store. Reading every
  page before the answer is still a full scan.
- On `410 Gone` (an expired continue token or watch history, about 5 minutes), relist. On `429`,
  wait for `Retry-After`.
- No GET or LIST per row (N+1): use one LIST with a label selector, or the store.
- A write carries `resourceVersion` or a JSON Patch `test`, and handles `409 Conflict`.
- An admission webhook times out at 1–30 s (default 10 s). Answer in milliseconds, make no
  network call per request, and scope it with selectors.

**Objects and size**
- Hard limits: an etcd request 1.5 MiB (default), a Secret or ConfigMap 1 MiB, all annotations of
  an object 256 KiB. Logs, plans and large blobs go elsewhere.
- Soft thresholds (sig-scalability): 150,000 objects of one type; per namespace 3,000 Pods,
  2,000 Deployments, 5,000 Services; 10,000 namespaces; Pods per node min(110, 10 × cores).
  A design far below them passes; growth with no bound fails.
- Everything stored has a retention bound: Job `ttlSecondsAfterFinished`, CronJob
  `successfulJobsHistoryLimit` and `failedJobsHistoryLimit`, a cap on history objects.
- A Kubernetes label identifies a set that something selects. A Prometheus label takes a bounded
  set of values.

**Work and replicas**
- One controller reconciles all items. Do not create a controller, Deployment or CronJob per
  tenant item. A CronJob can run zero or two times, so its Job is idempotent.
- Bound fan-out and queues: acquire a `Semaphore` permit before `tokio::spawn`, or use
  `buffer_unordered(n)`. A per-replica bound multiplies by the replica count.
- Required state lives in Kubernetes objects. An in-process cache is rebuilt from a watch and
  never authorizes a write.
- An idempotent reconcile needs no leader election. A Lease does not fence: guard an external
  effect that must not repeat separately.

**Observability**
- A metric label takes a bounded set of values (aim under 10, look again over 100): no person,
  project or object IDs.
- No log line per object per resync at info level, and no full object dumps.

```rust
// Before: one GET per row
for name in &names { services.push(api.get(name).await?); }
// After: one LIST of the namespace, selected by label
let services = api.list(&ListParams::default().labels("app.kubernetes.io/part-of=shop")).await?;
```

```yaml
# Before: a CronJob per database, every finished Job kept
# After:  one CronJob per cluster that backs up every database it selects
successfulJobsHistoryLimit: 3
failedJobsHistoryLimit: 1
jobTemplate:
  spec:
    ttlSecondsAfterFinished: 86400
```

Leave alone: a GET by name in a watch-driven reconcile; one-shot LISTs in a CLI; bounded fan-out
over a fixed small set; a namespace per project; one Job per real workload; replicas without
leader election when reconcile is idempotent; polling an external system that has no watch API,
with backoff; a scan bounded by a quota or a documented ceiling; the cost of fail-closed
admission, RBAC and plan review.
