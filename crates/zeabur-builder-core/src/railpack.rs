//! Railpack images that run as a non-root user.
//!
//! Railpack's frontend writes an image config without `User` ([`convert.go`]), so its images run
//! as root, and Kubernetes refuses to start them under `runAsNonRoot`. Its deploy copies the paths
//! each layer includes out of the step that made them, and BuildKit's copy keeps the owner those
//! files had there when the copy names none ([`backend_unix.go`]): root.
//!
//! Two changes make the image run as [`IMAGE_UID`]:
//!
//! - [`own_deploy_layers`] ends each step the deploy copies from with [`OWN_COMMAND`], so the
//!   copies already belong to the user and the image carries no second copy of re-owned files.
//! - [`dockerfile`] is a second build `FROM` the Railpack image. It hands over what is still root's
//!   under `/app` and `/root` (the directories the copies created, the base image's `/root`), adds
//!   the user to `/etc/passwd` and `/etc/group`, and sets `USER`. Its one layer holds directory
//!   entries and the two files.
//!
//! `HOME` stays `/root`: Railpack's layers put mise's state and the build's `~/.cache` there.
//!
//! [`convert.go`]: https://github.com/railwayapp/railpack/blob/v0.43.0/buildkit/convert.go
//! [`backend_unix.go`]: https://github.com/moby/buildkit/blob/v0.33.1/solver/llbsolver/file/backend_unix.go

use serde_json::{Value, json};

/// The user and group a Railpack image runs as.
pub const IMAGE_UID: u32 = 1000;

/// The directories the user owns: the app, and `HOME`.
const OWNED: &str = "/app /root";

/// Gives `/app` and `/root` to [`IMAGE_UID`] at the end of a step. `-xdev` leaves the step's cache
/// mounts alone; a step image without `find` keeps root's files, and the [`dockerfile`] build hands
/// them over instead.
pub const OWN_COMMAND: &str = "sh -c 'command -v find >/dev/null || exit 0; for d in /app /root; do if [ -e \"$d\" ]; then find \"$d\" -xdev -exec chown -h 1000:1000 {} +; fi; done'";

const OWN_NAME: &str = "give /app and /root to uid 1000";

/// Appends [`OWN_COMMAND`] to every step a deploy layer copies from, once; returns their names.
/// A plan without steps or deploy inputs is left as it is.
pub fn own_deploy_layers(plan: &mut Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for layer in plan
        .pointer("/deploy/inputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if layer.get("local").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(step) = layer.get("step").and_then(Value::as_str)
            && !names.iter().any(|n| n == step)
        {
            names.push(step.to_owned());
        }
    }
    let Some(steps) = plan.get_mut("steps").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let own = json!({"cmd": OWN_COMMAND, "customName": OWN_NAME});
    let mut owned = Vec::new();
    for step in steps {
        let Some(name) = step.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !names.iter().any(|n| n == name) {
            continue;
        }
        let name = name.to_owned();
        let Some(step) = step.as_object_mut() else {
            continue;
        };
        let commands = step.entry("commands").or_insert_with(|| json!([]));
        let Some(commands) = commands.as_array_mut() else {
            continue;
        };
        if commands.last() != Some(&own) {
            commands.push(own.clone());
        }
        owned.push(name);
    }
    owned
}

