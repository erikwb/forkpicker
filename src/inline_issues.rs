//! Issue matching during full-diff inspection, with exact catalog and group evidence.
use crate::{classify, discovery::Discovery, model::Claim};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    pub issue_url: String,
    pub relation: String,
    pub reason: Claim,
}

pub fn try_attach(pack: &mut Value, catalog: &Value, max_bytes: usize) -> Result<bool> {
    let baseline = crate::json_size(pack)?;
    let mut inline = pack.clone();
    attach(&mut inline, catalog);
    let size = crate::json_size(&inline)?;
    if size <= max_bytes && size.saturating_sub(baseline) <= max_bytes / 8 {
        *pack = inline;
        return Ok(true);
    }
    Ok(false)
}

pub fn attach(pack: &mut Value, catalog: &Value) {
    let full = catalog["threads"].as_array().unwrap();
    let urls: Vec<_> = full.iter().map(|t| t["url"].clone()).collect();
    let mut threads = pack["issue_context"]["threads"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    threads.retain(|t| !urls.contains(&t["url"]));
    threads.extend(full.iter().cloned());
    let context = json!({"threads":threads,"cache_fingerprint":catalog["cache_fingerprint"],"fetched_at":catalog["fetched_at"]});
    crate::issue_context::attach(pack, context);
    pack["inline_issues"] =
        json!({"cache_fingerprint":catalog["cache_fingerprint"],"issue_urls":urls});
    pack["issue_matching_instructions"] = json!(include_str!("prompts/inline-issues.md"));
    let issue_url = if urls.is_empty() {
        json!({"type":"string"})
    } else {
        json!({"type":"string","enum":urls})
    };
    let matches = json!({"type":"array","maxItems":urls.len(),"items":classify::object(json!({"issue_url":issue_url,"relation":{"type":"string","enum":["likely_addresses","partially_addresses","related"]},"reason":{"$ref":"#/$defs/claim"}}))});
    let group = &mut pack["response_schema"]["properties"]["groups"]["items"];
    group["properties"]["issue_matches"] = matches;
    group["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("issue_matches"));
}

pub fn validate_group(group: &classify::Group, pack: &Value) -> Result<()> {
    let Some(catalog) = pack.get("inline_issues").filter(|v| !v.is_null()) else {
        ensure!(
            group.issue_matches.is_none(),
            "issue matches require a supplied inline catalog"
        );
        return Ok(());
    };
    let matches = group
        .issue_matches
        .as_ref()
        .context("missing inline issue assessment")?;
    let urls = catalog["issue_urls"]
        .as_array()
        .context("inline issue URLs missing")?;
    let own: BTreeSet<_> = pack["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            group
                .members
                .iter()
                .any(|m| c["candidate_id"] == m.candidate_id)
        })
        .flat_map(|c| c["evidence_ids"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    let mut seen = BTreeSet::new();
    for m in matches {
        ensure!(
            urls.contains(&json!(m.issue_url)) && seen.insert(&m.issue_url),
            "unknown or duplicate inline issue"
        );
        let issue = pack["issue_context"]["threads"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["url"] == m.issue_url)
            .context("inline issue body missing")?;
        ensure!(
            issue["kind"] == "issue"
                && issue["state"] == "open"
                && issue["body_truncated"] == false
                && crate::issue_context::project_thread_url(
                    &m.issue_url,
                    pack["repository"].as_str().unwrap_or("")
                ),
            "inline match must use a complete open project issue"
        );
        ensure!(
            ["likely_addresses", "partially_addresses", "related"].contains(&m.relation.as_str()),
            "invalid inline issue relationship"
        );
        let request = format!("request:{}", m.issue_url);
        ensure!(
            !m.reason.text.trim().is_empty() && m.reason.text.len() <= 1600,
            "invalid inline issue rationale"
        );
        ensure!(
            m.reason.evidence.iter().any(|e| e == &request)
                && m.reason.evidence.iter().any(|e| own.contains(e.as_str()))
                && m.reason
                    .evidence
                    .iter()
                    .all(|e| e == &request || own.contains(e.as_str())),
            "inline match must cite its issue and its own group's code only"
        );
    }
    Ok(())
}

pub fn matches(group: &classify::Group, context: &Value) -> Vec<crate::issue_review::Match> {
    group
        .issue_matches
        .iter()
        .flatten()
        .filter_map(|m| {
            let issue = context["issue_context"]["threads"]
                .as_array()?
                .iter()
                .find(|t| t["url"] == m.issue_url)?;
            Some(crate::issue_review::Match {
                issue_url: m.issue_url.clone(),
                title: issue["title"].as_str()?.into(),
                relation: m.relation.clone(),
                reason: m.reason.clone(),
            })
        })
        .collect()
}

pub fn covered(group: &classify::Group, context: &Value, fingerprint: &str) -> bool {
    group.issue_matches.is_some() && context["inline_issues"]["cache_fingerprint"] == fingerprint
}

pub fn needs_review(d: &Discovery, fingerprint: &str) -> bool {
    d.inspections.iter().any(|r| {
        let Ok(response) = serde_json::from_value::<classify::Response>(r.value.clone()) else {
            return true;
        };
        response.groups.iter().any(|g| {
            !covered(g, &r.context, fingerprint)
                && !g.members.iter().all(|m| {
                    response.usefulness.iter().any(|u| {
                        u.candidate_id == m.candidate_id
                            && u.verdict == classify::UsefulnessVerdict::NotUseful
                    })
                })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Value, Value, classify::Group) {
        let cards = vec![
            classify::Card {
                candidate_id: "a".into(),
                title: "Change rendering".into(),
                commits: vec![],
                evidence_ids: vec!["commit:a".into()],
                omitted_commits: 0,
            },
            classify::Card {
                candidate_id: "b".into(),
                title: "Other change".into(),
                commits: vec![],
                evidence_ids: vec!["commit:b".into()],
                omitted_commits: 0,
            },
        ];
        let pack = json!({"repository":"o/r","candidates":cards,"response_schema":classify::schema(&cards,"selection")});
        let url = "https://github.com/o/r/issues/1";
        let catalog = json!({"cache_fingerprint":"raw","threads":[{"url":url,"evidence_id":format!("request:{url}"),"title":"Dim output","body":"Observed dimness","kind":"issue","state":"open","body_truncated":false}]});
        let group=serde_json::from_value(json!({"name":"Rendering","summary":{"text":"Rendering changes","evidence":["commit:a"]},"members":[{"candidate_id":"a","role":"implementation"}],"issue_matches":[{"issue_url":url,"relation":"likely_addresses","reason":{"text":"Raises output level","evidence":[format!("request:{url}"),"commit:a"]}}]})).unwrap();
        (pack, catalog, group)
    }
    #[test]
    fn inline_issues_require_catalog_and_own_group_evidence() {
        let (mut pack, catalog, group) = fixture();
        assert!(validate_group(&group, &pack).is_err());
        assert!(try_attach(&mut pack, &catalog, 256000).unwrap());
        validate_group(&group, &pack).unwrap();
        let mut bad = group.clone();
        bad.issue_matches.as_mut().unwrap()[0].reason.evidence[1] = "commit:b".into();
        assert!(validate_group(&bad, &pack).is_err());
        let mut bad = group.clone();
        bad.issue_matches = None;
        assert!(validate_group(&bad, &pack).is_err());
        let mut empty = group.clone();
        empty.issue_matches = Some(vec![]);
        validate_group(&empty, &pack).unwrap();
        for field in ["state", "kind", "body_truncated"] {
            let mut bad = pack.clone();
            bad["issue_context"]["threads"][0][field] = json!(if field == "kind" {
                "discussion"
            } else {
                "closed"
            });
            assert!(validate_group(&group, &bad).is_err());
        }
        let mut bad = group.clone();
        bad.issue_matches.as_mut().unwrap()[0].issue_url =
            "https://github.com/other/repo/issues/1".into();
        assert!(validate_group(&bad, &pack).is_err());
        let mut bad = group.clone();
        bad.issue_matches
            .as_mut()
            .unwrap()
            .push(group.issue_matches.as_ref().unwrap()[0].clone());
        assert!(validate_group(&bad, &pack).is_err());
        assert!(covered(&group, &pack, "raw"));
        assert!(!covered(&group, &pack, "different snapshot"));
    }
    #[test]
    fn oversized_inline_context_falls_back_without_cutting_diffs_or_bodies() {
        let (mut pack, mut catalog, _) = fixture();
        pack["candidates"][0]["commits"] = json!([{"patch":"COMPLETE DIFF"}]);
        catalog["threads"][0]["body"] = json!("complete body ".repeat(400));
        let original = pack.clone();
        assert!(!try_attach(&mut pack, &catalog, 8000).unwrap());
        assert_eq!(pack, original);
        assert!(try_attach(&mut pack, &catalog, 256000).unwrap());
        assert_eq!(pack["candidates"], original["candidates"]);
        assert_eq!(
            pack["issue_context"]["threads"][0]["body"],
            catalog["threads"][0]["body"]
        );
    }
    #[test]
    fn small_catalog_uses_only_open_issues_and_has_size_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("issues.json");
        let issue = json!({"number":1,"url":"https://github.com/o/r/issues/1","title":"Dim","body":"Context","kind":"issue","state":"open","is_pull_request":false,"match_kind":"catalog"});
        let mut closed = issue.clone();
        closed["url"] = json!("https://github.com/o/r/issues/2");
        closed["state"] = json!("closed");
        let mut discussion = issue.clone();
        discussion["url"] = json!("https://github.com/o/r/discussions/3");
        discussion["kind"] = json!("discussion");
        let mut foreign = issue.clone();
        foreign["url"] = json!("https://github.com/other/project/issues/1");
        let snapshot = json!({"fetched_at":"now","query":null,"priority_labels":[],"warnings":[],"threads":[issue,closed,discussion,foreign]});
        let load = |s: &Value| {
            crate::write_json(&path, s).unwrap();
            crate::issue_context::Cache::load(&path, "o/r")
                .unwrap()
                .small_open_catalog()
        };
        assert_eq!(
            load(&snapshot).unwrap()["threads"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut large = snapshot.clone();
        large["threads"][0]["body"] = json!("x".repeat(12000));
        assert!(load(&large).is_none());
        let mut partial = snapshot.clone();
        partial["threads"][0]["body_truncated"] = json!(true);
        assert!(load(&partial).is_none());
        let mut many = snapshot.clone();
        many["threads"] = json!((1..=9)
            .map(|n| {
                let mut i = snapshot["threads"][0].clone();
                i["url"] = json!(format!("https://github.com/o/r/issues/{n}"));
                i
            })
            .collect::<Vec<_>>());
        assert!(load(&many).is_none());
    }
}
