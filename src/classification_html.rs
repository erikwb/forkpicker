//! Maintainer-facing classification output, with scan-backed commit links and counts.
use crate::{
    classify,
    model::{Feature, Report},
    related, render, shortlist,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
};

struct View<'a> {
    report: &'a Report,
    features: BTreeMap<&'a str, &'a Feature>,
}
impl View<'_> {
    fn text(&self, value: &str) -> String {
        let mut remaining = value;
        let mut text = String::new();
        while let Some(start) = remaining.find("fp-") {
            text.push_str(&remaining[..start]);
            let tail = &remaining[start + 3..];
            let len = tail.bytes().take_while(u8::is_ascii_hexdigit).count();
            if len == 0 {
                text.push_str("fp-");
            } else {
                let id = &remaining[start..start + 3 + len];
                text.push_str(
                    self.features
                        .get(id)
                        .map(|f| f.title.as_str())
                        .unwrap_or("this change"),
                );
            }
            remaining = &tail[len..];
        }
        text.push_str(remaining);
        render::html_escape(&text)
    }
    fn commit(&self, repo: &str, sha: &str) -> String {
        let Some(c) = self.report.commits.get(sha) else {
            return String::new();
        };
        let label = self.text(&c.subject);
        let title = render::commit_url(repo, sha)
            .map(|url| format!("<a href=\"{}\">{label}</a>", render::html_escape(&url)))
            .unwrap_or(label);
        let additions: usize = c.files.iter().map(|f| f.additions).sum();
        let deletions: usize = c.files.iter().map(|f| f.deletions).sum();
        let binary = if c.files.iter().any(|f| f.binary) {
            " · includes binary changes"
        } else {
            ""
        };
        format!("<li class=\"commit\">{title} <span class=\"diff\" aria-label=\"{additions} lines added, {deletions} lines removed\"><span class=\"added\">+{additions}</span> <span class=\"removed\">−{deletions}</span></span><small>{binary}</small></li>")
    }
    fn candidate(&self, repo: &str, id: &str, role: Option<&classify::Role>) -> String {
        let Some(f) = self.features.get(id) else {
            return "<p>Change details unavailable in this scan.</p>".into();
        };
        let role = match role {
            Some(classify::Role::Alternative) => "Alternative version",
            Some(classify::Role::Supporting) => "Supporting change",
            Some(classify::Role::Tests) => "Tests",
            Some(classify::Role::Documentation) => "Documentation",
            Some(classify::Role::Unclear) => "Role unclear",
            _ => "",
        };
        let mut out = String::from("<div class=\"change\">");
        if !role.is_empty() {
            let _ = write!(out, "<small class=\"role\">{role}</small>");
        }
        if f.commits.len() > 1 {
            let _ = write!(out, "<p>{}</p>", self.text(&f.title));
        }
        out.push_str("<ul class=\"commits\">");
        for sha in &f.commits {
            out.push_str(&self.commit(repo, sha));
        }
        out.push_str("</ul></div>");
        out
    }
    fn change_link(&self, repo: &str, id: &str) -> String {
        let Some(f) = self.features.get(id) else {
            return "this change".into();
        };
        let title = self.text(&f.title);
        f.commits
            .first()
            .and_then(|sha| render::commit_url(repo, sha))
            .map(|url| format!("<a href=\"{}\">{title}</a>", render::html_escape(&url)))
            .unwrap_or(title)
    }
}

