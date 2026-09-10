//! Deterministic recommendations for spending review time and model quota.
use crate::{analyze::tokens, model::*};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Low,
    Medium,
    High,
}
impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
    pub fn action(self) -> &'static str {
        match self {
            Self::High => "Recommend model review",
            Self::Medium => "Skim before spending model quota",
            Self::Low => "Defer model review unless explicitly selected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemandSnapshot {
    pub fetched_at: String,
    pub query: Option<String>,
    pub priority_labels: Vec<String>,
    pub threads: Vec<Issue>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemandMatch {
    pub url: String,
    pub title: String,
    pub match_kind: String,
    pub points: i32,
    pub signals: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    pub tier: Tier,
    pub score: i32,
    pub code_score: i32,
    pub demand_points: i32,
    pub demand_status: String,
    pub action: String,
    pub reasons: Vec<String>,
    pub demand_matches: Vec<DemandMatch>,
    pub estimated_review_size: String,
    pub superseded_by: Option<String>,
}

pub fn default_priority_labels() -> Vec<String> {
    [
        "priority: high",
        "priority: critical",
        "high priority",
        "p0",
        "p1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub(crate) fn exact_reference(message: &str, repository: &str, issue: &Issue) -> bool {
    message.split_whitespace().any(|word| {
        let word =
            word.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | ',' | '.' | ':' | ';'));
        word == issue.url
            || (issue.kind != "discussion"
                && (word == format!("#{}", issue.number)
                    || word.eq_ignore_ascii_case(&format!("{repository}#{}", issue.number))))
    })
}

fn matching(report: &Report, feature: &Feature, thread: &Issue) -> Option<bool> {
    // Only project-owned threads establish project demand.
    let url = reqwest::Url::parse(&thread.url).ok()?;
    let segments: Vec<_> = url.path_segments()?.collect();
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || segments.len() != 4
        || !format!("{}/{}", segments[0], segments[1]).eq_ignore_ascii_case(&report.repository)
        || !["issues", "discussions"].contains(&segments[2])
    {
        return None;
    }
    let messages: Vec<_> = feature
        .commits
        .iter()
        .filter_map(|s| report.commits.get(s))
        .collect();
    if messages
        .iter()
        .any(|c| exact_reference(&c.message, &report.repository, thread))
    {
        return Some(true);
    }
    // A discussion referencing this exact fork commit is stronger than topic overlap.
    let linked = thread
        .body
        .split_whitespace()
        .chain(
            thread
                .comments
                .iter()
                .flat_map(|c| c.body.split_whitespace()),
        )
        .any(|word| {
            let word = word.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | ',' | '.' | '`'));
            let Ok(url) = reqwest::Url::parse(word) else {
                return false;
            };
            let parts: Vec<_> = url.path_segments().into_iter().flatten().collect();
            url.host_str() == Some("github.com")
                && parts.len() == 4
                && parts[2] == "commit"
                && (format!("{}/{}", parts[0], parts[1]).eq_ignore_ascii_case(&report.repository)
                    || feature.sources.iter().any(|s| {
                        s.repository
                            .eq_ignore_ascii_case(&format!("{}/{}", parts[0], parts[1]))
                    }))
                && parts[3].len() >= 12
                && report.commits.values().any(|c| {
                    c.sha.starts_with(parts[3])
                        && (feature.commits.contains(&c.sha)
                            || c.patch_id
                                .as_ref()
                                .is_some_and(|id| feature.patch_ids.contains(id)))
                })
        });
    if linked {
        return Some(true);
    }
    let title = tokens(&thread.title);
    let subjects = messages
        .iter()
        .map(|c| c.subject.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let candidates = tokens(&format!("{} {}", feature.title, subjects));
    let overlap = title.intersection(&candidates).count();
    // File boilerplate and entire commit bodies do not create weak popularity matches.
    (overlap >= 3 && overlap * 2 >= title.len()).then_some(false)
}

pub fn recommendation(report: &Report, feature: &Feature) -> Recommendation {
    recommend(
        report,
        feature,
        superseding(feature, report.features.iter()),
    )
}

fn superseding<'a>(feature: &Feature, others: impl Iterator<Item = &'a Feature>) -> Option<String> {
    others
        .filter(|other| {
            other.id != feature.id
                && other.patch_ids.len() > feature.patch_ids.len()
                && feature
                    .patch_ids
                    .iter()
                    .all(|id| other.patch_ids.contains(id))
        })
        .min_by_key(|other| (other.patch_ids.len(), &other.id))
        .map(|f| f.id.clone())
}

