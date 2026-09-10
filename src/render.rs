use crate::{analyze::search_score, metrics, model::*};
use std::fmt::Write;

pub fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}
pub fn html_escape(value: &str) -> String {
    clean(value)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn md(value: &str) -> String {
    clean(value)
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('*', "\\*")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "\\|")
        .replace('\n', " ")
}

pub fn selected<'a>(report: &'a Report, query: Option<&str>, all: bool) -> Vec<&'a Feature> {
    let mut features: Vec<_> = metrics::ordered(report)
        .into_iter()
        .filter(|f| all || !["dismissed", "adopted"].contains(&f.status.as_str()))
        .filter(|f| query.is_none_or(|q| search_score(f, &report.commits, q) > 0))
        .collect();
    if let Some(q) = query {
        features.sort_by_key(|f| std::cmp::Reverse(search_score(f, &report.commits, q)));
    }
    features
}

pub fn terminal(report: &Report, query: Option<&str>, all: bool) -> String {
    let features = selected(report, query, all);
    let inventory = metrics::measure(report, &metrics::Policy::default());
    let facts = facts_by_id(&inventory);
    let mut out = format!(
        "{} · {} feature candidates\nUpstream {} · {} branches · {} fork(s) selected\n",
        clean(&report.repository),
        features.len(),
        &report.base_sha[..report.base_sha.len().min(12)],
        report.branches.len(),
        report.coverage.forks_selected
    );
    out.push_str(
        "Ordered by author recency. Independent effort bands do not measure quality or impact.\n\n",
    );
    for f in features.iter().take(20) {
        let fact = facts[f.id.as_str()];
        let title = report
            .assessments
            .get(&f.id)
            .map(|a| format!("{} [LLM]", a.title))
            .unwrap_or_else(|| f.title.clone());
        let _ = writeln!(
            out,
            "{} [{}]\n     {}\n     {}",
            f.id,
            f.status,
            clean(&title),
            metrics::description(fact)
        );
        if let Some(reason) = &f.decision_reason {
            let _ = writeln!(out, "     Decision: {}", clean(reason));
        }
        for r in report
            .reviews
            .iter()
            .filter(|r| r.review.feature_id == f.id)
        {
            let _ = writeln!(
                out,
                "     Review: {} / {} · {} findings (unverified)",
                clean(&r.agent),
                clean(r.requested_model.as_deref().unwrap_or("CLI default")),
                r.review.findings.len()
            );
        }
    }
    if features.len() > 20 {
        let _ = writeln!(
            out,
            "\n{} more candidates; use report --format markdown or --query.",
            features.len() - 20
        );
    }
    if features.is_empty() {
        out.push_str("No visible candidates. Check coverage, try --all, or broaden the scan.\n");
    }
    if !report.coverage.warnings.is_empty() {
        out.push_str("\nCoverage notes:\n");
        for warning in &report.coverage.warnings {
            let _ = writeln!(out, "  - {}", clean(warning));
        }
    }
    if let Some(snapshot) = &report.demand {
        let _ = writeln!(
            out,
            "\nDemand: {} sampled threads, fetched {}. No match is not evidence of no demand.",
            snapshot.threads.len(),
            clean(&snapshot.fetched_at)
        );
        for warning in &snapshot.warnings {
            let _ = writeln!(out, "  - {}", clean(warning));
        }
    }
    let excluded: usize = report
        .branches
        .iter()
        .filter(|b| b.alias_of.is_none())
        .map(|b| b.equivalent_upstream.len())
        .sum();
    let _ = writeln!(
        out,
        "\n{} patch-equivalent upstream matches excluded; {} upstream commits indexed.",
        excluded, report.coverage.upstream_commits_indexed
    );
    out
}

pub fn commit_url(repository: &str, sha: &str) -> Option<String> {
    repository.parse::<crate::github::RepoName>().ok()?;
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("https://github.com/{repository}/commit/{sha}"))
}

