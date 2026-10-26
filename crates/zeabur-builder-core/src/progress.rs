//! Cache statistics from `buildctl build --progress rawjson`.
//!
//! In `rawjson` mode buildctl writes each `client.SolveStatus` as one JSON object per line on
//! stderr ([progressui], `RawJSONMode`). A vertex is reported many times as it starts and
//! completes; it counts once, by digest, and counts as cached when its completed report says
//! `cached: true` (`client.Vertex`, [graph.go]). Lines that are not JSON (buildctl's own messages)
//! are ignored, so the counter can read the whole stream.
//!
//! [progressui]: https://github.com/moby/buildkit/blob/v0.34.1/util/progress/progressui/display.go
//! [graph.go]: https://github.com/moby/buildkit/blob/v0.34.1/client/graph.go

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::result::CacheStats;

#[derive(Debug, Deserialize)]
struct SolveStatus {
    #[serde(default)]
    vertexes: Vec<Vertex>,
}

#[derive(Debug, Deserialize)]
struct Vertex {
    digest: String,
    #[serde(default)]
    completed: Option<String>,
    #[serde(default)]
    cached: bool,
}

/// Counts distinct and cached build steps.
#[derive(Debug, Clone, Default)]
pub struct CacheCounter {
    /// Digest → cached, for vertices seen completed.
    completed: BTreeMap<String, bool>,
}

impl CacheCounter {
    /// Reads one line of the progress stream.
    pub fn line(&mut self, line: &str) {
        let Ok(status) = serde_json::from_str::<SolveStatus>(line) else {
            return;
        };
        for vertex in status.vertexes {
            if vertex.completed.is_some() || vertex.cached {
                let cached = self.completed.entry(vertex.digest).or_insert(false);
                *cached |= vertex.cached;
            }
        }
    }

    /// The totals so far.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let total = u32::try_from(self.completed.len()).unwrap_or(u32::MAX);
        let cached =
            u32::try_from(self.completed.values().filter(|c| **c).count()).unwrap_or(u32::MAX);
        CacheStats { total, cached }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_each_vertex_once() {
        let stream = r#"
#1 [internal] load build definition
{"vertexes":[{"digest":"sha256:a","name":"[1/3] FROM node","started":"2026-10-22T00:00:00Z"}]}
{"vertexes":[{"digest":"sha256:a","name":"[1/3] FROM node","started":"2026-10-22T00:00:00Z","completed":"2026-10-22T00:00:01Z","cached":true}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/3] RUN npm ci","completed":"2026-10-22T00:00:09Z"}],"logs":[{"vertex":"sha256:b","data":"YWRkZWQ=","timestamp":"2026-10-22T00:00:02Z"}]}
{"vertexes":[{"digest":"sha256:c","name":"[3/3] COPY . .","cached":true}]}
{"statuses":[{"id":"x","current":1,"timestamp":"2026-10-22T00:00:02Z"}]}
not json
"#;
        let mut counter = CacheCounter::default();
        for line in stream.lines() {
            counter.line(line);
        }
        assert_eq!(
            counter.stats(),
            CacheStats {
                total: 3,
                cached: 2
            }
        );
    }
}
