//! The two reconcilers (plan §4.1, §4.2), on kube-runtime.
//!
//! - **Build**: resolves the ref, creates a `BuildRun` for a new commit, cancels or waits for older
//!   runs, prunes history and summarises the newest result in its status. Requeued every poll
//!   interval; the ref is asked again only when the last answer is older than the interval or the
//!   spec changed, so status changes of its runs cost no request to the Git server.
//! - **`BuildRun`**: picks a buildkitd instance, creates the Job and follows it to a result.
//!
//! Decisions are in [`crate::decide`]; this module only reads and writes objects. Status is
//! written with server-side apply, so a field the controller stops setting is removed.
//!
//! Not yet: leader election (run one replica), the repository-level poller and webhooks
//! (plan §4.3, §4.4), the `rebuild` annotation, and per-namespace quotas.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use jiff::Timestamp;
use k8s_openapi::ByteString;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::{
    Api, DeleteParams, ListParams, ObjectMeta, Patch, PatchParams, PostParams, PropagationPolicy,
};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::runtime::watcher;
use kube::{Client, Resource as _, ResourceExt as _};
use serde::Serialize;
use thiserror::Error;
use zeabur_builder_api::build::{Build, BuildStatus, SourceMode};
use zeabur_builder_api::build_run::{
    BuildRun, BuildRunSpec, BuildRunStatus, Phase, Reason, Trigger,
};
use zeabur_builder_api::cluster_builder::ClusterBuilder;
use zeabur_builder_api::common::ConditionStatus;
use zeabur_builder_api::labels;
use zeabur_builder_core::repo::RepoKey;
use zeabur_builder_core::route::{self, Instance, Route};
use zeabur_builder_jobspec::{GitAuth, JobInputs};

use crate::decide::{self, Observed};
use crate::remote::Resolver;
use crate::tls;

const MANAGER: &str = "zeabur-builder";
/// How often a waiting `BuildRun` is looked at again.
const PENDING_RETRY: Duration = Duration::from_secs(15);
/// The name of the client certificate Secret in a build namespace.
const CLIENT_TLS_COPY: &str = "zeabur-builder-buildkit-client";

/// Settings, from the command line.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// Where the certificates live.
    pub(crate) tls: tls::Locations,
    /// Selects buildkitd Pods in `tls.buildkitd_namespace`.
    pub(crate) buildkitd_selector: String,
    /// Builds one buildkitd instance runs at once.
    pub(crate) buildkitd_slots: u32,
    /// The registry for `Build`s without `spec.output.image`.
    pub(crate) default_registry: String,
    /// A `kubernetes.io/dockerconfigjson` Secret in the controller's namespace.
    pub(crate) registry_auth_secret: Option<String>,
    /// The image of `fetch` and `build`.
    pub(crate) job_image: String,
    /// `activeDeadlineSeconds`.
    pub(crate) job_deadline_seconds: i64,
    /// `ttlSecondsAfterFinished`.
    pub(crate) job_ttl_seconds: i32,
}

