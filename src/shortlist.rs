//! Evidence-led discovery: rank observed project requests, keep uncertainty explicit.
use crate::{
    github::{Github, RepoName},
    hash,
    model::*,
    priority, render,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pull {
    pub number: u64,
    pub title: String,
    pub draft: bool,
    pub head_sha: String,
    pub base_branch: String,
    pub commits: Vec<String>,
    pub commits_complete: bool,
    pub membership_verified: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullSnapshot {
    pub repository: String,
    pub fetched_at: String,
    pub pulls: Vec<Pull>,
    pub listing_complete: bool,
    pub warnings: Vec<String>,
    pub api_requests: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PullCoverage {
    pub open_prs: Vec<u64>,
    pub partial_prs: Vec<u64>,
}

/// Positive evidence only: incomplete listings cannot establish absence of a PR.
pub fn pull_coverage(
    report: &Report,
    snapshot: &PullSnapshot,
) -> Result<BTreeMap<String, PullCoverage>> {
    anyhow::ensure!(
        snapshot.repository.eq_ignore_ascii_case(&report.repository),
        "PR snapshot belongs to another repository"
    );
    let patches: Vec<_> = snapshot
        .pulls
        .iter()
        .filter(|p| p.membership_verified)
        .map(|p| {
            let ids: BTreeSet<_> = p
                .commits
                .iter()
                .filter_map(|sha| report.commits.get(sha))
                .map(|c| c.patch_id.as_deref().unwrap_or(&c.sha))
                .collect();
            (p, ids)
        })
        .collect();
    let mut results = BTreeMap::new();
    let all_patches: BTreeSet<_> = patches
        .iter()
        .flat_map(|(_, ids)| ids.iter().copied())
        .collect();
    let all_commits: BTreeSet<_> = patches
        .iter()
        .flat_map(|(pr, _)| pr.commits.iter().map(String::as_str))
        .collect();
    for f in &report.features {
        let ids: BTreeSet<_> = f.patch_ids.iter().map(String::as_str).collect();
        let mut coverage = PullCoverage::default();
        for (pr, patches) in &patches {
            // A scan source records membership in this exact branch head's history.
            let same_head = f.sources.iter().any(|s| s.tip == pr.head_sha);
            let same_commits =
                !f.commits.is_empty() && f.commits.iter().all(|sha| pr.commits.contains(sha));
            if same_head || same_commits || (!ids.is_empty() && ids.is_subset(patches)) {
                coverage.open_prs.push(pr.number);
            } else if !ids.is_disjoint(patches)
                || f.commits.iter().any(|sha| pr.commits.contains(sha))
            {
                coverage.partial_prs.push(pr.number);
            }
        }
        // A patch collection can also be fully represented across several PRs.
        if coverage.open_prs.is_empty()
            && !coverage.partial_prs.is_empty()
            && ((!ids.is_empty() && ids.is_subset(&all_patches))
                || (!f.commits.is_empty()
                    && f.commits
                        .iter()
                        .all(|sha| all_commits.contains(sha.as_str()))))
        {
            coverage.open_prs = std::mem::take(&mut coverage.partial_prs);
        }
        coverage.open_prs.sort_unstable();
        coverage.open_prs.dedup();
        coverage.partial_prs.sort_unstable();
        coverage.partial_prs.dedup();
        if !coverage.open_prs.is_empty() || !coverage.partial_prs.is_empty() {
            results.insert(f.id.clone(), coverage);
        }
    }
    Ok(results)
}

pub fn collect_pulls(api: &mut Github, repository: &str) -> Result<PullSnapshot> {
    let repo: RepoName = repository.parse()?;
    let started = api.requests();
    let mut snapshot = PullSnapshot {
        repository: repo.0.clone(),
        fetched_at: chrono::Utc::now().to_rfc3339(),
        pulls: vec![],
        listing_complete: false,
        warnings: vec![],
        api_requests: 0,
    };
    #[derive(Deserialize)]
    struct Head {
        sha: String,
    }
    #[derive(Deserialize)]
    struct Base {
        #[serde(rename = "ref")]
        branch: String,
        repo: crate::github::Repository,
    }
    #[derive(Deserialize)]
    struct Item {
        number: u64,
        title: String,
        draft: bool,
        state: String,
        head: Head,
        base: Base,
    }
    for page in 1.. {
        let result = api
            .get(&format!(
                "repos/{}/pulls?state=open&sort=updated&direction=desc&per_page=100&page={page}",
                repo.0
            ))
            .and_then(|(v, next)| Ok((serde_json::from_value::<Vec<Item>>(v)?, next)));
        match result {
            Ok((items, next)) => {
                for item in items {
                    if item.state != "open"
                        || !item.base.repo.full_name.eq_ignore_ascii_case(repository)
                        || !valid_sha(&item.head.sha)
                    {
                        snapshot.warnings.push(format!(
                            "Invalid or no longer open PR #{} omitted",
                            item.number
                        ));
                        continue;
                    }
                    snapshot.pulls.push(Pull {
                        number: item.number,
                        title: item.title,
                        draft: item.draft,
                        head_sha: item.head.sha,
                        base_branch: item.base.branch,
                        commits: vec![],
                        commits_complete: false,
                        membership_verified: false,
                    });
                }
                if !next {
                    snapshot.listing_complete = true;
                    break;
                }
            }
            Err(e) => {
                snapshot
                    .warnings
                    .push(format!("Open PR listing incomplete: {e}"));
                break;
            }
        }
    }
    snapshot.pulls.sort_by_key(|p| p.number);
    snapshot.pulls.dedup_by_key(|p| p.number);
    eprintln!(
        "Checking commit membership in {} open upstream PRs…",
        snapshot.pulls.len()
    );
    let results = crate::parallel::map(&snapshot.pulls, 4, |_, pull| {
        let mut client = api.clone();
        let (shas, complete, error) = (|| {
            let mut shas = Vec::new();
            for page in 1..=3 {
                match client.get(&format!(
                    "repos/{}/pulls/{}/commits?per_page=100&page={page}",
                    repo.0, pull.number
                )) {
                    Ok((v, next)) => {
                        let Some(items) = v.as_array() else {
                            return (shas, false, Some("invalid commit response".to_owned()));
                        };
                        for item in items {
                            let Some(sha) = item["sha"].as_str().filter(|s| valid_sha(s)) else {
                                return (shas, false, Some("invalid commit SHA".to_owned()));
                            };
                            shas.push(sha.to_owned());
                        }
                        // GitHub caps this endpoint at 250; at the cap completeness is unknown.
                        if !next && shas.len() < 250 {
                            return (shas, true, None);
                        }
                    }
                    Err(e) => return (shas, false, Some(e.to_string())),
                }
            }
            (
                shas,
                false,
                Some("commit membership capped at 250 by GitHub".into()),
            )
        })();
        match client
            .revalidating()
            .get(&format!("repos/{}/pulls/{}", repo.0, pull.number))
        {
            Ok((current, _))
                if current["state"] == "open"
                    && current["head"]["sha"].as_str() == Some(&pull.head_sha) =>
            {
                (shas, complete, error, true)
            }
            Ok(_) => (
                vec![],
                false,
                Some("PR state/head changed during membership lookup; ignored".into()),
                false,
            ),
            Err(e) => (
                vec![],
                false,
                Some(format!("PR head could not be revalidated: {e}")),
                false,
            ),
        }
    });
    for (pull, (shas, complete, error, verified)) in snapshot.pulls.iter_mut().zip(results) {
        pull.commits = shas;
        pull.commits_complete = complete;
        pull.membership_verified = verified;
        if let Some(error) = error {
            snapshot.warnings.push(format!(
                "PR #{} membership incomplete: {error}",
                pull.number
            ));
        }
    }
    snapshot.api_requests = api.requests() - started;
    Ok(snapshot)
}
fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub title: String,
    pub delta: String,
    pub newly_observed_patches: usize,
    pub open_prs: Vec<u64>,
    pub partial_prs: Vec<u64>,
    pub matched_requests: Vec<String>,
    pub possible_requests: Vec<String>,
    pub other_thread_links: Vec<String>,
    pub eligible: bool,
    pub reasons: Vec<String>,
    pub patches: usize,
    pub files: usize,
    pub changed_test_files: usize,
    pub additions: usize,
    pub deletions: usize,
    pub source_urls: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub url: String,
    pub title: String,
    pub positive_votes: Option<u64>,
    pub demand_class: String,
    pub discussion_category: Option<String>,
    pub priority_labels: Vec<String>,
    pub linked_candidates: Vec<String>,
    pub possible_candidates: Vec<String>,
    pub discovery_candidates: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shortlist {
    pub schema_version: u32,
    pub repository: String,
    pub base_sha: String,
    pub source_fingerprint: String,
    pub generated_at: String,
    pub scan_generated_at: String,
    pub demand_fetched_at: Option<String>,
    pub baseline_generated_at: Option<String>,
    pub prs_fetched_at: Option<String>,
    pub policy: String,
    pub only_new: bool,
    pub limit: usize,
    pub counts: BTreeMap<String, usize>,
    pub warnings: Vec<String>,
    pub requests: Vec<Request>,
    pub candidates: Vec<Candidate>,
    pub selected_feature_ids: Vec<String>,
}

pub fn fingerprint(report: &Report) -> String {
    let mut ids: Vec<_> = report
        .features
        .iter()
        .map(|f| (&f.id, &f.patch_ids, &f.commits, &f.sources))
        .collect();
    ids.sort_by_key(|x| x.0);
    let commits: Vec<_> = report
        .commits
        .iter()
        .map(|(sha, commit)| {
            (
                sha,
                hash(serde_json::to_vec(commit).expect("serializable commit")),
            )
        })
        .collect();
    hash(
        serde_json::to_vec(&(&report.repository, &report.base_sha, ids, commits))
            .expect("serializable evidence"),
    )
}

// Index immutable fork evidence once; request bodies and titles are parsed once.
struct DemandIndex<'a> {
    report: &'a Report,
    references: BTreeMap<String, BTreeSet<usize>>,
    words: BTreeMap<String, BTreeSet<usize>>,
    patches: BTreeMap<&'a str, BTreeSet<usize>>,
}
fn reference_word(word: &str) -> String {
    word.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | ',' | '.' | ':' | ';' | '`'))
        .to_lowercase()
}
impl<'a> DemandIndex<'a> {
    fn new(report: &'a Report) -> Self {
        let mut index = Self {
            report,
            references: BTreeMap::new(),
            words: BTreeMap::new(),
            patches: BTreeMap::new(),
        };
        for (i, f) in report.features.iter().enumerate() {
            let mut subjects = f.title.clone();
            for sha in &f.commits {
                if let Some(commit) = report.commits.get(sha) {
                    subjects.push(' ');
                    subjects.push_str(&commit.subject);
                    for word in commit.message.split_whitespace() {
                        let word = reference_word(word);
                        if word.contains('#') || word.starts_with("https://github.com/") {
                            index.references.entry(word).or_default().insert(i);
                        }
                    }
                }
            }
            for word in crate::analyze::tokens(&subjects) {
                index.words.entry(word).or_default().insert(i);
            }
            for patch in &f.patch_ids {
                index.patches.entry(patch).or_default().insert(i);
            }
        }
        index
    }
    fn matches(&self, thread: &Issue) -> (BTreeSet<usize>, BTreeSet<usize>) {
        let mut explicit = BTreeSet::new();
        let Ok(url) = reqwest::Url::parse(&thread.url) else {
            return (explicit, BTreeSet::new());
        };
        let parts: Vec<_> = url.path_segments().into_iter().flatten().collect();
        if url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || parts.len() != 4
            || !["issues", "discussions"].contains(&parts[2])
            || !format!("{}/{}", parts[0], parts[1]).eq_ignore_ascii_case(&self.report.repository)
        {
            return (explicit, BTreeSet::new());
        }
        let mut keys = vec![thread.url.to_lowercase()];
        if parts[2] == "issues" {
            keys.extend([
                format!("#{}", thread.number),
                format!(
                    "{}#{}",
                    self.report.repository.to_lowercase(),
                    thread.number
                ),
            ]);
        }
        for key in keys {
            if let Some(ids) = self.references.get(&key) {
                explicit.extend(ids);
            }
        }
        for word in thread.body.split_whitespace().chain(
            thread
                .comments
                .iter()
                .flat_map(|c| c.body.split_whitespace()),
        ) {
            let word = reference_word(word);
            if !word.starts_with("https://github.com/") {
                continue;
            }
            let Ok(url) = reqwest::Url::parse(&word) else {
                continue;
            };
            let parts: Vec<_> = url.path_segments().into_iter().flatten().collect();
            if url.host_str() != Some("github.com")
                || parts.len() != 4
                || parts[2] != "commit"
                || parts[3].len() < 12
                || !parts[3].bytes().all(|b| b.is_ascii_hexdigit())
            {
                continue;
            }
            let source = format!("{}/{}", parts[0], parts[1]);
            for (_, c) in self
                .report
                .commits
                .range(parts[3].to_owned()..)
                .take_while(|(sha, _)| sha.starts_with(parts[3]))
            {
                if let Some(ids) = self.patches.get(c.patch_id.as_deref().unwrap_or(&c.sha)) {
                    explicit.extend(ids.iter().filter(|i| {
                        source.eq_ignore_ascii_case(&self.report.repository)
                            || self.report.features[**i]
                                .sources
                                .iter()
                                .any(|s| s.repository.eq_ignore_ascii_case(&source))
                    }));
                }
            }
        }
        let title = crate::analyze::tokens(&thread.title);
        let mut overlaps = BTreeMap::<usize, usize>::new();
        for word in &title {
            if let Some(ids) = self.words.get(word) {
                for id in ids {
                    *overlaps.entry(*id).or_default() += 1;
                }
            }
        }
        let possible = overlaps
            .into_iter()
            .filter(|(id, n)| *n >= 3 && *n * 2 >= title.len() && !explicit.contains(id))
            .map(|(id, _)| id)
            .collect();
        (explicit, possible)
    }
}

