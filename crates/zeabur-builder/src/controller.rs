//! The two reconcilers, on kube-runtime.
//!
//! - **Build**: resolves the ref, creates a `BuildRun` for a new commit or a new `rebuild` value,
//!   cancels or waits for older runs, prunes history, summarises the newest result in its status
//!   and sends that result to its notify targets. Requeued every poll interval; the ref is asked
//!   again only when the last answer is older than the interval, the spec changed or a rebuild is
//!   pending, so status changes of its runs cost no request to the Git server. Any change to the
//!   `Build`, annotations included, wakes it.
//! - **`BuildRun`**: waits for a slot in its namespace and for the namespace's buildkitd
//!   ([`crate::daemon`]), creates the Job and follows it to a result.
//!
//! Decisions are in [`crate::decide`]; this module only reads and writes objects. Status is
//! written with server-side apply, so a field the controller stops setting is removed.
//!
//! A webhook ([`crate::webhook`]), received by any replica, writes `status.lastWebhookAt` with its
//! own field manager; the reconcile asks the Git server when that is later than `lastResolvedAt`,
//! at least [`decide::WAKE_GAP`] after the last answer ([`decide::wake`]). The reconcile never
//! writes `lastWebhookAt`, so its apply cannot put back an older time. A `githubApp` source is
//! resolved and fetched with installation tokens and gets commit statuses ([`crate::github`]).
//!
//! Only the holder of the Lease reconciles ([`crate::leader`]). The other replicas watch and
//! receive webhooks.
//!
//! Not yet: `flux-receiver` and `generic` notify targets.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use jiff::Timestamp;
use k8s_openapi::ByteString;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Namespace, ObjectReference, Pod, Secret};
use kube::api::{
    Api, DeleteParams, ListParams, ObjectMeta, Patch, PatchParams, PostParams, PropagationPolicy,
};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::runtime::reflector::Store;
use kube::runtime::watcher;
use kube::{Client, Resource as _, ResourceExt as _};
use serde::Serialize;
use thiserror::Error;
use zeabur_builder_api::build::{Build, BuildStatus, NotifyTarget, NotifyType, SourceMode};
use zeabur_builder_api::build_run::{
    BuildRun, BuildRunSpec, BuildRunStatus, Phase, Reason, Trigger,
};
use zeabur_builder_api::cluster_builder::ClusterBuilder;
use zeabur_builder_api::common::ConditionStatus;
use zeabur_builder_api::{annotations, labels};
use zeabur_builder_core::repo::RepoKey;
use zeabur_builder_jobspec::{GitAuth, JobInputs};

use crate::credentials::{GitCredentials, Refusal};
use crate::decide::{self, Observed, Wake};
use crate::github::{self, Access, GitHub, State};
use crate::leader::{self, Leader};
use crate::notify::{self, Credentials};
use crate::remote::{self, HttpClient, Resolver};
use crate::webhook::{self, Lookup, Receiver};
use crate::{daemon, tls};

const MANAGER: &str = "zeabur-builder";
/// The field manager of `status.lastWebhookAt`, apart from [`MANAGER`] so neither removes the
/// other's fields.
const WEBHOOK_MANAGER: &str = "zeabur-builder-webhook";
/// How often a waiting `BuildRun` is looked at again.
const PENDING_RETRY: Duration = Duration::from_secs(15);
/// How often a `BuildRun` waiting for its buildkitd to start is looked at again.
const STARTING_RETRY: Duration = Duration::from_secs(2);
/// Selects the `BuildRun`s that are not finished, by the selectable field `status.phase`.
const UNFINISHED: &str = "status.phase!=Succeeded,status.phase!=Failed,status.phase!=Cancelled";
/// Objects read per page of a LIST.
const PAGE: u32 = 100;
/// The name of the client certificate Secret in a build namespace.
const CLIENT_TLS_COPY: &str = "zeabur-builder-buildkit-client";

/// Settings, from the command line.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// The controller's namespace, where the push credentials are kept.
    pub(crate) namespace: String,
    /// How each namespace's buildkitd runs.
    pub(crate) daemon: Arc<daemon::Settings>,
    /// Builds a namespace runs at once, unless its annotation says otherwise.
    pub(crate) max_concurrent: u32,
    /// The registry for `Build`s without `spec.output.image`.
    pub(crate) default_registry: String,
    /// With a prefix, the `kubernetes.io/dockerconfigjson` Secret `<prefix><namespace>` in the
    /// controller's namespace holds a namespace's push credentials; without, pushes are anonymous.
    pub(crate) registry_auth_prefix: Option<String>,
    /// The image of `fetch` and `build`.
    pub(crate) job_image: String,
    /// `activeDeadlineSeconds`.
    pub(crate) job_deadline_seconds: i64,
    /// `ttlSecondsAfterFinished`.
    pub(crate) job_ttl_seconds: i32,
    /// The GitHub App's Secret, in the controller's namespace.
    pub(crate) github_app_secret: String,
    /// The `ConfigMap` of namespaces allowed to use each installation, in the controller's
    /// namespace.
    pub(crate) github_app_grants: String,
    /// The webhook receiver, when it runs.
    pub(crate) webhook: Option<webhook::Settings>,
}

struct Context {
    client: Client,
    leader: Leader,
    http: HttpClient,
    resolver: Resolver,
    recorder: Recorder,
    settings: Settings,
    github: Arc<GitHub>,
}

