//! Release windows use raw patch author dates, never previous model judgments.
use crate::{classify::ClassifyArgs, discovery::Entry, github::Github, model::Report};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Duration, FixedOffset, Months};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

type Date = DateTime<FixedOffset>;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Activity {
    pub latest_patch_date: Option<String>,
    pub age_days: Option<i64>,
    pub missing_patch_dates: usize,
    pub scope: Scope,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Recent,
    Unknown,
    Outside,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Window {
    pub tag: String,
    pub release_url: String,
    pub published_at: String,
    pub since: String,
    pub as_of: String,
    pub overlap: String,
    pub eligible_before_window: usize,
    pub recent: usize,
    pub unknown: usize,
    pub outside: usize,
    pub age_distribution: BTreeMap<String, usize>,
    /// Raw release evidence is saved with the run, independent of API caching.
    pub release: Value,
}
impl Window {
    pub fn eligible(&self) -> usize {
        self.recent + self.unknown
    }
}

pub fn configure(
    args: &ClassifyArgs,
    report: &Report,
    entries: &mut [Entry],
    cache: &Path,
) -> Result<Option<Window>> {
    ensure!(
        args.release_overlap_days.is_none()
            || args.since_release
            || args.release_snapshot.is_some(),
        "--release-overlap-days requires --since-release or --release-snapshot"
    );
    let release = if let Some(path) = &args.release_snapshot {
        serde_json::from_slice(&std::fs::read(path)?)?
    } else if args.since_release {
        let mut api = Github::new(cache.to_owned(), true, args.api_budget)?;
        api.get(&format!("repos/{}/releases/latest", report.repository))?
            .0
    } else {
        return Ok(None);
    };
    apply(report, entries, release, args.release_overlap_days).map(Some)
}

pub fn apply(
    report: &Report,
    entries: &mut [Entry],
    release: Value,
    overlap_days: Option<u32>,
) -> Result<Window> {
    ensure!(
        release["draft"] == false && release["prerelease"] == false,
        "release window requires a published stable release"
    );
    let tag = release["tag_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("release tag missing")?;
    let url = release["html_url"]
        .as_str()
        .context("release URL missing")?;
    ensure!(
        url.to_lowercase().starts_with(&format!(
            "https://github.com/{}/releases/tag/",
            report.repository.to_lowercase()
        )),
        "release belongs to another repository"
    );
    let published = Date::parse_from_rfc3339(
        release["published_at"]
            .as_str()
            .context("release publication date missing")?,
    )?;
    let as_of = Date::parse_from_rfc3339(&report.generated_at)?;
    ensure!(published <= as_of,
        "latest release postdates this scan; refresh the scan or supply an earlier --release-snapshot");
    let since = match overlap_days {
        Some(days) => published.checked_sub_signed(Duration::days(i64::from(days))),
        None => published.checked_sub_months(Months::new(1)),
    }
    .context("release window date out of range")?;
    // %aI is saved by Git::commit. A normal rebase changes committer dates, not these.
    // Across identical patch IDs, choose the earliest observed author date to avoid
    // promoting a later cherry-pick/copy of unchanged work. Rewritten authorship and
    // unseen older copies cannot be detected from this snapshot.
    let mut dates = BTreeMap::<&str, Date>::new();
    for commit in report
        .commits
        .values()
        .filter(|c| c.parents.len() <= 1 && !c.files.is_empty())
    {
        if let Ok(date) = Date::parse_from_rfc3339(&commit.date) {
            if date <= as_of {
                let key = commit.patch_id.as_deref().unwrap_or(&commit.sha);
                dates
                    .entry(key)
                    .and_modify(|old| *old = (*old).min(date))
                    .or_insert(date);
            }
        }
    }
    let features: BTreeMap<_, _> = report.features.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut window = Window {
        tag: tag.into(),
        release_url: url.into(),
        published_at: published.to_rfc3339(),
        since: since.to_rfc3339(),
        as_of: as_of.to_rfc3339(),
        overlap: overlap_days
            .map(|d| format!("{d} days"))
            .unwrap_or_else(|| "1 calendar month".into()),
        eligible_before_window: entries.len(),
        recent: 0,
        unknown: 0,
        outside: 0,
        age_distribution: BTreeMap::new(),
        release,
    };
    for entry in entries {
        let f = features
            .get(entry.candidate_id.as_str())
            .context("candidate absent from scan")?;
        let latest = f
            .patch_ids
            .iter()
            .filter_map(|p| dates.get(p.as_str()))
            .max()
            .copied();
        let missing = f
            .patch_ids
            .iter()
            .filter(|p| !dates.contains_key(p.as_str()))
            .count();
        let scope = if latest.is_some_and(|d| d >= since) {
            Scope::Recent
        } else if missing > 0 || latest.is_none() {
            Scope::Unknown
        } else {
            Scope::Outside
        };
        match scope {
            Scope::Recent => window.recent += 1,
            Scope::Unknown => window.unknown += 1,
            Scope::Outside => window.outside += 1,
        }
        let age = latest.map(|d| (as_of - d).num_days());
        if scope == Scope::Recent {
            let bucket = match age.unwrap_or_default() {
                0..=6 => "0–6 days",
                7..=29 => "7–29 days",
                _ => "30+ days",
            };
            *window.age_distribution.entry(bucket.into()).or_default() += 1;
        }
        entry.activity = Some(Activity {
            latest_patch_date: latest.map(|d| d.to_rfc3339()),
            age_days: age,
            missing_patch_dates: missing,
            scope,
        });
    }
    Ok(window)
}

pub fn eligible(entry: &Entry) -> bool {
    entry
        .activity
        .as_ref()
        .is_none_or(|a| a.scope != Scope::Outside)
}

/// Newer fork-local bundles first; every fork gets a turn before another bundle.
/// Age orders opportunities for screening, never predicts usefulness.
pub fn ordered(entries: &[Entry]) -> Vec<&Entry> {
    let mut counts = BTreeMap::<&str, usize>::new();
    for entry in entries.iter().filter(|e| eligible(e)) {
        for fork in &entry.forks {
            *counts.entry(fork).or_default() += 1;
        }
    }
    let date = |e: &Entry| {
        e.activity
            .as_ref()
            .and_then(|a| a.latest_patch_date.as_ref())
            .and_then(|s| Date::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
    };
    let mut forks = BTreeMap::<String, Vec<&Entry>>::new();
    for entry in entries.iter().filter(|e| eligible(e)) {
        let fork = entry
            .forks
            .iter()
            .min_by_key(|f| (counts[f.as_str()], crate::hash(f)))
            .cloned()
            .unwrap_or_default();
        forks.entry(fork).or_default().push(entry);
    }
    let mut forks: Vec<_> = forks
        .into_iter()
        .map(|(fork, mut es)| {
            es.sort_by(|a, b| {
                date(b)
                    .cmp(&date(a))
                    .then_with(|| a.candidate_id.cmp(&b.candidate_id))
            });
            (fork, es)
        })
        .collect();
    let mut result = Vec::new();
    while !forks.is_empty() {
        forks.sort_by(|(fa, a), (fb, b)| date(b[0]).cmp(&date(a[0])).then_with(|| fa.cmp(fb)));
        for (_, entries) in &mut forks {
            result.extend(entries.drain(..entries.len().min(12)));
        }
        forks.retain(|(_, es)| !es.is_empty());
    }
    result
}
