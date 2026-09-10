//! Offline feature inbox with hash routes; no server or model needed to navigate.
use crate::{
    classify,
    discovery::{Discovery, Mapping},
    model::Report,
    render,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub fn html(d: &Discovery, report: &Report) -> String {
    let by_id: BTreeMap<_, _> = report.features.iter().map(|f| (f.id.as_str(), f)).collect();
    let index: BTreeMap<_, _> = d
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.candidate_id.as_str(), i))
        .collect();
    let screened = d.screened();
    let inspected = d.inspected();
    let mut evidence = BTreeMap::<String, Value>::new();
    let issue_review = d
        .issue_review
        .as_ref()
        .filter(|r| r.inspection_fingerprint == crate::issue_review::fingerprint(d));
    let mut groups = Vec::new();
    let mut rejected = BTreeSet::new();
    for mapping in &d.mappings {
        for t in mapping.context["screening_demand"]["threads"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(id) = t["evidence_id"].as_str() {
                evidence.insert(id.into(),json!({"title":t["title"],"content":t["body"],"truncated":t["body_truncated"],"url":t["url"],"kind":"issue","state":t["state"],"comments":t["comments"],"comments_omitted":t["comments_omitted"]}));
            }
        }
    }
    for r in &d.inspections {
        let Ok(response) = serde_json::from_value::<classify::Response>(r.value.clone()) else {
            continue;
        };
        rejected.extend(
            response
                .usefulness
                .iter()
                .filter(|u| u.verdict == classify::UsefulnessVerdict::NotUseful)
                .map(|u| u.candidate_id.clone()),
        );
        for c in r.context["candidates"].as_array().into_iter().flatten() {
            for commit in c["commits"].as_array().into_iter().flatten() {
                if let Some(sha) = commit["sha"].as_str() {
                    evidence.insert(format!("commit:{sha}"),json!({"title":commit["subject"],"content":commit["patch"],"truncated":commit["patch_truncated"],"kind":"patch"}));
                }
            }
        }
        for s in r.context["source_context"].as_array().into_iter().flatten() {
            if let Some(id) = s["evidence_id"].as_str() {
                evidence.insert(id.into(),json!({"title":s["path"],"content":s["content"],"truncated":s["truncated"],"kind":"upstream"}));
            }
        }
        for t in r.context["issue_context"]["threads"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(id) = t["evidence_id"].as_str() {
                let item = json!({"title":t["title"],"content":t["body"],"truncated":t["body_truncated"],"url":t["url"],"kind":"issue"});
                if r.context["inline_issues"]["issue_urls"]
                    .as_array()
                    .is_some_and(|urls| urls.contains(&t["url"]))
                {
                    evidence.insert(id.into(), item);
                } else {
                    evidence.entry(id.into()).or_insert(item);
                }
            }
        }
        for (group_index, g) in response.groups.iter().enumerate() {
            let key = crate::issue_review::feature_key(&r.key, group_index);
            let inline = crate::inline_issues::matches(g, &r.context);
            let issue_matches = issue_review
                .and_then(|review| review.findings.iter().find(|f| f.feature_key == key))
                .map(|f| f.matches.as_slice())
                .unwrap_or_else(|| {
                    if issue_review.is_none_or(|review| {
                        r.context["inline_issues"]["cache_fingerprint"]
                            == review.issue_cache_fingerprint
                    }) {
                        &inline
                    } else {
                        &[]
                    }
                });
            let members: Vec<_> = g
                .members
                .iter()
                .filter_map(|m| {
                    index
                        .get(m.candidate_id.as_str())
                        .map(|i| json!({"index":i,"role":m.role}))
                })
                .collect();
            let judgments: Vec<_> = response
                .usefulness
                .iter()
                .filter(|u| g.members.iter().any(|m| m.candidate_id == u.candidate_id))
                .collect();
            let excluded = !judgments.is_empty()
                && judgments.len() == g.members.len()
                && judgments
                    .iter()
                    .all(|u| u.verdict == classify::UsefulnessVerdict::NotUseful);
            let mut claims = vec![json!({"text":g.summary.text,"evidence":g.summary.evidence})];
            claims.extend(
                judgments
                    .iter()
                    .map(|u| json!({"text":u.reason.text,"evidence":u.reason.evidence})),
            );
            let relations:Vec<_>=response.relationships.iter().filter(|rel|g.members.iter().any(|m|m.candidate_id==rel.from || m.candidate_id==rel.to)).map(|rel|json!({"kind":rel.kind,"from":index.get(rel.from.as_str()),"to":index.get(rel.to.as_str()),"text":rel.reason.text,"evidence":rel.reason.evidence})).collect();
            groups.push(json!({"issue_matches":issue_matches,"title":g.name,"summary":g.summary.text,"stage":"inspected","members":members,"excluded":excluded,"claims":claims,"relations":relations,"limitations":response.limitations,"question":r.context["proposed_features"][0]["question"]}));
        }
        for u in &response.unclassified {
            if let Some(i) = index.get(u.candidate_id.as_str()) {
                groups.push(json!({"title":by_id[u.candidate_id.as_str()].title,"summary":u.reason,"stage":"uncertain","members":[{"index":i,"role":"unclear"}],"excluded":false,"claims":[],"relations":[],"limitations":response.limitations,"question":""}));
            }
        }
    }
    for m in &d.mappings {
        let Ok(mapping) = serde_json::from_value::<Mapping>(m.value.clone()) else {
            continue;
        };
        for g in mapping.groups {
            if g.candidate_ids
                .iter()
                .all(|id| inspected.contains(id.as_str()))
            {
                continue;
            }
            groups.push(json!({"title":g.headline,"summary":g.benefit,"stage":"mapped","members":g.candidate_ids.iter().filter_map(|id|index.get(id.as_str()).map(|i|json!({"index":i,"role":"proposed"}))).collect::<Vec<_>>(),"excluded":false,"claims":g.demand.as_ref().filter(|d| !d.reason.evidence.is_empty()).map(|d|vec![json!({"text":d.reason.text,"evidence":d.reason.evidence})]).unwrap_or_default(),"relations":[],"limitations":[],"question":g.question}));
        }
    }
    let mut assembled = BTreeMap::new();
    for check in &d.assembled_checks {
        let ids: BTreeSet<_> = check.candidate_ids.iter().map(String::as_str).collect();
        assembled.entry(ids).or_insert(check);
    }
    for group in &mut groups {
        let ids: BTreeSet<_> = group["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["index"].as_u64())
            .map(|i| d.entries[i as usize].candidate_id.as_str())
            .collect();
        if let Some(check) = assembled.get(&ids) {
            group["assembled"] = json!(check);
        }
    }
    if let Some(review) = issue_review {
        for call in review.calls.iter().filter(|c| c.stage == "confirm-issues") {
            for issue in call.input["issues"].as_array().into_iter().flatten() {
                if let Some(id) = issue["evidence_id"].as_str() {
                    evidence.insert(id.into(),json!({"title":issue["title"],"content":issue["body"],"truncated":issue["body_truncated"],"url":issue["url"],"kind":"issue","state":issue["state"]}));
                }
            }
        }
    }
    let entries: Vec<_> =
        d.entries
            .iter()
            .map(|entry| {
                let feature = by_id[entry.candidate_id.as_str()];
                let mut seen = BTreeSet::new();
                let commits: Vec<_> = feature.commits.iter()
            .filter_map(|sha| report.commits.get(sha))
            .filter(|commit| seen.insert(&commit.sha))
            .map(|commit| json!({
                "sha": commit.sha,
                "title": commit.subject,
                "date": commit.date,
                "additions": commit.files.iter().map(|file| file.additions).sum::<usize>(),
                "deletions": commit.files.iter().map(|file| file.deletions).sum::<usize>(),
                "evidence": format!("commit:{}", commit.sha)
            }))
            .collect();
                let stage = if inspected.contains(feature.id.as_str()) {
                    "inspected"
                } else if screened.contains(feature.id.as_str()) {
                    "screened"
                } else {
                    "unseen"
                };
                let issues: Vec<_> = feature
                    .issue_links
                    .iter()
                    .filter(|url| crate::issue_context::project_thread_url(url, &report.repository))
                    .collect();
                json!({
                    "activity": entry.activity,
                    "title": feature.title,
                    "forks": entry.forks,
                    "paths": feature.files,
                    "additions": feature.additions,
                    "deletions": feature.deletions,
                    "commits": commits,
                    "issues": issues,
                    "stage": stage,
                    "aliases": entry.aliases.len(),
                    "integration": entry.integration,
                    "excluded": rejected.contains(&entry.candidate_id)
                })
            })
            .collect();
    let forks: Vec<_> = d
        .entries
        .iter()
        .flat_map(|e| e.forks.iter())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let data = json!({"application_pool":d.application_pool,"window":d.window,"repository":report.repository,"base":report.base_sha,"entries":entries,"groups":groups,"forks":forks,"evidence":evidence,"screened":screened.len(),"inspected":inspected.len(),"calls":d.run.attempted_calls,"reused":d.run.reused_calls,"bytes":d.new_input_bytes,"output_bytes":d.new_output_bytes,"seconds":d.elapsed_ms as f64/1000.0,"errors":d.run.errors,"stop":d.stop_reason});
    let payload = serde_json::to_string(&data)
        .unwrap()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    format!("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{} · Fork features</title><style>{}</style></head><body><header><a class=\"brand\" href=\"#features\">Forkpicker</a><nav aria-label=\"Main\"><a href=\"#features\">Features</a><a href=\"#forks\">Forks</a><a href=\"#catalog\">All changes</a></nav></header><main id=\"app\"></main><noscript>This offline inbox needs JavaScript enabled for navigation.</noscript><script id=\"data\" type=\"application/json\">{payload}</script><script>{}</script></body></html>",render::html_escape(&report.repository),include_str!("ui/discovery.css"),include_str!("ui/discovery.js"))
}