/// What the receiver reads: `Build`s from the reconciler's cache, Secrets from the API server.
struct ClusterLookup {
    builds: Store<Build>,
    client: Client,
    github: Arc<GitHub>,
}

impl Lookup for ClusterLookup {
    fn builds(&self) -> Vec<Arc<Build>> {
        self.builds.state()
    }

    fn secret<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let secret = Api::<Secret>::namespaced(self.client.clone(), namespace)
                .get_opt(name)
                .await;
            match secret {
                Ok(secret) => secret?.data?.remove(key).map(|v| v.0),
                Err(e) => {
                    tracing::warn!(%namespace, secret = name, error = %e, "webhook secret unreadable");
                    None
                }
            }
        })
    }

    fn touch<'a>(&'a self, namespace: &'a str, name: &'a str, at: Timestamp) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let patch = serde_json::json!({
                "apiVersion": Build::api_version(&()),
                "kind": Build::kind(&()),
                "status": {"lastWebhookAt": at.to_string()},
            });
            let patched = Api::<Build>::namespaced(self.client.clone(), namespace)
                .patch_status(
                    name,
                    &PatchParams::apply(WEBHOOK_MANAGER).force(),
                    &Patch::Apply(patch),
                )
                .await;
            if let Err(e) = patched {
                tracing::warn!(%namespace, build = name, error = %e, "webhook wake not written");
            }
        })
    }

    fn app_secret(&self) -> BoxFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            match self.github.app().await {
                Ok(app) => app.webhook_secret.clone(),
                Err((_, message)) => {
                    tracing::warn!(message, "GitHub App webhook refused");
                    None
                }
            }
        })
    }
}

#[derive(Debug, Error)]
enum Error {
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Invalid(String),
}

fn now() -> Timestamp {
    Timestamp::now()
}

async fn apply_status<K, S>(api: &Api<K>, name: &str, status: &S) -> Result<(), Error>
where
    S: Serialize + Sync,
    K: kube::Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let body = serde_json::json!({
        "apiVersion": K::api_version(&()),
        "kind": K::kind(&()),
        "status": status,
    });
    api.patch_status(
        name,
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(body),
    )
    .await?;
    Ok(())
}

/// Runs both controllers, the buildkitd sweep and the webhook receiver until a termination
/// signal.
pub(crate) async fn run(client: Client, settings: Settings) -> Result<(), String> {
    let identity = std::env::var("POD_NAME").ok();
    let (leader, election) = leader::elect(client.clone(), &settings.namespace, identity).await?;
    let served = run_replica(client, settings, leader).await;
    election.release().await;
    served
}

