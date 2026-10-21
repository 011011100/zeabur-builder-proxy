//! Choosing a buildkitd instance: rendezvous (highest random weight) hashing.
//!
//! `RUN --mount=type=cache` contents never leave the buildkitd that ran the step (registry cache
//! export does not carry them), and Railpack and zbplan rely on cache mounts. So builds of one
//! repository should keep landing on one instance. Rendezvous hashing [Thaler & Ravishankar]
//! gives every (key, instance) pair a score and picks the highest: each key keeps its instance
//! while that instance exists, and adding or removing an instance moves only the keys whose top
//! score changes, about `1/n` of them. Unlike a hash ring it needs no virtual nodes or shared
//! state; every controller replica computes the same answer.
//!
//! The key is the repository plus the first directory of the context, so the services of one
//! monorepo share an instance and its dependency caches.
//!
//! When the first choice is not ready or has no free slot, the next-highest instance is used and
//! the choice is reported as a fallback, which the controller counts in
//! `buildrun_route_fallback_total`.
//!
//! [Thaler & Ravishankar]: D. G. Thaler and C. V. Ravishankar, "Using name-based mappings to increase
//! hit rates," IEEE/ACM Transactions on Networking, vol. 6, no. 1, pp. 1-14, 1998.

use sha2::{Digest as _, Sha256};

use crate::repo::RepoKey;

/// One buildkitd instance as the controller sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// Its stable name, such as `buildkitd-2`.
    pub name: String,
    /// Whether its pod is ready.
    pub ready: bool,
    /// Builds it runs now.
    pub running: u32,
    /// Builds it may run at once.
    pub slots: u32,
}

/// The routing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route<'a> {
    /// The key's own instance.
    Preferred(&'a Instance),
    /// Another instance, because the preferred one was not ready or full.
    Fallback(&'a Instance),
    /// Every instance is unready or full; the `BuildRun` waits (`NoBuildkitdCapacity`).
    NoCapacity,
}

/// The routing key of a build: `<repo>#<first directory of the context>`.
#[must_use]
pub fn route_key(repo: &RepoKey, context_dir: &str) -> String {
    let first = context_dir
        .trim_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    format!("{repo}#{first}")
}

/// The score of an instance for a key.
#[must_use]
pub fn score(key: &str, instance: &str) -> u64 {
    let digest = Sha256::new()
        .chain_update(key)
        .chain_update([0])
        .chain_update(instance)
        .finalize();
    let mut bytes = [0_u8; 8];
    for (to, from) in bytes.iter_mut().zip(digest.iter()) {
        *to = *from;
    }
    u64::from_be_bytes(bytes)
}

/// Instances in preference order for a key; ties (never seen in practice) go by name.
#[must_use]
pub fn rank<'a>(key: &str, instances: &'a [Instance]) -> Vec<&'a Instance> {
    let mut ranked: Vec<&Instance> = instances.iter().collect();
    ranked.sort_by(|a, b| {
        score(key, &b.name)
            .cmp(&score(key, &a.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    ranked
}

/// Picks the instance for a key.
#[must_use]
pub fn pick<'a>(key: &str, instances: &'a [Instance]) -> Route<'a> {
    let ranked = rank(key, instances);
    let usable = |i: &&Instance| i.ready && i.running < i.slots;
    match ranked.iter().position(usable) {
        Some(0) => ranked
            .first()
            .map_or(Route::NoCapacity, |i| Route::Preferred(i)),
        Some(n) => ranked
            .get(n)
            .map_or(Route::NoCapacity, |i| Route::Fallback(i)),
        None => Route::NoCapacity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(n: u32) -> Vec<Instance> {
        (0..n)
            .map(|i| Instance {
                name: format!("buildkitd-{i}"),
                ready: true,
                running: 0,
                slots: 4,
            })
            .collect()
    }

    fn keys() -> Vec<String> {
        (0..1000)
            .map(|i| format!("github.com/acme/repo-{i}#"))
            .collect()
    }

    #[test]
    fn monorepo_services_share_a_key() {
        let repo = RepoKey::parse("https://github.com/acme/mono").unwrap();
        assert_eq!(
            route_key(&repo, "services/api"),
            route_key(&repo, "/services/web/")
        );
        assert_ne!(
            route_key(&repo, "services/api"),
            route_key(&repo, "tools/cli")
        );
        assert_eq!(route_key(&repo, ""), "github.com/acme/mono#");
    }

    #[test]
    fn stable_and_spread() {
        let pool = pool(4);
        let mut counts = [0_u32; 4];
        for key in keys() {
            let Route::Preferred(first) = pick(&key, &pool) else {
                panic!("all ready")
            };
            assert_eq!(pick(&key, &pool), Route::Preferred(first));
            let index: usize = first.name.trim_start_matches("buildkitd-").parse().unwrap();
            counts[index] += 1;
        }
        assert!(
            counts.iter().all(|&c| (180..=320).contains(&c)),
            "{counts:?}"
        );
    }

    #[test]
    fn adding_an_instance_moves_few_keys() {
        let (three, four) = (pool(3), pool(4));
        let moved = keys()
            .iter()
            .filter(|key| {
                let before = rank(key, &three)[0].name.clone();
                let after = rank(key, &four)[0].name.clone();
                before != after && after != "buildkitd-3"
            })
            .count();
        assert_eq!(moved, 0, "keys only move to the new instance");
    }

    #[test]
    fn falls_back_when_unready_or_full() {
        let key = "github.com/acme/api#";
        let mut pool = pool(3);
        let first = rank(key, &pool)[0].name.clone();
        let index = pool.iter().position(|i| i.name == first).unwrap();
        pool[index].ready = false;
        assert!(matches!(pick(key, &pool), Route::Fallback(i) if i.name != first));
        pool[index].ready = true;
        pool[index].running = 4;
        assert!(matches!(pick(key, &pool), Route::Fallback(_)));
        for i in &mut pool {
            i.running = i.slots;
        }
        assert_eq!(pick(key, &pool), Route::NoCapacity);
        assert_eq!(pick(key, &[]), Route::NoCapacity);
    }
}
