use crate::{analyze, git::Git, model::*};
use anyhow::{Context, Result};
use std::{collections::BTreeMap, path::Path};

pub struct ScanOptions {
    pub max_commits: usize,
    pub upstream_history: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_commits: 500,
            upstream_history: 2000,
        }
    }
}

pub fn inspect(
    git: &Git,
    repository: &str,
    base_ref: &str,
    upstream_refs: BTreeMap<String, String>,
    sources: Vec<Source>,
    cache: &Path,
    options: &ScanOptions,
) -> Result<Report> {
    let base_sha = git.resolve(base_ref)?;
    anyhow::ensure!(
        !upstream_refs.is_empty(),
        "at least one upstream ref is required"
    );
    eprintln!("Indexing upstream patch history…");
    let index = git.patch_index(&upstream_refs, options.upstream_history, cache)?;
    let mut coverage = Coverage {
        upstream_commits_indexed: index.indexed,
        upstream_commits_available: index.available,
        branches_discovered: sources.len(),
        ..Default::default()
    };
    if index.indexed < index.available {
        coverage.warnings.push(format!("Patch-equivalence index covers {} of {} upstream non-merge commits; older rewritten/backported patches may appear novel. Reachability uses full available history. Use --upstream-history 0 for the full patch index.", index.indexed, index.available));
    }
    if git.run(&["rev-parse", "--is-shallow-repository"])?.trim() == "true" {
        coverage.warnings.push(
            "Repository is shallow: ancestry and patch-equivalence results may be incomplete"
                .into(),
        );
    }
    let mut report = Report {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        repository: repository.into(),
        base_ref: base_ref.into(),
        base_sha: base_sha.clone(),
        upstream_refs,
        coverage,
        branches: Vec::new(),
        features: Vec::new(),
        commits: BTreeMap::new(),
        issues: Vec::new(),
        assessments: BTreeMap::new(),
        reviews: Vec::new(),
        demand: None,
        recommendations: BTreeMap::new(),
    };
    let mut seen: BTreeMap<String, (BranchReport, Vec<Feature>)> = BTreeMap::new();
    for (i, source) in sources.iter().enumerate() {
        eprintln!(
            "[{}/{}] {}:{}",
            i + 1,
            sources.len(),
            source.repository,
            source.branch
        );
        if let Some((original, features)) = seen.get(&source.tip) {
            let mut branch = original.clone();
            branch.source = source.clone();
            branch.alias_of = Some(format!(
                "{}:{}",
                original.source.repository, original.source.branch
            ));
            report.branches.push(branch);
            for feature in features {
                let mut feature = feature.clone();
                feature.sources = vec![source.clone()];
                report.features.push(feature);
            }
            continue;
        }
        let mut branch = BranchReport {
            source: source.clone(),
            merge_base: None,
            ahead: 0,
            behind: 0,
            novel_commits: 0,
            inspected_commits: 0,
            merges_omitted: 0,
            commits_omitted: 0,
            equivalent_upstream: Vec::new(),
            alias_of: None,
            error: None,
        };
        let result = (|| -> Result<Vec<Feature>> {
            let base = git
                .run(&["merge-base", &base_sha, &source.tip])
                .context("no common ancestor with upstream")?;
            branch.merge_base = Some(base.trim().into());
            let counts = git.run(&[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{base_sha}...{}", source.tip),
                "--",
            ])?;
            let counts: Vec<usize> = counts
                .split_whitespace()
                .map(str::parse)
                .collect::<std::result::Result<_, _>>()?;
            branch.behind = counts[0];
            branch.ahead = counts[1];
            let mut range = vec![source.tip.clone(), "--not".into()];
            range.extend(report.upstream_refs.values().cloned());
            range.push("--".into());
            let mut merge_range = vec!["--merges".into()];
            merge_range.extend(range.clone());
            branch.merges_omitted = git.count(&merge_range)?;
            let mut linear_range = vec!["--no-merges".into()];
            linear_range.extend(range.clone());
            let total = git.count(&linear_range)?;
            let mut args = vec![
                "rev-list".to_string(),
                "--topo-order".into(),
                "--reverse".into(),
                "--no-merges".into(),
            ];
            if options.max_commits > 0 {
                args.push(format!("--max-count={}", options.max_commits));
            }
            args.extend(range);
            let shas = git.run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
            branch.inspected_commits = shas.lines().count();
            branch.commits_omitted = total.saturating_sub(branch.inspected_commits);
            let mut novel = Vec::new();
            for sha in shas.lines() {
                // Overlapping branch histories reuse already-decoded evidence.
                let commit = match report.commits.get(sha) {
                    Some(commit) => commit.clone(),
                    None => git.commit(sha, cache)?,
                };
                if let Some(matched) = analyze::upstream_match(&commit, &index) {
                    branch.equivalent_upstream.push(matched);
                } else {
                    novel.push(commit.clone());
                }
                report.commits.insert(sha.into(), commit);
            }
            branch.novel_commits = novel.len();
            let mut features = analyze::group(&novel, source, repository);
            for f in &mut features {
                // Earlier touching commits can contain prerequisites even if they were
                // excluded as backports or deliberately grouped into another feature.
                let first = shas
                    .lines()
                    .position(|s| f.commits.iter().any(|c| c == s))
                    .unwrap_or(0);
                for prior in shas.lines().take(first) {
                    if let Some(commit) = report.commits.get(prior) {
                        if commit.files.iter().any(|file| f.files.contains(&file.path)) {
                            f.context_commits.push(prior.into());
                        }
                    }
                }
                if !f.context_commits.is_empty() {
                    f.review_notes.push("Earlier commits touch the same files; inspect context_commits for possible prerequisites".into());
                }
            }
            Ok(features)
        })();
        match result {
            Ok(features) => {
                if branch.commits_omitted > 0 || branch.merges_omitted > 0 {
                    report.coverage.warnings.push(format!("{}:{}: {} commits omitted by limit; {} merge commits not analyzed for merge-resolution-only changes", source.repository, source.branch, branch.commits_omitted, branch.merges_omitted));
                }
                seen.insert(source.tip.clone(), (branch.clone(), features.clone()));
                report.features.extend(features);
            }
            Err(e) => {
                branch.error = Some(format!("{e:#}"));
                report.coverage.warnings.push(format!(
                    "Failed {}:{}: {e:#}",
                    source.repository, source.branch
                ));
            }
        }
        report.branches.push(branch);
    }
    report.features = analyze::consolidate(report.features);
    crate::priority::refresh(&mut report);
    Ok(report)
}