pub fn html(
    run: &classify::Run,
    report: &Report,
    exploration: &[related::Exploration],
    demand: Option<&shortlist::Shortlist>,
) -> String {
    let v = View {
        report,
        features: report.features.iter().map(|f| (f.id.as_str(), f)).collect(),
    };
    let mut out = format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{} · Fork changes</title><style>{}</style><main id=\"forks\"><h1>{} · Fork changes</h1>", v.text(&report.repository), include_str!("ui/classification.css"), v.text(&report.repository));
    let rejected_count = run
        .forks
        .iter()
        .flat_map(|f| {
            f.classification
                .as_ref()
                .map(|r| vec![r])
                .unwrap_or_else(|| f.batches.iter().flatten().map(|b| &b.response).collect())
        })
        .flat_map(|r| &r.usefulness)
        .filter(|u| u.verdict == classify::UsefulnessVerdict::NotUseful)
        .map(|u| u.candidate_id.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    out.push_str("<p class=\"muted\" id=\"visible-count\"></p>");
    if rejected_count > 0 {
        let _ = write!(out, "<label class=\"muted\"><input type=\"checkbox\" id=\"show-excluded\"> Show excluded changes ({rejected_count})</label>");
    }
    let mut catalogs = String::new();
    for (index, fork) in run.forks.iter().enumerate() {
        let n = index + 1;
        let ex = exploration
            .iter()
            .find(|e| e.repository.eq_ignore_ascii_case(&fork.repository));
        let responses: Vec<_> = fork
            .classification
            .as_ref()
            .map(|r| vec![r])
            .unwrap_or_else(|| fork.batches.iter().flatten().map(|b| &b.response).collect());
        let scope: BTreeSet<_> = fork
            .candidate_ids
            .iter()
            .map(String::as_str)
            .chain(responses.iter().flat_map(|r| {
                r.groups
                    .iter()
                    .flat_map(|g| g.members.iter().map(|m| m.candidate_id.as_str()))
                    .chain(r.unclassified.iter().map(|u| u.candidate_id.as_str()))
            }))
            .collect();
        let not_useful: BTreeMap<_, _> = responses
            .iter()
            .flat_map(|r| &r.usefulness)
            .filter(|u| u.verdict == classify::UsefulnessVerdict::NotUseful)
            .map(|u| (u.candidate_id.as_str(), u.reason.text.as_str()))
            .collect();
        let fork_class = if fork.classification.is_some()
            && !scope.is_empty()
            && scope.iter().all(|id| not_useful.contains_key(id))
        {
            " llm-excluded"
        } else {
            ""
        };
        let excluded: BTreeSet<_> = ex
            .map(|e| e.seed_ids.iter().map(String::as_str).collect())
            .unwrap_or_else(|| scope.clone());
        let mut catalog: Vec<_> = report
            .features
            .iter()
            .filter(|f| {
                !f.patch_ids.is_empty()
                    && !f.files.is_empty()
                    && !excluded.contains(f.id.as_str())
                    && f.sources
                        .iter()
                        .any(|s| s.repository.eq_ignore_ascii_case(&fork.repository))
            })
            .collect();
        catalog.sort_by(|a, b| a.title.cmp(&b.title).then(a.id.cmp(&b.id)));
        let headline = fork
            .classification
            .as_ref()
            .and_then(|r| r.headline.as_deref())
            .unwrap_or("Patch classification");
        let _ = write!(
            out,
            "<article class=\"fork{fork_class}\" id=\"fork-{n}\"><h2>{}</h2>",
            v.text(headline)
        );
        if fork.repository.parse::<crate::github::RepoName>().is_ok() {
            let repo = render::html_escape(&fork.repository);
            let owner = render::html_escape(fork.repository.split('/').next().unwrap_or(""));
            let _ = write!(out,"<p class=\"repository\"><a href=\"https://github.com/{repo}\"><img class=\"avatar\" src=\"https://github.com/{owner}.png?size=48\" loading=\"lazy\" referrerpolicy=\"no-referrer\" alt=\"\">{repo}</a></p>");
        } else {
            let _ = write!(out, "<p>{}</p>", v.text(&fork.repository));
        }
        if !catalog.is_empty() {
            let _ = write!(
                out,
                "<p class=\"scope\"><a href=\"#catalog-{n}\">{} other changes in this fork</a>",
                catalog.len()
            );
            if let Some(e) = ex {
                let _ = write!(out, " · {} additional changes inspected", e.requested.len());
            }
            out.push_str("</p>");
            let _ = write!(catalogs,"<section class=\"catalog\" id=\"catalog-{n}\" hidden><a href=\"#fork-{n}\">← Back to forks</a><h1>{} · Other changes</h1><p>{} changes recorded in this scan</p><label>Filter changes <input type=\"search\" class=\"catalog-search\"></label><div class=\"catalog-list\">",v.text(&fork.repository),catalog.len());
            for f in catalog {
                let mut pr_links = String::new();
                if let Some(prs) = run
                    .pr_filter
                    .as_ref()
                    .and_then(|p| p.excluded_candidates.get(&f.id))
                {
                    if let Ok(repo) = report.repository.parse::<crate::github::RepoName>() {
                        pr_links.push_str("<small>Already in ");
                        for (i, number) in prs.iter().enumerate() {
                            if i > 0 {
                                pr_links.push_str(", ");
                            }
                            let _ = write!(
                                pr_links,
                                "<a href=\"https://github.com/{}/pull/{number}\">PR #{number}</a>",
                                render::html_escape(&repo.0)
                            );
                        }
                        pr_links.push_str("</small>");
                    }
                }
                let hidden_class = if not_useful.contains_key(f.id.as_str()) {
                    " llm-excluded"
                } else {
                    ""
                };
                let inspected = if scope.contains(f.id.as_str()) {
                    "<small>Included in this analysis</small>"
                } else {
                    ""
                };
                let _ = write!(
                    catalogs,
                    "<article class=\"catalog-item{hidden_class}\">{inspected}{pr_links}{}</article>",
                    v.candidate(&fork.repository, &f.id, None)
                );
            }
            catalogs.push_str("</div></section>");
        }
        let mut demand_links = BTreeMap::new();
        if let Some(d) = demand {
            for r in &d.requests {
                if r.linked_candidates
                    .iter()
                    .any(|id| scope.contains(id.as_str()))
                {
                    demand_links.insert(r.url.clone(), r.title.clone());
                }
            }
        }
        let mut related_links = BTreeMap::new();
        for id in &scope {
            if let Some(f) = v.features.get(id) {
                for url in &f.issue_links {
                    // A bare reference can also be a PR or incidental mention.
                    if let Some(issue) = report
                        .issues
                        .iter()
                        .find(|i| i.url == *url && !i.is_pull_request)
                    {
                        if !demand_links.contains_key(url) {
                            related_links.insert(url.clone(), issue.title.clone());
                        }
                    }
                }
            }
        }
        for r in &responses {
            for evidence in r
                .summary
                .evidence
                .iter()
                .chain(r.groups.iter().flat_map(|g| &g.summary.evidence))
                .chain(r.relationships.iter().flat_map(|r| &r.reason.evidence))
            {
                if let Some(url) = evidence.strip_prefix("request:") {
                    let title = ex
                        .and_then(|e| e.issue_context.as_ref())
                        .and_then(|c| c["threads"].as_array())
                        .and_then(|ts| ts.iter().find(|t| t["url"] == url))
                        .and_then(|t| t["title"].as_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            format!("Issue #{}", url.rsplit('/').next().unwrap_or(""))
                        });
                    if !demand_links.contains_key(url) {
                        related_links.insert(url.to_owned(), title);
                    }
                }
            }
        }
        for (label, links) in [("Demand", demand_links), ("Related issues", related_links)] {
            for (url, title) in links {
                if crate::issue_context::project_thread_url(&url, &report.repository) {
                    let _ = write!(
                        out,
                        "<p class=\"demand\">{label}: <a href=\"{}\">{}</a></p>",
                        render::html_escape(&url),
                        v.text(&title)
                    );
                }
            }
        }
        if fork.classification.is_none() {
            out.push_str("<p>Analysis incomplete.</p>");
        }
        for r in responses {
            for group in &r.groups {
                let hidden_class = if !group.members.is_empty()
                    && group
                        .members
                        .iter()
                        .all(|m| not_useful.contains_key(m.candidate_id.as_str()))
                {
                    " llm-excluded"
                } else {
                    ""
                };
                let _ = write!(
                    out,
                    "<section class=\"group patch-group{hidden_class}\"><h3>{}</h3><p>{}</p>",
                    v.text(&group.name),
                    v.text(&group.summary.text)
                );
                for member in &group.members {
                    let reason = not_useful.get(member.candidate_id.as_str());
                    if let Some(reason) = reason {
                        let _ = write!(
                            out,
                            "<div class=\"llm-excluded\"><p>Excluded by the model: {}</p>",
                            v.text(reason)
                        );
                    }
                    out.push_str(&v.candidate(
                        &fork.repository,
                        &member.candidate_id,
                        Some(&member.role),
                    ));
                    if reason.is_some() {
                        out.push_str("</div>");
                    }
                }
                out.push_str("</section>");
            }
            for u in &r.unclassified {
                let hidden_class = if not_useful.contains_key(u.candidate_id.as_str()) {
                    " llm-excluded"
                } else {
                    ""
                };
                let reason = not_useful
                    .get(u.candidate_id.as_str())
                    .copied()
                    .unwrap_or(&u.reason);
                let _ = write!(
                    out,
                    "<section class=\"group{hidden_class}\"><h3>Unclassified change</h3><p>{}</p>{}</section>",
                    v.text(reason),
                    v.candidate(&fork.repository, &u.candidate_id, None)
                );
            }
            for rel in &r.relationships {
                let hidden_class = if not_useful.contains_key(rel.from.as_str())
                    || not_useful.contains_key(rel.to.as_str())
                {
                    " llm-excluded"
                } else {
                    ""
                };
                let kind = match rel.kind {
                    classify::Relation::DependsOn => "depends on",
                    classify::Relation::Supports => "supports",
                    classify::Relation::Alternative => "is an alternative to",
                };
                let _ = write!(
                    out,
                    "<p class=\"relationship{hidden_class}\">{} {kind} {}. {}</p>",
                    v.change_link(&fork.repository, &rel.from),
                    v.change_link(&fork.repository, &rel.to),
                    v.text(&rel.reason.text)
                );
            }
        }
        out.push_str("</article>");
    }
    out.push_str("</main>");
    out.push_str(&catalogs);
    out.push_str("<script>");
    out.push_str(include_str!("ui/classification.js"));
    out.push_str("</script></html>");
    out
}
