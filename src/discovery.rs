//! Budgeted metadata mapping followed by targeted, evidence-backed inspection.
use crate::{
    classify::{self, ClassifyArgs},
    hash, issue_context, metrics,
    model::{Feature, Report},
    related, review, structured, write_atomic, write_json,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    time::{Duration, Instant},
};

const MAP: &str = include_str!("prompts/map.md");
const INSPECT: &str = include_str!("prompts/inspect.md");
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lead {
    pub headline: String,
    pub candidate_ids: Vec<String>,
    pub basis: Basis,
    pub benefit: String,
    pub question: String,
    pub files_to_inspect: Vec<String>,
    pub issue_queries: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demand: Option<DemandFinding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemandFinding {
    pub status: DemandStatus,
    pub reason: crate::model::Claim,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemandStatus {
    ObservedRequest,
    PossibleMatch,
    ResolvedOrDeclined,
    NotEstablished,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    PlausibleFeature,
    Unclear,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub groups: Vec<Lead>,
    #[serde(default)]
    pub deferred_ids: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub candidate_id: String,
    pub aliases: Vec<String>,
    pub forks: Vec<String>,
    pub metadata: Value,
    #[serde(default)]
    pub integration: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<crate::discovery_window::Activity>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Saved {
    pub key: String,
    pub stage: String,
    pub candidate_ids: Vec<String>,
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_value: Option<Value>,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub duration_ms: u64,
    pub provider_usage: Option<Value>,
    pub context: Value,
}
#[derive(Serialize, Deserialize)]
pub struct BudgetExtension {
    pub recorded_at: String,
    pub previous_call_limit: usize,
    pub call_limit: usize,
    pub previous_input_limit: usize,
    pub input_limit: usize,
    pub calls_spent: usize,
    pub input_bytes_spent: usize,
}
#[derive(Serialize, Deserialize)]
pub struct Discovery {
    pub discovery_version: u32,
    #[serde(default)]
    pub fresh: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<crate::discovery_window::Window>,
    #[serde(default)]
    pub interruptions: Vec<String>,
    #[serde(default)]
    pub budget_extensions: Vec<BudgetExtension>,
    #[serde(default)]
    pub application_pool: Option<crate::discovery_application::Pool>,
    #[serde(default)]
    pub assembled_checks: Vec<crate::discovery_application::SetCheck>,
    #[serde(flatten)]
    pub run: classify::Run,
    pub entries: Vec<Entry>,
    pub mappings: Vec<Saved>,
    pub inspections: Vec<Saved>,
    #[serde(default)]
    pub superseded_inspections: Vec<Saved>,
    #[serde(default)]
    pub inspection_deferrals: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_review: Option<crate::issue_review::Review>,
    pub new_input_bytes: usize,
    pub new_output_bytes: usize,
    pub elapsed_ms: u64,
    pub map_calls: usize,
    pub input_limit: usize,
    pub call_limit: usize,
    pub stop_reason: String,
}
impl Discovery {
    pub fn screened(&self) -> BTreeSet<&str> {
        self.mappings
            .iter()
            .flat_map(|m| m.candidate_ids.iter().map(String::as_str))
            .collect()
    }
    pub fn inspected(&self) -> BTreeSet<&str> {
        self.inspections
            .iter()
            .flat_map(|m| m.candidate_ids.iter().map(String::as_str))
            .collect()
    }
}
fn own_issue(f: &Feature, repo: &str) -> bool {
    f.issue_links
        .iter()
        .any(|s| issue_context::project_thread_url(s, repo))
}
/// Exact patch-set equivalence only; overlapping/contained sets remain distinct.
fn inventory(report: &Report, selected: &related::SelectedForks<'_>) -> Vec<Entry> {
    let eligible: BTreeSet<_> = selected
        .forks
        .iter()
        .flat_map(|f| &f.groups)
        .flat_map(|g| &g.candidate_ids)
        .collect();
    let mut sets = BTreeMap::<Vec<String>, Vec<&Feature>>::new();
    for f in &report.features {
        if !eligible.contains(&f.id)
            || f.patch_ids.is_empty()
            || f.files.is_empty()
            || matches!(f.status.as_str(), "dismissed" | "adopted")
        {
            continue;
        }
        let mut ids = f.patch_ids.clone();
        ids.sort();
        ids.dedup();
        sets.entry(ids).or_default().push(f);
    }
    let mut entries = Vec::new();
    for mut aliases in sets.into_values() {
        aliases.sort_by(|a, b| a.id.cmp(&b.id));
        let f = aliases[0];
        let forks: Vec<_> = aliases
            .iter()
            .flat_map(|f| &f.sources)
            .map(|s| s.repository.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut seen = BTreeSet::new();
        let commits: Vec<_> = f.commits.iter().filter_map(|s| report.commits.get(s)).filter(|c| seen.insert(c.patch_id.as_deref().unwrap_or(&c.sha))).take(3).map(|c| json!({"sha":c.sha,"parents":c.parents,"message":classify::clip(&c.message,400),"message_truncated":c.message.len()>400})).collect();
        let links: Vec<_> = aliases
            .iter()
            .flat_map(|f| &f.issue_links)
            .filter(|s| issue_context::project_thread_url(s, &report.repository))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let metadata = json!({"candidate_id":f.id,"title":classify::clip(&f.title,160),"commits":commits,"commits_omitted":f.commits.len().saturating_sub(commits.len()),"paths":f.files.iter().take(16).collect::<Vec<_>>(),"paths_omitted":f.files.len().saturating_sub(16),"additions":f.additions,"deletions":f.deletions,"issue_links":links});
        entries.push(Entry {
            candidate_id: f.id.clone(),
            aliases: aliases.iter().skip(1).map(|f| f.id.clone()).collect(),
            forks,
            metadata,
            integration: None,
            activity: None,
        });
    }
    entries.sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
    entries
}
fn fork_rounds<T>(mut forks: Vec<VecDeque<T>>) -> VecDeque<Vec<T>> {
    let mut rounds = VecDeque::new();
    while forks.iter().any(|f| !f.is_empty()) {
        for fork in &mut forks {
            let n = fork.len().min(12);
            if n > 0 {
                rounds.push_back(fork.drain(..n).collect());
            }
        }
    }
    rounds
}
/// Preserve small fork-local bundles, alternate referenced and unreferenced work.
/// Hash order is a reproducible exploration sample, never a usefulness ranking.
fn ordered<'a>(entries: &'a [Entry], report: &Report) -> Vec<&'a Entry> {
    let by_id: BTreeMap<_, _> = report.features.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut counts = BTreeMap::<&str, usize>::new();
    for e in entries {
        for f in &e.forks {
            *counts.entry(f).or_default() += 1;
        }
    }
    let mut pools = [BTreeMap::<String, Vec<&Entry>>::new(), BTreeMap::new()];
    for e in entries {
        let fork = e
            .forks
            .iter()
            .min_by_key(|f| (counts[f.as_str()], hash(f)))
            .cloned()
            .unwrap_or_default();
        let linked = own_issue(by_id[e.candidate_id.as_str()], &report.repository);
        pools[usize::from(!linked)]
            .entry(hash(fork))
            .or_default()
            .push(e);
    }
    let mut queues: Vec<VecDeque<Vec<&Entry>>> = pools
        .into_iter()
        .map(|p| {
            let forks: Vec<VecDeque<&Entry>> = p
                .into_values()
                .map(|mut es| {
                    es.sort_by_key(|e| e.candidate_id.clone());
                    es.into()
                })
                .collect();
            fork_rounds(forks)
        })
        .collect();
    let mut result = Vec::new();
    while !queues.iter().all(VecDeque::is_empty) {
        for q in &mut queues {
            if let Some(es) = q.pop_front() {
                result.extend(es);
            }
        }
    }
    result
}
fn map_pack(
    report: &Report,
    entries: &[&Entry],
    demand: Option<(&issue_context::Cache, &issue_context::ScreeningIndex)>,
) -> Value {
    let ids: Vec<_> = entries.iter().map(|e| &e.candidate_id).collect();
    let string = json!({"type":"string"});
    let id = json!({"$ref":"#/$defs/candidate"});
    let mut schema = classify::object(
        json!({"groups":{"type":"array","maxItems":12,"items":classify::object(json!({"headline":string,"candidate_ids":{"type":"array","minItems":1,"maxItems":12,"items":id},"basis":{"type":"string","enum":["plausible_feature","unclear"]},"benefit":string,"question":string,"files_to_inspect":{"type":"array","maxItems":3,"items":string},"issue_queries":{"type":"array","maxItems":2,"items":string}}))}}),
    );
    schema["$defs"] = json!({"candidate":{"type":"string","enum":ids}});
    // Compact semantic hints: full messages, hashes and source bodies belong in
    // inspection. Local ancestry links preserve relationships without repeating
    // long SHAs and shared commit records throughout the screening input.
    let forks: BTreeMap<_, _> = entries
        .iter()
        .flat_map(|e| e.forks.iter())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .enumerate()
        .map(|(i, f)| (f, i))
        .collect();
    let commit_owners: BTreeMap<_, _> = entries
        .iter()
        .flat_map(|e| {
            e.metadata["commits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |c| c["sha"].as_str().map(|sha| (sha, e.candidate_id.as_str())))
        })
        .collect();
    let candidates: Vec<_> = entries.iter().map(|e| {
        let title=e.metadata["title"].as_str().unwrap_or("");
        let paths=e.metadata["paths"].as_array().cloned().unwrap_or_default();
        let commits=e.metadata["commits"].as_array().cloned().unwrap_or_default();
        let messages: Vec<_> = commits.iter().filter_map(|c| {
            let msg=c["message"].as_str().unwrap_or("");
            let (subject,body)=msg.split_once('\n').unwrap_or((msg,""));
            let text=if subject==title {body.trim()} else {msg.trim()};
            (!text.is_empty()).then(|| classify::clip(text,120))
        }).take(2).collect();
        let parents: BTreeSet<_> = commits.iter().flat_map(|c| c["parents"].as_array().into_iter().flatten())
            .filter_map(|p| p.as_str().and_then(|sha| commit_owners.get(sha)).copied())
            .filter(|id| *id != e.candidate_id).collect();
        let links=e.metadata["issue_links"].as_array().cloned().unwrap_or_default();
        let mut v=json!({"candidate_id":e.candidate_id,"title":title,"paths":paths.iter().take(5).collect::<Vec<_>>(),
            "lines":[e.metadata["additions"],e.metadata["deletions"]],
            "fork_tokens":e.forks.iter().take(4).map(|f|forks[f]).collect::<Vec<_>>()});
        let omitted_paths=paths.len().saturating_sub(5)+e.metadata["paths_omitted"].as_u64().unwrap_or(0) as usize;
        if let Some(status) = e.integration.as_ref().and_then(|i| i["status"].as_str()) { v["application_status"] = json!(status); }
        if let Some(activity) = &e.activity { v["patch_age_days"]=json!(activity.age_days); if activity.missing_patch_dates>0 {v["missing_patch_dates"]=json!(activity.missing_patch_dates);} }
        if omitted_paths>0 {v["paths_omitted"]=json!(omitted_paths);}
        if e.forks.len()>4 {v["fork_tokens_omitted"]=json!(e.forks.len()-4);}
        if !messages.is_empty() {v["message_excerpts"]=json!(messages);}
        let commit_count=commits.len()+e.metadata["commits_omitted"].as_u64().unwrap_or(0) as usize;
        if commit_count>1 {v["commit_count"]=json!(commit_count);}
        if !parents.is_empty() {v["parent_candidates"]=json!(parents);}
        if !links.is_empty() {v["issue_links"]=json!(links.iter().take(2).collect::<Vec<_>>());}
        if links.len()>2 {v["issue_links_omitted"]=json!(links.len()-2);}
        v
    }).collect();
    let mut pack = json!({"discovery_version":3,"stage":"map","instructions":MAP,"repository":report.repository,"candidates":candidates,
        "scope":"Partial inventory batch, not a complete fork. Titles and up to two message excerpts (120 bytes each), five paths, four fork tokens and two issue links per candidate. Message excerpts may be truncated or absent; parent_candidates only lists direct ancestry visible inside this batch. Missing context is uncertainty. All non-nominated inputs are deferred by Rust.","response_schema":schema});
    if let Some((cache, index)) = demand {
        let mut context = cache.screening_context(
            index,
            &entries
                .iter()
                .map(|e| e.candidate_id.as_str())
                .collect::<Vec<_>>(),
            6000,
        );
        // Put compact pointers beside each candidate instead of repeating IDs
        // and lexical match diagnostics in the model input.
        let matches = context
            .as_object_mut()
            .unwrap()
            .remove("candidate_matches")
            .unwrap_or(json!([]));
        let by_id: BTreeMap<_, _> = matches
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["candidate_id"].as_str().map(|id| (id, m)))
            .collect();
        for candidate in pack["candidates"].as_array_mut().unwrap() {
            if let Some(m) = by_id.get(candidate["candidate_id"].as_str().unwrap()) {
                let hits = m["matches"].as_array().cloned().unwrap_or_default();
                if !hits.is_empty() {
                    candidate["demand_threads"] =
                        json!(hits.iter().map(|h| h["thread"].clone()).collect::<Vec<_>>());
                }
                let explicit: Vec<_> = hits
                    .iter()
                    .filter(|h| h["kind"] == "explicit_reference")
                    .map(|h| h["thread"].clone())
                    .collect();
                if !explicit.is_empty() {
                    candidate["explicit_demand_threads"] = json!(explicit);
                }
                if m["matches_without_excerpt"].as_u64().unwrap_or(0) > 0 {
                    candidate["demand_matches_without_excerpt"] =
                        m["matches_without_excerpt"].clone();
                }
            }
        }
        let evidence: Vec<_> = context["threads"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| t["evidence_id"].clone())
            .collect();
        let citations = if evidence.is_empty() {
            json!({"type":"array","maxItems":0,"items":{"type":"string"}})
        } else {
            json!({"type":"array","items":{"type":"string","enum":evidence}})
        };
        let finding = classify::object(
            json!({"status":{"type":"string","enum":["observed_request","possible_match","resolved_or_declined","not_established"]},"reason":classify::object(json!({"text":{"type":"string"},"evidence":citations}))}),
        );
        pack["response_schema"]["properties"]["groups"]["items"]["properties"]["demand"] = finding;
        pack["response_schema"]["properties"]["groups"]["items"]["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("demand"));
        pack["screening_demand"] = context;
    }
    pack
}
fn validate_map(value: &Value, pack: &Value) -> Result<()> {
    let m: Mapping = serde_json::from_value(value.clone())?;
    ensure!(m.groups.len() <= 12, "too many nominations");
    let allowed: BTreeMap<_, _> = pack["candidates"]
        .as_array()
        .context("missing candidates")?
        .iter()
        .map(|c| (c["candidate_id"].as_str().unwrap_or(""), c))
        .collect();
    let demand_ids: BTreeSet<_> = pack["screening_demand"]["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["evidence_id"].as_str())
        .collect();
    let require_demand = pack["response_schema"]["properties"]["groups"]["items"]["properties"]
        .get("demand")
        .is_some();
    for g in &m.groups {
        ensure!(
            !require_demand || g.demand.is_some(),
            "missing demand assessment"
        );
        if let Some(d) = &g.demand {
            ensure!(
                !d.reason.text.trim().is_empty() && d.reason.text.len() <= 1000,
                "empty/oversized demand reason"
            );
            ensure!(
                d.reason
                    .evidence
                    .iter()
                    .all(|id| demand_ids.contains(id.as_str())),
                "demand citation was not supplied"
            );
            ensure!(
                d.status == DemandStatus::NotEstablished || !d.reason.evidence.is_empty(),
                "demand assessment requires source evidence"
            );
        }
        let mut seen = BTreeSet::new();
        ensure!(
            !g.headline.trim().is_empty()
                && g.headline.chars().count() <= 120
                && g.headline.split_whitespace().count() <= 12,
            "invalid feature headline"
        );
        ensure!(
            !g.candidate_ids.is_empty()
                && g.candidate_ids.len() <= 12
                && g.files_to_inspect.len() <= 3
                && g.issue_queries.len() <= 2,
            "oversized nomination"
        );
        for s in [&g.benefit, &g.question] {
            ensure!(
                !s.trim().is_empty() && s.len() <= 1000,
                "missing/oversized inspection rationale"
            );
        }
        for id in &g.candidate_ids {
            ensure!(
                allowed.contains_key(id.as_str()) && seen.insert(id.as_str()),
                "unknown or duplicate candidate"
            );
        }
        // File requests are hints for a bounded read, not evidence claims. A
        // missing path must not throw away an otherwise valid feature map.
        ensure!(
            g.files_to_inspect.iter().all(|p| !p.trim().is_empty()
                && p.len() <= 1024
                && !p.chars().any(char::is_control)),
            "invalid file request"
        );
        ensure!(
            g.issue_queries.iter().all(|s| !s.trim().is_empty()
                && s.len() <= 200
                && !s.chars().any(char::is_control)),
            "invalid issue query"
        );
    }
    // Rust owns exhaustive accounting: any non-nominated input stays deferred.
    // Metadata nominations may overlap (shared support / competing hypotheses).
    // Inspection still requires an exact evidence-backed partition.
    ensure!(
        m.deferred_ids
            .iter()
            .all(|id| allowed.contains_key(id.as_str())),
        "unknown deferred candidate"
    );
    Ok(())
}
fn validate(stage: &str, value: &Value, pack: &Value) -> Result<()> {
    if stage == "map" {
        validate_map(value, pack)
    } else {
        classify::validate(&serde_json::from_value(value.clone())?, pack)
    }
}
// Recover independently valid nominations, or optional relationship claims.
// Never manufacture evidence or downgrade an unsupported claim into a fact.
fn validate_record(record: &mut Saved, pack: &Value) -> Result<()> {
    if validate(&record.stage, &record.value, pack).is_ok() {
        return Ok(());
    }
    if record.stage == "map" {
        let mut mapping: Mapping = serde_json::from_value(record.value.clone())?;
        ensure!(mapping.groups.len() <= 12, "too many nominations");
        let mut omissions = Vec::new();
        let mut retained = Vec::new();
        for (i, group) in std::mem::take(&mut mapping.groups).into_iter().enumerate() {
            let value = json!({"groups":[group]});
            match validate_map(&value, pack) {
                Ok(()) => retained.push(group),
                Err(error) => omissions.push(json!({"index":i,"reason":format!("{error:#}")})),
            }
        }
        ensure!(
            !retained.is_empty(),
            "no valid nominations in mapping response"
        );
        mapping.groups = retained;
        let value = serde_json::to_value(mapping)?;
        validate_map(&value, pack)?;
        record.raw_value = Some(record.value.clone());
        record.value = value;
        record.context["validation_omissions"] = json!(omissions);
        return Ok(());
    }
    let mut response: classify::Response = serde_json::from_value(record.value.clone())?;
    let relationships = std::mem::take(&mut response.relationships);
    classify::validate(&response, pack)?;
    let mut omitted = 0;
    for relationship in relationships {
        response.relationships.push(relationship);
        if classify::validate(&response, pack).is_err() {
            response.relationships.pop();
            omitted += 1;
        }
    }
    ensure!(omitted > 0, "invalid classification cannot be recovered");
    response.limitations.push(format!("{omitted} relationship claim(s) omitted because their references, citations, or dependency structure failed validation. Prerequisites remain unverified."));
    classify::validate(&response, pack)?;
    record.raw_value = Some(record.value.clone());
    record.value = serde_json::to_value(response)?;
    Ok(())
}
fn key(pack: &Value, agent: &structured::Agent, profile: &review::AgentProfile) -> Result<String> {
    review::Request {
        agent: agent.name(),
        profile,
        pack,
    }
    .key()
}
fn cached(cache: &Path, key: &str, stage: &str, pack: &Value) -> Option<Saved> {
    ["discovery", "discovery-rejected"]
        .iter()
        .find_map(|directory| {
            std::fs::read(cache.join(directory).join(format!("{key}.json")))
                .ok()
                .and_then(|b| serde_json::from_slice::<Saved>(&b).ok())
                .and_then(|mut r| {
                    (r.key == key && r.stage == stage && validate_record(&mut r, pack).is_ok())
                        .then_some(r)
                })
        })
}
struct Session<'a> {
    args: &'a ClassifyArgs,
    agent: &'a structured::Agent,
    profile: &'a review::AgentProfile,
    cache: &'a Path,
    start: Instant,
}
enum Prepared {
    Cached(Saved),
    New(Pending),
    Skipped,
}
struct Pending {
    key: String,
    pack: Value,
    stage: String,
    bytes: usize,
    timeout: Duration,
}
impl Session<'_> {
    fn prepare(
        &self,
        d: &mut Discovery,
        pack: &Value,
        stage: &str,
        allow_new: bool,
    ) -> Result<Prepared> {
        let key = key(pack, self.agent, self.profile)?;
        if let Some(dir) = &self.args.plan_dir {
            for (suffix, value) in [("request", pack), ("schema", &pack["response_schema"])] {
                let path = dir.join(format!("{key}.{suffix}.json"));
                let dest = classify::absolute_output(&path)?;
                for p in std::iter::once(&self.args.report)
                    .chain(self.args.inventory.iter())
                    .chain(self.args.issue_cache.iter())
                    .chain(self.args.pr_snapshot.iter())
                    .chain(self.args.resume_discovery.iter())
                    .chain(self.args.release_snapshot.iter())
                    .chain(std::iter::once(&self.args.output))
                    .chain(self.args.html.iter())
                {
                    ensure!(
                        dest != classify::absolute_output(p)?,
                        "plan export collides with input/output"
                    );
                }
                write_json(&path, value)?;
            }
        }
        if self.args.continue_run {
            // Only this run's local responses, written after it started.
            // They were already charged to its persisted call/input counters.
            let root = self.args.output.with_extension("responses");
            let started =
                chrono::DateTime::parse_from_rfc3339(&d.run.generated_at)?.timestamp_millis();
            for directory in ["discovery", "discovery-rejected"] {
                let path = root.join(directory).join(format!("{key}.json"));
                let current = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .is_some_and(|t| t.as_millis() as i128 >= i128::from(started));
                if current {
                    if let Ok(bytes) = std::fs::read(&path) {
                        if let Ok(mut record) = serde_json::from_slice::<Saved>(&bytes) {
                            if record.key == key
                                && record.stage == stage
                                && validate_record(&mut record, pack).is_ok()
                            {
                                write_json(
                                    &root.join("discovery").join(format!("{key}.json")),
                                    &record,
                                )?;
                                eprintln!("Retained this run's {stage} response after validation; no new call");
                                return Ok(Prepared::Cached(record));
                            }
                        }
                    }
                }
            }
        }
        if !self.args.fresh {
            if let Some(r) = cached(self.cache, &key, stage, pack) {
                d.run.reused_calls += 1;
                return Ok(Prepared::Cached(r));
            }
        }
        let bytes = crate::json_size(pack)?;
        ensure!(
            bytes <= self.args.max_bytes,
            "discovery request exceeds per-call input budget"
        );
        if !allow_new
            || self.args.dry_run
            || !d.run.errors.is_empty()
            || d.run.attempted_calls >= self.args.limit
            || bytes > d.input_limit.saturating_sub(d.new_input_bytes)
            || self.start.elapsed().as_secs() >= self.args.max_seconds
        {
            return Ok(Prepared::Skipped);
        }
        d.run.attempted_calls += 1;
        d.new_input_bytes += bytes;
        write_json(&self.args.output, d)?;
        eprintln!(
            "Discovery {stage} · call {}/{} · {bytes} input bytes",
            d.run.attempted_calls, self.args.limit
        );
        let timeout = Duration::from_secs(self.args.timeout)
            .min(Duration::from_secs(self.args.max_seconds).saturating_sub(self.start.elapsed()));
        Ok(Prepared::New(Pending {
            key,
            pack: pack.clone(),
            stage: stage.into(),
            bytes,
            timeout,
        }))
    }
    fn finish(
        &self,
        d: &mut Discovery,
        pending: &Pending,
        result: Result<(Value, Option<Value>, u64)>,
    ) -> Result<Option<Saved>> {
        let Pending {
            key,
            pack,
            stage,
            bytes,
            ..
        } = pending;
        let response_root = if self.args.fresh {
            self.args.output.with_extension("responses")
        } else {
            self.cache.to_owned()
        };
        match result {
            Ok((value, provider_usage, duration_ms)) => {
                let output_bytes = crate::json_size(&value)?;
                d.new_output_bytes += output_bytes;
                let mut r = Saved {
                    key: key.clone(),
                    stage: stage.clone(),
                    candidate_ids: pack["candidates"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|c| c["candidate_id"].as_str().unwrap().into())
                        .collect(),
                    value,
                    raw_value: None,
                    input_bytes: *bytes,
                    output_bytes,
                    duration_ms,
                    provider_usage,
                    context: json!({"inline_issues":pack.get("inline_issues"),"diff_scope":pack.get("diff_scope"),"assembled_application":pack.get("assembled_application"),"screening_demand":pack.get("screening_demand"),"issue_context":pack.get("issue_context"),"source_context":pack.get("source_context"),"proposed_features":pack.get("proposed_features"),"unavailable_file_requests":pack.get("unavailable_file_requests"),"candidates": if stage=="inspect" { pack.get("candidates").cloned().unwrap_or(Value::Null) } else {Value::Null}}),
                };
                if let Err(e) = validate_record(&mut r, pack) {
                    write_json(
                        &response_root.join("discovery-rejected").join(format!(
                            "{key}-{}.json",
                            hash(serde_json::to_vec(&r.value)?)
                        )),
                        &r,
                    )?;
                    write_json(
                        &response_root
                            .join("discovery-rejected")
                            .join(format!("{key}.json")),
                        &r,
                    )?;
                    d.run
                        .errors
                        .push(format!("{stage}: {e:#}; rejected response preserved"));
                    return Ok(None);
                }
                write_json(
                    &response_root.join("discovery").join(format!("{key}.json")),
                    &r,
                )?;
                Ok(Some(r))
            }
            Err(e) => {
                d.run.errors.push(format!("{stage}: {e:#}"));
                Ok(None)
            }
        }
    }
    fn request(
        &self,
        d: &mut Discovery,
        pack: &Value,
        stage: &str,
        allow_new: bool,
    ) -> Result<Option<Saved>> {
        match self.prepare(d, pack, stage, allow_new)? {
            Prepared::Cached(r) => Ok(Some(r)),
            Prepared::Skipped => Ok(None),
            Prepared::New(pending) => {
                let result =
                    structured::run(self.agent, self.profile, &pending.pack, pending.timeout);
                self.finish(d, &pending, result)
            }
        }
    }
    fn inspect_batch(
        &self,
        d: &mut Discovery,
        packs: &mut Vec<Value>,
        report: &Report,
    ) -> Result<()> {
        let mut pending = Vec::new();
        let mut order = Vec::new();
        for pack in packs.drain(..) {
            match self.prepare(d, &pack, "inspect", true)? {
                Prepared::Cached(r) => {
                    order.push(r.key.clone());
                    d.inspections.push(r);
                }
                Prepared::New(p) => {
                    order.push(p.key.clone());
                    pending.push(p);
                }
                Prepared::Skipped => (),
            }
        }
        // Workers only invoke the CLI. Validation, persistence, and shared budget
        // accounting stay on the owning thread. A failure stops subsequent waves;
        // calls already in flight finish and retain their valid results.
        std::thread::scope(|scope| -> Result<()> {
            let (tx, rx) = std::sync::mpsc::channel();
            for p in &pending {
                let tx = tx.clone();
                scope.spawn(move || {
                    let result = structured::run(self.agent, self.profile, &p.pack, p.timeout);
                    let _ = tx.send((p, result));
                });
            }
            drop(tx);
            let mut persistence_error = None;
            for (p, result) in rx {
                match self.finish(d, p, result) {
                    Ok(Some(r)) => d.inspections.push(r),
                    Ok(None) => (),
                    Err(e) => {
                        d.run.errors.push(format!("inspection persistence: {e:#}"));
                        persistence_error = Some(e);
                    }
                }
                d.elapsed_ms = self.start.elapsed().as_millis() as u64;
                // Stable feature order even when workers finish out of order.
                let positions: BTreeMap<_, _> = order
                    .iter()
                    .enumerate()
                    .map(|(i, k)| (k.as_str(), i + 1))
                    .collect();
                d.inspections
                    .sort_by_key(|r| positions.get(r.key.as_str()).copied().unwrap_or(0));
                if let Err(e) = write_json(&self.args.output, d) {
                    persistence_error = Some(e);
                }
                if let Some(path) = &self.args.html {
                    if let Err(e) =
                        write_atomic(path, crate::discovery_html::html(d, report).as_bytes())
                    {
                        persistence_error = Some(e);
                    }
                }
            }
            if let Some(e) = persistence_error {
                return Err(e);
            }
            Ok(())
        })
    }
}

fn complete_card(
    report: &Report,
    feature: &Feature,
    git: Option<&crate::git::Git>,
) -> Result<classify::Card> {
    let mut seen = BTreeSet::new();
    let mut commits = Vec::new();
    for sha in &feature.commits {
        if !seen.insert(sha) {
            continue;
        }
        let c = report
            .commits
            .get(sha)
            .with_context(|| format!("missing commit {sha}; cannot send a complete diff"))?;
        let patch = if c.patch_truncated {
            git.with_context(|| format!("complete diff for {sha} needs local Git objects"))?
                .full_patch(sha)
                .with_context(|| format!("cannot restore complete diff for {sha}"))?
        } else {
            c.patch.clone()
        };
        commits.push(json!({"sha":c.sha,"parents":c.parents,"subject":c.subject,"message":c.message,"message_truncated":false,"patch":patch,"patch_truncated":false,"files":c.files.iter().map(|f|json!({"path":f.path,"symbols":f.symbols})).collect::<Vec<_>>(),"files_omitted":0}));
    }
    let evidence_ids = commits
        .iter()
        .map(|c| format!("commit:{}", c["sha"].as_str().unwrap()))
        .collect();
    Ok(classify::Card {
        candidate_id: feature.id.clone(),
        title: feature.title.clone(),
        omitted_commits: 0,
        commits,
        evidence_ids,
    })
}

fn inspection_pack(
    report: &Report,
    leads: &[Lead],
    args: &ClassifyArgs,
    issues: Option<&issue_context::Cache>,
    cache: &Path,
    application: Option<&crate::integration::Check>,
) -> Result<Value> {
    let ids: BTreeSet<_> = leads.iter().flat_map(|l| &l.candidate_ids).collect();
    let allowed_paths: BTreeSet<_> = report
        .features
        .iter()
        .filter(|f| ids.contains(&f.id))
        .flat_map(|f| &f.files)
        .collect();
    let requested_paths: BTreeSet<_> = leads.iter().flat_map(|l| &l.files_to_inspect).collect();
    let paths: BTreeSet<_> = requested_paths
        .intersection(&allowed_paths)
        .copied()
        .collect();
    let unavailable_paths: Vec<_> = requested_paths
        .difference(&allowed_paths)
        .copied()
        .collect();
    let git = review::source_git(report, cache, None);
    let cards = report
        .features
        .iter()
        .filter(|f| ids.contains(&f.id))
        .map(|f| complete_card(report, f, git.as_ref()))
        .collect::<Result<Vec<_>>>()?;
    let mut pack = classify::context(report, &cards, "selection", &[]);
    pack["inspection_instructions"] = json!(INSPECT);
    pack["diff_scope"] = json!("complete-nominated-commits-v1");
    pack["response_schema"]["properties"]["groups"]["items"]["properties"]["members"]["maxItems"] =
        json!(cards.len());
    pack["proposed_features"] = json!(leads);
    pack["unavailable_file_requests"] = json!(unavailable_paths);
    if let Some(issues) = issues {
        let queries: Vec<_> = leads
            .iter()
            .flat_map(|l| l.issue_queries.iter().cloned())
            .take(3)
            .collect();
        let references: Vec<_> = leads
            .iter()
            .filter_map(|l| l.demand.as_ref())
            .flat_map(|d| &d.reason.evidence)
            .filter_map(|e| e.strip_prefix("request:").map(str::to_owned))
            .collect();
        issue_context::attach(
            &mut pack,
            issues.inspection_context(&cards, &queries, &references),
        );
    }
    let mut sources = Vec::new();
    if let Some(git) = review::source_git(report, cache, None) {
        for path in paths.iter().take(3) {
            if let Ok(Some((text, truncated))) = git.source_file(&report.base_sha, path, 6000) {
                sources.push(json!({"evidence_id":format!("source:{}:{path}",report.base_sha),"commit":report.base_sha,"path":path,"first_line":1,"content":text,"truncated":truncated}));
            }
        }
    }
    pack["source_context"] = json!(sources);
    if let Some(evidence) = pack
        .pointer_mut("/response_schema/$defs/claim/properties/evidence/items/enum")
        .and_then(Value::as_array_mut)
    {
        evidence.extend(sources.iter().map(|s| s["evidence_id"].clone()));
    }
    if let Some(check) = application {
        pack["assembled_application"] = json!({"target_sha":check.target_sha,"status":check.status,"ordered_commits":check.ordered_commits,"scope":"Exactly the nominated union; clean application does not establish complete dependencies, correctness, or tested behavior."});
    }
    // Full diffs and commit metadata are indivisible. Bound optional context only.
    while crate::json_size(&pack)? > args.max_bytes {
        let mut fields = Vec::<(usize, String, String)>::new();
        for (i, s) in pack["source_context"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            fields.push((
                s["content"].as_str().unwrap_or("").len(),
                format!("/source_context/{i}/content"),
                format!("/source_context/{i}/truncated"),
            ));
        }
        for (i, t) in pack["issue_context"]["threads"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            fields.push((
                t["body"].as_str().unwrap_or("").len(),
                format!("/issue_context/threads/{i}/body"),
                format!("/issue_context/threads/{i}/body_truncated"),
            ));
        }
        let (len, path, flag) = fields.into_iter().max().unwrap_or_default();
        ensure!(
            len > 0,
            "complete nominated diffs and metadata need {} bytes, exceeding --max-bytes {}; raise the allowance to inspect this set intact",
            crate::json_size(&pack)?, args.max_bytes
        );
        let text = classify::clip(
            pack.pointer(&path).unwrap().as_str().unwrap(),
            if len < 128 { 0 } else { len / 2 },
        );
        *pack.pointer_mut(&path).unwrap() = json!(text);
        *pack.pointer_mut(&flag).unwrap() = json!(true);
    }
    if args.inline_issues {
        if let Some(catalog) = issues.and_then(issue_context::Cache::small_open_catalog) {
            crate::inline_issues::try_attach(&mut pack, &catalog, args.max_bytes)?;
        }
    }
    Ok(pack)
}

pub fn run(
    args: &ClassifyArgs,
    agent: &structured::Agent,
    profile: &review::AgentProfile,
    report: &Report,
    measurements: &metrics::Inventory,
    selected: related::SelectedForks<'_>,
    cache: &Path,
) -> Result<bool> {
    ensure!(
        args.total_bytes >= 8000,
        "--total-bytes must be at least 8000"
    );
    let mut entries = inventory(report, &selected);
    let window = crate::discovery_window::configure(args, report, &mut entries, cache)?;
    if let Some(w) = &window {
        eprintln!("Release window: {} published {} · since {} · {} recent + {} undated retained / {} eligible · {} outside", w.tag, w.published_at, w.since, w.recent, w.unknown, w.eligible_before_window, w.outside);
    }
    let policy = args.application.unwrap_or(if window.is_some() {
        crate::discovery_application::Policy::Clean
    } else {
        crate::discovery_application::Policy::All
    });
    let mut measured = measurements.clone();
    if policy == crate::discovery_application::Policy::Clean
        && measured.candidates.iter().any(|c| {
            c.integration
                .as_ref()
                .is_none_or(|i| i.target_sha != report.base_sha)
        })
    {
        let git = review::source_git(report, cache, None).context("Clean screening needs local Git objects or an inventory made with --check-apply; use --application all to include unchecked work")?;
        crate::integration::run(
            report,
            &mut measured,
            &git,
            cache,
            None,
            crate::parallel::local_jobs() as usize,
        )?;
    }
    let facts: BTreeMap<_, _> = measured
        .candidates
        .iter()
        .map(|c| (c.feature_id.as_str(), c))
        .collect();
    for entry in &mut entries {
        entry.integration = facts
            .get(entry.candidate_id.as_str())
            .and_then(|c| c.integration.as_ref())
            .filter(|c| c.target_sha == report.base_sha)
            .map(|c| json!(c));
    }
    let application_pool =
        crate::discovery_application::Pool::new(&entries, policy, &report.base_sha);
    eprintln!(
        "Application pool: {} / {} changes · {:?} · {}",
        application_pool.eligible,
        application_pool.before_application,
        policy,
        serde_json::to_string(&application_pool.statuses)?
    );
    let mut d=Discovery{discovery_version:1,fresh:args.fresh,window,application_pool:Some(application_pool),assembled_checks:vec![],interruptions:vec![],budget_extensions:vec![],entries,mappings:vec![],inspections:vec![],superseded_inspections:vec![],inspection_deferrals:vec![],issue_review:None,new_input_bytes:0,new_output_bytes:0,elapsed_ms:0,map_calls:0,input_limit:args.total_bytes,call_limit:args.limit,stop_reason:String::new(),run:classify::Run{schema_version:1,repository:report.repository.clone(),base_sha:report.base_sha.clone(),source_fingerprint:crate::shortlist::fingerprint(report),generated_at:chrono::Utc::now().to_rfc3339(),agent:agent.name().into(),requested_model:profile.model.clone(),requested_effort:profile.effort.clone(),planned_batch_calls:0,attempted_calls:0,reused_calls:0,forks_with_candidates:selected.forks.len(),forks:vec![],errors:vec![],limitations:vec!["Metadata mapping is a hypothesis, targeted code inspection is not a comprehensive adoption review. Deferred changes are not rejected. Ordering alternates explicit-reference and exploration bundles; neither is a quality score.".into()],pr_filter:selected.pr_filter.cloned()}};
    if d.window.is_some() {
        d.run.limitations = vec!["Release-window candidates are screened in recent fork-local bundles, with at most twelve candidates per fork per round. Recency is not usefulness. Dates use earliest observed authorship per identical patch, not pushes, merges, or committer dates. Missing or future-only dates remain eligible as unknown. Older commits inside eligible candidates remain intact; separately cataloged older work remains available in the historical catalog. Metadata mapping and targeted inspection are not comprehensive adoption reviews.".into()];
    }
    let mut previous_ms = 0;
    if args.continue_run {
        let prior: Discovery = serde_json::from_slice(
            &std::fs::read(&args.output)
                .context("--continue-run needs an existing fresh output")?,
        )?;
        ensure!(
            prior.fresh
                && prior.run.repository == d.run.repository
                && prior.run.base_sha == d.run.base_sha
                && prior.run.source_fingerprint == d.run.source_fingerprint
                && prior.run.agent == d.run.agent
                && prior.run.requested_model == d.run.requested_model
                && prior.run.requested_effort == d.run.requested_effort
                && serde_json::to_value(&prior.window)? == serde_json::to_value(&d.window)?
                && serde_json::to_value(&prior.application_pool)?
                    == serde_json::to_value(&d.application_pool)?
                && serde_json::to_value(&prior.entries)? == serde_json::to_value(&d.entries)?
                && serde_json::to_value(&prior.run.pr_filter)?
                    == serde_json::to_value(&d.run.pr_filter)?,
            "continuation must keep the same fresh run inputs, model, and window"
        );
        ensure!(
            if args.extend_budget {
                d.call_limit >= prior.call_limit && d.input_limit >= prior.input_limit
            } else {
                d.call_limit == prior.call_limit && d.input_limit == prior.input_limit
            },
            "continuation must keep its budgets unless --extend-budget explicitly raises them; ceilings cannot decrease"
        );
        if d.call_limit != prior.call_limit || d.input_limit != prior.input_limit {
            d.budget_extensions.push(BudgetExtension {
                recorded_at: chrono::Utc::now().to_rfc3339(),
                previous_call_limit: prior.call_limit,
                call_limit: d.call_limit,
                previous_input_limit: prior.input_limit,
                input_limit: d.input_limit,
                calls_spent: prior.run.attempted_calls,
                input_bytes_spent: prior.new_input_bytes,
            });
        }
        let metadata: BTreeMap<_, _> = d
            .entries
            .iter()
            .map(|e| (e.candidate_id.as_str(), &e.metadata))
            .collect();
        for record in &prior.mappings {
            let pack = json!({"candidates":record.candidate_ids.iter().map(|id| metadata.get(id.as_str()).copied().unwrap_or(&Value::Null)).collect::<Vec<_>>(),"screening_demand":record.context["screening_demand"]});
            validate_map(&record.value, &pack)?;
        }
        for record in &prior.inspections {
            let cards: Vec<classify::Card> =
                serde_json::from_value(record.context["candidates"].clone())?;
            let mut pack = classify::context(report, &cards, "selection", &[]);
            issue_context::attach(&mut pack, record.context["issue_context"].clone());
            pack["source_context"] = record.context["source_context"].clone();
            pack["inline_issues"] = record.context["inline_issues"].clone();
            validate("inspect", &record.value, &pack)?;
        }
        previous_ms = prior.elapsed_ms;
        d.mappings = prior.mappings;
        d.issue_review = prior.issue_review;
        d.superseded_inspections = prior.superseded_inspections;
        if args.reinspect {
            d.superseded_inspections.extend(prior.inspections);
        } else {
            d.inspections = prior.inspections;
        }
        d.assembled_checks = prior.assembled_checks;
        d.new_input_bytes = prior.new_input_bytes;
        d.new_output_bytes = prior.new_output_bytes;
        d.map_calls = prior.map_calls;
        d.run.attempted_calls = prior.run.attempted_calls;
        d.run.generated_at = prior.run.generated_at;
        let extension = std::mem::take(&mut d.budget_extensions);
        d.budget_extensions = prior.budget_extensions;
        d.budget_extensions.extend(extension);
        d.interruptions = prior.interruptions;
        d.interruptions.extend(prior.run.errors);
        eprintln!(
            "Continuing this run: {} calls already spent, {} remaining; no global model cache",
            d.run.attempted_calls,
            args.limit.saturating_sub(d.run.attempted_calls)
        );
    }
    let session = Session {
        args,
        agent,
        profile,
        cache,
        start: Instant::now()
            .checked_sub(Duration::from_millis(previous_ms))
            .context("invalid elapsed time in continuation")?,
    };
    let issues = args
        .issue_cache
        .as_ref()
        .map(|p| issue_context::Cache::load(p, &report.repository))
        .transpose()?;
    if let Some(path) = &args.resume_discovery {
        let prior: Discovery = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            prior
                .run
                .repository
                .eq_ignore_ascii_case(&report.repository)
                && prior.run.base_sha == report.base_sha
                && prior.run.source_fingerprint == d.run.source_fingerprint,
            "resume discovery does not match this scan"
        );
        ensure!(
            serde_json::to_value(&prior.window)? == serde_json::to_value(&d.window)?
                && serde_json::to_value(&prior.application_pool)?
                    == serde_json::to_value(&d.application_pool)?,
            "resume discovery has a different release window; start a fresh run"
        );
        let eligible: BTreeSet<_> = d.entries.iter().map(|e| e.candidate_id.as_str()).collect();
        let metadata: BTreeMap<_, _> = d
            .entries
            .iter()
            .map(|e| (e.candidate_id.as_str(), &e.metadata))
            .collect();
        // A newly excluded member invalidates its whole mapping/inspection scope;
        // remaining members can then be screened again without stale PR coverage.
        for record in prior.mappings {
            if record
                .candidate_ids
                .iter()
                .all(|id| eligible.contains(id.as_str()))
            {
                let pack = json!({"candidates":record.candidate_ids.iter().map(|id|metadata[id.as_str()]).collect::<Vec<_>>(),"screening_demand":record.context.get("screening_demand")});
                validate_map(&record.value, &pack)?;
                d.mappings.push(record);
                d.run.reused_calls += 1;
            }
        }
        for mut record in prior.inspections {
            if record
                .candidate_ids
                .iter()
                .all(|id| eligible.contains(id.as_str()))
            {
                let cards: Vec<classify::Card> =
                    serde_json::from_value(record.context["candidates"].clone())?;
                ensure!(
                    cards
                        .iter()
                        .map(|c| &c.candidate_id)
                        .collect::<BTreeSet<_>>()
                        == record.candidate_ids.iter().collect::<BTreeSet<_>>(),
                    "resumed inspection scope mismatch"
                );
                let mut pack = classify::context(report, &cards, "selection", &[]);
                issue_context::attach(&mut pack, record.context["issue_context"].clone());
                pack["source_context"] = record.context["source_context"].clone();
                pack["inline_issues"] = record.context["inline_issues"].clone();
                validate_record(&mut record, &pack)?;
                d.inspections.push(record);
                d.run.reused_calls += 1;
            }
        }
    }
    let demand_index = issues.as_ref().map(|cache| {
        cache.screening_index(
            &d.entries
                .iter()
                .map(|e| e.metadata.clone())
                .collect::<Vec<_>>(),
        )
    });
    let already_screened: BTreeSet<_> = d.screened().into_iter().map(str::to_owned).collect();
    let entries: Vec<_> = d
        .entries
        .iter()
        .filter(|e| d.application_pool.as_ref().unwrap().includes(e))
        .cloned()
        .collect();
    let order: Vec<_> = if d.window.is_some() {
        crate::discovery_window::ordered(&entries)
    } else {
        ordered(&entries, report)
    }
    .into_iter()
    .filter(|e| !already_screened.contains(&e.candidate_id))
    .collect();
    let mut chunks = Vec::new();
    let mut at = 0;
    // Fit two mapping requests in the default one-third input allocation.
    // Stable when --limit/--map-calls changes, so cache-only/resume reuses packs.
    let mapping_request_limit = args.max_bytes.min((args.total_bytes / 6).max(8000));
    while at < order.len() {
        let mut count = (args.screen_size as usize).min(order.len() - at);
        let mut low = 1;
        let mut high = count;
        let mut fitting = None;
        while low <= high {
            let middle = low + (high - low) / 2;
            let pack = map_pack(
                report,
                &order[at..at + middle],
                issues.as_ref().zip(demand_index.as_ref()),
            );
            if crate::json_size(&pack)? <= mapping_request_limit {
                fitting = Some((middle, pack));
                low = middle + 1;
            } else {
                high = middle - 1;
            }
        }
        let (n, pack) = fitting.context("single mapping entry exceeds mapping input allowance")?;
        count = n;
        chunks.push(pack);
        at += count;
    }
    d.run.planned_batch_calls = chunks.len();
    // Cached maps don't consume the per-run mapping allowance. New work advances
    // through deterministic unseen batches; every cached result remains in the inbox.
    let mapping_budget = args.total_bytes / 3;
    for pack in chunks {
        let bytes = crate::json_size(&pack)?;
        let allow = d.map_calls < args.map_calls && d.new_input_bytes + bytes <= mapping_budget;
        let before = d.run.attempted_calls;
        if let Some(r) = session.request(&mut d, &pack, "map", allow)? {
            d.mappings.push(r);
        }
        d.map_calls += d.run.attempted_calls - before;
    }
    let mut plausible = VecDeque::new();
    let mut unclear = VecDeque::new();
    // Round-robin nomination rank across batches, so one fork/batch cannot take
    // every inspection slot merely because its result arrived first.
    let maps: Vec<Mapping> = d
        .mappings
        .iter()
        .map(|m| serde_json::from_value(m.value.clone()))
        .collect::<std::result::Result<_, _>>()?;
    for rank in 0..12 {
        for m in &maps {
            if let Some(g) = m.groups.get(rank) {
                if g.basis == Basis::Unclear {
                    unclear.push_back(g.clone());
                } else {
                    plausible.push_back(g.clone());
                }
            }
        }
    }
    let mut leads = Vec::new();
    let mut n = 0;
    while !plausible.is_empty() || !unclear.is_empty() {
        let group = if n % 3 == 2 {
            unclear.pop_front().or_else(|| plausible.pop_front())
        } else {
            plausible.pop_front().or_else(|| unclear.pop_front())
        };
        if let Some(g) = group {
            leads.push(g);
        }
        n += 1;
    }
    // Stable per-feature request keys let a later run reuse prior inspections
    // independently of which other features were mapped in the meantime.
    let mut wave = Vec::new();
    let mut wave_commits = BTreeSet::<String>::new();
    for lead in leads {
        let commits: BTreeSet<String> = lead
            .candidate_ids
            .iter()
            .filter_map(|id| report.features.iter().find(|f| &f.id == id))
            .flat_map(|f| f.commits.iter().cloned())
            .collect();
        if !wave_commits.is_disjoint(&commits) || wave.len() >= args.jobs as usize {
            session.inspect_batch(&mut d, &mut wave, report)?;
            wave_commits.clear();
        }
        if !d.run.errors.is_empty() {
            break;
        }
        if lead
            .candidate_ids
            .iter()
            .all(|id| d.inspected().contains(id.as_str()))
        {
            continue;
        }
        let application = if policy == crate::discovery_application::Policy::Clean {
            if let Some(saved) = d
                .assembled_checks
                .iter()
                .find(|c| c.candidate_ids == lead.candidate_ids)
            {
                saved.check.clone()
            } else {
                let result = review::source_git(report, cache, None)
                    .context("Local Git objects unavailable for assembled check")
                    .and_then(|git| {
                        crate::integration::check_set(report, &lead.candidate_ids, &git)
                    });
                let (check, error) = match result {
                    Ok(check) => (Some(check), None),
                    Err(error) => (None, Some(format!("{error:#}"))),
                };
                d.assembled_checks
                    .push(crate::discovery_application::SetCheck {
                        headline: lead.headline.clone(),
                        candidate_ids: lead.candidate_ids.clone(),
                        check: check.clone(),
                        error,
                    });
                check
            }
        } else {
            None
        };
        if policy == crate::discovery_application::Policy::Clean
            && !application
                .as_ref()
                .is_some_and(|c| crate::discovery_application::clean(&c.status))
        {
            continue;
        }
        let pack = match inspection_pack(
            report,
            std::slice::from_ref(&lead),
            args,
            issues.as_ref(),
            cache,
            application.as_ref(),
        ) {
            Ok(pack) => pack,
            Err(error) => {
                let reason = format!("{error:#}");
                eprintln!("Inspection deferred for {}: {reason}", lead.headline);
                d.inspection_deferrals.push(json!({"headline":lead.headline,"candidate_ids":lead.candidate_ids,"reason":reason}));
                continue;
            }
        };
        d.run.planned_batch_calls += 1;
        wave_commits.extend(commits);
        wave.push(pack);
    }
    session.inspect_batch(&mut d, &mut wave, report)?;
    d.elapsed_ms = session.start.elapsed().as_millis() as u64;
    d.stop_reason=if !d.run.errors.is_empty() {"A CLI request failed; valid results were retained."}else if args.dry_run {"Dry run; no new model calls."}else if d.run.attempted_calls>=args.limit {"Per-run call budget reached."}else if session.start.elapsed().as_secs()>=args.max_seconds {"Per-run time budget reached."}else {"No further planned request fits the mapping/input budget, or the nominated queue is exhausted."}.into();
    write_json(&args.output, &d)?;
    if let Some(path) = &args.html {
        write_atomic(path, crate::discovery_html::html(&d, report).as_bytes())?;
    }
    eprintln!("{} / {} candidates screened · {} inspected · {} new calls ({} mapping), {} reused · {} input bytes · {:.1}s",d.screened().len(),d.application_pool.as_ref().map_or(d.entries.len(), |p| p.eligible),d.inspected().len(),d.run.attempted_calls,d.map_calls,d.run.reused_calls,d.new_input_bytes,d.elapsed_ms as f64/1000.0);
    for e in &d.run.errors {
        eprintln!("{e}");
    }
    Ok(!d.run.errors.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_nomination_does_not_discard_independently_valid_nominations() {
        let pack = json!({"candidates":[{"candidate_id":"a"}],"screening_demand":{"threads":[]}});
        let group = json!({"headline":"Improve parsing","candidate_ids":["a"],"basis":"plausible_feature","benefit":"Handle more input","question":"Does it validate input?","files_to_inspect":[],"issue_queries":[],"demand":{"status":"not_established","reason":{"text":"No observed request","evidence":[]}}});
        let mut bad = group.clone();
        bad["demand"]["status"] = json!("possible_match");
        let raw = json!({"groups":[group,bad]});
        let mut record = Saved {
            key: "test".into(),
            stage: "map".into(),
            candidate_ids: vec!["a".into()],
            value: raw.clone(),
            raw_value: None,
            input_bytes: 0,
            output_bytes: 0,
            duration_ms: 0,
            provider_usage: None,
            context: json!({}),
        };
        validate_record(&mut record, &pack).unwrap();
        assert_eq!(record.value["groups"].as_array().unwrap().len(), 1);
        assert_eq!(record.raw_value, Some(raw));
        assert_eq!(record.context["validation_omissions"][0]["index"], 1);
        validate_map(&record.value, &pack).unwrap();
    }

    #[test]
    fn large_forks_wait_for_other_forks_before_receiving_a_second_bundle() {
        let rounds = fork_rounds(vec![vec!["large"; 25].into(), vec!["small"; 1].into()]);
        assert_eq!(
            rounds.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![12, 1, 12, 1]
        );
        assert_eq!(rounds[1], vec!["small"]);
        assert_eq!(rounds.into_iter().flatten().count(), 26);
    }
    #[test]
    fn demand_claims_require_supplied_thread_evidence() {
        let pack = json!({"candidates":[{"candidate_id":"a"}],"screening_demand":{"threads":[{"evidence_id":"request:https://github.com/upstream/repo/issues/1"}]},"response_schema":{"properties":{"groups":{"items":{"properties":{"demand":{}}}}}}});
        let mut value = json!({"groups":[{"headline":"Improve parsing","candidate_ids":["a"],"basis":"plausible_feature","benefit":"Possible parser fix","question":"Does it handle invalid input?","files_to_inspect":[],"issue_queries":[],"demand":{"status":"observed_request","reason":{"text":"A user reports this problem","evidence":["request:https://github.com/upstream/repo/issues/1"]}}}]});
        validate_map(&value, &pack).unwrap();
        value["groups"][0]["demand"]["reason"]["evidence"] = json!([]);
        assert!(validate_map(&value, &pack).is_err());
        value["groups"][0]["demand"]["status"] = json!("not_established");
        validate_map(&value, &pack).unwrap();
        value["groups"][0]["demand"]["reason"]["evidence"] =
            json!(["request:https://github.com/foreign/repo/issues/1"]);
        assert!(validate_map(&value, &pack).is_err());
    }

    #[test]
    fn mapping_keeps_partial_nominations_but_rejects_invented_candidates() {
        let pack = json!({"candidates":[{"candidate_id":"a","paths":["src/a.rs"]},{"candidate_id":"b","paths":[]}]});
        let mut value = json!({"groups":[{"headline":"Improve parsing","candidate_ids":["a"],"basis":"unclear","benefit":"Possible parser fix","question":"Does it handle invalid input?","files_to_inspect":["missing.rs"],"issue_queries":[]}],"deferred_ids":[]});
        // Missing bookkeeping and an unavailable optional file must not force a repair call.
        validate_map(&value, &pack).unwrap();
        value["groups"][0]["candidate_ids"] = json!(["a", "a"]);
        assert!(validate_map(&value, &pack).is_err());
        value["groups"][0]["candidate_ids"] = json!(["foreign"]);
        assert!(validate_map(&value, &pack).is_err());
    }
    #[test]
    fn recovery_omits_unsupported_relationships_without_weakening_member_evidence() {
        let cards: Vec<_> = ["a", "b"]
            .iter()
            .map(|id| classify::Card {
                candidate_id: id.to_string(),
                title: id.to_string(),
                commits: vec![],
                evidence_ids: vec![format!("commit:{id}")],
                omitted_commits: 0,
            })
            .collect();
        let pack = json!({"scope":"selection","candidates":cards,"response_schema":classify::schema(&cards,"selection")});
        let claim = json!({"text":"Observed change","evidence":["commit:a","commit:b"]});
        let value = json!({"schema_version":1,"scope":"selection","headline":"Improve behavior","summary":claim,
            "groups":[{"name":"Behavior","summary":claim,"members":[{"candidate_id":"a","role":"implementation"},{"candidate_id":"b","role":"supporting"}]}],
            "relationships":[{"from":"a","to":"b","kind":"depends_on","reason":{"text":"Depends on another change","evidence":["commit:a"]},"confidence":"low"}],
            "unclassified":[],"usefulness":[{"candidate_id":"a","verdict":"uncertain","reason":claim},{"candidate_id":"b","verdict":"uncertain","reason":claim}],"limitations":[]});
        let mut record = Saved {
            key: "key".into(),
            stage: "inspect".into(),
            candidate_ids: vec!["a".into(), "b".into()],
            value: value.clone(),
            raw_value: None,
            input_bytes: 1,
            output_bytes: 1,
            duration_ms: 1,
            provider_usage: None,
            context: json!({}),
        };
        validate_record(&mut record, &pack).unwrap();
        assert_eq!(record.raw_value, Some(value.clone()));
        assert!(record.value["relationships"].as_array().unwrap().is_empty());
        assert_eq!(record.value["groups"], value["groups"]);
        assert!(!record.value["limitations"].as_array().unwrap().is_empty());
        record.value = value;
        record.value["groups"][0]["summary"]["evidence"] = json!(["commit:a"]);
        assert!(validate_record(&mut record, &pack).is_err());
    }
}
