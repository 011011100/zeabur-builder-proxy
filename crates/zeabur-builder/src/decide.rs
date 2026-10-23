//! The reconcilers' decisions, as functions of what they observed: no I/O and no clock (the time
//! is an argument), so every transition is tested without a cluster (plan §14).

use std::time::Duration;

use jiff::Timestamp;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde::Serialize;
use zeabur_builder_api::annotations;
use zeabur_builder_api::build::{Build, BuildStatus, Concurrency};
use zeabur_builder_api::build_run::{
    BuildRun, BuildRunStatus, CacheStats, Phase, PlanSource, Reason, Trigger,
};
use zeabur_builder_api::common::{Condition, ConditionStatus};
use zeabur_builder_core::result::{self, BuildResult, FailureReason, Outcome};
use zeabur_builder_core::tag::{self, TagError, TagInputs};

/// Polling interval for plain Git when the `Build` sets none (plan §3.2).
pub(crate) const DEFAULT_POLL: Duration = Duration::from_mins(1);
/// Finished `BuildRun`s kept when the `Build` sets no limit.
pub(crate) const DEFAULT_HISTORY: usize = 10;

/// Reads a Go duration such as `90s`, `1m30s` or `1.5h`, the form `DURATION_PATTERN` admits.
pub(crate) fn parse_duration(text: &str) -> Option<Duration> {
    let mut total = 0.0_f64;
    let mut rest = text;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .filter(|&n| n > 0)?;
        let (number, after) = rest.split_at(digits);
        let number: f64 = number.parse().ok()?;
        let (unit, after) = [("ms", 0.001), ("s", 1.0), ("m", 60.0), ("h", 3600.0)]
            .iter()
            .find_map(|(unit, seconds)| after.strip_prefix(unit).map(|a| (*seconds, a)))?;
        total = number.mul_add(unit, total);
        rest = after;
    }
    Duration::try_from_secs_f64(total).ok()
}

/// Whether a phase is final.
pub(crate) const fn is_finished(phase: Option<Phase>) -> bool {
    matches!(
        phase,
        Some(Phase::Succeeded | Phase::Failed | Phase::Cancelled)
    )
}

/// How long a Job may go unrecorded in its run's status before the controller takes it as
/// stopped between creating the Job and recording it.
pub(crate) const START_GRACE: Duration = Duration::from_mins(1);

/// The status to build on once a run's Job exists: as it is when it names the Job; `None` while
/// the start may still be recording it, since writing an older status would drop what the start
/// writes; past [`START_GRACE`], the Job adopted.
pub(crate) fn recorded(
    status: BuildRunStatus,
    job: &Job,
    now: Timestamp,
) -> Option<BuildRunStatus> {
    if status.job_ref.is_some() {
        return Some(status);
    }
    let created = job.metadata.creation_timestamp.as_ref()?.0;
    let age = now.duration_since(created).unsigned_abs();
    (age >= START_GRACE).then(|| BuildRunStatus {
        job_ref: job.metadata.name.clone(),
        start_time: Some(created.to_string()),
        ..status
    })
}

fn phase(run: &BuildRun) -> Option<Phase> {
    run.status.as_ref().and_then(|s| s.phase)
}

/// Sets a condition, keeping its transition time when the status did not change.
pub(crate) fn set_condition(
    conditions: &mut Vec<Condition>,
    kind: &str,
    status: ConditionStatus,
    reason: &str,
    message: &str,
    now: Timestamp,
    generation: Option<i64>,
) {
    let transition = conditions
        .iter()
        .find(|c| c.kind == kind && c.status == status)
        .map_or_else(|| now.to_string(), |c| c.last_transition_time.clone());
    conditions.retain(|c| c.kind != kind);
    conditions.push(Condition {
        kind: kind.to_owned(),
        status,
        reason: reason.to_owned(),
        message: message.to_owned(),
        last_transition_time: transition,
        observed_generation: generation,
    });
}