fn demand_class(thread: &Issue) -> &'static str {
    if thread.kind != "discussion" {
        return "tracked_issue";
    }
    let slug = thread.discussion_category.as_deref().unwrap_or("");
    if [
        "bug",
        "bugs",
        "feature",
        "features",
        "feature-request",
        "feature-requests",
        "enhancement",
        "enhancements",
        "ideas",
        "suggestions",
    ]
    .iter()
    .any(|p| slug == *p || slug.starts_with(&format!("{p}-")))
    {
        "bug_or_feature_request"
    } else if [
        "announcements",
        "show-and-tell",
        "polls",
        "guides",
        "how-to",
    ]
    .contains(&slug)
    {
        "community_activity"
    } else if thread.discussion_answerable == Some(true) {
        "support_question"
    } else {
        "unclassified"
    }
}
fn actionable(class: &str) -> bool {
    matches!(class, "tracked_issue" | "bug_or_feature_request")
}

pub fn build(
    report: &Report,
    baseline: Option<&Report>,
    prs: Option<&PullSnapshot>,
    only_new: bool,
    limit: usize,
) -> Result<Shortlist> {
    anyhow::ensure!(
        !only_new || baseline.is_some(),
        "--only-new requires --baseline"
    );
    if let Some(base) = baseline {
        anyhow::ensure!(
            base.repository.eq_ignore_ascii_case(&report.repository),
            "baseline belongs to a different repository"
        );
    }
    if let Some(prs) = prs {
        anyhow::ensure!(
            prs.repository.eq_ignore_ascii_case(&report.repository),
            "PR snapshot belongs to a different repository"
        );
    }
    anyhow::ensure!(
        report
            .demand
            .as_ref()
            .and_then(|d| d.query.as_deref())
            .is_none_or(|q| q.trim().is_empty()),
        "Demand snapshot is topic-filtered; use --refresh-demand for project-wide evidence"
    );
    let prior_sets: BTreeSet<BTreeSet<&str>> = baseline
        .into_iter()
        .flat_map(|b| &b.features)
        .map(|f| f.patch_ids.iter().map(String::as_str).collect())
        .collect();
    let prior_patches: BTreeSet<&str> = prior_sets.iter().flat_map(|s| s.iter().copied()).collect();
    let mut warnings = report.coverage.warnings.clone();
    if let Some(base) = baseline {
        warnings.push("Baseline comparison measures newly observed patches, not creation time or semantic novelty. Incomplete prior coverage can surface old work.".into());
        if !base.coverage.warnings.is_empty() {
            warnings.push(format!(
                "Baseline has {} coverage notes",
                base.coverage.warnings.len()
            ));
        }
    }
    let mut requests = Vec::new();
    if let Some(demand) = &report.demand {
        warnings.extend(demand.warnings.clone());
        let mut seen = BTreeSet::new();
        for t in &demand.threads {
            if t.is_pull_request
                || !["open", "unanswered"].contains(&t.state.to_lowercase().as_str())
                || !seen.insert(&t.url)
            {
                continue;
            }
            let prefix = format!("https://github.com/{}/", report.repository).to_lowercase();
            if !t.url.to_lowercase().starts_with(&prefix) {
                continue;
            }
            requests.push(Request {
                url: t.url.clone(),
                title: t.title.clone(),
                demand_class: demand_class(t).into(),
                discussion_category: t.discussion_category.clone(),
                positive_votes: match (t.thumbs_up, t.upvotes) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(0).max(b.unwrap_or(0))),
                },
                priority_labels: t
                    .labels
                    .iter()
                    .filter(|l| {
                        demand
                            .priority_labels
                            .iter()
                            .any(|p| p.eq_ignore_ascii_case(l))
                    })
                    .cloned()
                    .collect(),
                linked_candidates: vec![],
                possible_candidates: vec![],
                discovery_candidates: vec![],
            });
        }
    } else {
        warnings.push("Project demand not collected; impact is unknown for every candidate".into());
    }
    if let Some(prs) = prs {
        warnings.extend(prs.warnings.clone());
    } else {
        warnings.push("Open upstream PR membership not checked".into());
    }
    warnings.push("An explicit reference proves a link was made, not that the implementation satisfies the request. Topic overlap is unverified and does not admit a candidate to the demand shortlist.".into());
    warnings.push("Votes and project priority labels are observed demand signals, not measurements of real-world impact. No topic, test-file, recency, fork-popularity, or patch-size boost is used.".into());
    let index = DemandIndex::new(report);
    let mut matches_by_feature = vec![(Vec::new(), Vec::new(), Vec::new()); report.features.len()];
    if let Some(demand) = &report.demand {
        let threads: BTreeMap<_, _> = demand.threads.iter().map(|t| (&t.url, t)).collect();
        for request in &mut requests {
            let (explicit, possible) = index.matches(threads[&request.url]);
            for i in explicit {
                request
                    .linked_candidates
                    .push(report.features[i].id.clone());
                if actionable(&request.demand_class) {
                    matches_by_feature[i].0.push(request.url.clone());
                } else {
                    matches_by_feature[i].2.push(request.url.clone());
                }
            }
            for i in possible {
                request
                    .possible_candidates
                    .push(report.features[i].id.clone());
                if actionable(&request.demand_class) {
                    matches_by_feature[i].1.push(request.url.clone());
                }
            }
        }
    }
    let pr_matches = prs
        .map(|p| pull_coverage(report, p))
        .transpose()?
        .unwrap_or_default();
    let mut candidates = Vec::new();
    for (feature_index, f) in report.features.iter().enumerate() {
        let set: BTreeSet<_> = f.patch_ids.iter().map(String::as_str).collect();
        let new = set.difference(&prior_patches).count();
        let delta = if baseline.is_none() {
            "no_baseline"
        } else if prior_sets.contains(&set) {
            "unchanged_patch_set"
        } else if new == 0 {
            "previously_observed_patches"
        } else {
            "newly_observed_patches"
        };
        let mut candidate = Candidate {
            id: f.id.clone(),
            title: f.title.clone(),
            delta: delta.into(),
            newly_observed_patches: if baseline.is_some() { new } else { 0 },
            open_prs: vec![],
            partial_prs: vec![],
            matched_requests: vec![],
            possible_requests: vec![],
            other_thread_links: vec![],
            eligible: false,
            reasons: vec![],
            patches: f.patch_ids.len(),
            files: f.files.len(),
            changed_test_files: f.test_files.len(),
            additions: f.additions,
            deletions: f.deletions,
            source_urls: f
                .sources
                .iter()
                .filter_map(|s| s.url.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        };
        if let Some(coverage) = pr_matches.get(&f.id) {
            candidate.open_prs = coverage.open_prs.clone();
            candidate.partial_prs = coverage.partial_prs.clone();
        }
        candidate.matched_requests = matches_by_feature[feature_index].0.clone();
        candidate.possible_requests = matches_by_feature[feature_index].1.clone();
        candidate.other_thread_links = matches_by_feature[feature_index].2.clone();
        if candidate.matched_requests.is_empty() {
            candidate
                .reasons
                .push("No explicit link to a catalogued open request; demand unknown".into());
        }
        if !candidate.other_thread_links.is_empty() {
            candidate.reasons.push("Explicit links to other open threads are retained; their intent is not classified as a bug/feature request".into());
        }
        if !candidate.open_prs.is_empty() {
            candidate.reasons.push("Patch history represented in an open upstream PR; follow its existing review queue".into());
        }
        if ["dismissed", "adopted"].contains(&f.status.as_str()) {
            candidate
                .reasons
                .push(format!("Maintainer decision: {}", f.status));
        }
        if only_new && new == 0 {
            candidate
                .reasons
                .push("No newly observed patches relative to baseline".into());
        }
        candidate.eligible = !candidate.matched_requests.is_empty()
            && candidate.open_prs.is_empty()
            && !["dismissed", "adopted"].contains(&f.status.as_str())
            && (!only_new || new > 0);
        if candidate.eligible {
            for request in &mut requests {
                if candidate.matched_requests.contains(&request.url) {
                    request.discovery_candidates.push(f.id.clone());
                }
            }
        }
        candidates.push(candidate);
    }
    // Equal evidence remains equal demand. URL/ID are deterministic display tie-breakers only.
    requests.sort_by(|a, b| {
        (!actionable(&a.demand_class))
            .cmp(&(!actionable(&b.demand_class)))
            .then(
                a.priority_labels
                    .is_empty()
                    .cmp(&b.priority_labels.is_empty())
                    .then(
                        b.positive_votes
                            .unwrap_or(0)
                            .cmp(&a.positive_votes.unwrap_or(0)),
                    )
                    .then(a.url.cmp(&b.url)),
            )
    });
    for request in &mut requests {
        request.discovery_candidates.sort();
        request.linked_candidates.sort();
        request.possible_candidates.sort();
    }
    let mut selected = Vec::new();
    // One representative per request per round; avoid spending the first batch on its variants.
    let rounds = requests
        .iter()
        .map(|r| r.discovery_candidates.len())
        .max()
        .unwrap_or(0);
    'selection: for round in 0..rounds {
        for request in &requests {
            if let Some(id) = request.discovery_candidates.get(round) {
                if !selected.contains(id) {
                    selected.push(id.clone());
                }
                if limit > 0 && selected.len() >= limit {
                    break 'selection;
                }
            }
        }
    }
    let mut counts = BTreeMap::from([
        ("inventory".into(), candidates.len()),
        (
            "legacy_high".into(),
            report
                .features
                .iter()
                .filter(|f| priority::get(report, f).tier == priority::Tier::High)
                .count(),
        ),
        (
            "explicit_demand_link".into(),
            candidates
                .iter()
                .filter(|c| !c.matched_requests.is_empty())
                .count(),
        ),
        (
            "topic_overlap_only".into(),
            candidates
                .iter()
                .filter(|c| c.matched_requests.is_empty() && !c.possible_requests.is_empty())
                .count(),
        ),
        (
            "no_observed_demand_link".into(),
            candidates
                .iter()
                .filter(|c| c.matched_requests.is_empty() && c.possible_requests.is_empty())
                .count(),
        ),
        (
            "represented_in_open_pr".into(),
            candidates.iter().filter(|c| !c.open_prs.is_empty()).count(),
        ),
        (
            "newly_observed_patch_sets".into(),
            candidates
                .iter()
                .filter(|c| c.delta == "newly_observed_patches")
                .count(),
        ),
        (
            "eligible".into(),
            candidates.iter().filter(|c| c.eligible).count(),
        ),
        ("selected".into(), selected.len()),
    ]);
    counts.insert("open_threads".into(), requests.len());
    counts.insert(
        "classified_open_requests".into(),
        requests
            .iter()
            .filter(|r| actionable(&r.demand_class))
            .count(),
    );
    counts.insert(
        "other_open_threads".into(),
        requests
            .iter()
            .filter(|r| !actionable(&r.demand_class))
            .count(),
    );
    counts.insert(
        "catalog_threads".into(),
        report.demand.as_ref().map_or(0, |d| d.threads.len()),
    );
    counts.insert(
        "truncated_thread_bodies".into(),
        report
            .demand
            .as_ref()
            .map_or(0, |d| d.threads.iter().filter(|t| t.body_truncated).count()),
    );
    counts.insert(
        "threads_with_omitted_comments".into(),
        report.demand.as_ref().map_or(0, |d| {
            d.threads.iter().filter(|t| t.comments_omitted > 0).count()
        }),
    );
    counts.insert(
        "open_prs_checked".into(),
        prs.map_or(0, |p| {
            p.pulls.iter().filter(|p| p.membership_verified).count()
        }),
    );
    counts.insert(
        "unverified_pr_membership".into(),
        prs.map_or(0, |p| {
            p.pulls.iter().filter(|p| !p.membership_verified).count()
        }),
    );
    Ok(Shortlist { schema_version: 1, repository: report.repository.clone(), base_sha: report.base_sha.clone(), source_fingerprint: fingerprint(report), generated_at: chrono::Utc::now().to_rfc3339(), scan_generated_at: report.generated_at.clone(), demand_fetched_at: report.demand.as_ref().map(|d| d.fetched_at.clone()), baseline_generated_at: baseline.map(|b| b.generated_at.clone()), prs_fetched_at: prs.map(|p| p.fetched_at.clone()), policy: "Explicitly linked open project requests; configured priority labels first, then positive votes. Only tracked issues and project categories naming bugs/features/ideas/suggestions supply request evidence; other categories remain separate. No inferred impact score. Round-robin candidate representatives across ranked requests; ID breaks equal-evidence ties. Open-PR work stays in its existing queue; absent demand/PR matches remain unknown.".into(), only_new, limit, counts, warnings, requests, candidates, selected_feature_ids: selected })
}