/// Runs the controllers, the sweep and the receiver until a termination signal.
async fn run_replica(client: Client, settings: Settings, leader: Leader) -> Result<(), String> {
    let http = remote::http_client()?;
    let github = Arc::new(GitHub::new(
        &client,
        http.clone(),
        &settings.namespace,
        settings.github_app_secret.clone(),
        settings.github_app_grants.clone(),
    ));
    let receiver_settings = settings.webhook.clone();
    let daemon_settings = Arc::clone(&settings.daemon);
    let context = Arc::new(Context {
        leader: leader.clone(),
        github: Arc::clone(&github),
        recorder: Recorder::new(
            client.clone(),
            Reporter {
                controller: MANAGER.to_owned(),
                instance: std::env::var("POD_NAME").ok(),
            },
        ),
        resolver: Resolver::new(http.clone()),
        http,
        client: client.clone(),
        settings,
    });
    let builds = Controller::new(
        Api::<Build>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(
        Api::<BuildRun>::all(client.clone()),
        watcher::Config::default(),
    )
    .reconcile_all_on(leader.taken());
    let receiver = receiver_settings.map(|settings| {
        let lookup = ClusterLookup {
            builds: builds.store(),
            client: client.clone(),
            github,
        };
        Arc::new(Receiver::new(Box::new(lookup), settings))
    });
    let builds = builds
        .shutdown_on_signal()
        .run(reconcile_build, retry, Arc::clone(&context))
        .for_each(|result| async move {
            if let Err(e) = result {
                tracing::warn!(error = %e, "build reconcile");
            }
        });
    let runs = Controller::new(
        Api::<BuildRun>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(
        Api::<Job>::all(client.clone()),
        watcher::Config::default().labels(&format!("{}={}", labels::MANAGED_BY, labels::MANAGER)),
    )
    .reconcile_all_on(leader.taken());
    let sweep = daemon::sweep_forever(client.clone(), daemon_settings, runs.store(), leader);
    let runs = runs
        .shutdown_on_signal()
        .run(reconcile_run, retry, context)
        .for_each(|result| async move {
            if let Err(e) = result {
                tracing::warn!(error = %e, "buildrun reconcile");
            }
        });
    let receive = async {
        match receiver {
            Some(receiver) => webhook::serve(receiver).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        ((), ()) = async { tokio::join!(builds, runs) } => {}
        () = sweep => {}
        served = receive => served?,
    }
    Ok(())
}

fn retry<K>(_: Arc<K>, error: &Error, _: Arc<Context>) -> Action {
    tracing::warn!(%error, "reconcile failed; retrying in 30s");
    Action::requeue(Duration::from_secs(30))
}

/// Spreads polls by up to ±10%, from the name and the clock's nanoseconds.
fn jitter(interval: Duration, name: &str) -> Duration {
    let seed = name.bytes().fold(
        u64::from(now().subsec_nanosecond().unsigned_abs()),
        |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)),
    );
    let spread = interval.as_millis() / 10;
    let spread = u64::try_from(spread).unwrap_or(0).max(1);
    let offset = seed % (2 * spread);
    (interval + Duration::from_millis(offset)).saturating_sub(Duration::from_millis(spread))
}

/// The Build's `ClusterBuilder`, recorded in its `BuilderReady` condition.
async fn find_builder(
    ctx: &Context,
    build: &Build,
    status: &mut BuildStatus,
    at: Timestamp,
) -> Result<Option<ClusterBuilder>, Error> {
    let builder = Api::<ClusterBuilder>::all(ctx.client.clone())
        .get_opt(&build.spec.builder_ref)
        .await?;
    let (ready, reason, message) = if builder.is_some() {
        (ConditionStatus::True, "Found", String::new())
    } else {
        (
            ConditionStatus::False,
            "BuilderNotFound",
            format!("no ClusterBuilder `{}`", build.spec.builder_ref),
        )
    };
    decide::set_condition(
        &mut status.conditions,
        "BuilderReady",
        ready,
        reason,
        &message,
        at,
        build.metadata.generation,
    );
    Ok(builder)
}

async fn reconcile_build(build: Arc<Build>, ctx: Arc<Context>) -> Result<Action, Error> {
    if !ctx.leader.is_leader() {
        return Ok(Action::await_change());
    }
    let namespace = build.namespace().unwrap_or_default();
    let name = build.name_any();
    let generation = build.metadata.generation;
    let builds: Api<Build> = Api::namespaced(ctx.client.clone(), &namespace);
    let runs_api: Api<BuildRun> = Api::namespaced(ctx.client.clone(), &namespace);
    let at = now();
    let mut status = build.status.clone().unwrap_or_default();
    status.source_mode = Some(if build.spec.source.github_app.is_some() {
        SourceMode::GitHubApp
    } else {
        SourceMode::PlainGit
    });
    let interval = build
        .spec
        .triggers
        .poll_interval
        .as_deref()
        .and_then(decide::parse_duration)
        .unwrap_or_else(|| decide::default_poll(&build, ctx.settings.webhook.is_some()));
    let (woken, wake_after) = match decide::wake(build.status.as_ref(), at) {
        Wake::Idle => (false, None),
        Wake::Now => (true, None),
        Wake::After(wait) => (false, Some(wait)),
    };
    // Owned by the receiver's field manager.
    status.last_webhook_at = None;

    let builder = find_builder(&ctx, &build, &mut status, at).await?;

    let mut runs = runs_api
        .list(&ListParams::default().fields(&format!("spec.buildRef={name}")))
        .await?
        .items;
    let rebuild = decide::pending_rebuild(&build, &runs).is_some();
    let resolved = resolve_source(&ctx, &build, &mut status, rebuild || woken, interval, at).await;
    let mut requeue = jitter(decide::next_poll(&status, interval, at), &name);
    if let Some(wait) = wake_after {
        requeue = requeue.min(wait);
    }

    if let (Some(sha), Some(_)) = (&resolved, &builder) {
        let found_by = if woken {
            Trigger::Webhook
        } else {
            Trigger::Poll
        };
        let next = decide::next_run(&build, sha, &runs, found_by);
        for cancel in &next.cancel {
            cancel_run(&ctx, &build, cancel).await?;
        }
        if let Some(trigger) = next.create {
            let run =
                create_run(&runs_api, &build, sha, status.resolved_ref.clone(), trigger).await?;
            tracing::info!(build = %name, namespace = %namespace, buildrun = %run.name_any(), revision = %sha, ?trigger, "new run");
            if !runs.iter().any(|r| r.metadata.name == run.metadata.name) {
                runs.push(run);
            }
        }
    }
    let limit = build
        .spec
        .history_limit
        .and_then(|l| usize::try_from(l).ok())
        .unwrap_or(decide::DEFAULT_HISTORY);
    for old in decide::prune(&runs, limit) {
        runs_api
            .delete(&old, &DeleteParams::background())
            .await
            .map(drop)
            .or_else(not_found)?;
        runs.retain(|r| r.metadata.name.as_deref() != Some(old.as_str()));
    }

    let image = decide::image(&build, &ctx.settings.default_registry);
    decide::summarise(&mut status, &runs, &image, at, generation);
    status.observed_generation = generation;
    let unchanged = build.status.as_ref().is_some_and(|old| {
        BuildStatus {
            last_webhook_at: None,
            ..old.clone()
        } == status
    });
    if !unchanged {
        apply_status(&builds, &name, &status).await?;
    }
    if let Some(run) = decide::notify_due(&build, &runs) {
        notify_targets(&ctx, &build, run, &image).await?;
    }
    Ok(Action::requeue(requeue))
}

/// Resolves the ref, or reuses the last answer while it is fresh, and sets `SourceReady`.
async fn resolve_source(
    ctx: &Context,
    build: &Build,
    status: &mut BuildStatus,
    force: bool,
    interval: Duration,
    at: Timestamp,
) -> Option<String> {
    let generation = build.metadata.generation;
    let git = &build.spec.source.git;
    let resolved = if decide::needs_resolve(build.status.as_ref(), generation, force, interval, at)
    {
        match git_credentials(ctx, build).await {
            Err(refusal) => Err(refusal),
            Ok(credentials) => match ctx
                .resolver
                .resolve(&git.url, &git.git_ref, credentials.as_ref())
                .await
            {
                Ok(resolved) => {
                    status.last_resolved_at = Some(at.to_string());
                    status.resolved_ref = Some(resolved.name);
                    Ok(resolved.commit)
                }
                Err(e) => Err((e.reason(), e.to_string())),
            },
        }
    } else {
        status
            .remote_revision
            .clone()
            .ok_or(("ResolveFailed", String::new()))
    };
    match &resolved {
        Ok(sha) => {
            status.remote_revision = Some(sha.clone());
            decide::set_condition(
                &mut status.conditions,
                "SourceReady",
                ConditionStatus::True,
                "Resolved",
                &decide::resolved_message(&git.git_ref, status.resolved_ref.as_deref(), sha),
                at,
                generation,
            );
        }
        Err((reason, message)) => {
            tracing::info!(build = %build.name_any(), reason, message, "ref not resolved");
            decide::set_condition(
                &mut status.conditions,
                "SourceReady",
                ConditionStatus::False,
                reason,
                message,
                at,
                generation,
            );
        }
    }

    resolved.ok()
}

/// The credentials the `Build`'s `credentialsRef` names, from its namespace, or a token of its
/// GitHub App installation.
async fn git_credentials(ctx: &Context, build: &Build) -> Result<Option<GitCredentials>, Refusal> {
    if build.spec.source.github_app.is_some() {
        let token = app_token(ctx, build, Access::Controller).await?;
        return Ok(Some(GitCredentials::Basic {
            username: "x-access-token".to_owned(),
            password: token,
        }));
    }
    let Some(secret_ref) = &build.spec.source.credentials_ref else {
        return Ok(None);
    };
    let name = &secret_ref.name;
    let namespace = build.namespace().unwrap_or_default();
    let secret = Api::<Secret>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(name)
        .await
        .map_err(|e| ("CredentialsUnreadable", format!("Secret `{name}`: {e}")))?
        .ok_or_else(|| {
            (
                "CredentialsMissing",
                format!("no Secret `{name}` in `{namespace}`"),
            )
        })?;
    GitCredentials::from_secret(name, &secret).map(Some)
}

/// The plain Git credentials the Job mounts; a `githubApp` source gets its token later.
async fn git_credentials_for_job(
    ctx: &Context,
    build: &Build,
) -> Result<Option<GitCredentials>, Refusal> {
    if build.spec.source.github_app.is_some() {
        return Ok(None);
    }
    git_credentials(ctx, build).await
}

/// A token of the `Build`'s installation for its repository, once the namespace is allowed to use
/// the installation.
async fn app_token(ctx: &Context, build: &Build, access: Access) -> Result<String, Refusal> {
    let Some(app) = &build.spec.source.github_app else {
        return Err(("CredentialsMissing", "no githubApp source".to_owned()));
    };
    let namespace = build.namespace().unwrap_or_default();
    ctx.github.allowed(app.installation_id, &namespace).await?;
    let key = RepoKey::parse(&build.spec.source.git.url)
        .map_err(|e| ("UnsupportedURL", e.to_string()))?;
    ctx.github.token(app.installation_id, &key, access).await
}

/// Sets the commit status of a `githubApp` source's run. Nothing happens for other sources.
async fn report(ctx: &Context, build: &Build, sha: &str, state: State, description: &str) {
    let Some(app) = &build.spec.source.github_app else {
        return;
    };
    let namespace = build.namespace().unwrap_or_default();
    let Ok(key) = RepoKey::parse(&build.spec.source.git.url) else {
        return;
    };
    if ctx
        .github
        .allowed(app.installation_id, &namespace)
        .await
        .is_err()
    {
        return;
    }
    let context = format!("zeabur-builder/{namespace}/{}", build.name_any());
    let body = github::status_body(state, &context, description);
    ctx.github
        .report(app.installation_id, &key, sha, &body)
        .await;
}

fn not_found(e: kube::Error) -> Result<(), Error> {
    if let kube::Error::Api(status) = &e
        && status.code == 404
    {
        return Ok(());
    }
    Err(e.into())
}

/// Creates a run of `sha`, carrying the `Build`'s `rebuild` value as the request it answers, with
/// the name [`decide::run_name`] gives it. If that run exists, returns it. If another run has the
/// name, the API server generates one.
async fn create_run(
    api: &Api<BuildRun>,
    build: &Build,
    sha: &str,
    git_ref: Option<String>,
    trigger: Trigger,
) -> Result<BuildRun, Error> {
    let name = build.name_any();
    let owner = build
        .controller_owner_ref(&())
        .ok_or_else(|| Error::Invalid("the Build has no UID".to_owned()))?;
    let rebuild = build
        .annotations()
        .get(annotations::REBUILD)
        .map(|v| BTreeMap::from([(annotations::REBUILD.to_owned(), v.clone())]));
    let run_name = decide::run_name(build, sha);
    let mut run = BuildRun {
        metadata: ObjectMeta {
            name: Some(run_name.clone()),
            namespace: build.namespace(),
            annotations: rebuild,
            labels: Some(BTreeMap::from([
                (labels::BUILD.to_owned(), name.clone()),
                (labels::MANAGED_BY.to_owned(), labels::MANAGER.to_owned()),
            ])),
            owner_references: Some(vec![owner]),
            ..ObjectMeta::default()
        },
        spec: BuildRunSpec {
            build_ref: name,
            revision: sha.to_owned(),
            git_ref,
            plan_digest: None,
            force_replan: false,
            trigger: Some(trigger),
        },
        status: None,
    };
    match api.create(&PostParams::default(), &run).await {
        Err(kube::Error::Api(e)) if e.code == 409 => {}
        created => return Ok(created?),
    }
    if let Some(existing) = api.get_opt(&run_name).await?
        && existing.spec.revision == run.spec.revision
        && existing.annotations().get(annotations::REBUILD)
            == run.annotations().get(annotations::REBUILD)
        && existing
            .owner_references()
            .iter()
            .any(|o| Some(&o.uid) == build.metadata.uid.as_ref())
    {
        return Ok(existing);
    }
    run.metadata.name = None;
    run.metadata.generate_name = Some(format!("{}-", run.spec.build_ref));
    Ok(api.create(&PostParams::default(), &run).await?)
}

async fn cancel_run(ctx: &Context, build: &Build, name: &str) -> Result<(), Error> {
    let namespace = &build.namespace().unwrap_or_default();
    let runs: Api<BuildRun> = Api::namespaced(ctx.client.clone(), namespace);
    let Some(run) = runs.get_opt(name).await? else {
        return Ok(());
    };
    let mut status = run.status.clone().unwrap_or_default();
    status.phase = Some(Phase::Cancelled);
    status.reason = Some(Reason::Cancelled);
    status.message = Some("a newer run replaced it".to_owned());
    status.completion_time = Some(now().to_string());
    apply_status(&runs, name, &status).await?;
    report(
        ctx,
        build,
        &run.spec.revision,
        State::Error,
        "replaced by a newer commit",
    )
    .await;
    Api::<Job>::namespaced(ctx.client.clone(), namespace)
        .delete(
            name,
            &DeleteParams {
                propagation_policy: Some(PropagationPolicy::Background),
                ..DeleteParams::default()
            },
        )
        .await
        .map(drop)
        .or_else(not_found)
}

async fn reconcile_run(run: Arc<BuildRun>, ctx: Arc<Context>) -> Result<Action, Error> {
    if !ctx.leader.is_leader() {
        return Ok(Action::await_change());
    }
    let status = run.status.clone().unwrap_or_default();
    if decide::is_finished(status.phase) {
        return Ok(Action::await_change());
    }
    let namespace = run.namespace().unwrap_or_default();
    let name = run.name_any();
    let runs: Api<BuildRun> = Api::namespaced(ctx.client.clone(), &namespace);
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);

    let Some(job) = jobs.get_opt(&name).await? else {
        if status.job_ref.is_some() {
            return finish(
                &ctx,
                &run,
                BuildRunStatus {
                    phase: Some(Phase::Failed),
                    reason: Some(Reason::InfraError),
                    message: Some("the Job disappeared".to_owned()),
                    ..status
                },
            )
            .await;
        }
        return start(&ctx, &run, status).await;
    };

    let Some(status) = decide::recorded(status, &job, now()) else {
        return Ok(Action::requeue(Duration::from_secs(2)));
    };
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let pod = pods
        .list(&ListParams::default().labels(&format!("batch.kubernetes.io/job-name={name}")))
        .await?
        .items
        .into_iter()
        .max_by_key(|p| p.metadata.creation_timestamp.as_ref().map(|t| t.0));
    match decide::observe(&job, pod.as_ref()) {
        Observed::Running(phase) => {
            if status.phase != Some(phase) || status.reason.is_some() {
                let next = BuildRunStatus {
                    phase: Some(phase),
                    reason: None,
                    ..status
                };
                apply_status(&runs, &name, &next).await?;
            }
            Ok(Action::await_change())
        }
        Observed::Finished(result) => {
            let finished = BuildRunStatus {
                job_ref: status.job_ref,
                buildkitd_instance: status.buildkitd_instance,
                start_time: status.start_time,
                ..*result
            };
            finish(&ctx, &run, finished).await
        }
    }
}

