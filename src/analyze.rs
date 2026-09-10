use crate::{git::PatchIndex, hash, model::*};
use std::collections::{BTreeMap, BTreeSet};

pub fn tokens(text: &str) -> BTreeSet<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "from", "that", "this", "when", "then", "into", "use",
        "using", "used", "add", "adds", "added", "fix", "fixes", "fixed", "update", "updates",
        "support", "change", "changes", "test", "tests", "feat", "chore", "refactor", "docs",
        "only", "not", "all", "are", "can", "new", "make", "more", "set", "get", "should", "does",
        "its", "has", "have",
    ];
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !STOP.contains(w) && !w.chars().all(|c| c.is_numeric()))
        .map(String::from)
        .collect()
}

pub fn issue_numbers(text: &str) -> BTreeSet<u64> {
    text.split('#')
        .skip(1)
        .filter_map(|s| {
            let number: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            number.parse().ok()
        })
        .collect()
}

pub fn upstream_match(commit: &Commit, index: &PatchIndex) -> Option<UpstreamMatch> {
    if let Some(matches) = commit.patch_id.as_ref().and_then(|p| index.patches.get(p)) {
        return Some(UpstreamMatch {
            commit: commit.sha.clone(),
            upstream_commits: matches.clone(),
            kind: "stable_patch_id".into(),
        });
    }
    // A selective backport can match files from a larger upstream commit, or several commits.
    // Require every changed file, including tests/configuration, to be accounted for.
    if commit.files.is_empty() {
        return None;
    }
    let mut upstream = BTreeSet::new();
    for f in &commit.files {
        let matches = index.files.get(f.patch_fingerprint.as_ref()?)?;
        upstream.extend(matches.iter().cloned());
    }
    Some(UpstreamMatch {
        commit: commit.sha.clone(),
        upstream_commits: upstream.into_iter().collect(),
        kind: "all_file_patches".into(),
    })
}

fn relation(a: &Commit, b: &Commit) -> Option<(u32, String)> {
    let a_issues = issue_numbers(&a.message);
    let b_issues = issue_numbers(&b.message);
    if let Some(number) = a_issues.intersection(&b_issues).next() {
        return Some((95, format!("Both reference #{number}")));
    }
    let ta = tokens(&a.subject);
    let tb = tokens(&b.subject);
    let shared: Vec<_> = ta.intersection(&tb).cloned().collect();
    let files_a: BTreeSet<_> = a
        .files
        .iter()
        .filter(|f| !f.is_routine)
        .map(|f| &f.path)
        .collect();
    let files_b: BTreeSet<_> = b
        .files
        .iter()
        .filter(|f| !f.is_routine)
        .map(|f| &f.path)
        .collect();
    let overlap: Vec<_> = files_a.intersection(&files_b).collect();
    let symbols_a: BTreeSet<_> = a.files.iter().flat_map(|f| f.symbols.iter()).collect();
    let symbols_b: BTreeSet<_> = b.files.iter().flat_map(|f| f.symbols.iter()).collect();
    let shared_symbol = symbols_a.intersection(&symbols_b).next();
    if shared.len() >= 2 && (!overlap.is_empty() || shared_symbol.is_some()) {
        return Some((
            85,
            format!("Shared topic ({}) and changed code", shared.join(", ")),
        ));
    }
    if !shared.is_empty() && shared_symbol.is_some() {
        return Some((
            80,
            format!("Shared changed symbol and topic ({})", shared.join(", ")),
        ));
    }
    // Commit bodies often name the behavior more precisely than terse subjects.
    // Require bidirectional topic overlap plus shared files, not just a common
    // subsystem prefix such as "render:".
    let subject_tokens = |c: &Commit| {
        tokens(
            c.subject
                .split_once(':')
                .map(|(_, s)| s)
                .unwrap_or(&c.subject),
        )
    };
    let body_a = tokens(&a.message);
    let body_b = tokens(&b.message);
    let a_in_b = subject_tokens(a).intersection(&body_b).count();
    let b_in_a = subject_tokens(b).intersection(&body_a).count();
    if !overlap.is_empty() && a_in_b >= 2 && b_in_a >= 2 {
        return Some((
            78,
            "Shared changed files and bidirectional subject/body topic overlap".into(),
        ));
    }
    let stems = |c: &Commit| -> BTreeSet<String> {
        c.files
            .iter()
            .filter_map(|f| std::path::Path::new(&f.path).file_stem()?.to_str())
            .map(|s| {
                s.trim_start_matches("test_")
                    .trim_end_matches("_test")
                    .to_lowercase()
            })
            .collect()
    };
    if !shared.is_empty()
        && !stems(a).is_disjoint(&stems(b))
        && (a.files.iter().any(|f| f.is_test) || b.files.iter().any(|f| f.is_test))
    {
        return Some((
            75,
            format!(
                "Related source/test names and topic ({})",
                shared.join(", ")
            ),
        ));
    }
    None
}