pub fn markdown(report: &Report, query: Option<&str>, all: bool) -> String {
    let mut out = format!("# Forkpicker: {}\n\nGenerated {}. Upstream `{}`.\n\n{} feature candidates across {} inspected branches. Ordered by author recency. Independent effort bands do not measure quality or impact.\n\n",md(&report.repository),md(&report.generated_at),report.base_sha,report.features.len(),report.branches.len());
    out.push_str("## Coverage\n\n");
    let _ = writeln!(out,"Scope: {}. Forks: {} discovered, {} selected, {} omitted. Branches omitted: {}. Upstream patch history: {}/{} non-merge commits. API requests: {}; cache hits: {}.\n",md(&report.coverage.scope),report.coverage.forks_discovered,report.coverage.forks_selected,report.coverage.forks_omitted,report.coverage.branches_omitted,report.coverage.upstream_commits_indexed,report.coverage.upstream_commits_available,report.coverage.api_requests,report.coverage.cache_hits);
    for warning in &report.coverage.warnings {
        let _ = writeln!(out, "- {}", md(warning));
    }
    let inventory = metrics::measure(report, &metrics::Policy::default());
    let facts = facts_by_id(&inventory);
    for f in selected(report, query, all) {
        out.push_str(&feature_markdown_with_facts(
            report,
            f,
            facts[f.id.as_str()],
        ));
    }
    out.push_str("\n## Upstream matches and branch coverage\n\n");
    for branch in &report.branches {
        let _ = writeln!(
            out,
            "- **{}:{}**: {} ahead / {} behind; {} novel; {} omitted; {} merge commits skipped{}",
            md(&branch.source.repository),
            md(&branch.source.branch),
            branch.ahead,
            branch.behind,
            branch.novel_commits,
            branch.commits_omitted,
            branch.merges_omitted,
            branch
                .alias_of
                .as_ref()
                .map(|s| format!("; alias of {}", md(s)))
                .unwrap_or_default()
        );
        if let Some(error) = &branch.error {
            let _ = writeln!(out, "  - Error: {}", md(error));
        }
        for m in &branch.equivalent_upstream {
            let _ = writeln!(out,"  - `{}` matches upstream {} via {}. This is patch equivalence, not proof of identical behavior.",m.commit,m.upstream_commits.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "),m.kind);
        }
    }
    out
}

pub fn feature_markdown(report: &Report, f: &Feature) -> String {
    let inventory = metrics::measure(report, &metrics::Policy::default());
    feature_markdown_with_facts(report, f, facts_by_id(&inventory)[f.id.as_str()])
}
fn facts_by_id(
    inventory: &metrics::Inventory,
) -> std::collections::BTreeMap<&str, &metrics::Facts> {
    inventory
        .candidates
        .iter()
        .map(|f| (f.feature_id.as_str(), f))
        .collect()
}
fn feature_markdown_with_facts(report: &Report, f: &Feature, fact: &metrics::Facts) -> String {
    let mut out = format!(
        "\n## {}\n\n`{}` · {}\n\n{}\n\n",
        md(&f.title),
        f.id,
        md(&f.status),
        md(&metrics::description(fact))
    );
    let _ = writeln!(out, "{} source repositories · {} possible context commits (dependencies unproven) · {} missing commit records · {} truncated patches.\n", fact.source_repositories, fact.possible_context_commits, fact.missing_commit_records, fact.truncated_patch_records);
    if let Some(reason) = &f.decision_reason {
        let _ = writeln!(out, "Decision: {}\n", md(reason));
    }
    for source in &f.sources {
        let _ = writeln!(
            out,
            "- Source: {} / `{}` at `{}`",
            md(&source.repository),
            md(&source.branch),
            source.tip
        );
    }
    out.push_str("\n**Grouping evidence**\n\n");
    for reason in &f.grouping_evidence {
        let _ = writeln!(out, "- {}", md(reason));
    }
    out.push_str("\n**Commits**\n\n");
    for sha in &f.commits {
        if let Some(c) = report.commits.get(sha) {
            let link = f
                .sources
                .first()
                .and_then(|s| commit_url(&s.repository, sha))
                .map(|u| format!("[{}]({u})", &sha[..12]))
                .unwrap_or_else(|| format!("`{sha}`"));
            let _ = writeln!(out, "- {link} — {} — {}", md(&c.subject), md(&c.author));
        }
    }
    out.push_str("\n**Files**\n\n");
    for file in &f.files {
        let _ = writeln!(
            out,
            "- `{}`{}",
            md(file),
            if f.test_files.contains(file) {
                " (test; not executed)"
            } else {
                ""
            }
        );
    }
    out.push_str("\n**Review questions and limits**\n\n");
    for note in &f.review_notes {
        let _ = writeln!(out, "- {}", md(note));
    }
    if !f.context_commits.is_empty() {
        let _ = writeln!(
            out,
            "- Possible prerequisites: {}",
            f.context_commits.join(", ")
        );
    }
    if !f.related_features.is_empty() {
        let _ = writeln!(
            out,
            "- Related candidates: {}",
            f.related_features.join(", ")
        );
    }
    for link in &f.issue_links {
        if safe_url(link) {
            let _ = writeln!(
                out,
                "- [Issue/PR context]({link}) — relevance and resolution require review"
            );
        }
    }
    if let Some(a) = report.assessments.get(&f.id) {
        out.push_str("\n**LLM assessment — unverified; evidence references validated only**\n\n");
        let _ = writeln!(out, "{}\n", md(&a.title));
        for claim in &a.summary {
            let _ = writeln!(
                out,
                "- {} [{}]",
                md(&claim.text),
                md(&claim.evidence.join(", "))
            );
        }
        for question in &a.review_questions {
            let _ = writeln!(out, "- Review: {}", md(question));
        }
        let _ = writeln!(out, "\nSuggested next step: {}", md(&a.suggested_next_step));
    }
    for r in report
        .reviews
        .iter()
        .filter(|r| r.review.feature_id == f.id)
    {
        let _ = writeln!(out, "\n### Code review · unverified\n\nAgent: {}. Requested model: {}. Effort: {}. Generated {}.\n", md(&r.agent),
            md(r.requested_model.as_deref().unwrap_or("CLI default (actual model not reported)")), md(r.requested_effort.as_deref().unwrap_or("CLI default")), md(&r.created_at));
        for claim in &r.review.summary {
            let _ = writeln!(
                out,
                "- {} [{}]",
                md(&claim.text),
                md(&claim.evidence.join(", "))
            );
        }
        for d in &r.review.dimensions {
            let label = match d.dimension {
                crate::review::Dimension::Quality => "Code cleanliness".into(),
                crate::review::Dimension::Style => "Fit with upstream style".into(),
                _ => enum_name(&d.dimension),
            };
            let verdict = d
                .verdict
                .as_ref()
                .map(enum_name)
                .unwrap_or_else(|| enum_name(&d.status))
                .replace('_', " ");
            let _ = writeln!(out, "- **{} · {}:** {}", label, verdict, md(&d.rationale));
            if !d.evidence.is_empty() {
                let _ = writeln!(out, "  Evidence: {}", md(&d.evidence.join(", ")));
            }
        }
        for finding in &r.review.findings {
            let _ = writeln!(out, "\n**{} · {} · {}** ({} confidence)\n\n{}\n\nLocation: {} at {}{}\n\nEvidence: {}\n\nSuggested fix: {}\n\nVerification to perform: {}\n",
                enum_name(&finding.severity), enum_name(&finding.dimension), md(&finding.title), enum_name(&finding.confidence), md(&finding.description),
                md(&finding.path), md(&finding.commit), finding.line.map(|l| format!(":{l}")).unwrap_or_default(), md(&finding.evidence.join(", ")), md(&finding.suggested_fix), md(&finding.verification));
        }
        for limitation in &r.review.limitations {
            let _ = writeln!(out, "- Limitation: {}", md(limitation));
        }
        out.push_str("\nNo findings does not establish correctness or safety. Tests and builds were not executed.\n");
    }
    out
}

fn enum_name(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn safe_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("github.com"))
}