/// Whether the ref must be asked again, or the last answer is recent enough. A new generation
/// (the spec changed) always asks, and so does a pending rebuild, which builds the commit the ref
/// points at now.
pub(crate) fn needs_resolve(
    status: Option<&BuildStatus>,
    generation: Option<i64>,
    rebuild: bool,
    interval: Duration,
    now: Timestamp,
) -> bool {
    let Some(status) = status else { return true };
    if rebuild || status.remote_revision.is_none() || status.observed_generation != generation {
        return true;
    }
    let Some(last) = status
        .last_resolved_at
        .as_deref()
        .and_then(|t| t.parse::<Timestamp>().ok())
    else {
        return true;
    };
    now.duration_since(last).unsigned_abs() >= interval
}

fn rebuild_value(meta: &ObjectMeta) -> Option<&str> {
    meta.annotations
        .as_ref()?
        .get(annotations::REBUILD)
        .map(String::as_str)
}

/// The newest run that records a `rebuild` value: the request last answered.
fn answered_rebuild(runs: &[BuildRun]) -> Option<&BuildRun> {
    runs.iter()
        .filter(|r| rebuild_value(&r.metadata).is_some())
        .max_by_key(|r| {
            (
                r.metadata.creation_timestamp.as_ref().map(|t| t.0),
                r.metadata.name.as_deref(),
            )
        })
}

/// The `Build`'s `rebuild` value when it differs from the one on the newest run that records
/// one. Every run the controller creates records the value of that moment, so a value set before
/// a build asks for nothing more once that build starts.
///
/// A suspended `Build` has none pending: plan §4.1 skips steps 4 to 6, the rebuild among them.
/// The request is kept and runs when `suspend` is cleared; a `BuildRun` created by hand still
/// runs while suspended.
pub(crate) fn pending_rebuild<'a>(build: &'a Build, runs: &[BuildRun]) -> Option<&'a str> {
    if build.spec.suspend {
        return None;
    }
    let wanted = rebuild_value(&build.metadata)?;
    let answered = answered_rebuild(runs).and_then(|r| rebuild_value(&r.metadata));
    (answered != Some(wanted)).then_some(wanted)
}

/// What the `Build` reconciler does about the commit the ref points at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NextRun {
    /// Start a `BuildRun` for the commit, with this trigger.
    pub(crate) create: Option<Trigger>,
    /// Cancel these unfinished runs (`Replace`).
    pub(crate) cancel: Vec<String>,
}

/// Decides whether `remote` needs a build (plan §4.1, steps 4 to 6).
///
/// A commit that already has a `BuildRun`, in any phase, gets no other: a failed build is not
/// retried by itself, which would loop on a broken commit. A [pending rebuild](pending_rebuild)
/// builds it again, as `Manual`, under the same concurrency rules as a new commit.
pub(crate) fn next_run(build: &Build, remote: &str, runs: &[BuildRun]) -> NextRun {
    if build.spec.suspend {
        return NextRun::default();
    }
    let latest = build
        .status
        .as_ref()
        .and_then(|s| s.latest_revision.as_deref());
    let new_commit = latest != Some(remote) && !runs.iter().any(|r| r.spec.revision == remote);
    let trigger = if new_commit {
        Trigger::Poll
    } else if pending_rebuild(build, runs).is_some() {
        Trigger::Manual
    } else {
        return NextRun::default();
    };
    let active: Vec<String> = runs
        .iter()
        .filter(|r| !is_finished(phase(r)))
        .filter_map(|r| r.metadata.name.clone())
        .collect();
    match build.spec.concurrency {
        Concurrency::Replace => NextRun {
            create: Some(trigger),
            cancel: active,
        },
        Concurrency::Queue if active.is_empty() => NextRun {
            create: Some(trigger),
            cancel: Vec::new(),
        },
        Concurrency::Queue => NextRun::default(),
    }
}

fn finished_at(run: &BuildRun) -> String {
    run.status
        .as_ref()
        .and_then(|s| s.completion_time.clone())
        .or_else(|| {
            run.metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.to_string())
        })
        .unwrap_or_default()
}

/// The newest successful run.
pub(crate) fn latest_success(runs: &[BuildRun]) -> Option<&BuildRun> {
    runs.iter()
        .filter(|r| phase(r) == Some(Phase::Succeeded))
        .max_by_key(|r| finished_at(r))
}

/// The newest finished run.
pub(crate) fn latest_finished(runs: &[BuildRun]) -> Option<&BuildRun> {
    runs.iter()
        .filter(|r| is_finished(phase(r)))
        .max_by_key(|r| finished_at(r))
}