pub fn group(commits: &[Commit], source: &Source, repository: &str) -> Vec<Feature> {
    let mut groups: Vec<(Vec<usize>, Vec<String>)> = Vec::new();
    for (i, commit) in commits.iter().enumerate() {
        let mut best: Option<(usize, u32, String)> = None;
        for (g, (members, _)) in groups.iter().enumerate() {
            if members.len() >= 12 {
                continue;
            }
            // Every member must relate to the new commit. Avoid transitive megaclusters
            // produced by a renderer/configuration file touched by unrelated features.
            let relations: Option<Vec<_>> = members
                .iter()
                .map(|j| relation(&commits[*j], commit))
                .collect();
            if let Some(relations) = relations {
                if let Some((weight, evidence)) = relations.into_iter().min_by_key(|r| r.0) {
                    if best.as_ref().is_none_or(|(_, w, _)| weight > *w) {
                        best = Some((g, weight, evidence));
                    }
                }
            }
        }
        if let Some((g, _, reason)) = best {
            groups[g].0.push(i);
            groups[g].1.push(reason);
        } else {
            groups.push((vec![i], Vec::new()));
        }
    }
    groups
        .into_iter()
        .map(|(members, mut evidence)| {
            let selected: Vec<_> = members.iter().map(|i| &commits[*i]).collect();
            let mut ids: Vec<_> = selected
                .iter()
                .map(|c| c.patch_id.clone().unwrap_or_else(|| c.sha.clone()))
                .collect();
            ids.sort();
            ids.dedup();
            let id = format!("fp-{}", &hash(ids.join("\n"))[..16]);
            let all_files: Vec<_> = selected.iter().flat_map(|c| &c.files).collect();
            let paths = |pred: fn(&FileChange) -> bool| -> Vec<String> {
                all_files
                    .iter()
                    .filter(|f| pred(f))
                    .map(|f| f.path.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            };
            let files = paths(|_| true);
            let test_files = paths(|f| f.is_test);
            let documentation_files = paths(|f| f.is_documentation);
            let routine = !all_files.is_empty()
                && all_files.iter().all(|f| f.is_routine || f.is_documentation);
            let mut reasons =
                vec!["Contains work not reachable from the indexed upstream refs".into()];
            let mut score = 50;
            if !test_files.is_empty() {
                score += 15;
                reasons.push("Changes test files (execution not verified)".into());
            }
            if !documentation_files.is_empty() {
                score += 5;
                reasons.push("Includes documentation".into());
            }
            let issues: BTreeSet<_> = selected
                .iter()
                .flat_map(|c| issue_numbers(&c.message))
                .collect();
            if !issues.is_empty() {
                reasons.push("References issue/PR numbers; relevance needs review".into());
            }
            if routine {
                score -= 25;
                reasons.push("Only documentation, CI, or lockfile changes".into());
            }
            let mut review_notes = vec![
                "Heuristic feature grouping; confirm scope and dependencies before adopting".into(),
            ];
            if test_files.is_empty() {
                review_notes.push(
                    "No test-file changes found; existing tests may still cover this work".into(),
                );
            }
            if selected.iter().any(|c| c.patch_truncated) {
                review_notes.push(
                    "A stored patch is truncated; fetch the exact commit for full review".into(),
                );
            }
            if all_files.iter().any(|f| f.binary) {
                review_notes.push("Contains binary changes that need separate inspection".into());
            }
            if files.len() > 15 {
                score -= 5;
                review_notes.push(
                    "Broad change across more than 15 files; consider splitting review".into(),
                );
            }
            if evidence.is_empty() {
                evidence.push(
                    "Single commit; retained even without popularity or matching forks".into(),
                );
            }
            evidence.sort();
            evidence.dedup();
            let category = if routine {
                "maintenance"
            } else if selected.iter().all(|c| c.files.iter().all(|f| f.is_test)) {
                "tests"
            } else {
                "feature_or_fix"
            };
            let issue_links = if repository.contains('/') && !repository.starts_with("local:") {
                issues
                    .iter()
                    .map(|n| format!("https://github.com/{repository}/issues/{n}"))
                    .collect()
            } else {
                Vec::new()
            };
            Feature {
                id,
                title: selected[0].subject.clone(),
                category: category.into(),
                commits: selected.iter().map(|c| c.sha.clone()).collect(),
                patch_ids: ids,
                sources: vec![source.clone()],
                files,
                test_files,
                documentation_files,
                additions: all_files.iter().map(|f| f.additions).sum(),
                deletions: all_files.iter().map(|f| f.deletions).sum(),
                score,
                reasons,
                grouping_evidence: evidence,
                review_notes,
                related_features: Vec::new(),
                context_commits: Vec::new(),
                issue_links,
                status: "new".into(),
                decision_reason: None,
            }
        })
        .collect()
}

pub fn consolidate(features: Vec<Feature>) -> Vec<Feature> {
    let mut merged: BTreeMap<String, Feature> = BTreeMap::new();
    for feature in features {
        if let Some(existing) = merged.get_mut(&feature.id) {
            for s in feature.sources {
                if !existing
                    .sources
                    .iter()
                    .any(|x| x.repository == s.repository && x.branch == s.branch)
                {
                    existing.sources.push(s);
                }
            }
            for c in feature.commits {
                if !existing.commits.contains(&c) {
                    existing.commits.push(c);
                }
            }
            for c in feature.context_commits {
                if !existing.context_commits.contains(&c) {
                    existing.context_commits.push(c);
                }
            }
        } else {
            merged.insert(feature.id.clone(), feature);
        }
    }
    let mut features: Vec<_> = merged.into_values().collect();
    // Only pairs sharing a patch or file can be related. Index those keys once
    // and tokenize each title once instead of repeating it for every pair.
    let mut by_patch: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, feature) in features.iter().enumerate() {
        for patch in &feature.patch_ids {
            by_patch.entry(patch).or_default().push(i);
        }
        for file in &feature.files {
            by_file.entry(file).or_default().push(i);
        }
    }
    let titles: Vec<_> = features.iter().map(|f| tokens(&f.title)).collect();
    let mut related = vec![Vec::new(); features.len()];
    for (i, feature) in features.iter().enumerate() {
        let shared_patch: BTreeSet<_> = feature
            .patch_ids
            .iter()
            .flat_map(|p| by_patch[p.as_str()].iter().copied())
            .filter(|j| *j > i)
            .collect();
        let shared_file: BTreeSet<_> = feature
            .files
            .iter()
            .flat_map(|p| by_file[p.as_str()].iter().copied())
            .filter(|j| *j > i)
            .collect();
        for &j in shared_patch.union(&shared_file) {
            if shared_patch.contains(&j) || titles[i].intersection(&titles[j]).take(2).count() >= 2
            {
                related[i].push(features[j].id.clone());
                related[j].push(features[i].id.clone());
            }
        }
    }
    for (feature, links) in features.iter_mut().zip(related) {
        feature.related_features.extend(links);
    }
    features.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(&b.id)));
    features
}

