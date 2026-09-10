//! Measured inventory with explicit, independent review-effort bands. No impact score.
use crate::{analyze, model::*, priority::DemandSnapshot};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub small_lines: usize,
    pub large_lines_above: usize,
    pub focused_files: usize,
    pub broad_files_above: usize,
    pub short_patches: usize,
    pub long_patches_above: usize,
    #[serde(default, skip_serializing)]
    pub near_behind: Option<usize>,
    #[serde(default, skip_serializing)]
    pub far_behind_above: Option<usize>,
    pub recent_days: i64,
    pub historical_days_above: i64,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            small_lines: 200,
            large_lines_above: 1000,
            focused_files: 5,
            broad_files_above: 20,
            short_patches: 3,
            long_patches_above: 10,
            near_behind: None,
            far_behind_above: None,
            recent_days: 30,
            historical_days_above: 180,
        }
    }
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.small_lines <= self.large_lines_above
                && self.focused_files <= self.broad_files_above
                && self.short_patches <= self.long_patches_above
                && self.recent_days >= 0
                && self.recent_days <= self.historical_days_above,
            "policy boundaries must be ordered and days nonnegative"
        );
        Ok(())
    }
}
fn band(n: usize, lo: usize, hi: usize, labels: [&str; 3]) -> String {
    labels[if n <= lo {
        0
    } else if n <= hi {
        1
    } else {
        2
    }]
    .into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Facts {
    #[serde(default)]
    pub inbox: crate::inbox::Card,
    pub feature_id: String,
    pub title: String,
    pub status: String,
    pub unique_patches: usize,
    pub commit_records: usize,
    pub additions: usize,
    pub deletions: usize,
    pub changed_lines: usize,
    pub files: usize,
    pub directories: usize,
    pub test_files: usize,
    pub documentation_files: usize,
    pub binary_files: usize,
    pub source_branches: usize,
    pub source_repositories: usize,
    pub possible_context_commits: usize,
    pub source_branches_measured: usize,
    pub source_behind_min: Option<usize>,
    pub source_behind_max: Option<usize>,
    pub source_ahead_min: Option<usize>,
    pub source_ahead_max: Option<usize>,
    pub latest_author_date: Option<String>,
    pub author_age_days: Option<i64>,
    pub change_size: String,
    pub file_spread: String,
    pub patch_series: String,
    #[serde(default)]
    pub integration: Option<crate::integration::Check>,
    pub recency: String,
    pub missing_commit_records: usize,
    pub truncated_patch_records: usize,
    pub topic_overlap_score: Option<f64>,
    pub topic_terms: Vec<TermMatch>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TermMatch {
    pub term: String,
    pub issue_documents: usize,
    pub contribution: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(default)]
    pub forks: Vec<crate::forks::Fork>,
    pub schema_version: u32,
    #[serde(default)]
    pub report_path: Option<String>,
    #[serde(default)]
    pub decision_state_dir: Option<String>,
    #[serde(default)]
    pub baseline: Option<crate::inbox::Baseline>,
    pub repository: String,
    pub base_sha: String,
    pub measured_at: String,
    pub policy: Policy,
    pub definitions: Vec<String>,
    pub warnings: Vec<String>,
    pub issue_documents: Option<usize>,
    pub issue_histogram: BTreeMap<String, usize>,
    pub candidates: Vec<Facts>,
    #[serde(default)]
    pub integration: Option<crate::integration::Summary>,
}

pub fn latest(report: &Report, f: &Feature) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    f.commits
        .iter()
        .filter_map(|sha| report.commits.get(sha))
        .filter_map(|c| chrono::DateTime::parse_from_rfc3339(&c.date).ok())
        .max()
}
pub fn ordered(report: &Report) -> Vec<&Feature> {
    let snapshot = chrono::DateTime::parse_from_rfc3339(&report.generated_at).ok();
    let mut features: Vec<_> = report
        .features
        .iter()
        .map(|f| {
            (
                f,
                latest(report, f).filter(|date| snapshot.is_some_and(|s| *date <= s)),
            )
        })
        .collect();
    features.sort_by(|(a, ad), (b, bd)| bd.cmp(ad).then(a.id.cmp(&b.id)));
    features.into_iter().map(|(f, _)| f).collect()
}