/// Finished runs beyond the `Build`'s history limit, oldest first. The newest success is kept
/// whatever its age, since `status.latestImage` points at it, and so is the run that answered the
/// last rebuild, without which the same request would look new.
pub(crate) fn prune(runs: &[BuildRun], limit: usize) -> Vec<String> {
    let keep = [latest_success(runs), answered_rebuild(runs)]
        .map(|r| r.and_then(|r| r.metadata.name.as_deref()));
    let mut finished: Vec<&BuildRun> = runs.iter().filter(|r| is_finished(phase(r))).collect();
    finished.sort_by_key(|r| std::cmp::Reverse(finished_at(r)));
    let mut names: Vec<String> = finished
        .into_iter()
        .skip(limit)
        .filter_map(|r| r.metadata.name.clone())
        .filter(|name| !keep.contains(&Some(name.as_str())))
        .collect();
    names.reverse();
    names
}

/// Concurrent builds per namespace when the controller sets none (plan §11).
pub(crate) const DEFAULT_MAX_CONCURRENT: u32 = 3;

/// A namespace's limit of concurrent builds: its annotation when it holds a positive number,
/// else the controller's.
pub(crate) fn concurrency_limit(annotation: Option<&str>, default: u32) -> u32 {
    annotation
        .and_then(|a| a.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Runs that hold a slot of their namespace's buildkitd: unfinished, with a Job.
pub(crate) fn slots_taken(runs: &[BuildRun]) -> usize {
    runs.iter()
        .filter(|r| !is_finished(phase(r)))
        .filter(|r| r.status.as_ref().is_some_and(|s| s.job_ref.is_some()))
        .count()
}

fn last_activity(run: &BuildRun) -> Option<Timestamp> {
    let created = run.metadata.creation_timestamp.as_ref().map(|t| t.0);
    let completed = run
        .status
        .as_ref()
        .and_then(|s| s.completion_time.as_deref())
        .and_then(|t| t.parse::<Timestamp>().ok());
    created.max(completed)
}

/// Whether a namespace's buildkitd may scale to zero: none of its runs is unfinished, and none
/// was created or finished within `after`. A run created a moment ago counts, so a build that is
/// starting keeps the daemon it is about to use.
pub(crate) fn idle(runs: &[BuildRun], now: Timestamp, after: Duration) -> bool {
    if runs.iter().any(|r| !is_finished(phase(r))) {
        return false;
    }
    runs.iter()
        .filter_map(last_activity)
        .max()
        .is_none_or(|last| now.duration_since(last).unsigned_abs() >= after)
}

/// The image repository a `Build` pushes to.
pub(crate) fn image(build: &Build, default_registry: &str) -> String {
    if let Some(image) = build.spec.output.as_ref().and_then(|o| o.image.clone()) {
        return image;
    }
    let namespace = build.metadata.namespace.as_deref().unwrap_or_default();
    let name = build.metadata.name.as_deref().unwrap_or_default();
    format!(
        "{}/{namespace}/{name}",
        default_registry.trim_end_matches('/')
    )
}

/// A ref without its `refs/heads/` or `refs/tags/` prefix.
pub(crate) fn short_ref(git_ref: &str) -> &str {
    git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(git_ref)
}

/// The `SourceReady` message: `main is <sha>`, or `HEAD (master) is <sha>` for the default
/// branch.
pub(crate) fn resolved_message(git_ref: &str, resolved: Option<&str>, sha: &str) -> String {
    match resolved.map(short_ref) {
        Some(name) if short_ref(git_ref) != name => {
            format!("{git_ref} ({name}) is {sha}")
        }
        _ => format!("{git_ref} is {sha}"),
    }
}

/// The tags of a run: the template's, then the extra ones.
pub(crate) fn tags(build: &Build, run: &BuildRun) -> Result<Vec<String>, TagError> {
    let output = build.spec.output.as_ref();
    let template = output
        .and_then(|o| o.tag_template.as_deref())
        .unwrap_or(tag::DEFAULT_TEMPLATE);
    let git_ref = run
        .spec
        .git_ref
        .as_deref()
        .unwrap_or(&build.spec.source.git.git_ref);
    let branch = short_ref(git_ref);
    let unix_time = run
        .metadata
        .creation_timestamp
        .as_ref()
        .map_or(0, |t| t.0.as_second());
    let first = tag::render(
        template,
        &TagInputs {
            branch,
            sha: &run.spec.revision,
            unix_time,
            build_run: run.metadata.name.as_deref().unwrap_or_default(),
        },
    )?;
    let mut tags = vec![first];
    tags.extend(output.iter().flat_map(|o| o.extra_tags.iter().cloned()));
    Ok(tags)
}

/// `Build.status` fields that follow from its runs.
pub(crate) fn summarise(
    status: &mut BuildStatus,
    runs: &[BuildRun],
    image: &str,
    now: Timestamp,
    generation: Option<i64>,
) {
    if let Some(run) = latest_success(runs) {
        let run_status = run.status.clone().unwrap_or_default();
        status.latest_revision = Some(run.spec.revision.clone());
        if let (Some(tag), Some(digest)) = (run_status.tags.first(), run_status.image_digest) {
            status.latest_image = Some(format!("{image}:{tag}@{digest}"));
        }
    }
    status.last_build_run_ref = runs
        .iter()
        .max_by_key(|r| r.metadata.creation_timestamp.as_ref().map(|t| t.0))
        .and_then(|r| r.metadata.name.clone());
    let (ready, reason, message) = latest_finished(runs).map_or_else(
        || {
            (
                ConditionStatus::Unknown,
                "NotBuilt".to_owned(),
                String::new(),
            )
        },
        |run| {
            let s = run.status.clone().unwrap_or_default();
            let name = run.metadata.name.clone().unwrap_or_default();
            if s.phase == Some(Phase::Succeeded) {
                (
                    ConditionStatus::True,
                    "Succeeded".to_owned(),
                    format!("{name} built {}", run.spec.revision),
                )
            } else {
                (
                    ConditionStatus::False,
                    s.reason
                        .map_or_else(|| "Failed".to_owned(), |r| format!("{r:?}")),
                    format!("{name}: {}", s.message.unwrap_or_default()),
                )
            }
        },
    );
    set_condition(
        &mut status.conditions,
        "Ready",
        ready,
        &reason,
        &message,
        now,
        generation,
    );
}

/// The run whose image the `Build`'s notify targets are owed: the newest success, which
/// `status.latestImage` names, until it is marked sent. Older successes are superseded and never
/// sent.
pub(crate) fn notify_due<'a>(build: &Build, runs: &'a [BuildRun]) -> Option<&'a BuildRun> {
    if build.spec.notify.is_empty() {
        return None;
    }
    latest_success(runs).filter(|r| r.status.as_ref().is_some_and(|s| s.notified_at.is_none()))
}