pub fn link_issues(features: &mut [Feature], issues: &[Issue], commits: &BTreeMap<String, Commit>) {
    for f in features.iter_mut() {
        let messages = f
            .commits
            .iter()
            .filter_map(|s| commits.get(s))
            .map(|c| c.message.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let feature_tokens = tokens(&format!("{} {} {}", f.title, f.files.join(" "), messages));
        let explicit: BTreeSet<_> = f
            .commits
            .iter()
            .filter_map(|s| commits.get(s))
            .flat_map(|c| issue_numbers(&c.message))
            .collect();
        let mut matches = Vec::new();
        for issue in issues {
            let overlap = feature_tokens.intersection(&tokens(&issue.title)).count();
            let linked = f.commits.iter().any(|sha| {
                let marker = format!("/commit/{}", &sha[..sha.len().min(7)]);
                issue.body.contains(&marker)
                    || issue.comments.iter().any(|c| c.body.contains(&marker))
            });
            if explicit.contains(&issue.number) || linked || overlap >= 2 {
                matches.push((explicit.contains(&issue.number) || linked, overlap, issue));
            }
        }
        matches.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(b.1.cmp(&a.1))
                .then(a.2.number.cmp(&b.2.number))
        });
        for (is_explicit, _, issue) in matches.iter().take(5) {
            if !f.issue_links.contains(&issue.url) {
                f.issue_links.push(issue.url.clone());
            }
            let label = if *is_explicit {
                "Explicit issue/commit reference"
            } else {
                "Possible topic match"
            };
            f.reasons.push(format!(
                "{label}: #{} {} ({})",
                issue.number, issue.title, issue.state
            ));
        }
    }
}

pub fn search_score(feature: &Feature, commits: &BTreeMap<String, Commit>, query: &str) -> usize {
    let q = tokens(query);
    let haystack = tokens(&format!(
        "{} {} {}",
        feature.title,
        feature.files.join(" "),
        feature
            .commits
            .iter()
            .filter_map(|s| commits.get(s))
            .map(|c| c.message.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    ));
    q.intersection(&haystack).count()
}
