//! Turns `buildctl --progress rawjson` into lines for people, in the shape of `--progress plain`.
//!
//! Each input line is one `client.SolveStatus` ([`zeabur_builder_core::progress`]). Steps are
//! numbered in the order they first appear; a step's logs are printed under its number, and its
//! end as `DONE`, `CACHED` or `ERROR`. Lines that are not JSON are buildctl's own messages and are
//! printed as they are.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SolveStatus {
    #[serde(default)]
    vertexes: Vec<Vertex>,
    #[serde(default)]
    logs: Vec<Log>,
}

#[derive(Debug, Deserialize)]
struct Vertex {
    digest: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    completed: Option<String>,
    #[serde(default)]
    cached: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Log {
    vertex: String,
    #[serde(default)]
    data: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct Step {
    number: usize,
    done: bool,
}

/// Renders the stream and remembers the last error, for the termination message.
#[derive(Debug, Default)]
pub(crate) struct Renderer {
    steps: BTreeMap<String, Step>,
    /// The last failed step's error, or buildctl's last `error:` line.
    pub(crate) last_error: Option<String>,
}

impl Renderer {
    /// The lines to print for one line of the stream.
    pub(crate) fn line(&mut self, line: &str) -> Vec<String> {
        let Ok(status) = serde_json::from_str::<SolveStatus>(line) else {
            if let Some(error) = line.strip_prefix("error: ") {
                self.last_error = Some(error.to_owned());
            }
            return vec![line.to_owned()];
        };
        let mut out = Vec::new();
        for vertex in status.vertexes {
            let next = self.steps.len() + 1;
            let step = self.steps.entry(vertex.digest).or_insert_with(|| {
                out.push(format!("#{next} {}", vertex.name));
                Step {
                    number: next,
                    done: false,
                }
            });
            if step.done {
                continue;
            }
            if let Some(error) = vertex.error {
                out.push(format!("#{} ERROR: {error}", step.number));
                self.last_error = Some(format!("{}: {error}", vertex.name));
                step.done = true;
            } else if vertex.cached {
                out.push(format!("#{} CACHED", step.number));
                step.done = true;
            } else if vertex.completed.is_some() {
                out.push(format!("#{} DONE", step.number));
                step.done = true;
            }
        }
        for log in status.logs {
            let number = self.steps.get(&log.vertex).map_or(0, |s| s.number);
            let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&log.data) else {
                continue;
            };
            for text in String::from_utf8_lossy(&data).lines() {
                out.push(format!("#{number} {text}"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_like_plain_progress() {
        let stream = r#"{"vertexes":[{"digest":"sha256:a","name":"[1/2] FROM alpine","started":"2026-10-22T00:00:00Z"}]}
{"vertexes":[{"digest":"sha256:a","name":"[1/2] FROM alpine","cached":true}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/2] RUN make","started":"2026-10-22T00:00:01Z"}]}
{"logs":[{"vertex":"sha256:b","stream":1,"data":"Y2MgLW8gYXBwCm9rCg==","timestamp":"2026-10-22T00:00:02Z"}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/2] RUN make","completed":"2026-10-22T00:00:03Z","error":"process \"/bin/sh -c make\" did not complete successfully: exit code: 2"}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/2] RUN make","completed":"2026-10-22T00:00:03Z"}]}
error: failed to solve: exit code: 2"#;
        let mut renderer = Renderer::default();
        let out: Vec<String> = stream.lines().flat_map(|l| renderer.line(l)).collect();
        assert_eq!(
            out,
            [
                "#1 [1/2] FROM alpine",
                "#1 CACHED",
                "#2 [2/2] RUN make",
                "#2 cc -o app",
                "#2 ok",
                "#2 ERROR: process \"/bin/sh -c make\" did not complete successfully: exit code: 2",
                "error: failed to solve: exit code: 2",
            ]
        );
        assert_eq!(
            renderer.last_error.as_deref(),
            Some("failed to solve: exit code: 2")
        );
    }
}
