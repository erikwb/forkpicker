//! Schema-directed, one-hop retrieval from the saved fork evidence.
use crate::{
    classify::{self, Card, ClassifyArgs},
    forks, metrics,
    model::Report,
    parallel, review, shortlist, structured, write_json,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::{Duration, Instant},
};

const PROMPT: &str = include_str!("prompts/related.md");
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub candidate_id: String,
    pub seed_id: String,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub inspect: Vec<Request>,
    pub limitations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_queries: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    pub value: Value,
    pub input_bytes: usize,
    pub duration_ms: u64,
    pub provider_usage: Option<Value>,
}
#[derive(Serialize, Deserialize)]
pub struct Exploration {
    pub repository: String,
    pub seed_ids: Vec<String>,
    pub catalog_candidates: usize,
    pub retrieval: Option<Record>,
    pub requested: Vec<Request>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_queries: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_context: Option<Value>,
    pub classification: Option<Record>,
}
#[derive(Serialize, Deserialize)]
pub struct Experiment {
    #[serde(flatten)]
    pub run: classify::Run,
    pub selection_policy: String,
    pub exploration: Vec<Exploration>,
}
#[derive(Clone)]
struct Work {
    fork: usize,
    retrieval: bool,
    pack: Value,
    key: String,
}

pub fn validate_selection(value: &Value, pack: &Value) -> Result<Selection> {
    let result: Selection = serde_json::from_value(value.clone())?;
    ensure!(
        result.issue_queries.len() <= 3
            && (pack.get("issue_context").is_some() || result.issue_queries.is_empty())
            && result.issue_queries.iter().all(|q| !q.trim().is_empty()
                && q.len() <= 200
                && !q.chars().any(char::is_control)),
        "invalid issue-cache queries"
    );
    let seeds: BTreeSet<_> = pack["seeds"]
        .as_array()
        .context("missing seeds")?
        .iter()
        .filter_map(|v| v["candidate_id"].as_str())
        .collect();
    let allowed: BTreeSet<_> = pack["catalog"]
        .as_array()
        .context("missing catalog")?
        .iter()
        .filter_map(|v| v["candidate_id"].as_str())
        .collect();
    ensure!(
        result.inspect.len() <= pack["related_limit"].as_u64().unwrap_or(0) as usize,
        "too many related requests"
    );
    let mut seen = BTreeSet::new();
    for r in &result.inspect {
        ensure!(
            allowed.contains(r.candidate_id.as_str())
                && !seeds.contains(r.candidate_id.as_str())
                && seeds.contains(r.seed_id.as_str())
                && seen.insert(&r.candidate_id),
            "unknown, duplicate or out-of-scope retrieval"
        );
        ensure!(
            !r.reason.trim().is_empty() && r.reason.len() <= 1000,
            "retrieval needs a bounded reason"
        );
    }
    ensure!(
        result
            .limitations
            .iter()
            .all(|s| !s.trim().is_empty() && s.len() <= 1600),
        "invalid retrieval limitations"
    );
    Ok(result)
}
fn validate(w: &Work, value: &Value) -> Result<()> {
    if w.retrieval {
        validate_selection(value, &w.pack)?;
    } else {
        let response: classify::Response = serde_json::from_value(value.clone())?;
        classify::validate(&response, &w.pack)?;
    }
    Ok(())
}
fn work(
    fork: usize,
    retrieval: bool,
    pack: Value,
    agent: &structured::Agent,
    profile: &review::AgentProfile,
) -> Result<Work> {
    let key = review::Request {
        agent: agent.name(),
        profile,
        pack: &pack,
    }
    .key()?;
    Ok(Work {
        fork,
        retrieval,
        pack,
        key,
    })
}
fn attach(e: &mut Experiment, w: &Work, r: Record) -> Result<()> {
    if w.retrieval {
        let selection = validate_selection(&r.value, &w.pack)?;
        e.exploration[w.fork].requested = selection.inspect;
        e.exploration[w.fork].issue_queries = selection.issue_queries;
        e.exploration[w.fork].retrieval = Some(r);
        e.run.forks[w.fork].status = "partial".into();
    } else {
        let response: classify::Response = serde_json::from_value(r.value.clone())?;
        let seeds: BTreeSet<_> = e.exploration[w.fork]
            .seed_ids
            .iter()
            .map(String::as_str)
            .collect();
        for g in &response.groups {
            if !g
                .members
                .iter()
                .any(|m| seeds.contains(m.candidate_id.as_str()))
            {
                e.run.forks[w.fork].notes.push(format!("Separate group: {}. Grouping alone does not establish a dependency on the seed.",g.name));
            }
        }
        e.run.forks[w.fork].classification = Some(response);
        e.exploration[w.fork].issue_context = w.pack.get("issue_context").cloned();
        e.run.forks[w.fork].candidate_ids = w.pack["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["candidate_id"].as_str().unwrap().into())
            .collect();
        e.run.forks[w.fork].status = "complete".into();
        e.exploration[w.fork].classification = Some(r);
    }
    Ok(())
}
fn execute(
    works: Vec<Work>,
    e: &mut Experiment,
    args: &ClassifyArgs,
    agent: &structured::Agent,
    profile: &review::AgentProfile,
    cache: &Path,
    start: Instant,
) -> Result<()> {
    let mut pending = Vec::new();
    let mut aliases = BTreeMap::<String, Vec<Work>>::new();
    for w in works {
        if let Some(dir) = &args.plan_dir {
            for (suffix, value) in [("request", &w.pack), ("schema", &w.pack["response_schema"])] {
                let path = dir.join(format!("{}.{}.json", w.key, suffix));
                let dest = classify::absolute_output(&path)?;
                for p in std::iter::once(&args.report)
                    .chain(args.inventory.iter())
                    .chain(args.shortlist.iter())
                    .chain(args.issue_cache.iter())
                    .chain(std::iter::once(&args.output))
                    .chain(args.html.iter())
                {
                    ensure!(
                        dest != classify::absolute_output(p)?,
                        "plan export collides with input or output"
                    );
                }
                write_json(&path, value)?;
            }
        }
        let cached = std::fs::read(cache.join("related").join(format!("{}.json", w.key)))
            .ok()
            .and_then(|b| serde_json::from_slice::<Record>(&b).ok())
            .filter(|r| r.key == w.key && validate(&w, &r.value).is_ok());
        if let Some(r) = cached {
            attach(e, &w, r)?;
            e.run.reused_calls += 1;
        } else {
            if !aliases.contains_key(&w.key) {
                pending.push(w.clone());
            }
            aliases.entry(w.key.clone()).or_default().push(w);
        }
    }
    write_json(&args.output, e)?;
    if args.dry_run {
        return Ok(());
    }
    let mut at = 0;
    while at < pending.len()
        && e.run.attempted_calls < args.limit
        && start.elapsed().as_secs() < args.max_seconds
        && e.run.errors.is_empty()
    {
        let n = (args.jobs as usize)
            .min(args.limit - e.run.attempted_calls)
            .min(pending.len() - at);
        let wave = &pending[at..at + n];
        e.run.attempted_calls += n;
        write_json(&args.output, e)?;
        let timeout = Duration::from_secs(args.timeout)
            .min(Duration::from_secs(args.max_seconds).saturating_sub(start.elapsed()));
        let results = parallel::map(wave, n, |_, w| -> Result<Record> {
            let input_bytes = crate::json_size(&w.pack)?;
            eprintln!(
                "{} · {} · {} bytes",
                e.exploration[w.fork].repository,
                if w.retrieval {
                    "requesting related patches"
                } else {
                    "classifying seed and requested patches"
                },
                input_bytes
            );
            let (value, provider_usage, duration_ms) =
                structured::run(agent, profile, &w.pack, timeout)?;
            let r = Record {
                key: w.key.clone(),
                value,
                input_bytes,
                duration_ms,
                provider_usage,
            };
            // Keep rejected structured responses inspectable without treating them
            // as validated cache hits or spending again merely to diagnose them.
            if let Err(error) = validate(w, &r.value) {
                let path = cache
                    .join("related-rejected")
                    .join(format!("{}.json", w.key));
                write_json(&path, &r)?;
                anyhow::bail!("{error:#}; response saved to {}", path.display());
            }
            write_json(&cache.join("related").join(format!("{}.json", w.key)), &r)?;
            Ok(r)
        });
        for (w, result) in wave.iter().zip(results) {
            match result {
                Ok(r) => {
                    for alias in &aliases[&w.key] {
                        attach(e, alias, r.clone())?;
                    }
                }
                Err(err) => e
                    .run
                    .errors
                    .push(format!("{}: {err:#}", e.exploration[w.fork].repository)),
            }
            write_json(&args.output, e)?;
        }
        at += n;
    }
    Ok(())
}

pub struct SelectedForks<'a> {
    pub forks: &'a [forks::Fork],
    pub pr_filter: Option<&'a classify::PrFilter>,
}

