//! Fork inspection order from observed application results, without model calls.
use crate::{metrics::Inventory, model::Report};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const POLICY: &str = "Default fork order: most confirmed clean groups, then repository name; forks without usable application checks follow measured forks. Clean share is displayed separately and never discounts the default count. A group is clean when at least one contained variant applies directly or with a three-way merge. Unknown groups remain in the clean-share denominator. Groups are formed within each fork by patch-set containment, not shared topics. Partial overlaps remain separate. These measurements describe adoption friction, not code quality, usefulness, authorship, maintainer identity, or a spending recommendation. Counts include all recorded decision statuses and do not change with browser filters. Empty sets are excluded; forks without branch records are not represented. The legacy inspection_priority JSON field retains the previous formula for compatibility only; it does not order the dashboard or inventory.";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Group {
    pub anchor: String,
    pub candidate_ids: Vec<String>,
    /// clean, blocked (every variant failed), or unknown.
    pub application: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fork {
    pub repository: String,
    pub candidate_ids: Vec<String>,
    pub groups: Vec<Group>,
    pub unique_patches: usize,
    pub empty_candidates: usize,
    pub clean_groups: usize,
    pub blocked_groups: usize,
    pub unknown_groups: usize,
    pub confirmed_clean_fraction: Option<f64>,
    /// Legacy ratio-weighted run; unused by the dashboard and default order.
    pub inspection_priority: Option<f64>,
}

pub fn priority(clean: usize, total: usize, has_checks: bool) -> Option<f64> {
    (total > 0 && has_checks).then(|| clean as f64 / total as f64 * (clean as f64).ln_1p())
}

pub fn summarize(report: &Report, inventory: &Inventory) -> Vec<Fork> {
    summarize_excluding(report, inventory, &BTreeSet::new())
}

pub fn summarize_excluding(
    report: &Report,
    inventory: &Inventory,
    excluded: &BTreeSet<String>,
) -> Vec<Fork> {
    let facts: BTreeMap<_, _> = inventory
        .candidates
        .iter()
        .map(|c| (c.feature_id.as_str(), c))
        .collect();
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
    let target = inventory
        .integration
        .as_ref()
        .map(|s| s.target_sha.as_str())
        .unwrap_or(&report.base_sha);
    let mut repositories = BTreeMap::<String, BTreeSet<&str>>::new();
    for branch in &report.branches {
        repositories
            .entry(branch.source.repository.to_lowercase())
            .or_default();
    }
    for f in &report.features {
        if excluded.contains(&f.id) {
            continue;
        }
        for source in &f.sources {
            repositories
                .entry(source.repository.to_lowercase())
                .or_default()
                .insert(&f.id);
        }
    }
    let status = |id: &str| {
        facts
            .get(id)
            .and_then(|f| f.integration.as_ref())
            .filter(|c| c.target_sha == target)
            .map(|c| c.status.as_str())
            .unwrap_or("unknown")
    };
    let mut result = Vec::new();
    for (repository, ids) in repositories {
        let is_empty = |id: &&str| {
            sets[*id].is_empty()
                || facts
                    .get(*id)
                    .is_some_and(|f| f.change_size == "no-file-changes")
        };
        let empty_candidates = ids.iter().filter(|id| is_empty(id)).count();
        let mut anchors: Vec<_> = ids.iter().copied().filter(|id| !is_empty(id)).collect();
        let unique_patches = anchors
            .iter()
            .flat_map(|id| &sets[id])
            .collect::<BTreeSet<_>>()
            .len();
        anchors.sort_by_key(|id| (std::cmp::Reverse(sets[id].len()), *id));
        let mut indexed = BTreeMap::<&str, Vec<&str>>::new();
        for id in &anchors {
            for patch in &sets[id] {
                indexed.entry(patch).or_default().push(id);
            }
        }
        let mut assigned = BTreeSet::new();
        let mut groups = Vec::new();
        let mut has_checks = false;
        for anchor in anchors {
            if !assigned.insert(anchor) {
                continue;
            }
            let mut members = vec![anchor];
            let neighbors: BTreeSet<_> = sets[anchor]
                .iter()
                .flat_map(|p| indexed[p].iter().copied())
                .collect();
            for id in neighbors {
                if !assigned.contains(id) && sets[id].is_subset(&sets[anchor]) {
                    assigned.insert(id);
                    members.push(id);
                }
            }
            members.sort();
            let clean = members
                .iter()
                .any(|id| matches!(status(id), "clean" | "clean-three-way"));
            let blocked = members
                .iter()
                .all(|id| matches!(status(id), "conflicts" | "not-applicable"));
            has_checks |= members.iter().any(|id| {
                matches!(
                    status(id),
                    "clean" | "clean-three-way" | "conflicts" | "not-applicable"
                )
            });
            groups.push(Group {
                anchor: anchor.into(),
                candidate_ids: members.into_iter().map(str::to_owned).collect(),
                application: if clean {
                    "clean"
                } else if blocked {
                    "blocked"
                } else {
                    "unknown"
                }
                .into(),
            });
        }
        let clean_groups = groups.iter().filter(|g| g.application == "clean").count();
        let blocked_groups = groups.iter().filter(|g| g.application == "blocked").count();
        let unknown_groups = groups.len() - clean_groups - blocked_groups;
        let score = priority(clean_groups, groups.len(), has_checks);
        result.push(Fork {
            repository,
            candidate_ids: ids.into_iter().map(str::to_owned).collect(),
            unique_patches,
            empty_candidates,
            clean_groups,
            blocked_groups,
            unknown_groups,
            confirmed_clean_fraction: (!groups.is_empty() && has_checks)
                .then(|| clean_groups as f64 / groups.len() as f64),
            inspection_priority: score,
            groups,
        });
    }
    result.sort_by(default_order);
    result
}

fn default_order(a: &Fork, b: &Fork) -> std::cmp::Ordering {
    let count = |f: &Fork| f.confirmed_clean_fraction.map(|_| f.clean_groups);
    count(b)
        .cmp(&count(a))
        .then(a.repository.cmp(&b.repository))
}

pub fn html(forks: &[Fork]) -> String {
    use std::fmt::Write;
    let h = crate::render::html_escape;
    let mut out = String::from(
        r#"<nav class="views" aria-label="Inbox view"><button id="show-forks" aria-pressed="true">Forks</button><button id="show-patches" aria-pressed="false">Patch sets</button></nav><section id="fork-view"><div class="toolbar"><input id="fork-search" type="search" aria-label="Search forks" placeholder="Find a fork…"><select id="fork-sort" aria-label="Sort forks"><option value="clean">Most clean groups</option><option value="share">Highest confirmed clean share</option><option value="patches">Most distinct patches</option><option value="name">Repository name</option></select></div><p id="fork-count" class="muted" aria-live="polite"></p><div class="fork-table"><table><thead><tr><th>Fork</th><th>Clean / groups</th><th>Clean share</th><th>Blocked</th><th>Unknown</th><th>Distinct patches</th></tr></thead><tbody id="fork-rows">"#,
    );
    for f in forks {
        let number = |v: Option<f64>| v.map(|n| n.to_string()).unwrap_or_default();
        let share = f
            .confirmed_clean_fraction
            .map(|n| format!("{:.1}%", n * 100.0))
            .unwrap_or_else(|| "Unknown".into());
        let clean_sort = f
            .confirmed_clean_fraction
            .map(|_| f.clean_groups.to_string())
            .unwrap_or_default();
        let name = h(&f.repository);
        let identity = if let Ok(repo) = f.repository.parse::<crate::github::RepoName>() {
            let owner = repo.0.split('/').next().unwrap_or_default();
            let initial = owner.chars().next().unwrap_or('?').to_ascii_uppercase();
            format!("<a class=\"fork-identity\" href=\"https://github.com/{}\" rel=\"noreferrer\"><span class=\"avatar\" aria-hidden=\"true\"><span>{initial}</span><img src=\"https://github.com/{}.png?size=48\" width=\"24\" height=\"24\" alt=\"\" loading=\"lazy\" decoding=\"async\" referrerpolicy=\"no-referrer\"></span><span>{name}</span></a>", h(&repo.0), h(owner))
        } else {
            format!("<span class=\"fork-identity\">{name}</span>")
        };
        let route: String = f
            .repository
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let _ = write!(out, "<tr data-repository=\"{name}\" data-clean=\"{}\" data-share=\"{}\" data-patches=\"{}\"><td>{identity}<a class=\"inspect-fork\" href=\"#fork={route}\" data-fork=\"{name}\" aria-label=\"Inspect patches in {name}\">Inspect patches</a></td><td>{} / {}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", clean_sort, number(f.confirmed_clean_fraction), f.unique_patches, f.clean_groups, f.groups.len(), share, f.blocked_groups, f.unknown_groups, f.unique_patches);
    }
    out.push_str("</tbody></table></div><button id=\"more-forks\" hidden>Show 50 more</button></section><section id=\"patch-view\"><p id=\"selected-fork\" hidden><strong id=\"selected-fork-name\"></strong> <a id=\"selected-fork-github\" rel=\"noreferrer\" hidden>View on GitHub</a> <button id=\"clear-fork\">Show all forks’ patches</button></p>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_formula_remains_available_for_existing_json_consumers() {
        assert!(priority(40, 50, true) > priority(4, 5, true));
        assert!(priority(4, 5, true) > priority(40, 500, true));
        assert!(priority(80, 100, true).unwrap() < 2.0 * priority(40, 50, true).unwrap());
        assert_eq!(priority(0, 5, true), Some(0.0));
        assert_eq!(priority(0, 5, false), None);
        assert_eq!(priority(0, 0, true), None);
    }
    #[test]
    fn default_order_does_not_penalize_additional_blocked_or_unknown_work() {
        let mut large = Fork {
            repository: "z/large".into(),
            candidate_ids: vec![],
            groups: vec![],
            unique_patches: 0,
            empty_candidates: 0,
            clean_groups: 85,
            blocked_groups: 534,
            unknown_groups: 0,
            confirmed_clean_fraction: Some(85.0 / 619.0),
            inspection_priority: priority(85, 619, true),
        };
        let small = Fork {
            repository: "a/small".into(),
            clean_groups: 8,
            blocked_groups: 1,
            confirmed_clean_fraction: Some(8.0 / 9.0),
            inspection_priority: priority(8, 9, true),
            ..large.clone()
        };
        assert!(large.inspection_priority < small.inspection_priority);
        assert!(default_order(&large, &small).is_lt());
        large.blocked_groups += 10_000;
        large.unknown_groups += 10_000;
        large.confirmed_clean_fraction = Some(85.0 / 20619.0);
        assert!(default_order(&large, &small).is_lt());
        let unchecked = Fork {
            confirmed_clean_fraction: None,
            clean_groups: 0,
            ..small.clone()
        };
        let measured_zero = Fork {
            repository: "z/zero".into(),
            confirmed_clean_fraction: Some(0.0),
            ..unchecked.clone()
        };
        assert!(default_order(&measured_zero, &unchecked).is_lt());
        assert!(!html(&[large, small]).contains("Inspection priority"));
    }
}