pub fn measure(report: &Report, policy: &Policy) -> Inventory {
    let branches: BTreeMap<_, _> = report
        .branches
        .iter()
        .filter(|b| b.error.is_none() && b.merge_base.is_some())
        .map(|b| (b.source.tip.as_str(), b))
        .collect();
    let snapshot = chrono::DateTime::parse_from_rfc3339(&report.generated_at).ok();
    let mut candidates = Vec::new();
    for f in ordered(report) {
        let mut patches = BTreeSet::new();
        let mut paths = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        let mut tests = BTreeSet::new();
        let mut docs = BTreeSet::new();
        let mut binaries = BTreeSet::new();
        let mut additions = 0;
        let mut deletions = 0;
        let mut missing = 0;
        let mut truncated = 0;
        for sha in &f.commits {
            let Some(c) = report.commits.get(sha) else {
                missing += 1;
                continue;
            };
            if c.patch_truncated {
                truncated += 1;
            }
            if !patches.insert(c.patch_id.as_deref().unwrap_or(&c.sha)) {
                continue;
            }
            for file in &c.files {
                additions += file.additions;
                deletions += file.deletions;
                paths.insert(&file.path);
                dirs.insert(file.path.rsplit_once('/').map_or(".", |(p, _)| p));
                if file.is_test {
                    tests.insert(&file.path);
                }
                if file.is_documentation {
                    docs.insert(&file.path);
                }
                if file.binary {
                    binaries.insert(&file.path);
                }
            }
        }
        let records: Vec<_> = f
            .sources
            .iter()
            .filter_map(|s| branches.get(s.tip.as_str()))
            .collect();
        let behind_min = records.iter().map(|b| b.behind).min();
        let behind_max = records.iter().map(|b| b.behind).max();
        let ahead_min = records.iter().map(|b| b.ahead).min();
        let ahead_max = records.iter().map(|b| b.ahead).max();
        let date = latest(report, f);
        let future = date.zip(snapshot).is_some_and(|(d, s)| d > s);
        let age = date
            .zip(snapshot)
            .filter(|(d, s)| d <= s)
            .map(|(d, s)| (s - d).num_days());
        let changed_lines = additions + deletions;
        candidates.push(Facts {
            inbox: crate::inbox::Card::default(),
            feature_id: f.id.clone(),
            title: f.title.clone(),
            status: f.status.clone(),
            unique_patches: patches.len(),
            commit_records: f.commits.len(),
            additions,
            deletions,
            changed_lines,
            files: paths.len(),
            directories: dirs.len(),
            test_files: tests.len(),
            documentation_files: docs.len(),
            binary_files: binaries.len(),
            source_branches: f.sources.len(),
            source_repositories: f
                .sources
                .iter()
                .map(|s| s.repository.to_lowercase())
                .collect::<BTreeSet<_>>()
                .len(),
            possible_context_commits: f.context_commits.iter().collect::<BTreeSet<_>>().len(),
            source_branches_measured: records.len(),
            source_behind_min: behind_min,
            source_behind_max: behind_max,
            source_ahead_min: ahead_min,
            source_ahead_max: ahead_max,
            latest_author_date: date.map(|d| d.to_rfc3339()),
            author_age_days: age,
            change_size: if missing > 0 || !binaries.is_empty() {
                "unknown".into()
            } else if paths.is_empty() {
                "no-file-changes".into()
            } else {
                band(
                    changed_lines,
                    policy.small_lines,
                    policy.large_lines_above,
                    ["small", "medium", "large"],
                )
            },
            file_spread: if missing > 0 {
                "unknown".into()
            } else if paths.is_empty() {
                "no-file-changes".into()
            } else {
                band(
                    paths.len(),
                    policy.focused_files,
                    policy.broad_files_above,
                    ["focused", "spread", "broad"],
                )
            },
            patch_series: if missing > 0 {
                "unknown".into()
            } else {
                band(
                    patches.len(),
                    policy.short_patches,
                    policy.long_patches_above,
                    ["short", "stacked", "long"],
                )
            },
            integration: None,
            recency: if future {
                "future-dated".into()
            } else {
                age.map(|d| {
                    if d <= policy.recent_days {
                        "recent"
                    } else if d <= policy.historical_days_above {
                        "aging"
                    } else {
                        "historical"
                    }
                    .into()
                })
                .unwrap_or("unknown".into())
            },
            missing_commit_records: missing,
            truncated_patch_records: truncated,
            topic_overlap_score: None,
            topic_terms: vec![],
        });
    }
    let mut inventory = Inventory{forks:vec![],decision_state_dir:None,report_path:None,baseline:None,integration:None,schema_version:4,repository:report.repository.clone(),base_sha:report.base_sha.clone(),measured_at:report.generated_at.clone(),policy:policy.clone(),definitions:vec!["Independent review-effort bands, never a combined quality/impact score. Cutoffs are configurable starting points, not validated universal rules.".into(),"Changed lines sum additions + deletions once per known patch identity, including docs/tests and repeated edits across commits. This is patch churn, not net branch LOC or semantic complexity. Binary changes do not contribute meaningful line counts and make the size band unknown. Candidates with no changed paths have a separate no-file-changes band; zero text lines can still include mode/rename changes when paths are present. Missing commit records make counts partial and size/spread/series bands unknown.".into(),"Behind/ahead are source-branch history counts against the scanned upstream base, not feature-specific conflicts. No near/far cutoff is applied. Counts and measurement coverage remain visible; they do not predict merge difficulty.".into(),"Recency uses Git author timestamps relative to the scan date. It does not measure activity, quality, first discovery, or current maintenance. Future timestamps are flagged, not promoted.".into(),"Test/docs file presence, source counts, and possible context commits are descriptive only. Tests were not executed; context commits are not a proven dependency graph.".into(),"Default order: latest non-future author date, then stable ID. Sorting is a browsing choice, not a recommendation to spend.".into()],warnings:report.coverage.warnings.clone(),issue_documents:None,issue_histogram:BTreeMap::new(),candidates};
    crate::inbox::prepare(report, &mut inventory);
    inventory.forks = crate::forks::summarize(report, &inventory);
    inventory.definitions.push(crate::forks::POLICY.into());
    inventory
}