/// The body of Keel's native webhook (plan §9): the repository without a tag, and the run's first
/// tag, the one rendered from the tag template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct KeelPayload<'a> {
    /// The repository.
    pub(crate) name: &'a str,
    /// The tag.
    pub(crate) tag: &'a str,
}

/// The Keel payload for a run, or `None` when it recorded no tag.
pub(crate) fn keel_payload<'a>(image: &'a str, run: &'a BuildRun) -> Option<KeelPayload<'a>> {
    let tag = run.status.as_ref()?.tags.first()?;
    Some(KeelPayload { name: image, tag })
}

/// The waits between notify attempts: three attempts with exponential backoff (plan §9).
pub(crate) const NOTIFY_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];

/// What the Job and its Pod say about a run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Observed {
    /// Still running, in this phase.
    Running(Phase),
    /// Finished.
    Finished(Box<BuildRunStatus>),
}

fn job_condition(job: &Job, kind: &str) -> Option<String> {
    job.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == kind && c.status == "True")
        .map(|c| c.reason.clone().unwrap_or_default())
}

fn terminated_with_error(statuses: Option<&Vec<ContainerStatus>>, name: &str) -> Option<String> {
    let terminated = statuses?
        .iter()
        .find(|s| s.name == name)?
        .state
        .as_ref()?
        .terminated
        .as_ref()?;
    (terminated.exit_code != 0).then(|| {
        terminated
            .message
            .clone()
            .or_else(|| terminated.reason.clone())
            .unwrap_or_else(|| format!("exit code {}", terminated.exit_code))
    })
}

