//! `ClusterBuilder`: how to plan a build and which frontend builds it.

#![expect(
    unreachable_pub,
    reason = "the KubeSchema derive re-emits these types inside a private module"
)]

use std::collections::BTreeMap;

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::DURATION_PATTERN;

/// Longest image reference.
pub const MAX_IMAGE: usize = 512;
/// Most frontend options.
pub const MAX_OPTS: usize = 32;

/// A way to build. Every builder reduces to the same shape: an optional planner that writes files
/// into `/workspace/plan`, then a `BuildKit` frontend with options that reads them.
#[expect(
    clippy::duplicated_attributes,
    reason = "repeated printcolumn arguments such as `type_ = \"string\"` are distinct columns"
)]
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kube(
    group = "builder.zeabur.com",
    version = "v1alpha1",
    kind = "ClusterBuilder",
    shortname = "cbuilder",
    category = "zeabur-builder",
    printcolumn(name = "Type", type_ = "string", json_path = ".spec.type"),
    printcolumn(name = "Frontend", type_ = "string", json_path = ".spec.frontend.type"),
    printcolumn(
        name = "Age",
        type_ = "date",
        json_path = ".metadata.creationTimestamp"
    ),
    doc = "How Zeabur Builder plans and builds: Dockerfile or Railpack."
)]
#[x_kube(
    validation = Rule::new("self.type == 'dockerfile' ? !has(self.planner) : has(self.planner)")
        .message("planner is required for railpack, and forbidden for dockerfile")
)]
#[serde(rename_all = "camelCase")]
pub struct ClusterBuilderSpec {
    /// What the planning step does.
    #[serde(rename = "type")]
    pub kind: BuilderType,
    /// The planner; absent when the builder needs no plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<PlannerSpec>,
    /// The frontend that builds the plan.
    pub frontend: FrontendSpec,
}

/// The builders.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BuilderType {
    /// The repository's own Dockerfile.
    Dockerfile,
    /// Railpack plans; its gateway frontend builds.
    Railpack,
}

/// The planning step.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlannerSpec {
    /// The planner image, pinned by digest in production.
    #[schemars(length(min = 1, max = MAX_IMAGE))]
    pub image: String,
    /// How long planning may take. Default `10m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32), regex(pattern = DURATION_PATTERN))]
    pub timeout: Option<String>,
    /// CPU and memory for the plan container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
}

/// CPU and memory, as Kubernetes quantities.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
pub struct Resources {
    /// Requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<ResourceList>,
    /// Limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ResourceList>,
}

/// A CPU and memory amount.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
pub struct ResourceList {
    /// CPU, such as `500m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32))]
    pub cpu: Option<String>,
    /// Memory, such as `1Gi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32))]
    pub memory: Option<String>,
}

/// The `BuildKit` frontend.
#[derive(KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("self.type == 'gateway.v0' ? has(self.source) && self.source != '' : !has(self.source)")
        .message("source is required for gateway.v0 and forbidden for dockerfile.v0")
)]
pub struct FrontendSpec {
    /// `dockerfile.v0` (built into buildkitd) or `gateway.v0` (an image).
    #[serde(rename = "type")]
    pub kind: FrontendType,
    /// The frontend image for `gateway.v0`, such as
    /// `ghcr.io/railwayapp/railpack-frontend:v0.43.0`. It must match the planner's version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_IMAGE))]
    pub source: Option<String>,
    /// Frontend options added to every build, as `buildctl --opt key=value`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("maxProperties" = MAX_OPTS, "additionalProperties" = {"type": "string", "maxLength": 4096}))]
    pub opts: BTreeMap<String, String>,
}

/// Frontend kinds.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum FrontendType {
    /// The Dockerfile frontend in buildkitd.
    #[serde(rename = "dockerfile.v0")]
    Dockerfile,
    /// A frontend image.
    #[serde(rename = "gateway.v0")]
    Gateway,
}