pub fn add_histogram(
    inventory: &mut Inventory,
    report: &Report,
    demand: &DemandSnapshot,
    include_discussions: bool,
) {
    crate::inbox::add_context(report, inventory, demand);
    let prefix = format!("https://github.com/{}/", report.repository).to_lowercase();
    let mut seen = BTreeSet::new();
    let mut histogram = BTreeMap::<String, usize>::new();
    let mut count = 0;
    for t in &demand.threads {
        if t.is_pull_request
            || (!include_discussions && t.kind == "discussion")
            || !["open", "unanswered"].contains(&t.state.to_lowercase().as_str())
            || !t.url.to_lowercase().starts_with(&prefix)
            || !seen.insert(&t.url)
        {
            continue;
        }
        count += 1;
        for term in analyze::tokens(&format!("{} {}", t.title, t.body)) {
            *histogram.entry(term).or_default() += 1;
        }
    }
    let feature_map: BTreeMap<_, _> = report.features.iter().map(|f| (&f.id, f)).collect();
    for facts in &mut inventory.candidates {
        let f = feature_map[&facts.feature_id];
        let mut terms = analyze::tokens(&f.title);
        for sha in &f.commits {
            if let Some(c) = report.commits.get(sha) {
                terms.extend(analyze::tokens(&c.message));
            }
        }
        let mut matches: Vec<_> = terms
            .into_iter()
            .filter_map(|term| {
                histogram.get(&term).map(|&df| TermMatch {
                    term: term.clone(),
                    issue_documents: df,
                    contribution: ((count as f64 + 1.0) / (df as f64 + 1.0)).ln() + 1.0,
                })
            })
            .collect();
        matches.sort_by(|a, b| {
            b.contribution
                .total_cmp(&a.contribution)
                .then(a.term.cmp(&b.term))
        });
        facts.topic_overlap_score = Some(matches.iter().fold(0.0, |sum, m| sum + m.contribution));
        facts.topic_terms = matches;
    }
    inventory.issue_documents = Some(count);
    inventory.issue_histogram = histogram;
    inventory.definitions.push(format!("Optional vocabulary overlap: sum ln((N+1)/(document frequency+1))+1 for unique matching terms in feature title/commit messages. N={count} open {}. Each thread counts once; votes/repeated words add nothing. Verbose messages can match more terms. This is not demand, fit, or impact.",if include_discussions{"issues and discussions (including unclassified discussion intent)"}else{"issues"}));
    inventory.warnings.extend(demand.warnings.clone());
    if demand
        .query
        .as_deref()
        .is_some_and(|q| !q.trim().is_empty())
    {
        inventory.warnings.push(
            "Word histogram uses a query-filtered catalog; topic coverage is biased by that query."
                .into(),
        );
    }
}

pub fn description(f: &Facts) -> String {
    let range = |lo: Option<usize>, hi: Option<usize>| match (lo, hi) {
        (Some(a), Some(b)) if a == b => a.to_string(),
        (Some(a), Some(b)) => format!("{a}–{b}"),
        _ => "unknown".into(),
    };
    format!("{} changed lines (+{} / −{}) · {} size · {} files / {} directories ({}) · {} patches ({}) · source branches {} behind / {} ahead ({}/{} measured) · author recency {} ({}) · {} test files, not executed · {} binary files",f.changed_lines,f.additions,f.deletions,f.change_size,f.files,f.directories,f.file_spread,f.unique_patches,f.patch_series,range(f.source_behind_min,f.source_behind_max),range(f.source_ahead_min,f.source_ahead_max),f.source_branches_measured,f.source_branches,f.recency,f.latest_author_date.as_deref().unwrap_or("unknown"),f.test_files,f.binary_files)
}
