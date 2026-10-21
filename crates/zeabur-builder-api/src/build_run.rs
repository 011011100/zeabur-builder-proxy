//! `BuildRun`: one build of one commit.

#![expect(
    unreachable_pub,
    reason = "the KubeSchema derive re-emits these types inside a private module"
)]

use std::collections::BTreeMap;

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{Condition, DIGEST_PATTERN, MAX_NAME, REVISION_PATTERN};

/// One build of one commit. The spec is immutable: to build again, create another.
#[expect(
    clippy::duplicated_attributes,
    reason = "repeated printcolumn arguments such as `type_ = \"string\"` are distinct columns"
)]
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kube(
    group = "builder.zeabur.com",
    version = "v1alpha1",
    kind = "BuildRun",
    namespaced,
    status = "BuildRunStatus",
    shortname = "zbrun",
    category = "zeabur-builder",
    selectable = ".spec.buildRef",
    printcolumn(name = "Build", type_ = "string", json_path = ".spec.buildRef"),
    printcolumn(
        name = "Revision",
        type_ = "string",
        json_path = ".spec.revision",
        priority = 1
    ),
    printcolumn(name = "Phase", type_ = "string", json_path = ".status.phase"),
    printcolumn(name = "Reason", type_ = "string", json_path = ".status.reason"),
    printcolumn(name = "Started", type_ = "date", json_path = ".status.startTime"),
    printcolumn(
        name = "Finished",
        type_ = "date",
        json_path = ".status.completionTime"
    ),
    printcolumn(
        name = "Age",
        type_ = "date",
        json_path = ".metadata.creationTimestamp"
    ),
    doc = "One build of one Git commit by Zeabur Builder."
)]
#[x_kube(validation = Rule::new("self == oldSelf").message("a BuildRun's spec is immutable; create another"))]
#[serde(rename_all = "camelCase")]
pub struct BuildRunSpec {
    /// The `Build`.
    #[schemars(length(min = 1, max = MAX_NAME))]
    pub build_ref: String,
    /// The full commit SHA.
    #[schemars(length(max = 40), regex(pattern = REVISION_PATTERN))]
    pub revision: String,
    /// Build with this plan instead of following the `Build`'s plan policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 71), regex(pattern = DIGEST_PATTERN))]
    pub plan_digest: Option<String>,
    /// Ignore the pinned plan and plan again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force_replan: bool,
    /// What started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
}

/// What started a `BuildRun`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    /// A push event.
    Webhook,
    /// The poller saw a new commit.
    Poll,
    /// A person.
    Manual,
    /// A base image changed.
    BaseImage,
    /// A pinned plan failed and the controller planned again.
    Replan,
}

/// Progress and result.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuildRunStatus {
    /// Where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// Why it waits or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// Details for people, at most 1 KiB from the Job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub message: Option<String>,
    /// The Job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_NAME))]
    pub job_ref: Option<String>,
    /// The buildkitd instance, such as `buildkitd-2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_NAME))]
    pub buildkitd_instance: Option<String>,
    /// The pushed manifest's digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 71), regex(pattern = DIGEST_PATTERN))]
    pub image_digest: Option<String>,
    /// The tags pushed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16), inner(length(max = 128)))]
    pub tags: Vec<String>,
    /// The plan used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 71), regex(pattern = DIGEST_PATTERN))]
    pub plan_digest: Option<String>,
    /// Where the plan came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_source: Option<PlanSource>,
    /// Base images, `name@sha256:…`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10), inner(length(max = 1024)))]
    pub base_images: Vec<String>,
    /// Time per phase (`fetch`, `plan`, `build`, `push`), as durations such as `82.0s`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("maxProperties" = 8, "additionalProperties" = {"type": "string", "maxLength": 32}))]
    pub phase_durations: BTreeMap<String, String>,
    /// Build steps and cache hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_stats: Option<CacheStats>,
    /// The GitHub check run.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "checkRunID"
    )]
    pub check_run_id: Option<i64>,
    /// When the Job was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    pub start_time: Option<String>,
    /// When it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    pub completion_time: Option<String>,
    /// `AwaitingApproval`, when the run only planned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 8))]
    pub conditions: Vec<Condition>,
}

/// The phases: `Pending → Planning → Building → Succeeded | Failed | Cancelled`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum Phase {
    /// Waiting for capacity, or for its Job to start.
    Pending,
    /// Fetching and planning.
    Planning,
    /// Building and pushing.
    Building,
    /// The image was pushed.
    Succeeded,
    /// It failed; see `reason`.
    Failed,
    /// A newer commit replaced it, or a person cancelled it.
    Cancelled,
}

/// Why a `BuildRun` waits or failed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum Reason {
    /// The namespace runs as many builds as it may.
    QuotaExceeded,
    /// Every buildkitd instance is unready or full.
    NoBuildkitdCapacity,
    /// Clone or checkout failed.
    SourceFailed,
    /// The planner failed.
    PlanFailed,
    /// A build step failed.
    BuildFailed,
    /// The registry refused the push.
    PushFailed,
    /// The Job ran past its deadline.
    Timeout,
    /// Replaced or cancelled.
    Cancelled,
    /// The Job or Pod disappeared, or buildkitd could not be reached.
    InfraError,
}

/// Where a plan came from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PlanSource {
    /// Reused.
    Pinned,
    /// Planned in this run.
    Generated,
    /// The builder needs no plan.
    None,
}

/// Build steps and cache hits.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema, Default)]
pub struct CacheStats {
    /// Distinct steps.
    #[schemars(range(min = 0))]
    pub total: i64,
    /// Steps answered from cache.
    #[schemars(range(min = 0))]
    pub cached: i64,
}