pub fn run(
    args: &ClassifyArgs,
    agent: &structured::Agent,
    profile: &review::AgentProfile,
    report: &Report,
    inventory: &metrics::Inventory,
    selected: SelectedForks<'_>,
    cache: &Path,
) -> Result<bool> {
    let SelectedForks { forks, pr_filter } = selected;
    ensure!(
        args.seed_limit > 0 && args.related_limit <= 20,
        "positive --seed-limit and --related-limit at most 20 required"
    );
    let issues = args
        .issue_cache
        .as_ref()
        .map(|p| crate::issue_context::Cache::load(p, &report.repository))
        .transpose()?;
    let demand_shortlist: Option<shortlist::Shortlist> = args
        .shortlist
        .as_ref()
        .map(|path| -> anyhow::Result<_> { Ok(serde_json::from_slice(&std::fs::read(path)?)?) })
        .transpose()?;
    let explicit = if let Some(s) = &demand_shortlist {
        Some(shortlist::validate_selection(s, report)?)
    } else if !args.candidates.is_empty() {
        Some(args.candidates.clone())
    } else {
        None
    };
    let by_id: BTreeMap<_, _> = report.features.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut chosen = BTreeMap::<usize, Vec<String>>::new();
    let mut seen = BTreeSet::new();
    let policy;
    if let Some(ids) = explicit {
        policy="Explicit patch seeds in supplied order; each shared seed is assigned to its first eligible fork in the inventory order. Other source forks are not explored.";
        for id in ids
            .into_iter()
            .filter(|id| !pr_filter.is_some_and(|p| p.excluded_candidates.contains_key(id)))
            .take(args.seed_limit)
        {
            ensure!(
                by_id.contains_key(id.as_str()) && seen.insert(id.clone()),
                "unknown or duplicate seed {id}"
            );
            let i=forks.iter().position(|f|f.groups.iter().any(|g|g.candidate_ids.contains(&id))).with_context(||format!("seed {id} has no nonempty candidate in selected forks; broaden --max-forks/--fork"))?;
            chosen.entry(i).or_default().push(id);
        }
    } else {
        ensure!(args.inventory.is_some(),"default related exploration requires --inventory for application evidence, or explicit --candidate/--shortlist seeds");
        policy="One clean candidate per fork in the inventory's clean-group order; newest author date first within a fork, candidate ID breaks ties. Duplicate patch seeds across forks are skipped. This measures adoption friction, not demand or quality.";
        let target = inventory
            .integration
            .as_ref()
            .map(|s| s.target_sha.as_str())
            .unwrap_or(&report.base_sha);
        for (i, fork) in forks.iter().enumerate() {
            let eligible: BTreeSet<_> = fork
                .groups
                .iter()
                .flat_map(|g| g.candidate_ids.iter().map(String::as_str))
                .collect();
            let mut facts: Vec<_> = inventory
                .candidates
                .iter()
                .filter(|c| {
                    eligible.contains(c.feature_id.as_str())
                        && !seen.contains(&c.feature_id)
                        && !matches!(c.status.as_str(), "dismissed" | "adopted")
                        && c.integration.as_ref().is_some_and(|r| {
                            r.target_sha == target
                                && matches!(r.status.as_str(), "clean" | "clean-three-way")
                        })
                })
                .collect();
            facts.sort_by(|a, b| {
                b.latest_author_date
                    .cmp(&a.latest_author_date)
                    .then_with(|| a.feature_id.cmp(&b.feature_id))
            });
            if let Some(c) = facts.first() {
                seen.insert(c.feature_id.clone());
                chosen.insert(i, vec![c.feature_id.clone()]);
            }
            if seen.len() >= args.seed_limit {
                break;
            }
        }
    }
    ensure!(
        !chosen.is_empty() || pr_filter.is_some(),
        "no eligible seeds selected"
    );
    let mut e=Experiment {selection_policy:policy.into(),exploration:vec![],run:classify::Run {pr_filter:pr_filter.cloned(), schema_version:1,repository:report.repository.clone(),base_sha:report.base_sha.clone(),source_fingerprint:shortlist::fingerprint(report),generated_at:chrono::Utc::now().to_rfc3339(),agent:agent.name().into(),requested_model:profile.model.clone(),requested_effort:profile.effort.clone(),planned_batch_calls:0,attempted_calls:0,reused_calls:0,forks_with_candidates:forks.len(),forks:vec![],errors:vec![],limitations:vec!["One-hop exploration of saved candidate evidence only; no live fork fetch, checkout, tests, or full code review. Titles are retrieval hints, not evidence of a relationship. Completion covers the seed scope, not the whole fork.".into()]}};
    e.run.limitations.extend(report.coverage.warnings.clone());
    let mut retrieval = Vec::new();
    let mut seed_cards = Vec::<Vec<Card>>::new();
    for (source_index, ids) in chosen {
        let f = &forks[source_index];
        let i = e.exploration.len();
        let seeds: Vec<_> = ids
            .iter()
            .map(|id| classify::card(report, by_id[id.as_str()]))
            .collect();
        let catalog:Vec<_>=f.groups.iter().flat_map(|g|&g.candidate_ids).filter(|id|!ids.contains(id)).map(|id|json!({"candidate_id":id,"title":classify::clip(&by_id[id.as_str()].title,120)})).collect();
        e.exploration.push(Exploration {
            repository: f.repository.clone(),
            seed_ids: ids.clone(),
            catalog_candidates: catalog.len(),
            retrieval: None,
            requested: vec![],
            issue_queries: vec![],
            issue_context: None,
            classification: None,
        });
        e.run.forks.push(classify::ForkResult {repository:f.repository.clone(),candidate_ids:ids.clone(),batches:vec![],classification:None,reconciliation:None,status:"pending".into(),notes:vec![format!("{} seed(s); {} additional candidates available by title. At most {} requested patches will be inspected. Completion covers this seed scope, not the whole fork.",ids.len(),catalog.len(),args.related_limit)]});
        if (!catalog.is_empty() && args.related_limit > 0) || issues.is_some() {
            let candidate_ids: Vec<_> = catalog.iter().map(|c| c["candidate_id"].clone()).collect();
            let mut schema = classify::object(
                json!({"inspect":{"type":"array","maxItems":args.related_limit,"items":classify::object(json!({"candidate_id":{"type":"string","enum":candidate_ids},"seed_id":{"type":"string","enum":ids},"reason":{"type":"string"}}))},"limitations":{"type":"array","items":{"type":"string"}}}),
            );
            if catalog.is_empty() {
                schema["properties"]["inspect"] =
                    json!({"type":"array","maxItems":0,"items":{"type":"string"}});
            }
            if issues.is_some() {
                schema["properties"]["issue_queries"] =
                    json!({"type":"array","maxItems":3,"items":{"type":"string","maxLength":200}});
                schema["required"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("issue_queries"));
            }
            let mut pack = json!({"transport_version":1,"instructions":PROMPT,"repository":report.repository,"base_sha":report.base_sha,"seeds":seeds,"catalog":catalog,"related_limit":args.related_limit,"response_schema":schema});
            if let Some(cache) = &issues {
                crate::issue_context::attach(&mut pack, cache.context(&seeds, &[]));
                pack["issue_search_instructions"] = json!("You can ask up to three short issue_queries to search the WHOLE saved project issue/discussion title/body cache. Use concrete user symptoms or code identifiers; avoid merely repeating broad repository/subsystem names. The application will retrieve up to five lexical hits per query for final classification, with ten excerpts total. Empty queries are valid if supplied context suffices. Shared issue context can suggest patch connections, but only supplied code supports implementation or dependency claims. Querying the cache makes no extra model or network calls.");
            }
            ensure!(crate::json_size(&pack)?<=args.max_bytes,"{} complete catalog and seed evidence exceed --max-bytes; increase it before running (no silent catalog omissions)",f.repository);
            retrieval.push(work(i, true, pack, agent, profile)?);
        }
        seed_cards.push(seeds);
    }
    e.run.planned_batch_calls = retrieval.len() + e.exploration.len();
    eprintln!("{} seeds across {} forks · at most {} initial retrieval/classification requests · {} new-call cap",seen.len(),e.exploration.len(),e.run.planned_batch_calls,args.limit);
    let start = Instant::now();
    execute(retrieval, &mut e, args, agent, profile, cache, start)?;
    let mut final_work = Vec::new();
    for (i, f) in e.exploration.iter().enumerate() {
        if ((f.catalog_candidates > 0 && args.related_limit > 0) || issues.is_some())
            && f.retrieval.is_none()
        {
            continue;
        }
        let mut cards = seed_cards[i].clone();
        for request in &f.requested {
            cards.push(classify::card(report, by_id[request.candidate_id.as_str()]));
        }
        let extra = json!({"seed_ids":f.seed_ids,"retrieval_hypotheses":f.requested,"focus_instructions":"This is a seed selection, NOT a whole-fork classification. Requested patches were chosen from titles and may be irrelevant. Check the supplied code and organize coherent feature components, supporting work, tests, fixes, and competing variants. Independent coherent work discovered incidentally may remain in separate groups; do not invent dependencies or force it into the seed feature. Put changes with insufficient evidence in unclassified. Retrieval reasons are hypotheses, not evidence. Do not invent demand or correctness. No further patch expansion was performed."});
        let issue_context = issues
            .as_ref()
            .map(|cache| cache.context(&cards, &f.issue_queries));
        // Reserve the exact issue context/schema extension before clipping code.
        let mut preview = classify::context(report, &cards, "selection", &[]);
        let before = crate::json_size(&preview)?;
        if let Some(context) = &issue_context {
            crate::issue_context::attach(&mut preview, context.clone());
        }
        let reserve =
            crate::json_size(&extra)? + crate::json_size(&preview)?.saturating_sub(before);
        let mut pack = classify::bounded_context(
            report,
            &cards,
            "selection",
            &[],
            args.max_bytes.saturating_sub(reserve),
        )?;
        pack.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        if let Some(context) = issue_context {
            crate::issue_context::attach(&mut pack, context);
        }
        ensure!(
            crate::json_size(&pack)? <= args.max_bytes,
            "expanded evidence exceeds byte budget"
        );
        final_work.push(work(i, false, pack, agent, profile)?);
    }
    execute(final_work, &mut e, args, agent, profile, cache, start)?;
    for f in &e.exploration {
        eprintln!(
            "{}: seeds={} requested={} classified={}",
            f.repository,
            f.seed_ids.len(),
            f.requested.len(),
            f.classification.is_some()
        );
    }
    write_json(&args.output, &e)?;
    if let Some(path) = &args.html {
        crate::write_atomic(
            path,
            crate::classification_html::html(
                &e.run,
                report,
                &e.exploration,
                demand_shortlist.as_ref(),
            )
            .as_bytes(),
        )?;
    }
    for error in &e.run.errors {
        eprintln!("{}", crate::render::clean(error));
    }
    eprintln!(
        "{} new calls · {} cached assignments · {} errors",
        e.run.attempted_calls,
        e.run.reused_calls,
        e.run.errors.len()
    );
    Ok(!args.dry_run
        && (e.exploration.iter().any(|f| f.classification.is_none()) || !e.run.errors.is_empty()))
}

