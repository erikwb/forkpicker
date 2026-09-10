//! Factual application gates, independent of model judgments.
use crate::{discovery::Entry, discovery_window, integration};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    Clean,
    All,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pool {
    pub policy: Policy,
    pub target_sha: String,
    pub before_application: usize,
    pub eligible: usize,
    pub statuses: BTreeMap<String, usize>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct SetCheck {
    pub headline: String,
    pub candidate_ids: Vec<String>,
    pub check: Option<integration::Check>,
    pub error: Option<String>,
}
pub fn clean(status: &str) -> bool {
    matches!(status, "clean" | "clean-three-way")
}
pub fn status<'a>(entry: &'a Entry, target: &str) -> &'a str {
    entry
        .integration
        .as_ref()
        .filter(|v| v["target_sha"].as_str() == Some(target))
        .and_then(|v| v["status"].as_str())
        .unwrap_or("not-checked")
}
impl Pool {
    pub fn new(entries: &[Entry], policy: Policy, target: &str) -> Self {
        let mut pool = Self {
            policy,
            target_sha: target.into(),
            before_application: 0,
            eligible: 0,
            statuses: BTreeMap::new(),
        };
        for entry in entries.iter().filter(|e| discovery_window::eligible(e)) {
            let status = status(entry, target);
            pool.before_application += 1;
            *pool.statuses.entry(status.into()).or_default() += 1;
            pool.eligible += usize::from(policy == Policy::All || clean(status));
        }
        pool
    }
    pub fn includes(&self, entry: &Entry) -> bool {
        discovery_window::eligible(entry)
            && (self.policy == Policy::All || clean(status(entry, &self.target_sha)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn clean_pool_requires_a_matching_successful_git_check() {
        let entries: Vec<_> = [
            "clean",
            "clean-three-way",
            "conflicts",
            "not-applicable",
            "unknown",
        ]
        .into_iter()
        .map(|s| Entry {
            candidate_id: s.into(),
            aliases: vec![],
            forks: vec![],
            metadata: json!({}),
            activity: None,
            integration: Some(json!({"target_sha":"head","status":s})),
        })
        .collect();
        let pool = Pool::new(&entries, Policy::Clean, "head");
        assert_eq!(pool.before_application, 5);
        assert_eq!(pool.eligible, 2);
        assert_eq!(entries.iter().filter(|e| pool.includes(e)).count(), 2);
        assert_eq!(
            Pool::new(&entries, Policy::Clean, "another-head").eligible,
            0
        );
        assert_eq!(Pool::new(&entries, Policy::All, "head").eligible, 5);
    }
}