async fn finish(
    ctx: &Context,
    run: &BuildRun,
    mut status: BuildRunStatus,
) -> Result<Action, Error> {
    let namespace = run.namespace().unwrap_or_default();
    let name = run.name_any();
    status.completion_time = Some(now().to_string());
    apply_status(
        &Api::<BuildRun>::namespaced(ctx.client.clone(), &namespace),
        &name,
        &status,
    )
    .await?;
    for copy in [format!("{name}-registry"), format!("{name}-git")] {
        Api::<Secret>::namespaced(ctx.client.clone(), &namespace)
            .delete(&copy, &DeleteParams::default())
            .await
            .map(drop)
            .or_else(not_found)?;
    }
    let (type_, reason, note) = match status.phase {
        Some(Phase::Succeeded) => (
            EventType::Normal,
            "BuildSucceeded",
            format!(
                "pushed {} ({})",
                status.tags.first().map_or("", String::as_str),
                status.image_digest.as_deref().unwrap_or_default()
            ),
        ),
        _ => (
            EventType::Warning,
            "BuildFailed",
            format!(
                "{:?}: {}",
                status.reason.unwrap_or(Reason::InfraError),
                status.message.as_deref().unwrap_or_default()
            ),
        ),
    };
    tracing::info!(buildrun = %name, namespace = %namespace, reason, note, "finished");
    record(ctx, &run.object_ref(&()), type_, reason, "Build", &note).await;
    if let Some(build) = Api::<Build>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(&run.spec.build_ref)
        .await?
    {
        let state = match (status.phase, status.reason) {
            (Some(Phase::Succeeded), _) => State::Success,
            (_, Some(Reason::PlanFailed | Reason::BuildFailed)) => State::Failure,
            _ => State::Error,
        };
        report(ctx, &build, &run.spec.revision, state, &note).await;
    }
    Ok(Action::await_change())
}