pub fn validate_selection(shortlist: &Shortlist, report: &Report) -> Result<Vec<String>> {
    anyhow::ensure!(
        shortlist.schema_version == 1
            && shortlist.repository == report.repository
            && shortlist.base_sha == report.base_sha
            && shortlist.source_fingerprint == fingerprint(report),
        "shortlist does not match this report's pinned evidence; regenerate it"
    );
    let mut seen = BTreeSet::new();
    for id in &shortlist.selected_feature_ids {
        anyhow::ensure!(
            seen.insert(id) && report.features.iter().any(|f| &f.id == id),
            "invalid or duplicate shortlist feature"
        );
    }
    Ok(shortlist.selected_feature_ids.clone())
}

pub fn terminal(s: &Shortlist) -> String {
    let mut out = format!(
        "{} · demand evidence shortlist\n{}\n\n",
        s.repository, s.policy
    );
    for (key, count) in &s.counts {
        let _ = writeln!(out, "{key}: {count}");
    }
    for id in &s.selected_feature_ids {
        let c = s.candidates.iter().find(|c| &c.id == id).unwrap();
        let _ = writeln!(
            out,
            "\n{}  {}\n  {} patches · {} files · {} changed test files (not executed) · {}",
            c.id,
            render::clean(&c.title),
            c.patches,
            c.files,
            c.changed_test_files,
            c.delta
        );
        for url in &c.matched_requests {
            let _ = writeln!(out, "  {url}");
        }
    }
    let _ = writeln!(
        out,
        "\n{} coverage/evidence notes. No model calls made.",
        s.warnings.len()
    );
    out
}

