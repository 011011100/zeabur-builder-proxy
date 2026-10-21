//! `zeabur-builder-job`: the build container's entry point.
//!
//! ```text
//! zeabur-builder-job [--secrets-hash-env NAME]… -- buildctl … build …
//! ```
//!
//! Runs the `buildctl` command it is given (built by `zeabur-builder-jobspec`), prints its
//! `rawjson` progress for people, counts cached steps, reads the digest from `--metadata-file`,
//! and writes the result to `/dev/termination-log`, where the controller reads it from the Pod's
//! status ([`zeabur_builder_core::result`]). The same JSON is the last line on stdout.
//!
//! `--secrets-hash-env` names variables whose values are hashed into
//! `--opt build-arg:secrets-hash=…`, so Railpack's frontend invalidates the steps that use a
//! secret when its value changes, without the value entering the cache key.

mod progress;

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use sha2::{Digest as _, Sha256};
use zeabur_builder_core::progress::CacheCounter;
use zeabur_builder_core::result::{BuildResult, FailureReason, Outcome, PlanSource};

const TERMINATION_LOG: &str = "/dev/termination-log";
const OUT: &str = "/workspace/out";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    secrets_hash_env: Vec<String>,
    command: Vec<String>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut secrets_hash_env = Vec::new();
    let mut rest = args.iter();
    loop {
        match rest.next().map(String::as_str) {
            Some("--secrets-hash-env") => {
                let name = rest.next().ok_or("--secrets-hash-env needs a name")?;
                secrets_hash_env.push(name.clone());
            }
            Some("--") => break,
            Some(other) => return Err(format!("unexpected argument `{other}`")),
            None => return Err("missing `-- buildctl …`".to_owned()),
        }
    }
    let command: Vec<String> = rest.cloned().collect();
    if command.is_empty() {
        return Err("missing the command after `--`".to_owned());
    }
    Ok(Args {
        secrets_hash_env,
        command,
    })
}

/// SHA-256 over `name NUL value NUL` of each variable, in the order given.
fn secrets_hash(vars: &[(String, String)]) -> String {
    let mut hasher = Sha256::new();
    for (name, value) in vars {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            for nibble in [b >> 4, b & 0xF] {
                hex.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
            }
            hex
        })
}

/// The value after `flag` in a command line.
fn flag_value<'a>(command: &'a [String], flag: &str) -> Option<&'a str> {
    command
        .iter()
        .position(|arg| arg == flag)
        .and_then(|i| command.get(i + 1))
        .map(String::as_str)
}

/// A failed push is the registry's fault, not the build's: report it as such.
fn classify(error: &str) -> FailureReason {
    let error = error.to_ascii_lowercase();
    if error.contains("failed to push") || error.contains("pushing") {
        FailureReason::PushFailed
    } else {
        FailureReason::BuildFailed
    }
}

/// The seconds `fetch` and `plan` recorded in `/workspace/out/<phase>.json`.
fn phase_seconds(phases: &mut BTreeMap<String, f64>) {
    for phase in ["fetch", "plan"] {
        let Ok(text) = std::fs::read_to_string(format!("{OUT}/{phase}.json")) else {
            continue;
        };
        if let Ok(map) = serde_json::from_str::<BTreeMap<String, f64>>(&text) {
            phases.extend(map);
        }
    }
}

fn digest(metadata_file: Option<&str>) -> Option<String> {
    let text = std::fs::read_to_string(metadata_file?).ok()?;
    let metadata: serde_json::Value = serde_json::from_str(&text).ok()?;
    metadata
        .get("containerimage.digest")?
        .as_str()
        .map(str::to_owned)
}

/// Splits a CSV record as `buildctl` reads `--output` (`encoding/csv`).
fn csv_fields(record: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = record.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            c => field.push(c),
        }
    }
    fields.push(field);
    fields
}

/// The tags in `--output type=image,"name=<image>:<tag>,…"`.
fn tags(command: &[String]) -> Vec<String> {
    let Some(output) = flag_value(command, "--output") else {
        return Vec::new();
    };
    csv_fields(output)
        .iter()
        .filter_map(|field| field.strip_prefix("name="))
        .flat_map(|names| names.split(','))
        .filter_map(|name| name.rsplit_once(':').map(|(_, tag)| tag.to_owned()))
        .filter(|tag| !tag.contains('/'))
        .collect()
}