async fn record(
    ctx: &Context,
    object: &ObjectReference,
    type_: EventType,
    reason: &str,
    action: &str,
    note: &str,
) {
    let event = Event {
        type_,
        reason: reason.to_owned(),
        note: Some(note.chars().take(1000).collect()),
        action: action.to_owned(),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&event, object).await {
        tracing::warn!(error = %e, "event not recorded");
    }
}

/// Sends a run's image to the `Build`'s notify targets, records each answer as an
/// Event on the `Build` and marks the run sent, whatever the answers: a failure never fails the
/// run nor is sent again. A crash between sending and marking sends once more.
async fn notify_targets(
    ctx: &Context,
    build: &Build,
    run: &BuildRun,
    image: &str,
) -> Result<(), Error> {
    let namespace = build.namespace().unwrap_or_default();
    let reference = build.object_ref(&());
    for target in &build.spec.notify {
        let sent = match target.kind {
            NotifyType::Keel => send_keel(ctx, &namespace, target, image, run).await,
            NotifyType::FluxReceiver | NotifyType::Generic => {
                tracing::warn!(build = %build.name_any(), %namespace, kind = ?target.kind, url = %target.url, "notify type not supported yet; skipped");
                continue;
            }
        };
        match sent {
            Ok(tag) => {
                let note = format!("sent {image}:{tag} to {}", target.url);
                record(
                    ctx,
                    &reference,
                    EventType::Normal,
                    "BuildNotified",
                    "Notify",
                    &note,
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(build = %build.name_any(), %namespace, url = %target.url, error = %e, "notify failed");
                let note = format!("{}: {e}", target.url);
                record(
                    ctx,
                    &reference,
                    EventType::Warning,
                    "NotifyFailed",
                    "Notify",
                    &note,
                )
                .await;
            }
        }
    }
    let status = BuildRunStatus {
        notified_at: Some(now().to_string()),
        ..run.status.clone().unwrap_or_default()
    };
    apply_status(
        &Api::<BuildRun>::namespaced(ctx.client.clone(), &namespace),
        &run.name_any(),
        &status,
    )
    .await
}

/// Keel's native webhook; the tag sent.
async fn send_keel(
    ctx: &Context,
    namespace: &str,
    target: &NotifyTarget,
    image: &str,
    run: &BuildRun,
) -> Result<String, String> {
    let payload =
        decide::keel_payload(image, run).ok_or_else(|| "the run recorded no tag".to_owned())?;
    let credentials = match &target.secret_ref {
        Some(secret) => Some(basic_auth(ctx, namespace, &secret.name).await?),
        None => None,
    };
    let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    notify::post(&ctx.http, &target.url, credentials.as_ref(), &body).await?;
    Ok(payload.tag.to_owned())
}

/// `username` and `password` of a Secret in the `Build`'s namespace.
async fn basic_auth(ctx: &Context, namespace: &str, name: &str) -> Result<Credentials, String> {
    let secret = Api::<Secret>::namespaced(ctx.client.clone(), namespace)
        .get_opt(name)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no Secret `{name}`"))?;
    let data = secret.data.unwrap_or_default();
    let key = |key: &str| {
        data.get(key)
            .and_then(|v| String::from_utf8(v.0.clone()).ok())
            .ok_or_else(|| format!("Secret `{name}` has no `{key}`"))
    };
    Ok(Credentials {
        username: key("username")?,
        password: key("password")?,
    })
}

async fn pending(
    ctx: &Context,
    run: &BuildRun,
    mut status: BuildRunStatus,
    reason: Reason,
    message: String,
) -> Result<Action, Error> {
    let retry = if reason == Reason::BuildkitdStarting {
        STARTING_RETRY
    } else {
        PENDING_RETRY
    };
    status.phase = Some(Phase::Pending);
    status.reason = Some(reason);
    status.message = Some(message);
    if run.status.as_ref() != Some(&status) {
        let api =
            Api::<BuildRun>::namespaced(ctx.client.clone(), &run.namespace().unwrap_or_default());
        apply_status(&api, &run.name_any(), &status).await?;
    }
    Ok(Action::requeue(retry))
}

/// The run's `Build` and its `ClusterBuilder`, or why one is missing.
async fn build_and_builder(
    ctx: &Context,
    run: &BuildRun,
) -> Result<Result<(Build, ClusterBuilder), String>, Error> {
    let builds: Api<Build> =
        Api::namespaced(ctx.client.clone(), &run.namespace().unwrap_or_default());
    let Some(build) = builds.get_opt(&run.spec.build_ref).await? else {
        return Ok(Err(format!("no Build `{}`", run.spec.build_ref)));
    };
    let builder = Api::<ClusterBuilder>::all(ctx.client.clone())
        .get_opt(&build.spec.builder_ref)
        .await?;
    Ok(match builder {
        Some(builder) => Ok((build, builder)),
        None => Err(format!("no ClusterBuilder `{}`", build.spec.builder_ref)),
    })
}

async fn create_if_missing<K>(api: &Api<K>, object: &K) -> Result<(), Error>
where
    K: kube::Resource + Clone + Serialize + serde::de::DeserializeOwned + std::fmt::Debug + Sync,
{
    match api.create(&PostParams::default(), object).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Whether the namespace may start another build: a free slot and push credentials, which it
/// returns; otherwise why it waits.
async fn admit(
    ctx: &Context,
    namespace: &str,
) -> Result<Result<ByteString, (Reason, String)>, Error> {
    let settings = &ctx.settings;
    let limit = decide::concurrency_limit(
        Api::<Namespace>::all(ctx.client.clone())
            .get_opt(namespace)
            .await?
            .as_ref()
            .and_then(|n| n.annotations().get(annotations::MAX_CONCURRENT_BUILDS))
            .map(String::as_str),
        settings.max_concurrent,
    );
    let limit_slots = usize::try_from(limit).unwrap_or(usize::MAX);
    let runs = Api::<BuildRun>::namespaced(ctx.client.clone(), namespace);
    let mut params = ListParams::default().fields(UNFINISHED).limit(PAGE);
    let mut taken = 0;
    loop {
        let page = runs.list(&params).await?;
        taken += decide::slots_taken(&page.items);
        match page.metadata.continue_ {
            Some(next) if !next.is_empty() && taken < limit_slots => {
                params = params.continue_token(&next);
            }
            _ => break,
        }
    }
    if taken >= limit_slots {
        return Ok(Err((
            Reason::QuotaExceeded,
            format!("{taken} of the namespace's {limit} builds are running"),
        )));
    }
    Ok(registry_auth(ctx, namespace).await?.ok_or_else(|| {
        (
            Reason::RegistryCredentialsMissing,
            format!(
                "no Secret `{}{namespace}` with push credentials in `{}`",
                settings.registry_auth_prefix.as_deref().unwrap_or_default(),
                settings.namespace
            ),
        )
    }))
}

/// Records the Job a run waits on.
async fn job_created(
    ctx: &Context,
    run: &BuildRun,
    status: BuildRunStatus,
    pod: &str,
    tags: Vec<String>,
) -> Result<(), Error> {
    let name = run.name_any();
    let next = BuildRunStatus {
        phase: Some(Phase::Pending),
        reason: None,
        message: None,
        job_ref: Some(name.clone()),
        buildkitd_instance: Some(pod.to_owned()),
        start_time: Some(now().to_string()),
        tags,
        ..status
    };
    apply_status(
        &Api::<BuildRun>::namespaced(ctx.client.clone(), &run.namespace().unwrap_or_default()),
        &name,
        &next,
    )
    .await
}

async fn start(ctx: &Context, run: &BuildRun, status: BuildRunStatus) -> Result<Action, Error> {
    let namespace = run.namespace().unwrap_or_default();
    let name = run.name_any();
    let (build, builder) = match build_and_builder(ctx, run).await? {
        Ok(found) => found,
        Err(message) => return pending(ctx, run, status, Reason::InfraError, message).await,
    };
    let fail = |message: String| BuildRunStatus {
        phase: Some(Phase::Failed),
        reason: Some(Reason::InfraError),
        message: Some(message),
        ..BuildRunStatus::default()
    };
    let source_failed = |message: String| BuildRunStatus {
        reason: Some(Reason::SourceFailed),
        ..fail(message)
    };
    if let Err(e) = RepoKey::parse(&build.spec.source.git.url) {
        return finish(ctx, run, fail(e.to_string())).await;
    }
    let tags = match decide::tags(&build, run) {
        Ok(tags) => tags,
        Err(e) => return finish(ctx, run, fail(e.to_string())).await,
    };

    let credentials = match git_credentials_for_job(ctx, &build).await {
        Ok(credentials) => credentials,
        Err(("CredentialsUnreadable", message)) => {
            return pending(ctx, run, status, Reason::InfraError, message).await;
        }
        Err((_, message)) => return finish(ctx, run, source_failed(message)).await,
    };
    let git_secret = format!("{name}-git");
    let mut git_auth = match (&credentials, &build.spec.source.credentials_ref) {
        (Some(credentials), Some(secret)) => credentials.job_auth(&secret.name),
        _ => GitAuth::Anonymous,
    };

    let settings = &ctx.settings;
    let registry_auth = match admit(ctx, &namespace).await? {
        Ok(auth) => auth,
        Err((reason, message)) => return pending(ctx, run, status, reason, message).await,
    };
    let woken = daemon::wake(&ctx.client, &settings.daemon, &namespace).await?;
    if !woken.ready {
        return pending(
            ctx,
            run,
            status,
            Reason::BuildkitdStarting,
            format!("waiting for {} to be ready", woken.pod),
        )
        .await;
    }
    if build.spec.source.github_app.is_some() {
        // Minted only now, so a run waiting for a slot does not mint a token every retry.
        match app_token(ctx, &build, Access::Fetch).await {
            Ok(token) => ensure_git_secret(ctx, run, &git_secret, token).await?,
            Err(("ResolveFailed", message)) => {
                return pending(ctx, run, status, Reason::InfraError, message).await;
            }
            Err((_, message)) => return finish(ctx, run, source_failed(message)).await,
        }
        git_auth = GitAuth::Basic {
            secret: &git_secret,
        };
    }
    tls::put_client(&ctx.client, &namespace, CLIENT_TLS_COPY, &woken.client).await?;
    let registry_secret = format!("{name}-registry");
    ensure_registry_secret(ctx, run, &registry_secret, registry_auth).await?;

    let image = decide::image(&build, &settings.default_registry);
    let job = zeabur_builder_jobspec::job(&JobInputs {
        build_run: run,
        build: &build,
        builder_name: &build.spec.builder_ref,
        builder: &builder.spec,
        image: &image,
        tags: &tags,
        buildkitd_addr: &woken.address,
        job_image: &settings.job_image,
        git_auth,
        planner_credentials: None,
        buildkit_tls_secret: CLIENT_TLS_COPY,
        registry_auth_secret: &registry_secret,
        deadline_seconds: settings.job_deadline_seconds,
        ttl_seconds: settings.job_ttl_seconds,
    });
    let job = match job {
        Ok(job) => job,
        Err(e) => return finish(ctx, run, fail(e.to_string())).await,
    };
    create_if_missing(&Api::namespaced(ctx.client.clone(), &namespace), &job).await?;
    job_created(ctx, run, status, &woken.pod, tags).await?;
    tracing::info!(buildrun = %name, namespace = %namespace, buildkitd = %woken.pod, image = %image, "job created");
    report(ctx, &build, &run.spec.revision, State::Pending, "building").await;
    Ok(Action::await_change())
}

/// A namespace's push credentials: its own Secret when the controller is given a
/// prefix, `None` while that Secret is missing; an empty Docker config otherwise, for a registry
/// that takes anonymous pushes.
async fn registry_auth(ctx: &Context, namespace: &str) -> Result<Option<ByteString>, Error> {
    let Some(prefix) = &ctx.settings.registry_auth_prefix else {
        return Ok(Some(ByteString(br#"{"auths":{}}"#.to_vec())));
    };
    let name = format!("{prefix}{namespace}");
    let Some(secret) = Api::<Secret>::namespaced(ctx.client.clone(), &ctx.settings.namespace)
        .get_opt(&name)
        .await?
    else {
        return Ok(None);
    };
    secret
        .data
        .and_then(|mut d| d.remove(".dockerconfigjson"))
        .map(Some)
        .ok_or_else(|| Error::Invalid(format!("Secret `{name}` has no .dockerconfigjson")))
}

/// The run's installation token, as a `kubernetes.io/basic-auth` Secret owned by the `BuildRun`
/// and deleted when it finishes.
async fn ensure_git_secret(
    ctx: &Context,
    run: &BuildRun,
    name: &str,
    token: String,
) -> Result<(), Error> {
    let namespace = run.namespace().unwrap_or_default();
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.clone()),
            labels: Some(BTreeMap::from([
                (labels::BUILD_RUN.to_owned(), run.name_any()),
                (labels::MANAGED_BY.to_owned(), labels::MANAGER.to_owned()),
            ])),
            owner_references: run.controller_owner_ref(&()).map(|o| vec![o]),
            ..ObjectMeta::default()
        },
        type_: Some("kubernetes.io/basic-auth".to_owned()),
        string_data: Some(BTreeMap::from([
            ("username".to_owned(), "x-access-token".to_owned()),
            ("password".to_owned(), token),
        ])),
        ..Secret::default()
    };
    create_if_missing(&Api::namespaced(ctx.client.clone(), &namespace), &secret).await
}

/// The run's copy of the push credentials, owned by the `BuildRun` and deleted when it finishes.
async fn ensure_registry_secret(
    ctx: &Context,
    run: &BuildRun,
    name: &str,
    config: ByteString,
) -> Result<(), Error> {
    let namespace = run.namespace().unwrap_or_default();
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.clone()),
            labels: Some(BTreeMap::from([
                (labels::BUILD_RUN.to_owned(), run.name_any()),
                (labels::MANAGED_BY.to_owned(), labels::MANAGER.to_owned()),
            ])),
            owner_references: run.controller_owner_ref(&()).map(|o| vec![o]),
            ..ObjectMeta::default()
        },
        type_: Some("kubernetes.io/dockerconfigjson".to_owned()),
        data: Some(BTreeMap::from([(".dockerconfigjson".to_owned(), config)])),
        ..Secret::default()
    };
    create_if_missing(&Api::namespaced(ctx.client.clone(), &namespace), &secret).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_ten_percent() {
        let minute = Duration::from_mins(1);
        for name in ["a", "api", "a-much-longer-build-name"] {
            let d = jitter(minute, name);
            assert!(
                d >= Duration::from_secs(54) && d <= Duration::from_secs(66),
                "{d:?}"
            );
        }
    }
}