fn recommend(report: &Report, feature: &Feature, superseded_by: Option<String>) -> Recommendation {
    let mut reasons = feature
        .reasons
        .iter()
        .filter(|s| {
            !s.starts_with("Possible topic match:")
                && !s.starts_with("Explicit issue/commit reference:")
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut matches = Vec::new();
    if let Some(snapshot) = &report.demand {
        let mut seen = BTreeSet::new();
        for thread in &snapshot.threads {
            // Answered/closed threads and PR activity are not unmet feature demand.
            if thread.is_pull_request
                || !["open", "unanswered"].contains(&thread.state.to_lowercase().as_str())
                || !seen.insert(thread.url.clone())
            {
                continue;
            }
            let Some(explicit) = matching(report, feature, thread) else {
                continue;
            };
            let votes = thread
                .upvotes
                .unwrap_or(0)
                .max(thread.thumbs_up.unwrap_or(0));
            let vote_points = match votes {
                0 => 0,
                1..=4 => 2,
                5..=19 => 5,
                20..=99 => 8,
                _ => 12,
            };
            let priority_labels: Vec<_> = thread
                .labels
                .iter()
                .filter(|label| {
                    snapshot
                        .priority_labels
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(label))
                })
                .cloned()
                .collect();
            let points = (4 + vote_points + if priority_labels.is_empty() { 0 } else { 8 })
                .min(if explicit { 20 } else { 12 });
            let mut signals = vec![
                "Relevant open request (+4)".into(),
                if thread.upvotes.is_none() && thread.thumbs_up.is_none() {
                    "Vote counts unavailable (no vote boost)".into()
                } else {
                    format!(
                        "{votes} upvotes/thumbs-up (+{vote_points}; counts are not added together)"
                    )
                },
            ];
            if !priority_labels.is_empty() {
                signals.push(format!(
                    "Configured project priority label: {} (+8)",
                    priority_labels.join(", ")
                ));
            }
            signals.push(if explicit {
                "Explicit reference; thread contribution capped at 20".into()
            } else {
                "Heuristic title match; thread contribution capped at 12".into()
            });
            matches.push(DemandMatch {
                url: thread.url.clone(),
                title: thread.title.clone(),
                match_kind: if explicit {
                    "explicit_reference"
                } else {
                    "topic_overlap"
                }
                .into(),
                points,
                signals,
            });
        }
    }
    matches.sort_by(|a, b| b.points.cmp(&a.points).then(a.url.cmp(&b.url)));
    matches.truncate(5);
    // Use only the strongest thread: duplicates and noisy comment counts do not pile up.
    let demand_points = matches.first().map_or(0, |m| m.points);
    let demand_status = if report.demand.is_none() {
        "not_collected"
    } else if matches.is_empty() {
        "no_match_in_sample"
    } else {
        "matched_open_request"
    };
    reasons.push(match demand_status {
        "not_collected" => "Project demand unknown; no demand snapshot collected".into(),
        "no_match_in_sample" => {
            "No matching open request in the sampled threads; demand remains unknown".into()
        }
        _ => format!("Project demand adds {demand_points}; strongest relevant thread only"),
    });
    let mut score = (feature.score + demand_points).clamp(0, 100);
    if let Some(other) = &superseded_by {
        score = score.min(44);
        reasons.push(format!("All patches also occur in larger candidate {other}; review that candidate first (score capped at 44)"));
    }
    let tier = if score >= 65 {
        Tier::High
    } else if score >= 45 {
        Tier::Medium
    } else {
        Tier::Low
    };
    let patch_bytes: usize = feature
        .commits
        .iter()
        .filter_map(|s| report.commits.get(s))
        .map(|c| c.patch.len())
        .sum();
    Recommendation {
        tier,
        score,
        code_score: feature.score,
        demand_points,
        demand_status: demand_status.into(),
        action: tier.action().into(),
        reasons,
        demand_matches: matches,
        estimated_review_size: if patch_bytes < 10_000 {
            "small"
        } else if patch_bytes < 50_000 {
            "medium"
        } else {
            "large"
        }
        .into(),
        superseded_by,
    }
}

pub fn refresh(report: &mut Report) {
    let mut by_patch: BTreeMap<&str, Vec<&Feature>> = BTreeMap::new();
    for feature in &report.features {
        for patch in &feature.patch_ids {
            by_patch.entry(patch).or_default().push(feature);
        }
    }
    report.recommendations = report
        .features
        .iter()
        .map(|feature| {
            // A superset must contain every patch; the rarest patch gives the
            // smallest candidate list without changing the subset rule.
            let candidates = feature
                .patch_ids
                .iter()
                .filter_map(|p| by_patch.get(p.as_str()))
                .min_by_key(|items| items.len());
            let superseded = match candidates {
                Some(items) => superseding(feature, items.iter().copied()),
                None => superseding(feature, report.features.iter()),
            };
            (feature.id.clone(), recommend(report, feature, superseded))
        })
        .collect();
}

pub fn selected(report: &Report, minimum: Tier) -> Vec<&Feature> {
    let mut features: Vec<_> = report
        .features
        .iter()
        .filter(|f| !["dismissed", "adopted"].contains(&f.status.as_str()))
        .filter(|f| get(report, f).tier >= minimum)
        .collect();
    features.sort_by_key(|f| (std::cmp::Reverse(get(report, f).score), &f.id));
    features
}

pub fn get<'a>(report: &'a Report, feature: &Feature) -> std::borrow::Cow<'a, Recommendation> {
    match report.recommendations.get(&feature.id) {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None => std::borrow::Cow::Owned(recommendation(report, feature)),
    }
}