struct Context {
    client: Client,
    resolver: Resolver,
    recorder: Recorder,
    settings: Settings,
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

/// Runs both controllers until a termination signal.
pub(crate) async fn run(client: Client, settings: Settings) -> Result<(), String> {
    tls::ensure(&client, &settings.tls)
        .await
        .map_err(|e| format!("cannot create buildkitd certificates: {e}"))?;
    let context = Arc::new(Context {
        recorder: Recorder::new(
            client.clone(),
            Reporter {
                controller: MANAGER.to_owned(),
                instance: std::env::var("POD_NAME").ok(),
            },
        ),
        resolver: Resolver::new()?,
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
    .shutdown_on_signal()
    .run(reconcile_run, retry, context)
    .for_each(|result| async move {
        if let Err(e) = result {
            tracing::warn!(error = %e, "buildrun reconcile");
        }
    });
    tokio::join!(builds, runs);
    Ok(())
}

fn retry<K>(_: Arc<K>, error: &Error, _: Arc<Context>) -> Action {
    tracing::warn!(%error, "reconcile failed; retrying in 30s");
    Action::requeue(Duration::from_secs(30))
}

/// Spreads polls by up to ±10% (plan §4.4), from the name and the clock's nanoseconds.
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

async fn reconcile_build(build: Arc<Build>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = build.namespace().unwrap_or_default();
    let name = build.name_any();
    let generation = build.metadata.generation;
    let builds: Api<Build> = Api::namespaced(ctx.client.clone(), &namespace);
    let runs_api: Api<BuildRun> = Api::namespaced(ctx.client.clone(), &namespace);
    let at = now();
    let mut status = build.status.clone().unwrap_or_default();
    status.source_mode = Some(SourceMode::PlainGit);
    let interval = build
        .spec
        .triggers
        .poll_interval
        .as_deref()
        .and_then(decide::parse_duration)
        .unwrap_or(decide::DEFAULT_POLL);

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
        generation,
    );

    let resolved = resolve_source(&ctx, &build, &mut status, interval, at).await;

    let mut runs = runs_api
        .list(&ListParams::default().fields(&format!("spec.buildRef={name}")))
        .await?
        .items;
    if let (Some(sha), Some(_)) = (&resolved, &builder) {
        let next = decide::next_run(&build, sha, &runs);
        for cancel in &next.cancel {
            cancel_run(&ctx, &namespace, cancel).await?;
        }
        if next.create {
            let run = create_run(&runs_api, &build, sha).await?;
            tracing::info!(build = %name, namespace = %namespace, buildrun = %run.name_any(), revision = %sha, "new commit");
            runs.push(run);
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
    if build.status.as_ref() != Some(&status) {
        apply_status(&builds, &name, &status).await?;
    }
    Ok(Action::requeue(jitter(interval, &name)))
}

/// Resolves the ref, or reuses the last answer while it is fresh, and sets `SourceReady`.
async fn resolve_source(
    ctx: &Context,
    build: &Build,
    status: &mut BuildStatus,
    interval: Duration,
    at: Timestamp,
) -> Option<String> {
    let generation = build.metadata.generation;
    let git = &build.spec.source.git;
    let resolved = if build.spec.source.credentials_ref.is_some() {
        Err((
            "CredentialsMissing",
            "Git credentials are not supported yet; only public repositories".to_owned(),
        ))
    } else if !decide::needs_resolve(build.status.as_ref(), generation, interval, at) {
        status
            .remote_revision
            .clone()
            .ok_or(("ResolveFailed", String::new()))
    } else {
        match ctx.resolver.resolve(&git.url, &git.git_ref).await {
            Ok(sha) => {
                status.last_resolved_at = Some(at.to_string());
                Ok(sha)
            }
            Err(e) => Err((e.reason(), e.to_string())),
        }
    };
    match &resolved {
        Ok(sha) => {
            status.remote_revision = Some(sha.clone());
            decide::set_condition(
                &mut status.conditions,
                "SourceReady",
                ConditionStatus::True,
                "Resolved",
                &format!("{} is {sha}", git.git_ref),
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

fn not_found(e: kube::Error) -> Result<(), Error> {
    if let kube::Error::Api(status) = &e
        && status.code == 404
    {
        return Ok(());
    }
    Err(e.into())
}

async fn create_run(api: &Api<BuildRun>, build: &Build, sha: &str) -> Result<BuildRun, Error> {
    let name = build.name_any();
    let owner = build
        .controller_owner_ref(&())
        .ok_or_else(|| Error::Invalid("the Build has no UID".to_owned()))?;
    let run = BuildRun {
        metadata: ObjectMeta {
            generate_name: Some(format!("{name}-")),
            namespace: build.namespace(),
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
            plan_digest: None,
            force_replan: false,
            trigger: Some(Trigger::Poll),
        },
        status: None,
    };
    Ok(api.create(&PostParams::default(), &run).await?)
}

async fn cancel_run(ctx: &Context, namespace: &str, name: &str) -> Result<(), Error> {
    let runs: Api<BuildRun> = Api::namespaced(ctx.client.clone(), namespace);
    let Some(run) = runs.get_opt(name).await? else {
        return Ok(());
    };
    let mut status = run.status.clone().unwrap_or_default();
    status.phase = Some(Phase::Cancelled);
    status.reason = Some(Reason::Cancelled);
    status.message = Some("a newer commit replaced it".to_owned());
    status.completion_time = Some(now().to_string());
    apply_status(&runs, name, &status).await?;
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
    Api::<Secret>::namespaced(ctx.client.clone(), &namespace)
        .delete(&format!("{name}-registry"), &DeleteParams::default())
        .await
        .map(drop)
        .or_else(not_found)?;
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
    let event = Event {
        type_,
        reason: reason.to_owned(),
        note: Some(note.chars().take(1000).collect()),
        action: "Build".to_owned(),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&event, &run.object_ref(&())).await {
        tracing::warn!(error = %e, "event not recorded");
    }
    Ok(Action::await_change())
}

async fn pending(
    ctx: &Context,
    run: &BuildRun,
    mut status: BuildRunStatus,
    reason: Reason,
    message: String,
) -> Result<Action, Error> {
    status.phase = Some(Phase::Pending);
    status.reason = Some(reason);
    status.message = Some(message);
    if run.status.as_ref() != Some(&status) {
        let api =
            Api::<BuildRun>::namespaced(ctx.client.clone(), &run.namespace().unwrap_or_default());
        apply_status(&api, &run.name_any(), &status).await?;
    }
    Ok(Action::requeue(PENDING_RETRY))
}

/// The buildkitd instance for a routing key (plan §7.3), counting unfinished runs against slots.
async fn pick_instance(ctx: &Context, key: &str, run: &str) -> Result<Option<String>, Error> {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ctx.settings.tls.buildkitd_namespace);
    let pods = pods
        .list(&ListParams::default().labels(&ctx.settings.buildkitd_selector))
        .await?
        .items;
    let runs = Api::<BuildRun>::all(ctx.client.clone())
        .list(&ListParams::default())
        .await?
        .items;
    let instances: Vec<Instance> = pods
        .iter()
        .map(|pod| {
            let name = pod.name_any();
            let ready = pod
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
            let running = runs
                .iter()
                .filter_map(|r| r.status.as_ref())
                .filter(|s| !decide::is_finished(s.phase) && s.job_ref.is_some())
                .filter(|s| s.buildkitd_instance.as_deref() == Some(name.as_str()))
                .count();
            Instance {
                name,
                ready,
                running: u32::try_from(running).unwrap_or(u32::MAX),
                slots: ctx.settings.buildkitd_slots,
            }
        })
        .collect();
    Ok(match route::pick(key, &instances) {
        Route::Preferred(i) => Some(i.name.clone()),
        Route::Fallback(i) => {
            tracing::info!(buildrun = %run, instance = %i.name, "routed to a fallback instance");
            Some(i.name.clone())
        }
        Route::NoCapacity => None,
    })
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
    let repo = match RepoKey::parse(&build.spec.source.git.url) {
        Ok(repo) => repo,
        Err(e) => return finish(ctx, run, fail(e.to_string())).await,
    };
    let tags = match decide::tags(&build, run) {
        Ok(tags) => tags,
        Err(e) => return finish(ctx, run, fail(e.to_string())).await,
    };

    let key = route::route_key(
        &repo,
        build
            .spec
            .source
            .git
            .context_dir
            .as_deref()
            .unwrap_or_default(),
    );
    let Some(instance) = pick_instance(ctx, &key, &name).await? else {
        return pending(
            ctx,
            run,
            status,
            Reason::NoBuildkitdCapacity,
            "no buildkitd instance is ready with a free slot".to_owned(),
        )
        .await;
    };
    let settings = &ctx.settings;
    let addr = format!(
        "tcp://{instance}.{}.{}.svc:1234",
        settings.tls.service, settings.tls.buildkitd_namespace
    );

    tls::copy_secret(
        &ctx.client,
        &settings.tls.controller_namespace,
        &settings.tls.client_secret,
        &namespace,
        CLIENT_TLS_COPY,
    )
    .await?;
    let registry_secret = format!("{name}-registry");
    ensure_registry_secret(ctx, run, &registry_secret).await?;

    let image = decide::image(&build, &settings.default_registry);
    let job = zeabur_builder_jobspec::job(&JobInputs {
        build_run: run,
        build: &build,
        builder_name: &build.spec.builder_ref,
        builder: &builder.spec,
        image: &image,
        tags: &tags,
        buildkitd_addr: &addr,
        job_image: &settings.job_image,
        git_auth: GitAuth::Anonymous,
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
    let next = BuildRunStatus {
        phase: Some(Phase::Pending),
        reason: None,
        message: None,
        job_ref: Some(name.clone()),
        buildkitd_instance: Some(instance.clone()),
        start_time: Some(now().to_string()),
        tags: tags.clone(),
        ..status
    };
    apply_status(
        &Api::<BuildRun>::namespaced(ctx.client.clone(), &namespace),
        &name,
        &next,
    )
    .await?;
    tracing::info!(buildrun = %name, namespace = %namespace, %instance, image = %image, "job created");
    Ok(Action::await_change())
}

/// The run's copy of the push credentials (plan §8), owned by the `BuildRun` and deleted when it
/// finishes. Without a configured Secret, an empty Docker config: the registry takes anonymous
/// pushes.
async fn ensure_registry_secret(ctx: &Context, run: &BuildRun, name: &str) -> Result<(), Error> {
    let namespace = run.namespace().unwrap_or_default();
    let config = match &ctx.settings.registry_auth_secret {
        Some(source) => {
            Api::<Secret>::namespaced(ctx.client.clone(), &ctx.settings.tls.controller_namespace)
                .get(source)
                .await?
                .data
                .and_then(|mut d| d.remove(".dockerconfigjson"))
                .ok_or_else(|| {
                    Error::Invalid(format!("Secret `{source}` has no .dockerconfigjson"))
                })?
        }
        None => ByteString(br#"{"auths":{}}"#.to_vec()),
    };
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
