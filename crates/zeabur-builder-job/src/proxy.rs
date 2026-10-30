//! Build-only proxy configuration, read from the job's validated infrastructure environment.
//!
//! Dockerfile frontends receive BuildKit's predefined proxy arguments. They are excluded from
//! image history and cache by default; a Dockerfile that explicitly references one can persist
//! it ([Docker's proxy arguments](https://docs.docker.com/build/building/variables/#proxy-arguments)).
//! Railpack v0.43.0 ignores those arguments: its frontend reads only `cache-key`, `secrets-hash`
//! and `github-token`. Its [build graph](https://github.com/railwayapp/railpack/blob/v0.43.0/buildkit/build_llb/build_graph.go#L263-L270)
//! mounts every name in `BuildPlan.Secrets` as an environment secret on each command. We therefore add only the proxy
//! names to the plan and send their values through BuildKit's secret session, without changing
//! the plan's deploy variables or the resulting image's environment.

use serde_json::Value;

const NAMES: [&str; 6] = [
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
];

/// Only standard proxy variables, not arbitrary variables or build secrets, reach `RUN` steps.
pub(crate) fn from_env() -> Vec<(String, String)> {
    NAMES
        .into_iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.to_owned(), value)))
        .collect()
}

/// Adds names, never values, to Railpack's run-time secret environment.
pub(crate) fn railpack_secrets(
    plan: &mut Value,
    vars: &[(String, String)],
) -> Result<(), String> {
    if vars.is_empty() {
        return Ok(());
    }
    let object = plan.as_object_mut().ok_or("Railpack plan must be an object")?;
    let secrets = object
        .entry("secrets")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("Railpack plan secrets must be an array")?;
    for (name, _) in vars {
        if !secrets.iter().any(|value| value.as_str() == Some(name.as_str())) {
            secrets.push(Value::String(name.clone()));
        }
    }
    Ok(())
}

/// Adds Dockerfile proxy arguments, or a Railpack secret session, only to the intended frontend.
pub(crate) fn configure_commands(
    commands: &mut [Vec<String>],
    vars: &[(String, String)],
    railpack: bool,
) {
    if vars.is_empty() {
        return;
    }
    for command in commands.iter_mut() {
        if super::flag_value(command, "--frontend") == Some("dockerfile.v0") {
            for (name, value) in vars {
                command.extend(["--opt".to_owned(), format!("build-arg:{name}={value}")]);
            }
        }
    }
    if railpack
        && let Some(first) = commands.first_mut()
    {
        for (name, _) in vars {
            let prefix = format!("id={name},");
            let id = format!("id={name}");
            let source = format!("id={name},env={name}");
            // Infrastructure proxy configuration wins over a Build variable with the same name;
            // buildctl must not receive duplicate secret IDs with different environment sources.
            let existing = first
                .windows(2)
                .position(|pair| {
                    matches!(pair, [flag, value] if flag == "--secret" && (value == &id || value.starts_with(&prefix)))
                });
            if let Some(existing) = existing.and_then(|index| first.get_mut(index + 1))
            {
                *existing = source;
            } else {
                first.extend(["--secret".to_owned(), source]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vec<(String, String)> {
        vec![
            ("HTTPS_PROXY".to_owned(), "http://100.66.31.168:3128".to_owned()),
            ("NO_PROXY".to_owned(), ".svc,.svc.cluster.local,10.43.0.0/16".to_owned()),
        ]
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn railpack_plan_keeps_proxy_values_out_of_deploy_environment() -> Result<(), String> {
        let mut plan = serde_json::json!({
            "secrets": ["NPM_TOKEN", "HTTPS_PROXY"],
            "steps": [{"name": "build", "variables": {"NODE_ENV": "production"}}],
            "deploy": {"variables": {"NODE_ENV": "production"}}
        });
        let original_deploy = plan.get("deploy").cloned();
        let original_steps = plan.get("steps").cloned();
        railpack_secrets(&mut plan, &vars())?;
        railpack_secrets(&mut plan, &vars())?;
        assert_eq!(plan.get("secrets"), Some(&serde_json::json!(["NPM_TOKEN", "HTTPS_PROXY", "NO_PROXY"])));
        assert_eq!(plan.get("deploy"), original_deploy.as_ref());
        assert_eq!(plan.get("steps"), original_steps.as_ref());
        assert!(!plan.to_string().contains("100.66.31.168"));
        assert!(railpack_secrets(&mut serde_json::json!({"secrets": {}}), &vars()).is_err());
        Ok(())
    }

    #[test]
    fn dockerfile_receives_predefined_proxy_arguments_without_environment_instructions() -> Result<(), String> {
        let mut commands = vec![strings(&["buildctl", "build", "--frontend", "dockerfile.v0"])];
        configure_commands(&mut commands, &vars(), false);
        let command = commands.first().ok_or("missing build command")?;
        assert!(command.contains(&"build-arg:HTTPS_PROXY=http://100.66.31.168:3128".to_owned()));
        assert!(command.contains(&"build-arg:NO_PROXY=.svc,.svc.cluster.local,10.43.0.0/16".to_owned()));
        assert!(!command.contains(&"--secret".to_owned()));
        Ok(())
    }

    #[test]
    fn railpack_receives_only_secret_names_and_replaces_colliding_sources() -> Result<(), String> {
        let mut commands = vec![strings(&[
            "buildctl", "build", "--frontend", "gateway.v0", "--secret",
            "id=HTTPS_PROXY,env=ZB_SECRET_HTTPS_PROXY",
        ])];
        configure_commands(&mut commands, &vars(), true);
        let command = commands.first().ok_or("missing build command")?;
        assert_eq!(command.iter().filter(|arg| arg.starts_with("id=HTTPS_PROXY,")).count(), 1);
        assert!(command.contains(&"id=HTTPS_PROXY,env=HTTPS_PROXY".to_owned()));
        assert!(command.contains(&"id=NO_PROXY,env=NO_PROXY".to_owned()));
        assert!(!command.iter().any(|arg| arg.contains("100.66.31.168")));
        Ok(())
    }

    #[test]
    fn disabled_proxy_preserves_commands_and_plan() -> Result<(), String> {
        let mut command = vec![strings(&["buildctl", "build", "--frontend", "dockerfile.v0"])];
        let original_command = command.clone();
        configure_commands(&mut command, &[], false);
        assert_eq!(command, original_command);
        let mut plan = serde_json::json!({"deploy": {}});
        let original_plan = plan.clone();
        railpack_secrets(&mut plan, &[])?;
        assert_eq!(plan, original_plan);
        Ok(())
    }
}