/// The second build: `FROM app`, the Railpack image passed as the named context `app`.
#[must_use]
pub fn dockerfile() -> String {
    let uid = IMAGE_UID;
    format!(
        r#"FROM app
RUN set -eu; \
    for dir in {OWNED}; do \
      if [ -e "$dir" ]; then find "$dir" -xdev -user 0 -exec chown -h {uid}:{uid} {{}} +; fi; \
    done; \
    grep -q '^[^:]*:[^:]*:{uid}:' /etc/passwd || echo 'app:x:{uid}:{uid}::/root:/bin/sh' >> /etc/passwd; \
    grep -q '^[^:]*:[^:]*:{uid}:' /etc/group || echo 'app:x:{uid}:' >> /etc/group
ENV HOME=/root
USER {uid}:{uid}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Value {
        json!({
            "steps": [
                {"name": "packages:mise", "inputs": [{"image": "railpack-builder"}],
                 "commands": [{"cmd": "mise install"}]},
                {"name": "install", "inputs": [{"step": "packages:mise"}],
                 "commands": [{"cmd": "npm ci"}]},
                {"name": "build", "inputs": [{"step": "install"}, {"local": true, "include": ["."]}],
                 "commands": [{"cmd": "npm run build"}]},
                {"name": "packages:apt:runtime", "inputs": [{"image": "railpack-runtime"}]}
            ],
            "deploy": {
                "base": {"step": "packages:apt:runtime"},
                "inputs": [
                    {"step": "packages:mise", "include": ["/mise/installs", "/root/.local/state/mise"]},
                    {"step": "build", "include": ["/app/node_modules"]},
                    {"step": "build", "include": [".", "/root/.cache"]},
                    {"local": true, "step": "install", "include": ["Caddyfile"]},
                    {"image": "caddy", "include": ["/usr/bin/caddy"]}
                ]
            }
        })
    }

    fn commands(plan: &Value, step: &str) -> Vec<String> {
        plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == step)
            .unwrap()
            .get("commands")
            .and_then(Value::as_array)
            .map_or_default(|c| {
                c.iter()
                    .map(|c| c["cmd"].as_str().unwrap().to_owned())
                    .collect()
            })
    }

    #[test]
    fn steps_the_deploy_copies_from_end_by_giving_the_app_to_the_user() {
        let mut plan = plan();
        assert_eq!(own_deploy_layers(&mut plan), ["packages:mise", "build"]);
        assert_eq!(
            commands(&plan, "packages:mise"),
            ["mise install", OWN_COMMAND]
        );
        assert_eq!(commands(&plan, "build"), ["npm run build", OWN_COMMAND]);
        assert_eq!(commands(&plan, "install"), ["npm ci"]);
        assert_eq!(
            commands(&plan, "packages:apt:runtime"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn owning_twice_appends_once() {
        let mut plan = plan();
        own_deploy_layers(&mut plan);
        let once = plan.clone();
        own_deploy_layers(&mut plan);
        assert_eq!(plan, once);
    }

    #[test]
    fn a_plan_without_deploy_inputs_is_left_alone() {
        for mut plan in [
            json!({}),
            json!({"steps": []}),
            json!({"deploy": {"inputs": "x"}}),
        ] {
            let before = plan.clone();
            assert_eq!(own_deploy_layers(&mut plan), Vec::<String>::new());
            assert_eq!(plan, before);
        }
        let mut no_commands = json!({
            "steps": [{"name": "build"}],
            "deploy": {"inputs": [{"step": "build", "include": ["."]}]}
        });
        assert_eq!(own_deploy_layers(&mut no_commands), ["build"]);
        assert_eq!(commands(&no_commands, "build"), [OWN_COMMAND]);
    }

    #[test]
    fn the_command_survives_railpacks_word_splitting() {
        // Railpack runs a step's command through `llb.Shlex`: one `sh -c` with one script.
        let script = OWN_COMMAND
            .strip_prefix("sh -c '")
            .and_then(|s| s.strip_suffix('\''))
            .unwrap();
        assert!(!script.contains('\''));
        assert!(script.contains(&format!("for d in {OWNED};")));
        assert!(script.contains(&format!("chown -h {IMAGE_UID}:{IMAGE_UID}")));
    }

    #[test]
    fn the_image_runs_as_a_numeric_user() {
        let dockerfile = dockerfile();
        assert!(dockerfile.starts_with("FROM app\n"));
        assert!(dockerfile.ends_with("ENV HOME=/root\nUSER 1000:1000\n"));
        assert!(dockerfile.contains("-user 0 -exec chown -h 1000:1000 {} +"));
        assert!(dockerfile.contains("echo 'app:x:1000:1000::/root:/bin/sh' >> /etc/passwd"));
    }
}