fn termination_message(pod: &Pod) -> Option<String> {
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find(|s| s.name == "build")?
        .state
        .as_ref()?
        .terminated
        .as_ref()?
        .message
        .clone()
}

fn failed(reason: Reason, message: String) -> Observed {
    Observed::Finished(Box::new(BuildRunStatus {
        phase: Some(Phase::Failed),
        reason: Some(reason),
        message: Some(message),
        ..BuildRunStatus::default()
    }))
}

fn from_result(result: BuildResult) -> Observed {
    let mut status = BuildRunStatus {
        message: (!result.message.is_empty()).then_some(result.message),
        tags: result.tags,
        image_digest: result.digest,
        plan_digest: result.plan_digest,
        plan_source: result.plan_source.map(|p| match p {
            result::PlanSource::Pinned => PlanSource::Pinned,
            result::PlanSource::Generated => PlanSource::Generated,
            result::PlanSource::None => PlanSource::None,
        }),
        base_images: result.base_images,
        phase_durations: result
            .phases
            .iter()
            .map(|(phase, secs)| (phase.clone(), format!("{secs:.1}s")))
            .collect(),
        cache_stats: result.cache.map(|c| CacheStats {
            total: c.total.into(),
            cached: c.cached.into(),
        }),
        ..BuildRunStatus::default()
    };
    match result.result {
        Outcome::Succeeded => status.phase = Some(Phase::Succeeded),
        Outcome::Failed => {
            status.phase = Some(Phase::Failed);
            status.reason = Some(match result.reason {
                Some(FailureReason::SourceFailed) => Reason::SourceFailed,
                Some(FailureReason::PlanFailed) => Reason::PlanFailed,
                Some(FailureReason::PushFailed) => Reason::PushFailed,
                Some(FailureReason::BuildFailed) | None => Reason::BuildFailed,
            });
        }
    }
    Observed::Finished(Box::new(status))
}

/// Reads a run's progress or result from its Job and Pod (plan §4.2, steps 2 and 3).
pub(crate) fn observe(job: &Job, pod: Option<&Pod>) -> Observed {
    let pod_status = pod.and_then(|p| p.status.as_ref());
    let message = pod.and_then(termination_message);
    if job_condition(job, "Complete").is_some() {
        return match message.as_deref().map(BuildResult::decode) {
            Some(Ok(result)) => from_result(result),
            Some(Err(e)) => failed(Reason::InfraError, e.to_string()),
            None => failed(
                Reason::InfraError,
                "the Job completed but its Pod and result are gone".to_owned(),
            ),
        };
    }
    if let Some(reason) = job_condition(job, "Failed") {
        if reason == "DeadlineExceeded" {
            return failed(
                Reason::Timeout,
                "the build ran past its deadline".to_owned(),
            );
        }
        let init = pod_status.and_then(|s| s.init_container_statuses.as_ref());
        if let Some(e) = terminated_with_error(init, "fetch") {
            return failed(Reason::SourceFailed, e);
        }
        if let Some(e) = terminated_with_error(init, "plan") {
            return failed(Reason::PlanFailed, e);
        }
        return match message.as_deref().map(BuildResult::decode) {
            Some(Ok(result)) => from_result(result),
            Some(Err(e)) => failed(Reason::InfraError, e.to_string()),
            None => failed(Reason::InfraError, format!("the Job failed: {reason}")),
        };
    }
    let building = pod_status
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|c| c.iter().find(|s| s.name == "build"))
        .and_then(|s| s.state.as_ref())
        .is_some_and(|s| s.running.is_some() || s.terminated.is_some());
    let planning = pod_status
        .and_then(|s| s.init_container_statuses.as_ref())
        .is_some_and(|c| {
            c.iter().any(|s| {
                s.state
                    .as_ref()
                    .is_some_and(|st| st.running.is_some() || st.terminated.is_some())
            })
        });
    Observed::Running(if building {
        Phase::Building
    } else if planning {
        Phase::Planning
    } else {
        Phase::Pending
    })
}

#[cfg(test)]
mod tests;
