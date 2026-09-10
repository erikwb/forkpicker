//! Feature-to-open-issue matching with catalog screening followed by full-evidence checks.
use crate::{
    classify, discovery::Discovery, hash, model::Claim, priority::DemandSnapshot, review,
    structured, write_json,
};
use anyhow::{ensure, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

#[derive(Args)]
pub struct IssueReviewArgs {
    pub report: PathBuf,
    #[arg(long)]
    pub classification: PathBuf,
    #[arg(long)]
    pub issue_cache: PathBuf,
    #[arg(long, value_enum)]
    pub agent: Option<structured::Agent>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    /// Total calls allowed in this issue-matching run, including failures.
    #[arg(long, default_value_t = 5)]
    pub limit: usize,
    #[arg(long, default_value_t = 256000)]
    pub max_bytes: usize,
    #[arg(long, default_value_t = 1000000)]
    pub total_bytes: usize,
    #[arg(long, default_value_t = 180)]
    pub timeout: u64,
    /// Retain this output's completed calls and accumulated spend.
    #[arg(long)]
    pub continue_run: bool,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(short, long, default_value = "issue-review.json")]
    pub output: PathBuf,
    #[arg(long)]
    pub html: Option<PathBuf>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Match {
    pub issue_url: String,
    pub title: String,
    pub relation: String,
    pub reason: Claim,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Finding {
    pub feature_key: String,
    pub checked_issue_urls: Vec<String>,
    pub matches: Vec<Match>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Call {
    pub key: String,
    pub stage: String,
    pub input_bytes: usize,
    pub duration_ms: u64,
    pub provider_usage: Option<Value>,
    pub input: Value,
    pub response: Value,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Review {
    pub repository: String,
    pub base_sha: String,
    pub inspection_fingerprint: String,
    pub issue_cache_fingerprint: String,
    pub generated_at: String,
    pub issues_fetched_at: String,
    pub available_open_issues: usize,
    pub screened_issue_urls: Vec<String>,
    pub findings: Vec<Finding>,
    pub agent: String,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub calls: Vec<Call>,
    pub attempted_calls: usize,
    pub input_bytes: usize,
    pub call_limit: usize,
    pub input_limit: usize,
    pub errors: Vec<String>,
    pub interruptions: Vec<String>,
    pub limitations: Vec<String>,
    pub complete: bool,
}
#[derive(Clone)]
struct Feature {
    id: String,
    key: String,
    title: String,
    summary: Value,
    cards: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Screening {
    assessments: Vec<Nomination>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Nomination {
    feature_id: String,
    issue_urls: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    assessments: Vec<Assessment>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assessment {
    feature_id: String,
    matches: Vec<ProposedMatch>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposedMatch {
    issue_url: String,
    relation: String,
    reason: Claim,
}

pub fn fingerprint(d: &Discovery) -> String {
    hash(serde_json::to_vec(&d.inspections).unwrap())
}
pub fn feature_key(record: &str, group: usize) -> String {
    format!("{record}:{group}")
}
fn features(d: &Discovery) -> Result<Vec<Feature>> {
    let mut out = Vec::new();
    for record in &d.inspections {
        let response: classify::Response = serde_json::from_value(record.value.clone())?;
        for (i, g) in response.groups.iter().enumerate() {
            if g.members.iter().all(|m| {
                response.usefulness.iter().any(|u| {
                    u.candidate_id == m.candidate_id
                        && u.verdict == classify::UsefulnessVerdict::NotUseful
                })
            }) {
                continue;
            }
            let ids: BTreeSet<_> = g.members.iter().map(|m| m.candidate_id.as_str()).collect();
            let cards: Vec<_> = record.context["candidates"]
                .as_array()
                .context("inspection cards missing")?
                .iter()
                .filter(|c| {
                    c["candidate_id"]
                        .as_str()
                        .is_some_and(|id| ids.contains(id))
                })
                .cloned()
                .collect();
            ensure!(cards.len() == ids.len(), "incomplete feature cards");
            for c in &cards {
                ensure!(c["omitted_commits"]==0 && c["commits"].as_array().is_some_and(|cs|!cs.is_empty() && cs.iter().all(|m|m["patch_truncated"]==false)),"issue matching requires complete inspected diffs; reinspect excerpt-based results first");
            }
            out.push(Feature {
                id: format!("feature-{}", out.len() + 1),
                key: feature_key(&record.key, i),
                title: g.name.clone(),
                summary: json!(g.summary),
                cards,
            });
        }
    }
    Ok(out)
}
fn array(items: Value) -> Value {
    json!({"type":"array","items":items})
}
fn enumeration(values: Vec<Value>) -> Value {
    json!({"type":"string","enum":values.iter().filter_map(Value::as_str).collect::<BTreeSet<_>>()})
}
fn schema(fs: &[Value], issues: &[Value], confirm: bool) -> Value {
    let ids = enumeration(fs.iter().map(|f| f["feature_id"].clone()).collect());
    let urls = enumeration(issues.iter().map(|i| i["url"].clone()).collect());
    let assessment = if confirm {
        let evidence: Vec<_> = fs
            .iter()
            .flat_map(|f| f["candidates"].as_array().into_iter().flatten())
            .flat_map(|c| c["evidence_ids"].as_array().into_iter().flatten())
            .cloned()
            .chain(issues.iter().map(|i| i["evidence_id"].clone()))
            .collect();
        classify::object(
            json!({"feature_id":ids,"matches":array(classify::object(json!({"issue_url":urls,"relation":{"type":"string","enum":["likely_addresses","partially_addresses","related"]},"reason":classify::object(json!({"text":{"type":"string"},"evidence":array(enumeration(evidence))}))})))}),
        )
    } else {
        classify::object(json!({"feature_id":ids,"issue_urls":array(urls)}))
    };
    classify::object(
        json!({"assessments":{"type":"array","items":assessment,"minItems":fs.len(),"maxItems":fs.len()}}),
    )
}
fn pack(fs: Vec<Value>, issues: Vec<Value>, confirm: bool) -> Value {
    let prompt = if confirm {
        include_str!("prompts/issue-confirm.md")
    } else {
        include_str!("prompts/issue-screen.md")
    };
    json!({"stage":if confirm{"confirm-issues"}else{"screen-issues"},"instructions":prompt,"response_schema":schema(&fs,&issues,confirm),"features":fs,"issues":issues})
}
fn validate(value: &Value, p: &Value) -> Result<()> {
    let fs: BTreeMap<_, _> = p["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["feature_id"].as_str().unwrap(), f))
        .collect();
    let issues: BTreeMap<_, _> = p["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["url"].as_str().unwrap(), i))
        .collect();
    let mut seen = BTreeSet::new();
    if p["stage"] == "screen-issues" {
        let r: Screening = serde_json::from_value(value.clone())?;
        for a in r.assessments {
            ensure!(
                fs.contains_key(a.feature_id.as_str()) && seen.insert(a.feature_id),
                "unknown or duplicate feature in issue screening"
            );
            let mut urls = BTreeSet::new();
            ensure!(
                a.issue_urls
                    .iter()
                    .all(|u| issues.contains_key(u.as_str()) && urls.insert(u)),
                "unknown or duplicate nominated issue"
            );
        }
    } else {
        let r: Confirmation = serde_json::from_value(value.clone())?;
        for a in r.assessments {
            let f = fs
                .get(a.feature_id.as_str())
                .context("unknown confirmed feature")?;
            ensure!(seen.insert(a.feature_id), "duplicate confirmed feature");
            let own: BTreeSet<_> = f["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|c| c["evidence_ids"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .collect();
            let mut urls = BTreeSet::new();
            for m in a.matches {
                let issue = issues
                    .get(m.issue_url.as_str())
                    .context("match cites issue outside supplied open catalog")?;
                ensure!(
                    issue["kind"] == "issue" && issue["state"] == "open",
                    "match is not an open issue"
                );
                ensure!(
                    f["issue_urls"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(m.issue_url))
                        && urls.insert(m.issue_url),
                    "issue was not nominated for this feature, or repeated"
                );
                ensure!(
                    ["likely_addresses", "partially_addresses", "related"]
                        .contains(&m.relation.as_str()),
                    "invalid issue relationship"
                );
                let request = issue["evidence_id"].as_str().unwrap();
                ensure!(
                    !m.reason.text.trim().is_empty() && m.reason.text.len() <= 1600,
                    "missing or oversized issue rationale"
                );
                ensure!(
                    m.reason.evidence.iter().any(|e| e == request)
                        && m.reason.evidence.iter().any(|e| own.contains(e.as_str()))
                        && m.reason
                            .evidence
                            .iter()
                            .all(|e| e == request || own.contains(e.as_str())),
                    "issue match must cite this issue and this feature's code only"
                );
            }
        }
    }
    ensure!(seen.len() == fs.len(), "issue assessment omitted features");
    Ok(())
}
fn request(
    args: &IssueReviewArgs,
    r: &mut Review,
    p: &Value,
    agent: &structured::Agent,
    profile: &review::AgentProfile,
) -> Result<Option<Value>> {
    let key = review::Request {
        agent: agent.name(),
        profile,
        pack: p,
    }
    .key()?;
    if let Some(c) = r.calls.iter().find(|c| c.key == key) {
        validate(&c.response, p)?;
        return Ok(Some(c.response.clone()));
    }
    let bytes = crate::json_size(p)?;
    ensure!(
        bytes <= args.max_bytes,
        "issue request needs {bytes} bytes; raise --max-bytes; no diffs were shortened"
    );
    write_json(
        &args
            .output
            .with_extension("inputs")
            .join(format!("{key}.json")),
        p,
    )?;
    if args.dry_run || r.attempted_calls >= args.limit || r.input_bytes + bytes > args.total_bytes {
        return Ok(None);
    }
    r.attempted_calls += 1;
    r.input_bytes += bytes;
    write_json(&args.output, r)?;
    eprintln!(
        "Issue matching {} · call {}/{} · {bytes} input bytes",
        p["stage"], r.attempted_calls, args.limit
    );
    let result = structured::run(agent, profile, p, Duration::from_secs(args.timeout));
    match result {
        Ok((value, usage, ms)) => {
            let c = Call {
                key: key.clone(),
                stage: p["stage"].as_str().unwrap().into(),
                input_bytes: bytes,
                duration_ms: ms,
                provider_usage: usage,
                input: p.clone(),
                response: value.clone(),
            };
            write_json(
                &args
                    .output
                    .with_extension("responses")
                    .join(format!("{}-{key}.json", r.attempted_calls)),
                &c,
            )?;
            if let Err(e) = validate(&value, p) {
                r.errors.push(format!("{}: {e:#}", c.stage));
                write_json(&args.output, r)?;
                return Ok(None);
            }
            r.calls.push(c);
            write_json(&args.output, r)?;
            Ok(Some(value))
        }
        Err(e) => {
            r.errors.push(format!("issue CLI request: {e:#}"));
            write_json(&args.output, r)?;
            Ok(None)
        }
    }
}
fn bytes(p: &Value) -> usize {
    crate::json_size(p).unwrap()
}

pub fn run(args: IssueReviewArgs, config: review::Config) -> Result<bool> {
    ensure!(
        args.max_bytes >= 8000 && args.total_bytes >= 8000 && args.timeout > 0,
        "invalid issue matching budgets"
    );
    let protected = [&args.report, &args.classification, &args.issue_cache]
        .into_iter()
        .map(|p| p.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;
    let destinations = std::iter::once(&args.output)
        .chain(args.html.iter())
        .map(|p| classify::absolute_output(p))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        destinations.iter().all(|p| !protected.contains(p))
            && destinations.iter().collect::<BTreeSet<_>>().len() == destinations.len(),
        "issue output paths must differ from input snapshots and each other"
    );
    let report = crate::scan::load_report(&args.report)?;
    let mut d: Discovery = serde_json::from_slice(&std::fs::read(&args.classification)?)?;
    ensure!(
        d.run.repository == report.repository
            && d.run.base_sha == report.base_sha
            && d.run.source_fingerprint == crate::shortlist::fingerprint(&report),
        "issue matching classification does not match report"
    );
    let raw = std::fs::read(&args.issue_cache)?;
    let snapshot: DemandSnapshot = serde_json::from_slice(&raw)?;
    let issues:Vec<_>=snapshot.threads.iter().filter(|i|i.kind=="issue" && i.state=="open" && !i.is_pull_request && crate::issue_context::project_thread_url(&i.url,&report.repository)).map(|i|json!({"url":i.url,"evidence_id":format!("request:{}",i.url),"title":i.title,"body":i.body,"body_truncated":i.body_truncated,"kind":i.kind,"state":i.state})).collect();
    ensure!(
        issues
            .iter()
            .map(|i| i["url"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len()
            == issues.len(),
        "duplicate open issue URLs"
    );
    ensure!(
        snapshot.threads.is_empty()
            || snapshot
                .threads
                .iter()
                .any(|i| crate::issue_context::project_thread_url(&i.url, &report.repository)),
        "issue snapshot contains no threads for this repository"
    );
    let catalog_fingerprint = hash(&raw);
    let mut inline_covered = BTreeSet::new();
    for r in &d.inspections {
        let response: classify::Response = serde_json::from_value(r.value.clone())?;
        for (i, g) in response.groups.iter().enumerate() {
            if crate::inline_issues::covered(g, &r.context, &catalog_fingerprint) {
                inline_covered.insert(feature_key(&r.key, i));
            }
        }
    }
    let fs: Vec<_> = features(&d)?
        .into_iter()
        .filter(|f| !inline_covered.contains(&f.key))
        .collect();
    let agent = args.agent.clone().map(Ok).unwrap_or_else(|| {
        structured::Agent::parse(
            config
                .default_agent
                .as_deref()
                .context("choose --agent codex|claude|grok")?,
        )
    })?;
    let profile = crate::model_defaults::resolve_classification(
        &config,
        agent.name(),
        args.model.as_deref(),
        args.effort.as_deref(),
    )?;
    let mut r = Review {
        repository: report.repository.clone(),
        base_sha: report.base_sha.clone(),
        inspection_fingerprint: fingerprint(&d),
        issue_cache_fingerprint: hash(&raw),
        generated_at: chrono::Utc::now().to_rfc3339(),
        issues_fetched_at: snapshot.fetched_at,
        available_open_issues: issues.len(),
        screened_issue_urls: vec![],
        findings: vec![],
        agent: agent.name().into(),
        requested_model: profile.model.clone(),
        requested_effort: profile.effort.clone(),
        calls: vec![],
        attempted_calls: 0,
        input_bytes: 0,
        call_limit: args.limit,
        input_limit: args.total_bytes,
        errors: vec![],
        interruptions: vec![],
        limitations: snapshot.warnings,
        complete: false,
    };
    if args.continue_run {
        let previous: Review = serde_json::from_slice(&std::fs::read(&args.output)?)?;
        ensure!(
            previous.repository == r.repository
                && previous.base_sha == r.base_sha
                && previous.inspection_fingerprint == r.inspection_fingerprint
                && previous.issue_cache_fingerprint == r.issue_cache_fingerprint
                && previous.agent == r.agent
                && previous.requested_model == r.requested_model
                && previous.requested_effort == r.requested_effort
                && args.limit >= previous.call_limit
                && args.total_bytes >= previous.input_limit,
            "continuation must keep issue inputs/model and cannot reduce budgets"
        );
        r = previous;
        r.call_limit = args.limit;
        r.input_limit = args.total_bytes;
        r.interruptions.append(&mut r.errors);
        for c in &r.calls {
            validate(&c.response, &c.input)?;
        }
    }
    eprintln!(
        "Issue matching: {} inspected features · {} open project issues",
        fs.len(),
        issues.len()
    );
    let summaries:Vec<_>=fs.iter().map(|f|json!({"feature_id":f.id,"title":f.title,"summary_hypothesis":f.summary,"commits":f.cards.iter().flat_map(|c|c["commits"].as_array().into_iter().flatten()).map(|c|json!({"subject":c["subject"],"message":classify::clip(c["message"].as_str().unwrap_or(""),1000),"paths":c["files"].as_array().into_iter().flatten().map(|f|f["path"].clone()).collect::<Vec<_>>()})).collect::<Vec<_>>()})).collect();
    let compact: Vec<_> = issues
        .iter()
        .map(|i| {
            let mut v = i.clone();
            let body = i["body"].as_str().unwrap();
            v["body"] = json!(classify::clip(body, 1000));
            v["body_truncated"] = json!(i["body_truncated"] == true || body.len() > 1000);
            v
        })
        .collect();
    let mut nominations = BTreeMap::<String, BTreeSet<String>>::new();
    let mut screened = BTreeSet::new();
    let mut at = 0;
    if !fs.is_empty() {
        while at < compact.len() {
            let mut n = compact.len() - at;
            while n > 1
                && bytes(&pack(
                    summaries.clone(),
                    compact[at..at + n].to_vec(),
                    false,
                )) > args.max_bytes
            {
                n = n.div_ceil(2);
            }
            let p = pack(summaries.clone(), compact[at..at + n].to_vec(), false);
            let Some(value) = request(&args, &mut r, &p, &agent, &profile)? else {
                break;
            };
            for a in serde_json::from_value::<Screening>(value)?.assessments {
                nominations
                    .entry(a.feature_id)
                    .or_default()
                    .extend(a.issue_urls);
            }
            screened.extend(
                compact[at..at + n]
                    .iter()
                    .map(|i| i["url"].as_str().unwrap().to_owned()),
            );
            at += n;
        }
    }
    r.screened_issue_urls = screened.into_iter().collect();
    let mut findings = Vec::new();
    let pending: Vec<_> = fs
        .iter()
        .filter(|f| nominations.get(&f.id).is_some_and(|u| !u.is_empty()))
        .collect();
    let confirmation_pack = |batch: &[&Feature]| {
        let urls: BTreeSet<_> = batch.iter().flat_map(|f| &nominations[&f.id]).collect();
        let items=batch.iter().map(|f|json!({"feature_id":f.id,"title":f.title,"summary_hypothesis":f.summary,"candidates":f.cards,"issue_urls":nominations[&f.id]})).collect();
        pack(
            items,
            issues
                .iter()
                .filter(|i| urls.contains(&i["url"].as_str().unwrap().to_string()))
                .cloned()
                .collect(),
            true,
        )
    };
    let mut confirmed = 0;
    if r.errors.is_empty() && at == compact.len() {
        while confirmed < pending.len() {
            let mut n = pending.len() - confirmed;
            while n > 1
                && bytes(&confirmation_pack(&pending[confirmed..confirmed + n])) > args.max_bytes
            {
                n = n.div_ceil(2);
            }
            let p = confirmation_pack(&pending[confirmed..confirmed + n]);
            let Some(value) = request(&args, &mut r, &p, &agent, &profile)? else {
                break;
            };
            for a in serde_json::from_value::<Confirmation>(value)?.assessments {
                let f = fs.iter().find(|f| f.id == a.feature_id).unwrap();
                findings.push(Finding {
                    feature_key: f.key.clone(),
                    checked_issue_urls: nominations[&f.id].iter().cloned().collect(),
                    matches: a
                        .matches
                        .into_iter()
                        .map(|m| Match {
                            title: issues.iter().find(|i| i["url"] == m.issue_url).unwrap()
                                ["title"]
                                .as_str()
                                .unwrap()
                                .into(),
                            issue_url: m.issue_url,
                            relation: m.relation,
                            reason: m.reason,
                        })
                        .collect(),
                });
            }
            confirmed += n;
        }
    }
    r.findings = findings;
    r.complete =
        r.errors.is_empty() && (fs.is_empty() || at == compact.len()) && confirmed == pending.len();
    write_json(&args.output, &r)?;
    if !args.dry_run {
        d.issue_review = Some(r.clone());
        write_json(&args.classification, &d)?;
        if let Some(path) = &args.html {
            crate::write_atomic(path, crate::discovery_html::html(&d, &report).as_bytes())?;
        }
    }
    eprintln!("{} / {} issues screened · {} features checked against full issue bodies · {} supported matches · {} calls · {} input bytes",r.screened_issue_urls.len(),r.available_open_issues,r.findings.len(),r.findings.iter().map(|f|f.matches.len()).sum::<usize>(),r.attempted_calls,r.input_bytes);
    for e in &r.errors {
        eprintln!("{e}");
    }
    Ok(!args.dry_run && !r.complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn issue_matches_require_open_issue_and_feature_code_evidence() {
        let p = pack(
            vec![
                json!({"feature_id":"a","issue_urls":["https://github.com/o/r/issues/1"],"candidates":[{"evidence_ids":["commit:a"]}]}),
            ],
            vec![
                json!({"url":"https://github.com/o/r/issues/1","evidence_id":"request:https://github.com/o/r/issues/1","state":"open","kind":"issue"}),
            ],
            true,
        );
        let v = json!({"assessments":[{"feature_id":"a","matches":[{"issue_url":"https://github.com/o/r/issues/1","relation":"likely_addresses","reason":{"text":"The code may address the reported problem","evidence":["commit:a","request:https://github.com/o/r/issues/1"]}}]}]});
        validate(&v, &p).unwrap();
        for evidence in [
            json!(["commit:a"]),
            json!(["request:https://github.com/o/r/issues/1"]),
            json!(["commit:another", "request:https://github.com/o/r/issues/1"]),
        ] {
            let mut bad = v.clone();
            bad["assessments"][0]["matches"][0]["reason"]["evidence"] = evidence;
            assert!(validate(&bad, &p).is_err());
        }
        let mut closed = p.clone();
        closed["issues"][0]["state"] = json!("closed");
        assert!(validate(&v, &closed).is_err());
        let mut bad = v.clone();
        bad["assessments"][0]["matches"][0]["issue_url"] =
            json!("https://github.com/other/repo/issues/1");
        assert!(validate(&bad, &p).is_err());
        assert!(validate(&json!({"assessments":[]}), &p).is_err());
        validate(
            &json!({"assessments":[{"feature_id":"a","matches":[]}]}),
            &p,
        )
        .unwrap();
    }
}
