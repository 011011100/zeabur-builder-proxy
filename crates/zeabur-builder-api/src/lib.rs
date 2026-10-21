//! Custom resources in the `builder.zeabur.com` API group.
//!
//! - [`cluster_builder::ClusterBuilder`]: how to plan and which `BuildKit` frontend to build with.
//!   Cluster-scoped and written by admins; the chart ships `dockerfile`, `railpack` and `zbplan`.
//! - [`build::Build`]: what to build, written by a team. The desired state: follow a Git ref and
//!   keep an image of its newest commit.
//! - [`build_run::BuildRun`]: one build of one commit. Its spec is immutable; the controller
//!   creates them from a `Build`, and people may create them by hand.
//!
//! Format rules (lengths, patterns, list sizes) are `OpenAPI` `maxLength`, `pattern` and
//! `maxItems`, which cost nothing against the CEL cost budget; CEL rules
//! (`x-kubernetes-validations`) are used only for immutability and cross-field checks. Every
//! string and list is bounded, because the API server rejects a definition whose estimated rule
//! cost exceeds the budget ([CRD validation]). Rules that compare a whole object with `oldSelf`
//! also catch a field being removed, which a rule on the field itself would not: a transition
//! rule does not run when the field is absent from the new object.
//!
//! The namespace rules (a `Build` pushes only below its own namespace's path in the default
//! registry) are a `ValidatingAdmissionPolicy` in the chart, since they depend on configuration,
//! not on the object alone.
//!
//! [CRD validation]: https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definitions/#validation-rules

pub mod build;
pub mod build_run;
pub mod cluster_builder;
pub mod common;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt as _;

/// The API group.
pub const GROUP: &str = "builder.zeabur.com";

/// Labels on every Job and Pod of a build, for log collectors and selectors.
pub mod labels {
    /// The `Build`.
    pub const BUILD: &str = "builder.zeabur.com/build";
    /// The `BuildRun`.
    pub const BUILD_RUN: &str = "builder.zeabur.com/buildrun";
    /// The `ClusterBuilder`.
    pub const BUILDER: &str = "builder.zeabur.com/builder";
    /// Marks a Secret the controller may read as Git credentials. Its informer watches only
    /// Secrets with this label set to `"true"`.
    pub const GIT_CREDENTIALS: &str = "builder.zeabur.com/git-credentials";
    /// `app.kubernetes.io/managed-by`.
    pub const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
    /// The value of [`MANAGED_BY`].
    pub const MANAGER: &str = "zeabur-builder";
}

/// Annotations people write to act on a `Build`, and that the controller writes for them.
pub mod annotations {
    /// On a `Build`: a new value rebuilds the ref's current commit.
    pub const REBUILD: &str = "builder.zeabur.com/rebuild";
    /// On a `Build`: a new value drops the pinned plan and plans again.
    pub const REPLAN: &str = "builder.zeabur.com/replan";
    /// On a `Build`: the digest of the plan to approve.
    pub const APPROVE_PLAN: &str = "builder.zeabur.com/approve-plan";
    /// On a Namespace: overrides the controller's limit of concurrent builds.
    pub const MAX_CONCURRENT_BUILDS: &str = "builder.zeabur.com/max-concurrent-builds";
    /// On a Job and Pod: the commit built.
    pub const REVISION: &str = "builder.zeabur.com/revision";
}

/// Every custom resource definition, in a stable order.
#[must_use]
pub fn definitions() -> Vec<CustomResourceDefinition> {
    vec![
        cluster_builder::ClusterBuilder::crd(),
        build::Build::crd(),
        build_run::BuildRun::crd(),
    ]
}

/// Renders all definitions as a Kubernetes `List`, ready for `kubectl apply -f -`, without a
/// trailing newline.
///
/// # Errors
///
/// Returns an error if serialization fails.
pub fn definitions_json() -> serde_json::Result<String> {
    let list = serde_json::json!({
        "apiVersion": "v1",
        "kind": "List",
        "items": definitions(),
    });
    serde_json::to_string_pretty(&list)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed manifest must match the code. Regenerate with
    /// `cargo run -q -- crds > deploy/crds.json`.
    #[test]
    fn committed_manifest_is_current() {
        let committed = include_str!("../../../deploy/crds.json");
        let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert!(
            committed.ends_with('\n') && parse(committed) == parse(&definitions_json().unwrap()),
            "deploy/crds.json is stale: run `cargo run -q -- crds > deploy/crds.json`"
        );
    }

    #[test]
    fn kinds_and_scopes() {
        let scopes: Vec<(String, String)> = definitions()
            .into_iter()
            .map(|crd| (crd.spec.names.kind, crd.spec.scope))
            .collect();
        assert_eq!(
            scopes,
            [
                ("ClusterBuilder", "Cluster"),
                ("Build", "Namespaced"),
                ("BuildRun", "Namespaced")
            ]
            .map(|(k, s)| (k.to_owned(), s.to_owned()))
        );
        for crd in definitions() {
            assert_eq!(crd.spec.group, GROUP);
            assert_eq!(crd.spec.versions.len(), 1);
            assert_eq!(crd.spec.versions[0].name, "v1alpha1");
        }
    }

    /// Every string, list and map in every schema has an upper bound, so CEL cost estimates stay
    /// finite.
    #[test]
    fn every_value_is_bounded() {
        fn walk(path: &str, schema: &serde_json::Value, unbounded: &mut Vec<String>) {
            let Some(object) = schema.as_object() else {
                return;
            };
            let ty = object.get("type").and_then(serde_json::Value::as_str);
            let bounded = match ty {
                Some("string") => {
                    object.contains_key("maxLength")
                        || object.contains_key("enum")
                        || object.contains_key("format")
                }
                Some("array") => object.contains_key("maxItems"),
                Some("object") if object.contains_key("additionalProperties") => {
                    object.contains_key("maxProperties")
                }
                _ => true,
            };
            if !bounded {
                unbounded.push(path.to_owned());
            }
            for (key, child) in object {
                match key.as_str() {
                    "properties" => {
                        for (name, property) in child.as_object().into_iter().flatten() {
                            walk(&format!("{path}.{name}"), property, unbounded);
                        }
                    }
                    "items" | "additionalProperties" => {
                        walk(&format!("{path}[]"), child, unbounded);
                    }
                    _ => {}
                }
            }
        }
        let mut unbounded = Vec::new();
        for crd in definitions() {
            let version = &crd.spec.versions[0];
            let schema =
                serde_json::to_value(&version.schema.as_ref().unwrap().open_api_v3_schema).unwrap();
            for part in ["spec", "status"] {
                walk(
                    &format!("{}.{part}", crd.spec.names.kind),
                    &schema["properties"][part],
                    &mut unbounded,
                );
            }
        }
        assert!(unbounded.is_empty(), "unbounded: {unbounded:#?}");
    }
}