pub fn html(e: &Experiment, report: &Report) -> String {
    crate::classification_html::html(&e.run, report, &e.exploration, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retrieval_rejects_foreign_candidates_and_classification_preserves_independent_groups() {
        let pack = json!({"seeds":[{"candidate_id":"seed"}],"catalog":[{"candidate_id":"related"}],"related_limit":1});
        let valid = json!({"inspect":[{"candidate_id":"related","seed_id":"seed","reason":"Check a companion implementation"}],"limitations":[]});
        validate_selection(&valid, &pack).unwrap();
        let mut searches = valid.clone();
        searches["issue_queries"] = json!(["reference white"]);
        assert!(validate_selection(&searches, &pack).is_err());
        let mut with_cache = pack.clone();
        with_cache["issue_context"] = json!({});
        validate_selection(&searches, &with_cache).unwrap();
        searches["issue_queries"] = json!(["a", "b", "c", "d"]);
        assert!(validate_selection(&searches, &with_cache).is_err());
        for field in ["candidate_id", "seed_id"] {
            let mut bad = valid.clone();
            bad["inspect"][0][field] = json!("foreign");
            assert!(validate_selection(&bad, &pack).is_err());
        }
        let mut bad = valid.clone();
        bad["inspect"]
            .as_array_mut()
            .unwrap()
            .push(valid["inspect"][0].clone());
        assert!(validate_selection(&bad, &pack).is_err());
        let cards = vec![
            Card {
                candidate_id: "seed".into(),
                title: "seed".into(),
                commits: vec![],
                evidence_ids: vec!["commit:a".into()],
                omitted_commits: 0,
            },
            Card {
                candidate_id: "related".into(),
                title: "related".into(),
                commits: vec![],
                evidence_ids: vec!["commit:b".into()],
                omitted_commits: 0,
            },
        ];
        let w = Work {
            fork: 0,
            retrieval: false,
            key: "unused".into(),
            pack: json!({"scope":"selection","seed_ids":["seed"],"candidates":cards}),
        };
        let mut response = json!({"schema_version":1,"scope":"selection","summary":{"text":"Seed work","evidence":["commit:a"]},"groups":[{"name":"Seed","summary":{"text":"Seed","evidence":["commit:a"]},"members":[{"candidate_id":"seed","role":"implementation"}]},{"name":"Unrelated","summary":{"text":"Different work","evidence":["commit:b"]},"members":[{"candidate_id":"related","role":"implementation"}]}],"relationships":[],"unclassified":[],"limitations":[]});
        validate(&w, &response).unwrap();
        response["groups"].as_array_mut().unwrap().pop();
        response["unclassified"] =
            json!([{"candidate_id":"related","reason":"No direct seed connection"}]);
        validate(&w, &response).unwrap();
    }
}
