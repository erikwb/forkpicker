//! Presentation evidence: patch-set relationships, observation deltas, and attributed links.
use crate::{analyze, metrics::Inventory, model::*, priority::DemandSnapshot};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Card {
    pub family: String,
    pub observation: String,
    pub newly_observed_patches: usize,
    pub prior_candidates: Vec<String>,
    pub context: Vec<ContextLink>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextLink {
    pub title: String,
    pub url: String,
    pub kind: String,
    pub explanation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Baseline {
    pub generated_at: String,
    pub coverage_notes: usize,
    pub note: String,
}

pub fn prepare(report: &Report, inventory: &mut Inventory) {
    let sets: BTreeMap<_, BTreeSet<_>> = report
        .features
        .iter()
        .map(|f| {
            (
                f.id.as_str(),
                f.patch_ids.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let mut indexed = BTreeMap::<&str, Vec<&str>>::new();
    for (id, set) in &sets {
        for patch in set {
            indexed.entry(patch).or_default().push(id);
        }
    }
    // Every member is a subset of its anchor. Do not create transitive groups from
    // a single shared patch across otherwise unrelated candidate sets.
    let mut anchors: Vec<_> = sets.keys().copied().collect();
    anchors.sort_by_key(|id| (std::cmp::Reverse(sets[id].len()), *id));
    let mut assigned = BTreeMap::new();
    for anchor in anchors {
        if assigned.contains_key(anchor) {
            continue;
        }
        assigned.insert(anchor, anchor);
        let members: BTreeSet<_> = sets[anchor]
            .iter()
            .flat_map(|p| indexed[p].iter().copied())
            .collect();
        for member in members {
            if !assigned.contains_key(member) && sets[member].is_subset(&sets[anchor]) {
                assigned.insert(member, anchor);
            }
        }
    }
    for f in &mut inventory.candidates {
        f.inbox = Card {
            family: assigned[f.feature_id.as_str()].into(),
            observation: "not-compared".into(),
            ..Card::default()
        };
    }
    let empty = DemandSnapshot {
        fetched_at: report.generated_at.clone(),
        query: None,
        priority_labels: vec![],
        threads: vec![],
        warnings: vec![],
    };
    add_context(report, inventory, report.demand.as_ref().unwrap_or(&empty));
}

pub fn compare(report: &Report, inventory: &mut Inventory, baseline: &Report) -> Result<()> {
    ensure!(
        report.repository.eq_ignore_ascii_case(&baseline.repository),
        "baseline belongs to a different repository"
    );
    let current_date = chrono::DateTime::parse_from_rfc3339(&report.generated_at)?;
    let prior_date = chrono::DateTime::parse_from_rfc3339(&baseline.generated_at)?;
    ensure!(
        prior_date <= current_date,
        "baseline is newer than the current scan"
    );
    let prior_sets: BTreeSet<BTreeSet<&str>> = baseline
        .features
        .iter()
        .map(|f| f.patch_ids.iter().map(String::as_str).collect())
        .collect();
    let known: BTreeSet<_> = prior_sets.iter().flat_map(|s| s.iter().copied()).collect();
    let mut indexed = BTreeMap::<&str, Vec<&str>>::new();
    for f in &baseline.features {
        for p in &f.patch_ids {
            indexed.entry(p).or_default().push(&f.id);
        }
    }
    let features: BTreeMap<_, _> = report.features.iter().map(|f| (&f.id, f)).collect();
    for facts in &mut inventory.candidates {
        let f = features[&facts.feature_id];
        let set: BTreeSet<_> = f.patch_ids.iter().map(String::as_str).collect();
        let new = set.difference(&known).count();
        facts.inbox.observation = if prior_sets.contains(&set) {
            "unchanged"
        } else if new == 0 {
            "regrouped"
        } else if new == set.len() {
            "new"
        } else {
            "extended"
        }
        .into();
        facts.inbox.newly_observed_patches = new;
        facts.inbox.prior_candidates = set
            .iter()
            .filter_map(|p| indexed.get(p))
            .flatten()
            .map(|id| id.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
    }
    inventory.baseline=Some(Baseline{generated_at:baseline.generated_at.clone(),coverage_notes:baseline.coverage.warnings.len(),note:"Newly observed means absent from the prior scan, not recently authored. Earlier coverage gaps can reveal old work. Changed patch sets share known patches; a completely rewritten patch may appear newly observed. This is not a review-history comparison.".into()});
    Ok(())
}

pub fn add_context(report: &Report, inventory: &mut Inventory, catalog: &DemandSnapshot) {
    let prefix = format!("https://github.com/{}/", report.repository.to_lowercase());
    let threads: Vec<_> = catalog
        .threads
        .iter()
        .chain(report.issues.iter())
        .filter(|t| t.url.to_lowercase().starts_with(&prefix))
        .collect();
    let mut titles = BTreeMap::<String, Vec<usize>>::new();
    for (i, t) in threads.iter().enumerate() {
        for term in analyze::tokens(&t.title) {
            titles.entry(term).or_default().push(i);
        }
    }
    let mut numbers = BTreeMap::<u64, Vec<usize>>::new();
    let mut urls = BTreeMap::<&str, Vec<usize>>::new();
    for (i, t) in threads.iter().enumerate() {
        if t.kind != "discussion" {
            numbers.entry(t.number).or_default().push(i);
        }
        urls.entry(&t.url).or_default().push(i);
    }
    let features: BTreeMap<_, _> = report.features.iter().map(|f| (&f.id, f)).collect();
    for facts in &mut inventory.candidates {
        let f = features[&facts.feature_id];
        let messages = f
            .commits
            .iter()
            .filter_map(|s| report.commits.get(s))
            .map(|c| c.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut links = BTreeMap::<String, ContextLink>::new();
        let mut references = BTreeSet::new();
        for number in analyze::issue_numbers(&messages) {
            if let Some(ids) = numbers.get(&number) {
                references.extend(ids.iter().copied());
            }
        }
        for word in messages.split_whitespace() {
            let word =
                word.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | ',' | '.' | ':' | ';'));
            if let Some(ids) = urls.get(word) {
                references.extend(ids.iter().copied());
            }
        }
        for index in references {
            let t = threads[index];
            if crate::priority::exact_reference(&messages, &report.repository, t) {
                links.insert(
                    t.url.clone(),
                    ContextLink {
                        title: t.title.clone(),
                        url: t.url.clone(),
                        kind: "explicit".into(),
                        explanation: format!(
                            "Referenced in a commit · {} · {}",
                            if t.is_pull_request {
                                "pull request"
                            } else if t.kind == "discussion" {
                                "discussion"
                            } else {
                                "issue"
                            },
                            t.state
                        ),
                    },
                );
            }
        }
        // Only titles retrieve possible context; issue templates/bodies don't make
        // generic words such as 'description' look like a behavioral connection.
        let tokens = analyze::tokens(&f.title);
        let mut hits = BTreeMap::<usize, BTreeSet<String>>::new();
        for term in &tokens {
            if let Some(ids) = titles.get(term) {
                for &id in ids {
                    hits.entry(id).or_default().insert(term.clone());
                }
            }
        }
        let mut matches: Vec<_> = hits
            .into_iter()
            .filter(|(id, terms)| {
                terms.len() >= 2
                    && ["open", "unanswered"].contains(&threads[*id].state.to_lowercase().as_str())
            })
            .collect();
        matches.sort_by(|(a, at), (b, bt)| {
            bt.len()
                .cmp(&at.len())
                .then(threads[*a].url.cmp(&threads[*b].url))
        });
        let mut count = 0;
        for (id, terms) in matches {
            let t = threads[id];
            if links.contains_key(&t.url) {
                continue;
            }
            links.insert(
                t.url.clone(),
                ContextLink {
                    title: t.title.clone(),
                    url: t.url.clone(),
                    kind: "text".into(),
                    explanation: format!(
                        "Shared title words: {}. Relevance unverified.",
                        terms.into_iter().collect::<Vec<_>>().join(", ")
                    ),
                },
            );
            count += 1;
            if count == 3 {
                break;
            }
        }
        for url in &f.issue_links {
            if !url.to_lowercase().starts_with(&prefix) {
                continue;
            }
            links.entry(url.clone()).or_insert_with(|| ContextLink {
                title: urls
                    .get(url.as_str())
                    .and_then(|ids| ids.first())
                    .map(|&i| threads[i].title.clone())
                    .unwrap_or_else(|| "Upstream context".into()),
                url: url.clone(),
                kind: "context".into(),
                explanation: "Collected context link; relationship unverified.".into(),
            });
        }
        facts.inbox.context = links.into_values().collect();
        facts
            .inbox
            .context
            .sort_by_key(|l| (l.kind != "explicit", l.url.clone()));
    }
}

pub fn observation_label(value: &str) -> &str {
    match value {
        "new" => "Newly observed",
        "extended" => "Changed patch set",
        "regrouped" => "Regrouped known patches",
        "unchanged" => "Previously seen",
        _ => "Not compared",
    }
}