pub fn load_report(path: &Path) -> Result<Report> {
    let mut report: Report = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )?;
    anyhow::ensure!(
        report.schema_version == SCHEMA_VERSION,
        "unsupported report schema {} (expected {})",
        report.schema_version,
        SCHEMA_VERSION
    );
    let valid_sha =
        |sha: &str| [40, 64].contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit());
    anyhow::ensure!(
        valid_sha(&report.base_sha),
        "report has an invalid upstream SHA"
    );
    let mut ids = std::collections::BTreeSet::new();
    for f in &report.features {
        anyhow::ensure!(ids.insert(&f.id), "report contains duplicate feature IDs");
        anyhow::ensure!(
            !f.commits.is_empty() && !f.patch_ids.is_empty() && !f.sources.is_empty(),
            "feature {} has no source evidence",
            f.id
        );
        for sha in f.commits.iter().chain(&f.context_commits) {
            anyhow::ensure!(
                valid_sha(sha) && report.commits.get(sha).is_some_and(|c| c.sha == *sha),
                "feature {} references an invalid or missing commit",
                f.id
            );
        }
    }
    crate::priority::refresh(&mut report);
    Ok(report)
}

pub fn update_report(path: &Path, edit: impl FnOnce(&mut Report) -> Result<()>) -> Result<()> {
    use fs2::FileExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("json.lock"))?;
    lock.lock_exclusive()?;
    let mut report = load_report(path)?;
    edit(&mut report)?;
    crate::priority::refresh(&mut report);
    crate::write_json(path, &report)
}