fn run(args: &Args) -> BuildResult {
    let mut command = args.command.clone();
    if !args.secrets_hash_env.is_empty() {
        let vars: Vec<(String, String)> = args
            .secrets_hash_env
            .iter()
            .map(|name| (name.clone(), std::env::var(name).unwrap_or_default()))
            .collect();
        command.push("--opt".to_owned());
        command.push(format!("build-arg:secrets-hash={}", secrets_hash(&vars)));
    }
    let mut result = BuildResult {
        plan_source: Some(PlanSource::None),
        ..BuildResult::default()
    };
    phase_seconds(&mut result.phases);
    if result.phases.contains_key("plan") {
        result.plan_source = Some(PlanSource::Generated);
    }

    let Some((program, rest)) = command.split_first() else {
        result.reason = Some(FailureReason::BuildFailed);
        result.message = String::from("no command");
        return result;
    };
    let start = Instant::now();
    let child = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            result.reason = Some(FailureReason::BuildFailed);
            result.message = format!("cannot run {program}: {e}");
            return result;
        }
    };
    let mut renderer = progress::Renderer::default();
    let mut counter = CacheCounter::default();
    if let Some(stderr) = child.stderr.take() {
        let mut out = std::io::stdout().lock();
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            counter.line(&line);
            for text in renderer.line(&line) {
                if writeln!(out, "{text}").is_err() {
                    // stdout gone: keep reading so buildctl is not blocked.
                }
            }
        }
    }
    let status = child.wait();
    result
        .phases
        .insert("build".to_owned(), start.elapsed().as_secs_f64());
    result.cache = Some(counter.stats());

    let succeeded = status.as_ref().is_ok_and(std::process::ExitStatus::success);
    let digest = digest(flag_value(&command, "--metadata-file"));
    match (succeeded, digest) {
        (true, Some(digest)) => {
            result.result = Outcome::Succeeded;
            result.digest = Some(digest);
            result.tags = tags(&command);
        }
        (true, None) => {
            result.reason = Some(FailureReason::PushFailed);
            result.message = String::from("buildctl succeeded but reported no image digest");
        }
        (false, _) => {
            let message = renderer
                .last_error
                .unwrap_or_else(|| format!("buildctl failed: {status:?}"));
            result.reason = Some(classify(&message));
            result.message = message;
        }
    }
    result
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let result = match parse_args(&raw) {
        Ok(args) => run(&args),
        Err(message) => BuildResult {
            reason: Some(FailureReason::BuildFailed),
            message: format!("zeabur-builder-job: {message}"),
            ..BuildResult::default()
        },
    };
    let encoded = result.encode();
    let written = std::fs::write(TERMINATION_LOG, &encoded);
    let mut out = std::io::stdout().lock();
    let printed = writeln!(out, "{encoded}");
    if written.is_err() || printed.is_err() || result.result != Outcome::Succeeded {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(
            parse_args(&strings(&[
                "--secrets-hash-env",
                "ZB_SECRET_A",
                "--",
                "buildctl",
                "build"
            ])),
            Ok(Args {
                secrets_hash_env: strings(&["ZB_SECRET_A"]),
                command: strings(&["buildctl", "build"]),
            })
        );
        assert!(parse_args(&strings(&["buildctl"])).is_err());
        assert!(parse_args(&strings(&["--"])).is_err());
        assert!(parse_args(&strings(&["--secrets-hash-env"])).is_err());
    }

    #[test]
    fn secrets_hash_changes_with_any_value() {
        let a = secrets_hash(&[("A".to_owned(), "1".to_owned())]);
        let b = secrets_hash(&[("A".to_owned(), "2".to_owned())]);
        let c = secrets_hash(&[("A1".to_owned(), String::new())]);
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn tags_from_the_output() {
        let quoted = strings(&[
            "--output",
            "type=image,\"name=r.local:5000/a/api:main-1-abc,r.local:5000/a/api:latest\",push=true",
        ]);
        assert_eq!(tags(&quoted), ["main-1-abc", "latest"]);
        let single = strings(&[
            "--output",
            "type=image,name=r.local:5000/a/api:v1,push=true",
        ]);
        assert_eq!(tags(&single), ["v1"]);
        assert_eq!(tags(&strings(&["build"])), Vec::<String>::new());
    }

    #[test]
    fn push_failures_are_told_apart() {
        assert_eq!(
            classify("failed to solve: failed to push registry.local/a/api:v1: unexpected status"),
            FailureReason::PushFailed
        );
        assert_eq!(
            classify("[2/2] RUN make: exit code: 2"),
            FailureReason::BuildFailed
        );
    }
}
