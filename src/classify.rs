//! Fork-centric classification with native schemas, bounded calls, and resumable evidence packs.
use crate::{
    metrics, model::*, model_defaults, parallel, review, scan, shortlist, structured, write_atomic,
    write_json,
};
use anyhow::{ensure, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const VERSION: u32 = 1;
pub const PROMPT: &str = include_str!("prompts/classify.md");
#[derive(Args)]
pub struct ClassifyArgs {
    pub report: PathBuf,
    /// Saved measurements for clean-count fork order; must match this scan.
    #[arg(long)]
    pub inventory: Option<PathBuf>,
    /// Inspect these forks only (repeatable).
    #[arg(long = "fork")]
    pub forks: Vec<String>,
    /// Map distinct features broadly, then inspect nominated sets within a persistent budget.
    #[arg(long, conflicts_with_all = ["explore_related", "candidates", "shortlist"])]
    pub discover: bool,
    /// Start an independent run without reading or writing the model-result cache.
    #[arg(long, requires = "discover", conflicts_with = "resume_discovery")]
    pub fresh: bool,
    /// Continue this output's interrupted fresh run using its remaining budget.
    #[arg(long, requires = "fresh", conflicts_with = "resume_discovery")]
    pub continue_run: bool,
    /// Explicitly raise a continued run's call/input ceilings without resetting spend.
    #[arg(long, requires = "continue_run")]
    pub extend_budget: bool,
    /// Archive completed inspections and inspect nominations with the current input policy.
    #[arg(long, requires = "continue_run")]
    pub reinspect: bool,
    /// Screen work since one calendar month before the latest stable GitHub release.
    #[arg(long, requires = "discover", conflicts_with = "release_snapshot")]
    pub since_release: bool,
    /// Use saved raw GitHub release JSON to apply the same window offline.
    #[arg(long, requires = "discover")]
    pub release_snapshot: Option<PathBuf>,
    /// Override the calendar-month overlap with this many days before publication.
    #[arg(long, requires = "discover")]
    pub release_overlap_days: Option<u32>,
    /// Initial discovery pool: clean application, or all work. Release windows default to clean.
    #[arg(long, value_enum, requires = "discover")]
    pub application: Option<crate::discovery_application::Policy>,
    /// Carry forward a matching discovery inbox and screen previously unseen changes.
    #[arg(long, requires = "discover")]
    pub resume_discovery: Option<PathBuf>,
    /// Maximum new mapping calls per discovery run; remaining calls inspect features.
    #[arg(long, default_value_t = 2, requires = "discover")]
    pub map_calls: usize,
    /// Compact candidates per mapping batch (large inputs split automatically).
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u16).range(1..=1000), requires = "discover")]
    pub screen_size: u16,
    /// Total serialized input bytes for NEW discovery calls, including schemas.
    #[arg(long, default_value_t = 240_000, requires = "discover")]
    pub total_bytes: usize,
    /// Start from selected patches and request directly related cached changes in their forks.
    #[arg(long)]
    pub explore_related: bool,
    /// Explicit seed candidate (repeatable); requires --explore-related.
    #[arg(
        long = "candidate",
        requires = "explore_related",
        conflicts_with = "shortlist"
    )]
    pub candidates: Vec<String>,
    /// Seed from a matching saved demand shortlist instead of the fork order.
    #[arg(long, requires = "explore_related")]
    pub shortlist: Option<PathBuf>,
    /// Saved issue/discussion snapshot, searched offline during related exploration.
    #[arg(long)]
    pub issue_cache: Option<PathBuf>,
    /// Match small open-issue catalogs during code inspection, avoiding separate model calls.
    #[arg(long, requires = "discover")]
    pub inline_issues: bool,
    /// Exclude changes fully represented in verified open PRs from this saved snapshot.
    #[arg(long, conflicts_with = "with_prs")]
    pub pr_snapshot: Option<PathBuf>,
    /// Fetch current open upstream PR membership before selecting any model inputs.
    #[arg(long)]
    pub with_prs: bool,
    /// Save the fetched PR snapshot for offline reuse.
    #[arg(long, requires = "with_prs")]
    pub write_pr_snapshot: Option<PathBuf>,
    /// Maximum API requests for --with-prs; no model calls are used for filtering.
    #[arg(long, default_value_t = 500)]
    pub api_budget: usize,
    /// Maximum seeds; default exploration takes one clean candidate per ordered fork.
    #[arg(long, default_value_t = 10)]
    pub seed_limit: usize,
    /// Maximum additional candidates inspected per seed fork.
    #[arg(long, default_value_t = 5)]
    pub related_limit: usize,
    #[arg(long, value_enum)]
    pub agent: Option<structured::Agent>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    /// Forks with candidate changes; 0 includes all.
    #[arg(long, default_value_t = 50, default_value_if("discover", "true", "0"))]
    pub max_forks: usize,
    #[arg(long,default_value_t=10,value_parser=clap::value_parser!(u16).range(1..=40))]
    pub batch_size: u16,
    /// Maximum new CLI calls, including reconciliation and failures; 0 makes no calls.
    #[arg(long, default_value_t = 10, default_value_if("discover", "true", "5"))]
    pub limit: usize,
    /// Concurrent model calls; discovery parallelizes independent inspections only.
    #[arg(long,default_value_t=2,value_parser=clap::value_parser!(u16).range(1..=8))]
    pub jobs: u16,
    /// Maximum serialized input bytes, including schema, per model call.
    #[arg(
        long,
        default_value_t = 60_000,
        default_value_if("discover", "true", "256000")
    )]
    pub max_bytes: usize,
    #[arg(long, default_value_t = 180)]
    pub timeout: u64,
    #[arg(long, default_value_t = 900)]
    pub max_seconds: u64,
    #[arg(long)]
    pub dry_run: bool,
    /// Export exact request JSON and native schema files for inspection.
    #[arg(long)]
    pub plan_dir: Option<PathBuf>,
    #[arg(short, long, default_value = "classification.json")]
    pub output: PathBuf,
    #[arg(long)]
    pub html: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Implementation,
    Supporting,
    Tests,
    Documentation,
    Alternative,
    Unclear,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    DependsOn,
    Supports,
    Alternative,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub candidate_id: String,
    pub role: Role,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub name: String,
    pub summary: Claim,
    pub members: Vec<Member>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_matches: Option<Vec<crate::inline_issues::Match>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relationship {
    pub from: String,
    pub to: String,
    pub kind: Relation,
    pub reason: Claim,
    pub confidence: Confidence,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unclassified {
    pub candidate_id: String,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsefulnessVerdict {
    Useful,
    NotUseful,
    Uncertain,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usefulness {
    pub candidate_id: String,
    pub verdict: UsefulnessVerdict,
    pub reason: Claim,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub schema_version: u32,
    pub scope: String,
    /// Older saved classifications remain readable; new requests require a headline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    pub summary: Claim,
    pub groups: Vec<Group>,
    pub relationships: Vec<Relationship>,
    pub unclassified: Vec<Unclassified>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usefulness: Vec<Usefulness>,
    pub limitations: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Card {
    pub candidate_id: String,
    pub title: String,
    pub commits: Vec<Value>,
    pub evidence_ids: Vec<String>,
    pub omitted_commits: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    pub response: Response,
    pub input_bytes: usize,
    pub duration_ms: u64,
    pub provider_usage: Option<Value>,
}
#[derive(Serialize, Deserialize)]
pub struct ForkResult {
    pub repository: String,
    pub candidate_ids: Vec<String>,
    pub batches: Vec<Option<Record>>,
    pub classification: Option<Response>,
    pub reconciliation: Option<Record>,
    pub status: String,
    pub notes: Vec<String>,
}
#[derive(Serialize, Deserialize)]
pub struct Run {
    pub schema_version: u32,
    pub repository: String,
    pub base_sha: String,
    pub source_fingerprint: String,
    pub generated_at: String,
    pub agent: String,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub planned_batch_calls: usize,
    pub attempted_calls: usize,
    pub reused_calls: usize,
    pub forks_with_candidates: usize,
    pub forks: Vec<ForkResult>,
    pub errors: Vec<String>,
    pub limitations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_filter: Option<PrFilter>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PrFilter {
    pub fetched_at: String,
    pub listing_complete: bool,
    pub verified_prs: usize,
    pub unverified_prs: usize,
    pub excluded_candidates: BTreeMap<String, Vec<u64>>,
    pub partial_candidates: BTreeMap<String, Vec<u64>>,
    pub warnings: Vec<String>,
}
impl PrFilter {
    pub fn from_snapshot(report: &Report, snapshot: &shortlist::PullSnapshot) -> Result<Self> {
        let coverage = shortlist::pull_coverage(report, snapshot)?;
        Ok(Self {
            fetched_at: snapshot.fetched_at.clone(),
            listing_complete: snapshot.listing_complete,
            verified_prs: snapshot
                .pulls
                .iter()
                .filter(|p| p.membership_verified)
                .count(),
            unverified_prs: snapshot
                .pulls
                .iter()
                .filter(|p| !p.membership_verified)
                .count(),
            excluded_candidates: coverage
                .iter()
                .filter(|(_, c)| !c.open_prs.is_empty())
                .map(|(id, c)| (id.clone(), c.open_prs.clone()))
                .collect(),
            partial_candidates: coverage
                .iter()
                .filter(|(_, c)| c.open_prs.is_empty() && !c.partial_prs.is_empty())
                .map(|(id, c)| (id.clone(), c.partial_prs.clone()))
                .collect(),
            warnings: snapshot.warnings.clone(),
        })
    }
}
#[derive(Clone)]
struct Work {
    fork: usize,
    part: Option<usize>,
    pack: Value,
    key: String,
}

pub(crate) fn clip(s: &str, n: usize) -> String {
    let mut n = n.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].into()
}
pub fn card(report: &Report, f: &Feature) -> Card {
    let mut seen = BTreeSet::new();
    let commits:Vec<_>=f.commits.iter().filter_map(|sha|report.commits.get(sha)).filter(|c|seen.insert(c.patch_id.as_deref().unwrap_or(&c.sha))).take(4).map(|c|json!({"sha":c.sha,"parents":c.parents,"subject":clip(&c.subject,200),"message":clip(&c.message,1000),"message_truncated":c.message.len()>1000,"patch":clip(&c.patch,4000),"patch_truncated":c.patch_truncated||c.patch.len()>4000,"files":c.files.iter().take(12).map(|f|json!({"path":f.path,"symbols":f.symbols.iter().take(8).collect::<Vec<_>>()})).collect::<Vec<_>>(),"files_omitted":c.files.len().saturating_sub(12)})).collect();
    let evidence_ids = commits
        .iter()
        .filter_map(|c| c["sha"].as_str())
        .map(|sha| format!("commit:{sha}"))
        .collect();
    Card {
        candidate_id: f.id.clone(),
        title: clip(&f.title, 200),
        omitted_commits: f.commits.len().saturating_sub(commits.len()),
        commits,
        evidence_ids,
    }
}
pub(crate) fn object(fields: Value) -> Value {
    let required: Vec<_> = fields.as_object().unwrap().keys().cloned().collect();
    json!({"type":"object","properties":fields,"required":required,"additionalProperties":false})
}
fn array(items: Value) -> Value {
    json!({"type":"array","items":items})
}
pub fn schema(cards: &[Card], scope: &str) -> Value {
    let ids: Vec<_> = cards.iter().map(|c| &c.candidate_id).collect();
    let evidence: Vec<_> = cards
        .iter()
        .flat_map(|c| &c.evidence_ids)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // An empty evidence catalog has no permissible citations. Use maxItems=0
    // rather than an invalid empty enum.
    let evidence_array = if evidence.is_empty() {
        json!({"type":"array","items":{"type":"string"},"maxItems":0})
    } else {
        array(json!({"type":"string","enum":evidence}))
    };
    let claim = object(json!({"text":{"type":"string"},"evidence":evidence_array}));
    let id = json!({"$ref":"#/$defs/candidate"});
    let claim_ref = json!({"$ref":"#/$defs/claim"});
    let mut root = object(
        json!({"schema_version":{"type":"integer","enum":[VERSION]},"scope":{"type":"string","enum":[scope]},"headline":{"type":"string"},"summary":claim_ref,"groups":array(object(json!({"name":{"type":"string"},"summary":claim_ref,"members":array(object(json!({"candidate_id":id,"role":{"type":"string","enum":["implementation","supporting","tests","documentation","alternative","unclear"]}})))}))),"relationships":array(object(json!({"from":id,"to":id,"kind":{"type":"string","enum":["depends_on","supports","alternative"]},"reason":claim_ref,"confidence":{"type":"string","enum":["low","medium","high"]}}))),"unclassified":array(object(json!({"candidate_id":id,"reason":{"type":"string"}}))),"usefulness":array(object(json!({"candidate_id":id,"verdict":{"type":"string","enum":["useful","not_useful","uncertain"]},"reason":claim_ref}))),"limitations":array(json!({"type":"string"}))}),
    );
    root["$defs"] = json!({"candidate":{"type":"string","enum":ids},"claim":claim});
    root
}
pub fn context(report: &Report, cards: &[Card], scope: &str, prior: &[Response]) -> Value {
    json!({"schema_transport_version":VERSION,"repository":report.repository,"base_sha":report.base_sha,"instructions":PROMPT,"scope":scope,"candidates":cards,"prior_hypotheses":prior,"response_schema":schema(cards,scope),"limitations":["Only supplied candidate evidence is available. Fork/owner identity and application scores are intentionally excluded from semantic judgment.","No tests or code execution. Prior model hypotheses are not established facts."]})
}
pub(crate) fn bounded_context(
    report: &Report,
    cards: &[Card],
    scope: &str,
    prior: &[Response],
    max: usize,
) -> Result<Value> {
    let mut pack = context(report, cards, scope, prior);
    loop {
        if crate::json_size(&pack)? <= max {
            return Ok(pack);
        }
        let mut largest: Option<(usize, usize, &str, usize)> = None;
        for (i, c) in pack["candidates"].as_array().unwrap().iter().enumerate() {
            for (j, m) in c["commits"].as_array().unwrap().iter().enumerate() {
                for field in ["patch", "message"] {
                    let len = m[field].as_str().unwrap_or("").len();
                    if len > 0 && largest.as_ref().is_none_or(|v| len > v.3) {
                        largest = Some((i, j, field, len));
                    }
                }
            }
        }
        let (i, j, field, len) = largest.context(
            "classification metadata/schema exceeds --max-bytes; increase it to include this scope",
        )?;
        let shortened = clip(
            pack["candidates"][i]["commits"][j][field].as_str().unwrap(),
            if len < 128 { 0 } else { len / 2 },
        );
        pack["candidates"][i]["commits"][j][field] = json!(shortened);
        pack["candidates"][i]["commits"][j][format!("{field}_truncated")] = json!(true);
    }
}

pub fn validate(response: &Response, pack: &Value) -> Result<()> {
    let cards: Vec<Card> = serde_json::from_value(pack["candidates"].clone())?;
    let own: BTreeMap<_, BTreeSet<_>> = cards
        .iter()
        .map(|c| {
            (
                c.candidate_id.as_str(),
                c.evidence_ids.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let issue_ids: Vec<String> = pack["issue_context"]["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["evidence_id"].as_str().map(str::to_owned))
        .collect();
    let source_ids: Vec<String> = pack["source_context"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["content"].as_str().is_some_and(|t| !t.is_empty()))
        .filter_map(|s| s["evidence_id"].as_str().map(str::to_owned))
        .collect();
    let all: BTreeSet<_> = own
        .values()
        .flatten()
        .copied()
        .chain(issue_ids.iter().map(String::as_str))
        .chain(source_ids.iter().map(String::as_str))
        .collect();
    ensure!(
        response.schema_version == VERSION
            && Some(response.scope.as_str()) == pack["scope"].as_str(),
        "classification version/scope mismatch"
    );
    if pack["response_schema"]["properties"]
        .get("headline")
        .is_some()
        || response.headline.is_some()
    {
        let headline = response.headline.as_deref().unwrap_or("");
        ensure!(
            !headline.trim().is_empty()
                && headline.chars().count() <= 120
                && headline.split_whitespace().count() <= 12
                && !headline.chars().any(char::is_control),
            "headline must be a nonempty single line of at most 12 words and 120 characters"
        );
    }
    let claim = |c: &Claim, empty: bool| -> Result<()> {
        ensure!(
            !c.text.trim().is_empty() && c.text.len() <= 1600,
            "empty or oversized classification claim"
        );
        ensure!(
            (empty || !c.evidence.is_empty())
                && c.evidence.iter().all(|e| all.contains(e.as_str())),
            "claim lacks supplied evidence"
        );
        Ok(())
    };
    claim(&response.summary, response.groups.is_empty())?;
    let mut assigned = BTreeSet::new();
    let mut names = BTreeSet::new();
    for g in &response.groups {
        ensure!(
            !g.name.trim().is_empty() && g.name.len() <= 200 && names.insert(g.name.to_lowercase()),
            "empty, duplicate or oversized group name"
        );
        ensure!(!g.members.is_empty(), "empty classification group");
        claim(&g.summary, false)?;
        for m in &g.members {
            let evidence = own
                .get(m.candidate_id.as_str())
                .context("unknown group member")?;
            ensure!(
                assigned.insert(m.candidate_id.as_str()),
                "candidate assigned more than once"
            );
            ensure!(
                g.summary
                    .evidence
                    .iter()
                    .any(|e| evidence.contains(e.as_str())),
                "group must cite evidence from every member"
            );
        }
        crate::inline_issues::validate_group(g, pack)?;
    }
    for c in &response.unclassified {
        ensure!(
            own.contains_key(c.candidate_id.as_str()) && assigned.insert(c.candidate_id.as_str()),
            "unknown or duplicate unclassified candidate"
        );
        ensure!(
            !c.reason.trim().is_empty() && c.reason.len() <= 1600,
            "unclassified candidate needs a bounded reason"
        );
    }
    ensure!(
        assigned.len() == cards.len(),
        "classification omitted candidates"
    );
    let mut judged = BTreeSet::new();
    for assessment in &response.usefulness {
        let evidence = own
            .get(assessment.candidate_id.as_str())
            .context("unknown usefulness candidate")?;
        ensure!(
            judged.insert(assessment.candidate_id.as_str()),
            "duplicate usefulness assessment"
        );
        let uncertain = assessment.verdict == UsefulnessVerdict::Uncertain;
        claim(&assessment.reason, uncertain)?;
        ensure!(
            uncertain
                || assessment
                    .reason
                    .evidence
                    .iter()
                    .any(|id| evidence.contains(id.as_str())),
            "usefulness judgment must cite this candidate's code"
        );
    }
    if pack["response_schema"]["properties"]
        .get("usefulness")
        .is_some()
        || !response.usefulness.is_empty()
    {
        ensure!(
            judged.len() == cards.len(),
            "usefulness must assess every candidate exactly once"
        );
    }
    let mut edges = BTreeSet::new();
    let mut dependencies = BTreeMap::<&str, Vec<&str>>::new();
    for r in &response.relationships {
        ensure!(
            r.from != r.to && own.contains_key(r.from.as_str()) && own.contains_key(r.to.as_str()),
            "invalid relationship endpoints"
        );
        ensure!(
            edges.insert((&r.from, &r.to, &r.kind)),
            "duplicate relationship"
        );
        claim(&r.reason, false)?;
        for id in [&r.from, &r.to] {
            ensure!(
                r.reason
                    .evidence
                    .iter()
                    .any(|e| own[id.as_str()].contains(e.as_str())),
                "relationship must cite both endpoints"
            );
        }
        if r.kind == Relation::DependsOn {
            dependencies.entry(&r.from).or_default().push(&r.to);
        }
    }
    for start in dependencies.keys() {
        let mut pending = dependencies[start].clone();
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            ensure!(&id != start, "cyclic dependency hypotheses");
            if seen.insert(id) {
                pending.extend(dependencies.get(id).into_iter().flatten().copied());
            }
        }
    }
    ensure!(
        response
            .limitations
            .iter()
            .all(|s| !s.trim().is_empty() && s.len() <= 1600),
        "invalid limitations"
    );
    Ok(())
}

fn key(agent: &structured::Agent, profile: &review::AgentProfile, pack: &Value) -> Result<String> {
    review::Request {
        agent: agent.name(),
        profile,
        pack,
    }
    .key()
}
fn attach(run: &mut Run, w: &Work, r: Record) {
    let f = &mut run.forks[w.fork];
    if let Some(part) = w.part {
        f.batches[part] = Some(r.clone());
        if f.batches.len() == 1 {
            f.classification = Some(r.response);
            f.status = "complete".into();
        }
    } else {
        f.classification = Some(r.response.clone());
        f.reconciliation = Some(r);
        f.status = "complete".into();
    }
}

pub(crate) fn absolute_output(p: &Path) -> Result<PathBuf> {
    if p.exists() {
        return Ok(p.canonicalize()?);
    }
    let parent = p
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    Ok(parent
        .canonicalize()?
        .join(p.file_name().context("output needs a filename")?))
}

fn execute(
    works: Vec<Work>,
    run: &mut Run,
    args: &ClassifyArgs,
    agent: &structured::Agent,
    profile: &review::AgentProfile,
    cache: &Path,
    start: Instant,
) -> Result<()> {
    let mut pending = Vec::new();
    let mut seen = BTreeMap::<String, Vec<Work>>::new();
    for w in works {
        if let Some(dir) = &args.plan_dir {
            let request_path = dir.join(format!("{}.request.json", w.key));
            let schema_path = dir.join(format!("{}.schema.json", w.key));
            let protected = std::iter::once(&args.report)
                .chain(args.inventory.iter())
                .chain(std::iter::once(&args.output))
                .chain(args.html.iter())
                .map(|p| absolute_output(p))
                .collect::<Result<Vec<_>>>()?;
            for path in [&request_path, &schema_path] {
                ensure!(
                    !protected.contains(&absolute_output(path)?),
                    "exported plan must differ from inputs and result outputs"
                );
            }
            write_json(&request_path, &w.pack)?;
            write_json(&schema_path, &w.pack["response_schema"])?;
        }
        let path = cache.join("classification").join(format!("{}.json", w.key));
        let cached = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Record>(&b).ok())
            .filter(|r| r.key == w.key && validate(&r.response, &w.pack).is_ok());
        if let Some(r) = cached {
            attach(run, &w, r);
            run.reused_calls += 1;
        } else {
            if !seen.contains_key(&w.key) {
                pending.push(w.clone());
            }
            seen.entry(w.key.clone()).or_default().push(w);
        }
    }
    eprintln!(
        "{} unique requests pending · {} cached assignments",
        pending.len(),
        run.reused_calls
    );
    write_json(&args.output, run)?;
    if args.dry_run {
        return Ok(());
    }
    let mut at = 0;
    while at < pending.len()
        && run.attempted_calls < args.limit
        && start.elapsed().as_secs() < args.max_seconds
        && run.errors.is_empty()
    {
        let n = (args.jobs as usize)
            .min(args.limit - run.attempted_calls)
            .min(pending.len() - at);
        let wave = &pending[at..at + n];
        run.attempted_calls += n;
        write_json(&args.output, run)?;
        let timeout = Duration::from_secs(args.timeout)
            .min(Duration::from_secs(args.max_seconds).saturating_sub(start.elapsed()));
        let results = parallel::map(wave, n, |_, w| -> Result<Record> {
            let input_bytes = crate::json_size(&w.pack)?;
            eprintln!(
                "Classifying {} · {} · {} input bytes",
                run.forks[w.fork].repository,
                if w.part.is_some() {
                    "batch"
                } else {
                    "reconciliation"
                },
                input_bytes
            );
            let (value, usage, duration_ms) = structured::run(agent, profile, &w.pack, timeout)?;
            let response: Response = serde_json::from_value(value)?;
            validate(&response, &w.pack)?;
            let record = Record {
                key: w.key.clone(),
                response,
                input_bytes,
                duration_ms,
                provider_usage: usage,
            };
            write_json(
                &cache.join("classification").join(format!("{}.json", w.key)),
                &record,
            )?;
            Ok(record)
        });
        for (w, result) in wave.iter().zip(results) {
            match result {
                Ok(r) => {
                    for alias in &seen[&w.key] {
                        attach(run, alias, r.clone());
                    }
                }
                Err(e) => run
                    .errors
                    .push(format!("{}: {e:#}", run.forks[w.fork].repository)),
            }
            write_json(&args.output, run)?;
        }
        at += n;
    }
    Ok(())
}

pub fn run(
    args: ClassifyArgs,
    config: review::Config,
    cache: &Path,
    decisions: &Path,
) -> Result<bool> {
    ensure!(
        args.timeout > 0 && args.max_seconds > 0 && args.max_bytes >= 8000,
        "positive time limits and at least 8000 input bytes required"
    );
    let agent = args.agent.clone().map(Ok).unwrap_or_else(|| {
        structured::Agent::parse(
            config
                .default_agent
                .as_deref()
                .context("choose --agent codex|claude|grok or set default_agent")?,
        )
    })?;
    let profile = model_defaults::resolve_classification(
        &config,
        agent.name(),
        args.model.as_deref(),
        args.effort.as_deref(),
    )?;
    let protected: Vec<_> = std::iter::once(&args.report)
        .chain(args.inventory.iter())
        .chain(args.shortlist.iter())
        .chain(args.issue_cache.iter())
        .chain(args.pr_snapshot.iter())
        .chain(args.resume_discovery.iter())
        .chain(args.release_snapshot.iter())
        .map(|p| p.canonicalize())
        .collect::<std::io::Result<_>>()?;
    let destinations: Vec<_> = std::iter::once(&args.output)
        .chain(args.html.iter())
        .chain(args.write_pr_snapshot.iter())
        .map(|p| absolute_output(p))
        .collect::<Result<_>>()?;
    ensure!(
        destinations.iter().collect::<BTreeSet<_>>().len() == destinations.len()
            && destinations.iter().all(|p| !protected.contains(p)),
        "classification outputs must differ from each other and inputs"
    );
    let mut report = scan::load_report(&args.report)?;
    if args.discover {
        crate::state::apply(&mut report, decisions)?;
    }
    if args.inventory.is_none() {
        eprintln!(
            "No inventory supplied; selecting forks alphabetically (no application ranking)."
        );
    }
    let inventory: metrics::Inventory = if let Some(p) = &args.inventory {
        serde_json::from_slice(&std::fs::read(p)?)?
    } else {
        metrics::measure(&report, &Default::default())
    };
    let expected: BTreeSet<_> = report.features.iter().map(|f| f.id.as_str()).collect();
    let actual: BTreeSet<_> = inventory
        .candidates
        .iter()
        .map(|f| f.feature_id.as_str())
        .collect();
    ensure!(
        inventory
            .repository
            .eq_ignore_ascii_case(&report.repository)
            && inventory.base_sha == report.base_sha
            && inventory.measured_at == report.generated_at
            && expected == actual,
        "inventory does not match this scan; regenerate it"
    );
    let snapshot: Option<shortlist::PullSnapshot> = if args.with_prs {
        let mut api = crate::github::Github::new(cache.to_owned(), true, args.api_budget)?;
        Some(shortlist::collect_pulls(&mut api, &report.repository)?)
    } else {
        args.pr_snapshot
            .as_ref()
            .map(|p| -> Result<_> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) })
            .transpose()?
    };
    let pr_filter = snapshot
        .as_ref()
        .map(|s| PrFilter::from_snapshot(&report, s))
        .transpose()?;
    if let (Some(path), Some(snapshot)) = (&args.write_pr_snapshot, &snapshot) {
        write_json(path, snapshot)?;
    }
    let excluded: BTreeSet<_> = pr_filter
        .as_ref()
        .map(|f| f.excluded_candidates.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(f) = &pr_filter {
        eprintln!("PR filter: {} fully covered changes excluded; {} partial matches retained; snapshot {}", excluded.len(), f.partial_candidates.len(), f.fetched_at);
        if !f.listing_complete || f.unverified_prs > 0 {
            eprintln!("PR coverage is incomplete; unmatched changes remain eligible.");
        }
        for warning in &f.warnings {
            eprintln!("{}", crate::render::clean(warning));
        }
    } else {
        eprintln!(
            "Open PRs not checked; use --with-prs or --pr-snapshot to filter existing submissions."
        );
    }
    let all = crate::forks::summarize_excluding(&report, &inventory, &excluded);
    let total = all.iter().filter(|f| !f.groups.is_empty()).count();
    for name in &args.forks {
        ensure!(
            all.iter().any(|f| f.repository.eq_ignore_ascii_case(name)),
            "fork not present in scan: {name}"
        );
    }
    let selected: Vec<_> = all
        .into_iter()
        .filter(|f| {
            !f.groups.is_empty()
                && (args.forks.is_empty()
                    || args
                        .forks
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&f.repository)))
        })
        .take(if args.max_forks == 0 {
            usize::MAX
        } else {
            args.max_forks
        })
        .collect();
    if args.discover {
        return crate::discovery::run(
            &args,
            &agent,
            &profile,
            &report,
            &inventory,
            crate::related::SelectedForks {
                forks: &selected,
                pr_filter: pr_filter.as_ref(),
            },
            cache,
        );
    }
    if args.explore_related {
        return crate::related::run(
            &args,
            &agent,
            &profile,
            &report,
            &inventory,
            crate::related::SelectedForks {
                forks: &selected,
                pr_filter: pr_filter.as_ref(),
            },
            cache,
        );
    }
    let by_id: BTreeMap<_, _> = report.features.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut run=Run{pr_filter, schema_version:VERSION,repository:report.repository.clone(),base_sha:report.base_sha.clone(),source_fingerprint:shortlist::fingerprint(&report),generated_at:chrono::Utc::now().to_rfc3339(),agent:agent.name().into(),requested_model:profile.model.clone(),requested_effort:profile.effort.clone(),planned_batch_calls:0,attempted_calls:0,reused_calls:0,forks_with_candidates:total,forks:vec![],errors:vec![],limitations:vec!["Classifications and dependencies are unverified model hypotheses, not quality judgments or adoption recommendations.".into(),"Completion covers observed candidates in this scan, not undiscovered fork changes. Empty sets are excluded. Large forks need all batches plus reconciliation; partial results are not whole-fork summaries.".into()]};
    run.limitations.extend(report.coverage.warnings.clone());
    let mut per_fork = Vec::new();
    let mut cards_by_fork = Vec::new();
    for (i, f) in selected.iter().enumerate() {
        let ids: BTreeSet<_> = f
            .groups
            .iter()
            .flat_map(|g| g.candidate_ids.iter())
            .collect();
        let cards: Vec<_> = ids
            .iter()
            .map(|id| card(&report, by_id[id.as_str()]))
            .collect();
        let chunks: Vec<_> = cards.chunks(args.batch_size as usize).collect();
        let mut work = Vec::new();
        for (part, chunk) in chunks.iter().enumerate() {
            let pack = bounded_context(
                &report,
                chunk,
                if chunks.len() == 1 { "fork" } else { "batch" },
                &[],
                args.max_bytes,
            )?;
            work.push(Work {
                fork: i,
                part: Some(part),
                key: key(&agent, &profile, &pack)?,
                pack,
            });
        }
        run.forks.push(ForkResult {
            repository: f.repository.clone(),
            candidate_ids: cards.iter().map(|c| c.candidate_id.clone()).collect(),
            batches: vec![None; chunks.len()],
            classification: None,
            reconciliation: None,
            status: "pending".into(),
            notes: vec![],
        });
        cards_by_fork.push(cards);
        per_fork.push(work);
    }
    // Round robin across forks, so one huge fork does not consume the call budget.
    let mut works = Vec::new();
    for round in 0..per_fork.iter().map(Vec::len).max().unwrap_or(0) {
        for fork in &per_fork {
            if let Some(w) = fork.get(round) {
                works.push(w.clone());
            }
        }
    }
    run.planned_batch_calls = works.iter().map(|w| &w.key).collect::<BTreeSet<_>>().len();
    eprintln!("{} forks · {} candidate appearances · {} distinct batch requests · at most {} new calls, {} workers · {} / {}",run.forks.len(),run.forks.iter().map(|f|f.candidate_ids.len()).sum::<usize>(),run.planned_batch_calls,args.limit,args.jobs,profile.model.as_deref().unwrap_or("CLI default"),profile.effort.as_deref().unwrap_or("CLI default"));
    let start = Instant::now();
    execute(works, &mut run, &args, &agent, &profile, cache, start)?;
    let mut reconciliation = Vec::new();
    for (i, f) in run.forks.iter().enumerate() {
        if f.batches.len() > 1 && f.batches.iter().all(Option::is_some) {
            let prior: Vec<_> = f
                .batches
                .iter()
                .flatten()
                .map(|r| r.response.clone())
                .collect();
            // Reconciliation reads compact primary metadata and cached hypotheses;
            // it must not pretend to have re-reviewed full patches.
            let mut cards = cards_by_fork[i].clone();
            for card in &mut cards {
                for c in &mut card.commits {
                    c["patch"] = json!("");
                    c["patch_truncated"] = json!(true);
                    c["message"] = json!("");
                    c["message_truncated"] = json!(true);
                }
            }
            match bounded_context(&report, &cards, "fork", &prior, args.max_bytes) {
                Ok(pack) => reconciliation.push(Work {
                    fork: i,
                    part: None,
                    key: key(&agent, &profile, &pack)?,
                    pack,
                }),
                Err(e) => run
                    .errors
                    .push(format!("{} reconciliation: {e:#}", f.repository)),
            }
        }
    }
    if !reconciliation.is_empty() {
        execute(
            reconciliation,
            &mut run,
            &args,
            &agent,
            &profile,
            cache,
            start,
        )?;
    }
    for f in &mut run.forks {
        if f.classification.is_none() {
            f.status = if f.batches.iter().any(Option::is_some) {
                "partial"
            } else {
                "pending"
            }
            .into();
            if f.batches.len() > 1 {
                f.notes.push("Whole-fork classification requires all batches and a reconciliation call; rerun to resume cached work.".into());
            }
        }
    }
    write_json(&args.output, &run)?;
    if let Some(p) = &args.html {
        write_atomic(p, html(&run, &report).as_bytes())?;
    }
    let complete = run.forks.iter().filter(|f| f.status == "complete").count();
    eprintln!(
        "{complete}/{} forks classified · {} new calls · {} cache reuses · {} errors · {}",
        run.forks.len(),
        run.attempted_calls,
        run.reused_calls,
        run.errors.len(),
        args.output.display()
    );
    for e in &run.errors {
        eprintln!("{}", crate::render::clean(e));
    }
    Ok(!args.dry_run && (complete < run.forks.len() || !run.errors.is_empty()))
}

pub fn html(run: &Run, report: &Report) -> String {
    crate::classification_html::html(run, report, &[], None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Value, Response) {
        let cards: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|id| Card {
                candidate_id: id.into(),
                title: id.into(),
                commits: vec![],
                evidence_ids: vec![format!("commit:{id}")],
                omitted_commits: 0,
            })
            .collect();
        let pack =
            json!({"scope":"fork","candidates":cards,"response_schema":schema(&cards,"fork")});
        let response = serde_json::from_value(json!({"schema_version":1,"scope":"fork","headline":"Update behavior and accompanying tests","summary":{"text":"Related work","evidence":["commit:a","commit:b"]},"groups":[{"name":"Feature","summary":{"text":"Implementation and accompanying tests","evidence":["commit:a","commit:b"]},"members":[{"candidate_id":"a","role":"implementation"},{"candidate_id":"b","role":"tests"}]}],"relationships":[],"unclassified":[],"limitations":[]})).unwrap();
        let mut response: Response = response;
        response.usefulness = ["a", "b"]
            .into_iter()
            .map(|id| Usefulness {
                candidate_id: id.into(),
                verdict: UsefulnessVerdict::Uncertain,
                reason: Claim {
                    text: "Insufficient context to judge benefit".into(),
                    evidence: vec![format!("commit:{id}")],
                },
            })
            .collect();
        (pack, response)
    }
    #[test]
    fn usefulness_requires_complete_candidate_specific_evidence() {
        let (pack, response) = fixture();
        let mut bad = response.clone();
        bad.usefulness.pop();
        assert!(validate(&bad, &pack).is_err());
        let mut bad = response.clone();
        bad.usefulness[0].verdict = UsefulnessVerdict::NotUseful;
        bad.usefulness[0].reason.evidence = vec!["commit:b".into()];
        assert!(
            validate(&bad, &pack).is_err(),
            "cannot hide one candidate using another's code"
        );
        bad.usefulness[0].reason.evidence = vec!["commit:a".into()];
        validate(&bad, &pack).unwrap();
        bad.usefulness.push(bad.usefulness[0].clone());
        assert!(validate(&bad, &pack).is_err());
        let mut uncertain = response;
        uncertain.usefulness[0].reason.evidence.clear();
        validate(&uncertain, &pack).unwrap();
    }
    #[test]
    fn headlines_are_required_for_new_requests_but_old_results_remain_readable() {
        let (mut pack, response) = fixture();
        validate(&response, &pack).unwrap();
        let mut raw = serde_json::to_value(&response).unwrap();
        raw.as_object_mut().unwrap().remove("headline");
        let legacy: Response = serde_json::from_value(raw).unwrap();
        assert!(validate(&legacy, &pack).is_err());
        for headline in [
            "",
            " \t ",
            "First line\nSecond line",
            &"x".repeat(121),
            &"word ".repeat(13),
        ] {
            let mut bad = response.clone();
            bad.headline = Some(headline.into());
            assert!(validate(&bad, &pack).is_err());
        }
        pack["response_schema"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("headline");
        validate(&legacy, &pack).unwrap();
    }
    #[test]
    fn partition_and_member_evidence_are_checked_beyond_json_shape() {
        let (pack, response) = fixture();
        validate(&response, &pack).unwrap();
        let mut bad = response.clone();
        bad.groups[0].members.pop();
        assert!(validate(&bad, &pack).is_err(), "omitted candidate");
        bad.unclassified.push(Unclassified {
            candidate_id: "b".into(),
            reason: "Insufficient diff".into(),
        });
        validate(&bad, &pack).unwrap();
        bad.unclassified.push(bad.unclassified[0].clone());
        assert!(validate(&bad, &pack).is_err(), "duplicate candidate");
        let mut bad = response.clone();
        bad.groups[0].summary.evidence.pop();
        assert!(validate(&bad, &pack).is_err(), "one member has no citation");
        let mut bad = response.clone();
        bad.summary.evidence.push("commit:invented".into());
        assert!(validate(&bad, &pack).is_err());
        let mut raw = serde_json::to_value(&response).unwrap();
        raw["quality_score"] = json!(100);
        assert!(serde_json::from_value::<Response>(raw).is_err());
    }
    #[test]
    fn relationships_need_both_endpoints_and_dependencies_cannot_cycle() {
        let (pack, mut response) = fixture();
        let edge = Relationship {
            from: "a".into(),
            to: "b".into(),
            kind: Relation::DependsOn,
            reason: Claim {
                text: "Uses implementation".into(),
                evidence: vec!["commit:a".into(), "commit:b".into()],
            },
            confidence: Confidence::Low,
        };
        response.relationships.push(edge.clone());
        validate(&response, &pack).unwrap();
        response.relationships[0].reason.evidence.pop();
        assert!(validate(&response, &pack).is_err());
        response.relationships[0] = edge.clone();
        response.relationships.push(Relationship {
            from: "b".into(),
            to: "a".into(),
            ..edge
        });
        assert!(validate(&response, &pack).is_err());
        response.relationships[1].kind = Relation::Supports;
        validate(&response, &pack).unwrap();
    }
    #[test]
    fn schema_is_closed_and_ids_are_restricted_to_the_request() {
        let (pack, _) = fixture();
        let schema = &pack["response_schema"];
        assert_eq!(schema["$defs"]["candidate"]["enum"], json!(["a", "b"]));
        fn check(v: &Value) {
            if v["type"] == "object" {
                assert_eq!(v["additionalProperties"], false);
                let keys: BTreeSet<_> = v["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                let required: BTreeSet<_> = v["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap())
                    .collect();
                assert_eq!(keys, required);
            }
            match v {
                Value::Object(o) => o.values().for_each(check),
                Value::Array(a) => a.iter().for_each(check),
                _ => {}
            }
        }
        check(schema);
    }
    #[test]
    fn issue_citations_cannot_replace_member_or_dependency_commit_evidence() {
        let (mut pack, mut response) = fixture();
        let id = "request:https://github.com/upstream/example/issues/1";
        crate::issue_context::attach(&mut pack, json!({"threads":[{"evidence_id":id}]}));
        response.groups[0].summary.evidence.push(id.into());
        validate(&response, &pack).unwrap();
        let ids = pack["response_schema"]["$defs"]["claim"]["properties"]["evidence"]["items"]
            ["enum"]
            .as_array()
            .unwrap();
        assert!(ids.contains(&json!(id)));
        response.groups[0]
            .summary
            .evidence
            .retain(|e| e != "commit:b");
        assert!(validate(&response, &pack).is_err());
        response.groups[0].summary.evidence.push("commit:b".into());
        response.relationships.push(Relationship {
            from: "a".into(),
            to: "b".into(),
            kind: Relation::DependsOn,
            reason: Claim {
                text: "Shared issue is not a dependency".into(),
                evidence: vec!["commit:a".into(), id.into()],
            },
            confidence: Confidence::Low,
        });
        assert!(validate(&response, &pack).is_err());
        response.relationships.clear();
        response
            .summary
            .evidence
            .push("request:https://github.com/upstream/example/issues/999".into());
        assert!(validate(&response, &pack).is_err());
    }
}
