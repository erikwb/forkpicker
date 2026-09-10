use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub repository: String,
    pub branch: String,
    pub tip: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
    pub symbols: Vec<String>,
    pub patch_fingerprint: Option<String>,
    pub is_test: bool,
    pub is_documentation: bool,
    pub is_routine: bool,
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub message: String,
    pub author: String,
    pub date: String,
    pub files: Vec<FileChange>,
    pub patch_id: Option<String>,
    pub patch: String,
    pub patch_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamMatch {
    pub commit: String,
    pub upstream_commits: Vec<String>,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchReport {
    pub source: Source,
    pub merge_base: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub novel_commits: usize,
    pub inspected_commits: usize,
    pub merges_omitted: usize,
    pub commits_omitted: usize,
    pub equivalent_upstream: Vec<UpstreamMatch>,
    pub alias_of: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feature {
    pub id: String,
    pub title: String,
    pub category: String,
    pub commits: Vec<String>,
    pub patch_ids: Vec<String>,
    pub sources: Vec<Source>,
    pub files: Vec<String>,
    pub test_files: Vec<String>,
    pub documentation_files: Vec<String>,
    pub additions: usize,
    pub deletions: usize,
    pub score: i32,
    pub reasons: Vec<String>,
    pub grouping_evidence: Vec<String>,
    pub review_notes: Vec<String>,
    pub related_features: Vec<String>,
    pub context_commits: Vec<String>,
    pub issue_links: Vec<String>,
    pub status: String,
    pub decision_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Coverage {
    pub scope: String,
    pub forks_discovered: usize,
    pub forks_selected: usize,
    pub forks_omitted: usize,
    pub branches_discovered: usize,
    pub branches_omitted: usize,
    pub api_requests: usize,
    pub cache_hits: usize,
    pub upstream_commits_indexed: usize,
    pub upstream_commits_available: usize,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub performance: ScanPerformance,
}

/// Wall-clock stage durations; fetch counters count attempts, including failures.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScanPerformance {
    /// Elapsed through discovery, fetching, analysis and optional context retrieval;
    /// excludes final recommendation refresh, rendering and report writes.
    pub total_ms: u64,
    pub branch_enumeration_ms: u64,
    pub fork_fetch_ms: u64,
    pub analysis_ms: u64,
    pub api_jobs: usize,
    pub fetch_jobs: usize,
    pub git_fetches: usize,
    pub cached_branch_tips: usize,
    pub fetched_branch_tips: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub is_pull_request: bool,
    pub match_kind: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub discussion_category: Option<String>,
    #[serde(default)]
    pub discussion_answerable: Option<bool>,
    #[serde(default)]
    pub body_truncated: bool,
    #[serde(default)]
    pub comments: Vec<EvidenceComment>,
    #[serde(default)]
    pub comments_omitted: usize,
    #[serde(default)]
    pub thumbs_up: Option<u64>,
    #[serde(default)]
    pub upvotes: Option<u64>,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceComment {
    pub author: String,
    pub url: String,
    pub body: String,
    pub body_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub tool_version: String,
    pub generated_at: String,
    pub repository: String,
    pub base_ref: String,
    pub base_sha: String,
    pub upstream_refs: BTreeMap<String, String>,
    pub coverage: Coverage,
    pub branches: Vec<BranchReport>,
    pub features: Vec<Feature>,
    pub commits: BTreeMap<String, Commit>,
    pub issues: Vec<Issue>,
    #[serde(default)]
    pub assessments: BTreeMap<String, Assessment>,
    #[serde(default)]
    pub reviews: Vec<crate::review::ReviewRecord>,
    #[serde(default)]
    pub demand: Option<crate::priority::DemandSnapshot>,
    #[serde(default)]
    pub recommendations: BTreeMap<String, crate::priority::Recommendation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub text: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assessment {
    pub feature_id: String,
    pub base_sha: String,
    pub title: String,
    pub summary: Vec<Claim>,
    pub review_questions: Vec<String>,
    pub suggested_next_step: String,
}

pub fn is_test(path: &str) -> bool {
    let path = path.to_lowercase();
    path.split('/')
        .any(|p| matches!(p, "test" | "tests" | "__tests__" | "spec" | "specs"))
        || path.rsplit('/').next().is_some_and(|p| {
            p.starts_with("test_")
                || p.contains("_test.")
                || p.contains(".test.")
                || p.contains(".spec.")
        })
}

pub fn is_doc(path: &str) -> bool {
    let p = path.to_lowercase();
    p.ends_with(".md") || p.ends_with(".rst") || p.starts_with("docs/") || p.starts_with("doc/")
}

pub fn is_routine(path: &str) -> bool {
    let p = path.to_lowercase();
    p.starts_with(".github/")
        || p.starts_with(".circleci/")
        || p.ends_with(".lock")
        || p.ends_with("package-lock.json")
        || p.ends_with("go.sum")
}