pub fn html(s: &Shortlist) -> String {
    let h = render::html_escape;
    let mut out = format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Forkpicker · demand evidence</title><style>:root{{color-scheme:light dark}}body{{overflow-wrap:anywhere;font:16px/1.6 system-ui;max-width:1000px;margin:40px auto;padding:0 20px}}article,aside,details{{border:1px solid #71809666;border-radius:10px;padding:20px;margin:16px 0}}h1{{line-height:1.2}}a{{color:light-dark(#076c66,#7ae0cf)}}small{{opacity:.75}}table{{width:100%;border-collapse:collapse}}td{{border-bottom:1px solid #71809644;padding:6px}}td:last-child{{text-align:right}}pre{{white-space:pre-wrap;overflow-wrap:anywhere}}input{{font:inherit;padding:10px;width:100%;box-sizing:border-box}}[hidden]{{display:none!important}}</style><h1>{}: observed demand</h1><p>Requests with linked fork implementations. These links do not establish that the code solves the request.</p><aside><strong>Selection policy</strong><p>{}</p><p>Demand snapshot: {}. PR snapshot: {}. Scan: {}.</p></aside><table>"#,
        h(&s.repository),
        h(&s.policy),
        h(s.demand_fetched_at.as_deref().unwrap_or("not collected")),
        h(s.prs_fetched_at.as_deref().unwrap_or("not checked")),
        h(&s.scan_generated_at)
    );
    for (k, v) in &s.counts {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{v}</td></tr>",
            h(&k.replace('_', " "))
        );
    }
    out.push_str("</table><h2>Suggested candidates</h2><p>Representatives from requests with explicitly linked implementations; tied evidence does not imply a quality difference.</p>");
    for id in &s.selected_feature_ids {
        let c = s.candidates.iter().find(|c| &c.id == id).unwrap();
        let _ = write!(
            out,
            "<aside><h3>{}</h3><p><code>{}</code> · {} patches · {} files</p>",
            h(&c.title),
            h(id),
            c.patches,
            c.files
        );
        for url in &c.matched_requests {
            if let Some(r) = s.requests.iter().find(|r| &r.url == url) {
                let _ = write!(
                    out,
                    "<p>{} · {} positive votes</p>",
                    render::github_link(url, &r.title),
                    r.positive_votes
                        .map(|n| n.to_string())
                        .unwrap_or("unknown".into())
                );
            }
        }
        out.push_str("</aside>");
    }
    if s.selected_feature_ids.is_empty() {
        out.push_str("<p>No candidates meet this evidence policy. Unknown demand and possible matches remain below.</p>");
    }
    out.push_str("<h2>Open project threads</h2><p>Classified requests appear first, ordered by priority labels and positive votes. Other threads follow with their classification shown.</p><input id=\"search\" aria-label=\"Search requests\" placeholder=\"Search project requests…\">");
    for r in &s.requests {
        let _ = write!(out,"<article><h3>{}</h3><p>{} positive votes · priority labels: {}</p><p>Thread classification: {} · project category: {}</p><p>{} explicitly linked candidates · {} possible topic matches · {} candidates outside an identified open PR queue</p>",render::github_link(&r.url, &r.title),r.positive_votes.map(|v| v.to_string()).unwrap_or("unknown".into()),h(&r.priority_labels.join(", ")),h(&r.demand_class),h(r.discussion_category.as_deref().unwrap_or("not supplied")),r.linked_candidates.len(),r.possible_candidates.len(),r.discovery_candidates.len());
        for id in &r.linked_candidates {
            let c = s.candidates.iter().find(|c| &c.id == id).unwrap();
            let _ = write!(out,"<details><summary>{} {}</summary><p><code>{}</code> · {} patches · {} files · {} changed test files (not executed)</p><p>{}</p>",if s.selected_feature_ids.contains(id){"Selected ·"}else{""},h(&c.title),h(id),c.patches,c.files,c.changed_test_files,h(&c.delta));
            for n in &c.open_prs {
                let _ = write!(out,"<p>Represented in <a href=\"https://github.com/{}/pull/{n}\">open PR #{n}</a> (patch/history evidence)</p>",h(&s.repository));
            }
            for url in &c.source_urls {
                if url.starts_with("https://github.com/") {
                    let _ = write!(out, "<p><a href=\"{}\">{}</a></p>", h(url), h(url));
                }
            }
            out.push_str("</details>");
        }
        out.push_str("</article>");
    }
    out.push_str("<details><summary>All fork candidates, including unknown demand</summary><input id=\"candidate-search\" aria-label=\"Search candidate inventory\" placeholder=\"Search all discovered candidates…\">");
    for c in &s.candidates {
        let _ = write!(out,"<div class=\"candidate\"><h4>{}</h4><p><code>{}</code> · {} · {} explicit request links · {} possible topic matches · {} open PR associations</p><p>{} patches · {} files · {} changed test files (not executed)</p>",h(&c.title),h(&c.id),h(&c.delta),c.matched_requests.len(),c.possible_requests.len(),c.open_prs.len(),c.patches,c.files,c.changed_test_files);
        for url in c.matched_requests.iter().chain(&c.other_thread_links) {
            let _ = write!(
                out,
                "<p>Linked thread: {}</p>",
                render::github_link(url, url)
            );
        }
        for reason in &c.reasons {
            let _ = write!(out, "<p>{}</p>", h(reason));
        }
        out.push_str("</div>");
    }
    out.push_str("</details><script>document.getElementById('candidate-search').addEventListener('input',e=>{const q=e.target.value.toLowerCase();document.querySelectorAll('.candidate').forEach(c=>c.hidden=!c.textContent.toLowerCase().includes(q))})</script>");
    let _ = write!(out,"<details><summary>Coverage and limitations · {} notes</summary><pre>{}</pre></details><p>Unknown demand is retained in the JSON inventory. The older high/medium/low score is shown only for comparison and does not order this shortlist.</p><script>document.getElementById('search').addEventListener('input',e=>{{const q=e.target.value.toLowerCase();document.querySelectorAll('article').forEach(a=>a.hidden=!a.textContent.toLowerCase().includes(q))}})</script></html>",s.warnings.len(),h(&s.warnings.join("\n")));
    out
}