pub fn html(report: &Report, query: Option<&str>, all: bool) -> String {
    html_with_inventory(
        report,
        query,
        all,
        &metrics::measure(report, &metrics::Policy::default()),
    )
}
pub fn html_with_inventory(
    report: &Report,
    query: Option<&str>,
    all: bool,
    inventory: &metrics::Inventory,
) -> String {
    let facts = facts_by_id(inventory);
    let features = selected(report, query, all);
    let h = html_escape;
    let forks = crate::forks::summarize(report, inventory);
    let count_status = |values: &[&str]| {
        inventory
            .candidates
            .iter()
            .filter(|f| {
                values.contains(
                    &f.integration
                        .as_ref()
                        .map(|c| c.status.as_str())
                        .unwrap_or("not-checked"),
                )
            })
            .count()
    };
    let represented: std::collections::BTreeSet<_> = report
        .branches
        .iter()
        .map(|b| b.source.repository.to_lowercase())
        .collect();
    let absent = report
        .coverage
        .forks_selected
        .saturating_sub(represented.len());
    let mut out=format!("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Forkpicker · {}</title><style>",h(&report.repository));
    out.push_str(include_str!("ui/inbox.css"));
    let _=write!(out,"</style></head><body><main><header><div class=\"brand\">FORKPICKER / MAINTAINER INBOX</div><h1>{}</h1><div class=\"meta\"><span>{} candidate patch sets</span><span>Upstream <code>{}</code></span><span>Scan {}</span></div><p class=\"muted\"><small>{} forks discovered · {} selected without branch records · <a href=\"#coverage\">Coverage details</a></small></p></header>",h(&report.repository),features.len(),h(&report.base_sha[..report.base_sha.len().min(12)]),h(&report.generated_at.chars().take(10).collect::<String>()),report.coverage.forks_discovered,absent);
    if let Some(baseline) = &inventory.baseline {
        let date = chrono::DateTime::parse_from_rfc3339(&baseline.generated_at)
            .map(|d| {
                d.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%d %H:%M UTC")
                    .to_string()
            })
            .unwrap_or_else(|_| baseline.generated_at.clone());
        let _=write!(out,"<p class=\"baseline-note\">Compared with {} · New means newly observed. <a href=\"#measurement-notes\">Comparison limits</a></p>",h(&date));
    }
    out.push_str(&crate::forks::html(&forks));
    let _=write!(out,"<div class=\"chips\"><button data-quick=\"\">All application results</button><button data-quick=\"applicable\">{} apply cleanly</button><button data-quick=\"blocked\">{} hit blockers</button><button data-quick=\"unknown\">{} could not be assessed</button></div>",count_status(&["clean","clean-three-way"]),count_status(&["conflicts","not-applicable"]),count_status(&["unknown"]));
    out.push_str(r#"<div class="toolbar"><input id="search" type="search" aria-label="Search candidates" placeholder="Search behavior, files, fork or author…"><select id="application" aria-label="Application result"><option value="">Any application result</option><option value="applicable">Applies cleanly (either method)</option><option value="clean">Direct application</option><option value="clean-three-way">Three-way application</option><option value="blocked">Hit an application blocker</option><option value="conflicts">Conflicting files</option><option value="not-applicable">Other application failure</option><option value="unknown">Could not assess</option><option value="not-checked">Not checked</option><option value="no-file-changes">No file changes</option></select><select id="observation" aria-label="Changes since previous scan">"#);
    out.push_str(if inventory.baseline.is_some(){r#"<option value="">Any observation</option><option value="new">Newly observed</option><option value="changed">Changed / regrouped patch sets</option><option value="unchanged">Previously seen</option>"#}else{r#"<option value="">No previous scan supplied</option>"#});
    out.push_str(r#"</select><select id="sort" aria-label="Sort candidates"><option value="original">Recent author date</option><option value="lines">Fewest changed lines</option><option value="files">Fewest files</option><option value="overlap">Fewest upstream-overlapping files</option><option value="conflicts">Fewest conflicting files</option></select></div><details class="filters"><summary>More filters</summary><div><label>Status <select id="status"><option value="">Active candidates</option><option value="new">Undecided</option><option value="updated">Decision needs revisit</option><option value="saved">Saved</option><option value="needs_adopter">Needs adopter</option><option value="dismissed">Dismissed</option><option value="adopted">Adopted</option></select></label><label>Scope <select id="size"><option value="">Any change size</option><option>small</option><option>medium</option><option>large</option><option>unknown</option></select></label><label><input id="group" type="checkbox" checked> Fold patch-set variants</label><label><input id="empty" type="checkbox"> Include empty patch sets</label></div></details><div class="countline"><p id="count" class="muted" aria-live="polite"></p><div><small id="local-count"></small> <button id="export-decisions">Export decisions</button></div></div><noscript>Enable JavaScript for filters and browser decisions. Candidate evidence and links remain available below.</noscript><div id="candidates">"#);
    for (index, f) in features.into_iter().enumerate() {
        let fact = facts[f.id.as_str()];
        let application = fact
            .integration
            .as_ref()
            .map(|c| c.status.as_str())
            .unwrap_or("not-checked");
        let number = |n: Option<usize>| n.map(|v| v.to_string()).unwrap_or_default();
        let lines = number((fact.change_size != "unknown").then_some(fact.changed_lines));
        let files = number((fact.file_spread != "unknown").then_some(fact.files));
        let overlap = number(fact.integration.as_ref().and_then(|c| c.overlap_max()));
        let conflicts = number(
            fact.integration
                .as_ref()
                .filter(|c| ["clean", "clean-three-way", "conflicts"].contains(&c.status.as_str()))
                .map(|c| c.conflicting_files.len()),
        );
        let family = if fact.inbox.family.is_empty() {
            &f.id
        } else {
            &fact.inbox.family
        };
        let status = match f.status.as_str() {
            "new" => "Undecided",
            "updated" => "Decision needs revisit",
            s => s,
        };
        let _=write!(out,"<article id=\"{}\" data-original=\"{index}\" data-family=\"{}\" data-status=\"{}\" data-size=\"{}\" data-lines=\"{lines}\" data-files=\"{files}\" data-patches=\"{}\" data-overlap=\"{overlap}\" data-conflicts=\"{conflicts}\" data-application=\"{}\" data-application-label=\"{}\" data-observation=\"{}\"><div class=\"meta\"><span class=\"decision-label\">{}</span>",h(&f.id),h(family),h(&f.status),h(&fact.change_size),fact.unique_patches,h(application),h(crate::integration::label(application)),h(&fact.inbox.observation),h(status));
        if inventory.baseline.is_some() && fact.inbox.observation != "unchanged" {
            let _ = write!(
                out,
                "<span class=\"badge\">{}</span>",
                h(crate::inbox::observation_label(&fact.inbox.observation))
            );
        }
        let repos: std::collections::BTreeSet<_> =
            f.sources.iter().map(|s| s.repository.as_str()).collect();
        if let Some(repo) = repos.first() {
            let _ = write!(
                out,
                "<span>{}{}</span>",
                h(repo),
                if repos.len() > 1 {
                    format!(" + {} other forks", repos.len() - 1)
                } else {
                    String::new()
                }
            );
        }
        let _ = write!(out, "</div><h2>{}</h2>", h(&f.title));
        if let Some(commit) = f.commits.first().and_then(|sha| report.commits.get(sha)) {
            if let Some((_, body)) = commit.message.split_once('\n') {
                let excerpt = body.split_whitespace().collect::<Vec<_>>().join(" ");
                if !excerpt.is_empty() {
                    let short = excerpt.chars().take(220).collect::<String>();
                    let _ = write!(
                        out,
                        "<p class=\"excerpt\">{}{} <small>— author’s description</small></p>",
                        h(&short),
                        if short.len() < excerpt.len() {
                            "…"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        let scope = if fact.missing_commit_records > 0 {
            format!(
                "{} measured lines · incomplete evidence",
                fact.changed_lines
            )
        } else if fact.binary_files > 0 {
            format!(
                "{} text lines + {} binary files",
                fact.changed_lines, fact.binary_files
            )
        } else {
            format!("{} changed lines", fact.changed_lines)
        };
        let _ = write!(
            out,
            "<div class=\"meta\"><span>{}</span><span>{} files</span><span>{} {}</span></div>",
            h(&scope),
            fact.files,
            fact.unique_patches,
            if fact.unique_patches == 1 {
                "patch"
            } else {
                "patches"
            }
        );
        if let Some(check) = &fact.integration {
            let class = if ["clean", "clean-three-way"].contains(&check.status.as_str()) {
                "clean"
            } else {
                "blocked"
            };
            let _ = write!(
                out,
                "<div class=\"integration {class}\"><strong>{}</strong>",
                h(crate::integration::label(&check.status))
            );
            if !check.conflicting_files.is_empty() {
                out.push_str("<p>Resolve conflicts in ");
                for (i, path) in check.conflicting_files.iter().take(3).enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    if let Some(url) = file_url(&report.repository, &check.target_sha, path) {
                        let _ = write!(
                            out,
                            "<a href=\"{}\" rel=\"noreferrer\">{}</a>",
                            h(&url),
                            h(path)
                        );
                    } else {
                        out.push_str(&h(path));
                    }
                }
                if check.conflicting_files.len() > 3 {
                    let _ = write!(out, " and {} more files", check.conflicting_files.len() - 3);
                }
                out.push_str(".</p><small>First failing patch; later patches untested. Check possible prerequisites.</small>");
            } else if check.status == "not-applicable" {
                let missing: std::collections::BTreeSet<_> = check
                    .notes
                    .iter()
                    .flat_map(|n| n.lines())
                    .filter_map(|l| {
                        l.trim()
                            .strip_prefix("error: ")
                            .and_then(|l| l.strip_suffix(": does not exist in index"))
                    })
                    .collect();
                if missing.is_empty() {
                    out.push_str("<p>Inspect the failed patch and its prerequisites; Git could not apply the selected series.</p>");
                } else {
                    let _ = write!(
                        out,
                        "<p>Target is missing <code>{}</code>.</p>",
                        h(&missing.into_iter().take(3).collect::<Vec<_>>().join(", "))
                    );
                }
                out.push_str("<small>Other conflicts may also exist. Missing prerequisites are possible, not proven.</small>");
            } else if check.status == "unknown" {
                let _ = write!(
                    out,
                    "<p>{}</p>",
                    h(&check
                        .notes
                        .last()
                        .map(|n| n.chars().take(240).collect::<String>())
                        .unwrap_or_default())
                );
            } else if class == "clean" {
                out.push_str("<small>Ready for code inspection. Behavior and tests remain unverified.</small>");
            }
            let _=write!(out,"<small>Checked against <code>{}</code> · <a href=\"#evidence-{}\">Evidence and affected files</a></small></div>",h(&check.target_sha[..check.target_sha.len().min(12)]),h(&f.id));
        }
        let mut limitations = Vec::new();
        if fact.possible_context_commits > 0 {
            limitations.push(format!(
                "{} possible prerequisite commits to inspect",
                fact.possible_context_commits
            ));
        }
        if fact.missing_commit_records > 0 {
            limitations.push(format!(
                "{} missing commit records",
                fact.missing_commit_records
            ));
        }
        if fact.truncated_patch_records > 0 {
            limitations.push(format!(
                "{} stored patch excerpts truncated",
                fact.truncated_patch_records
            ));
        }
        if !limitations.is_empty() {
            let _ = write!(
                out,
                "<p class=\"muted\"><small>{}</small></p>",
                h(&limitations.join(" · "))
            );
        }
        let explicit: Vec<_> = fact
            .inbox
            .context
            .iter()
            .filter(|l| l.kind == "explicit" && safe_url(&l.url))
            .collect();
        if !explicit.is_empty() {
            out.push_str("<div class=\"context\"><strong>Referenced upstream</strong><ul>");
            for link in explicit.iter().take(3) {
                let _ = write!(
                    out,
                    "<li><a href=\"{}\" rel=\"noreferrer\">{}</a><small>{}</small></li>",
                    h(&link.url),
                    h(&link.title),
                    h(&link.explanation)
                );
            }
            out.push_str("</ul></div>");
        }
        let possible: Vec<_> = fact
            .inbox
            .context
            .iter()
            .filter(|l| l.kind != "explicit" && safe_url(&l.url))
            .collect();
        if !possible.is_empty() {
            out.push_str("<details class=\"context\"><summary>Possibly related upstream context · unverified</summary><ul>");
            for link in possible.iter().take(3) {
                let _ = write!(
                    out,
                    "<li><a href=\"{}\" rel=\"noreferrer\">{}</a><small>{}</small></li>",
                    h(&link.url),
                    h(&link.title),
                    h(&link.explanation)
                );
            }
            out.push_str("</ul></details>");
        }
        let mut seen = std::collections::BTreeSet::new();
        let commits: Vec<_> = fact
            .integration
            .as_ref()
            .filter(|c| !c.ordered_commits.is_empty())
            .map(|c| c.ordered_commits.as_slice())
            .unwrap_or(&f.commits)
            .iter()
            .filter(|sha| {
                seen.insert(
                    report
                        .commits
                        .get(*sha)
                        .and_then(|c| c.patch_id.as_ref())
                        .unwrap_or(sha),
                )
            })
            .collect();
        let _ = write!(
            out,
            "<div class=\"actions\"><details><summary>Inspect {} {}</summary><ul>",
            commits.len(),
            if commits.len() == 1 {
                "patch"
            } else {
                "patches"
            }
        );
        for sha in commits {
            if let Some(url) = f
                .sources
                .first()
                .and_then(|s| commit_url(&s.repository, sha))
            {
                let title = report
                    .commits
                    .get(sha)
                    .map(|c| c.subject.as_str())
                    .unwrap_or(sha);
                let _ = write!(
                    out,
                    "<li><a href=\"{}\" rel=\"noreferrer\">{}</a> <code>{}</code></li>",
                    h(&url),
                    h(title),
                    h(&sha[..sha.len().min(12)])
                );
            }
        }
        out.push_str("</ul></details><button data-action=\"save\">Save</button><button data-action=\"dismiss\">Dismiss…</button><button data-action=\"review\">Prepare review…</button></div>");
        if let Some(reason) = &f.decision_reason {
            let _ = write!(out, "<p class=\"muted\">Decision: {}</p>", h(reason));
        }
        let review_count = report
            .reviews
            .iter()
            .filter(|r| r.review.feature_id == f.id)
            .count();
        if review_count > 0 || report.assessments.contains_key(&f.id) {
            let _=write!(out,"<p><a href=\"#evidence-{}\">Model assessment / review available · unverified</a></p>",h(&f.id));
        }
        let _=write!(out,"<details class=\"more\" id=\"evidence-{}\"><summary>Sources, measurements and review evidence</summary><p>{}</p>",h(&f.id),h(&metrics::description(fact)));
        if let Some(check) = &fact.integration {
            out.push_str(
                "<details><summary>Application diagnostics and changed paths</summary><pre>",
            );
            out.push_str(&h(&serde_json::to_string_pretty(check).unwrap_or_default()));
            out.push_str("</pre></details>");
        }
        out.push_str("<pre>");
        out.push_str(&h(&feature_markdown_with_facts(report, f, fact)));
        out.push_str("</pre></details></article>");
    }
    out.push_str("</div><p id=\"no-results\" class=\"empty\" hidden>No candidates match these filters. Try another application result or include empty patch sets.</p></section><section class=\"diagnostics\" id=\"coverage\"><details><summary>Scan coverage and diagnostics</summary><pre>");
    out.push_str(&h(
        &serde_json::to_string_pretty(&report.coverage).unwrap_or_default()
    ));
    out.push_str("</pre></details><details id=\"measurement-notes\"><summary>Measurement definitions and comparison limits</summary><p>Patch-set groups share actual patch identities; every member is contained in an anchor set. Grouping does not establish identical behavior. Default order is author recency, not importance. Empty patch sets are hidden initially.</p><pre>");
    out.push_str(&h(
        &serde_json::to_string_pretty(&inventory.policy).unwrap_or_default()
    ));
    out.push_str("</pre><ul>");
    for definition in inventory
        .definitions
        .iter()
        .filter(|d| !d.to_lowercase().contains("vocabulary overlap"))
    {
        let _ = write!(out, "<li>{}</li>", h(definition));
    }
    if let Some(b) = &inventory.baseline {
        let _ = write!(out, "<li>{}</li>", h(&b.note));
    }
    for warning in &inventory.warnings {
        let _ = write!(out, "<li>{}</li>", h(warning));
    }
    out.push_str("</ul></details></section></main><dialog id=\"action-dialog\"><div class=\"dialoghead\"><h2 id=\"dialog-title\"></h2><button id=\"dialog-close\" aria-label=\"Close dialog\">Close</button></div><div id=\"dialog-body\"></div></dialog><datalist id=\"agents\"><option>codex</option><option>claude</option><option>grok</option><option>muse</option><option>opencode</option><option>pi</option></datalist><div id=\"toast\" class=\"toast\" role=\"status\" hidden></div><script id=\"inbox-config\" type=\"application/json\">");
    let config = serde_json::json!({"include_inactive":all,"repository":report.repository,"report_path":inventory.report_path,"decision_state_dir":inventory.decision_state_dir,"forks":forks});
    out.push_str(
        &serde_json::to_string(&config)
            .unwrap_or_default()
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026"),
    );
    out.push_str("</script><script>");
    out.push_str(include_str!("ui/inbox.js"));
    out.push_str("</script></body></html>");
    out
}

fn file_url(repository: &str, sha: &str, path: &str) -> Option<String> {
    repository.parse::<crate::github::RepoName>().ok()?;
    if !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut url =
        reqwest::Url::parse(&format!("https://github.com/{repository}/blob/{sha}/")).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(path.split('/'));
    Some(url.to_string())
}
