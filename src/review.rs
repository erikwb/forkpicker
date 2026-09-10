//! CLI-backed static reviews. No model HTTP client or credential management.
use crate::{git::Git, hash, llm, model::*, scan, write_json};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const PROMPT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Clean,
    Mixed,
    NeedsCleanup,
    Fits,
    SomeDifferences,
    Diverges,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Quality,
    Style,
    Security,
    Correctness,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Note,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Reviewed,
    InsufficientContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DimensionReview {
    pub dimension: Dimension,
    pub status: ReviewStatus,
    pub rationale: String,
    /// Absent in older saved reviews. Required for quality/style in new requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub dimension: Dimension,
    pub severity: Severity,
    pub confidence: Confidence,
    pub title: String,
    pub description: String,
    pub evidence: Vec<String>,
    pub commit: String,
    pub path: String,
    pub line: Option<usize>,
    pub suggested_fix: String,
    pub verification: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeReview {
    #[serde(default)]
    pub schema_version: Option<u32>,
    pub feature_id: String,
    pub base_sha: String,
    pub summary: Vec<Claim>,
    pub dimensions: Vec<DimensionReview>,
    pub findings: Vec<Finding>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub request_key: String,
    pub context_sha256: String,
    pub agent: String,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub created_at: String,
    pub duration_ms: u64,
    pub input_bytes: usize,
    #[serde(default)]
    pub provider_usage: Option<Value>,
    pub review: CodeReview,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    /// Exact argv, never shell text. {input}/{output} are private temporary paths.
    pub command: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default = "model_args")]
    pub model_args: Vec<String>,
    #[serde(default)]
    pub effort_args: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub output: OutputFormat,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Json,
    ClaudeJson,
    OpencodeJson,
}

fn model_args() -> Vec<String> {
    vec!["--model".into(), "{model}".into()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub classification: BTreeMap<String, crate::model_defaults::Settings>,
    #[serde(default)]
    pub default_agent: Option<String>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentProfile>,
}

fn profile(command: &[&str], effort_args: &[&str], output: OutputFormat) -> AgentProfile {
    AgentProfile {
        command: command.iter().map(|s| s.to_string()).collect(),
        model: None,
        effort: None,
        model_args: model_args(),
        effort_args: effort_args.iter().map(|s| s.to_string()).collect(),
        environment: BTreeMap::new(),
        output,
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut agents = BTreeMap::new();
        agents.insert(
            "codex".into(),
            profile(
                &[
                    "codex",
                    "exec",
                    "--ignore-user-config",
                    "--ignore-rules",
                    "--disable",
                    "shell_tool",
                    "--disable",
                    "unified_exec",
                    "--disable",
                    "apps",
                    "--disable",
                    "plugins",
                    "-c",
                    "web_search=\"disabled\"",
                    "-c",
                    "approval_policy=\"never\"",
                    "--sandbox",
                    "read-only",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--color",
                    "never",
                    "--output-last-message",
                    "{output}",
                ],
                &["-c", "model_reasoning_effort=\"{effort}\""],
                OutputFormat::Json,
            ),
        );
        agents.insert(
            "claude".into(),
            profile(
                &[
                    "claude",
                    "--print",
                    "--safe-mode",
                    "--strict-mcp-config",
                    "--disable-slash-commands",
                    "--output-format",
                    "json",
                    "--tools",
                    "",
                    "--no-session-persistence",
                ],
                &["--effort", "{effort}"],
                OutputFormat::ClaudeJson,
            ),
        );
        agents.insert(
            "grok".into(),
            profile(
                &[
                    "grok",
                    "--prompt-file",
                    "{input}",
                    "--output-format",
                    "plain",
                    "--tools",
                    "",
                    "--disable-web-search",
                    "--no-subagents",
                    "--max-turns",
                    "1",
                ],
                &["--reasoning-effort", "{effort}"],
                OutputFormat::Json,
            ),
        );
        agents.insert(
            "muse".into(),
            profile(
                &[
                    "muse",
                    "exec",
                    "--prompt-file",
                    "{input}",
                    "--disable-write",
                    "--disable-shell",
                    "--disable-web-tools",
                    "--no-foreign-personal-context",
                    "--no-session-log",
                    "--max-model-steps",
                    "3",
                ],
                &["--reasoning-effort", "{effort}"],
                OutputFormat::Json,
            ),
        );
        let mut opencode = profile(
            &["opencode", "run", "--pure", "--format", "json"],
            &["--variant", "{effort}"],
            OutputFormat::OpencodeJson,
        );
        opencode
            .environment
            .insert("OPENCODE_PERMISSION".into(), r#"{"*":"deny"}"#.into());
        agents.insert("opencode".into(), opencode);
        agents.insert(
            "pi".into(),
            profile(
                &[
                    "pi",
                    "--print",
                    "--no-tools",
                    "--no-extensions",
                    "--no-skills",
                    "--no-session",
                ],
                &["--thinking", "{effort}"],
                OutputFormat::Json,
            ),
        );
        Self {
            classification: BTreeMap::new(),
            default_agent: None,
            agents,
        }
    }
}

pub fn load_config(path: &Path, required: bool) -> Result<Config> {
    let mut config = Config::default();
    match std::fs::read(path) {
        Ok(bytes) => {
            let custom: Config =
                serde_json::from_slice(&bytes).context("invalid agent configuration")?;
            config.default_agent = custom.default_agent;
            config.agents.extend(custom.agents);
            config.classification.extend(custom.classification);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => (),
        Err(e) => return Err(e).context("read agent configuration"),
    }
    Ok(config)
}

pub fn executable(program: &str) -> Option<PathBuf> {
    let candidates: Vec<_> = if Path::new(program).components().count() > 1 {
        vec![PathBuf::from(program)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(program))
            .collect()
    };
    candidates
        .into_iter()
        .find(|p| {
            let Ok(m) = p.metadata() else {
                return false;
            };
            if !m.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
        .and_then(|p| p.canonicalize().ok())
}

pub fn source_git(report: &Report, cache: &Path, explicit: Option<&Path>) -> Option<Git> {
    let path = explicit.map(Path::to_path_buf).unwrap_or_else(|| {
        report
            .repository
            .strip_prefix("local:")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                cache
                    .join("repositories")
                    .join(hash(report.repository.to_lowercase()))
                    .join("git")
            })
    });
    path.exists().then(|| Git::new(path))
}

const CONVENTION_FILES: &[&str] = &[
    ".clang-format",
    ".editorconfig",
    "CONTRIBUTING.md",
    "rustfmt.toml",
    ".rustfmt.toml",
];

fn review_evidence_ids(report: &Report, feature: &Feature) -> BTreeSet<String> {
    let mut ids = llm::evidence_ids(report, feature);
    for sha in &feature.commits {
        for file in &report.commits[sha].files {
            ids.insert(format!("file:{}:{}", report.base_sha, file.path));
            if let Some(parent) = report.commits[sha].parents.first() {
                ids.insert(format!("file:{parent}:{}", file.path));
            }
        }
    }
    for path in CONVENTION_FILES {
        ids.insert(format!("file:{}:{path}", report.base_sha));
    }
    ids
}

pub fn context(
    report: &Report,
    feature_id: &str,
    max_bytes: usize,
    git: Option<&Git>,
) -> Result<Value> {
    let mut pack = llm::context(report, feature_id, usize::MAX)?;
    let f = report
        .features
        .iter()
        .find(|f| f.id == feature_id)
        .context("unknown feature")?;
    let mut sources = Vec::new();
    let mut seen = BTreeSet::new();
    let mut omitted = 0;
    if let Some(git) = git {
        // Reserve half the snapshot allowance for actual upstream comparisons.
        // The rest stays available for fork before/after code, even in broad changes.
        let paths: BTreeSet<_> = f
            .commits
            .iter()
            .flat_map(|sha| &report.commits[sha].files)
            .filter(|file| !file.binary)
            .map(|file| file.path.as_str())
            .collect();
        for path in CONVENTION_FILES.iter().copied().chain(paths) {
            if !seen.insert((report.base_sha.clone(), path.to_owned())) {
                continue;
            }
            if sources.len() >= 40 {
                omitted += 1;
                continue;
            }
            match git.source_file(&report.base_sha, path, 64 * 1024) {
                Ok(Some((content, truncated))) => sources.push(json!({
                    "commit": report.base_sha, "path": path, "content": content,
                    "truncated": truncated, "first_line": 1, "role": "upstream",
                    "evidence_id": format!("file:{}:{path}", report.base_sha)
                })),
                Ok(None) => (),
                Err(_) => omitted += 1,
            }
        }
        for sha in &f.commits {
            let c = &report.commits[sha];
            for file in &c.files {
                for revision in std::iter::once(sha).chain(c.parents.first()) {
                    if !seen.insert((revision.clone(), file.path.clone())) {
                        continue;
                    }
                    if sources.len() >= 80 || file.binary {
                        omitted += 1;
                        continue;
                    }
                    // Read Git blobs only, without checkout, filters, hooks, or repository execution.
                    match git.source_file(revision, &file.path, 64 * 1024) {
                        Ok(Some((content, truncated))) => sources.push(json!({
                            "commit": revision, "path": file.path, "content": content,
                            "truncated": truncated, "first_line": 1,
                            "role": if revision == sha { "after" } else { "before" },
                            "evidence_id": format!("file:{revision}:{}", file.path)
                        })),
                        Ok(None) => (), // File absent at this revision (creation/deletion).
                        Err(_) => {
                            omitted += 1;
                        }
                    }
                }
            }
        }
    }
    pack["source_files"] = json!(sources);
    pack["source_coverage"] = json!({"git_available":git.is_some(), "files_omitted":omitted,
        "scope":"Changed files at exact feature commits and their first parents, plus changed paths and root convention files at the scanned upstream SHA. At most 40 upstream snapshots and 80 total; all excerpts share the byte budget. No checkout or code execution. Other dependencies and conventions are not supplied. Fork parents are not necessarily upstream."});
    pack["prompt_version"] = json!(PROMPT_VERSION);
    pack["instructions"] = json!(include_str!("prompts/review.md"));
    let file = report.commits[&f.commits[0]]
        .files
        .first()
        .map(|f| f.path.clone())
        .unwrap_or_default();
    pack["response_example"] = json!({"feature_id":f.id,"base_sha":report.base_sha,
        "summary":[{"text":"A supported observation or labeled inference", "evidence":[format!("commit:{}",f.commits[0])]}],
        "dimensions":[
            {"dimension":"quality","status":"reviewed","verdict":"mixed","rationale":"Explain cleanliness with a concrete design or readability example and what cleanup, if any, would help", "evidence":[format!("commit:{}",f.commits[0])]},
            {"dimension":"style","status":"insufficient_context","verdict":"unknown","rationale":"Explain which upstream comparison is missing", "evidence":[]},
            {"dimension":"security","status":"reviewed","rationale":"Explain examined trust boundaries and limits"},
            {"dimension":"correctness","status":"reviewed","rationale":"Explain checked invariants and missing validation"}],
        "findings":[{"dimension":"correctness","severity":"medium","confidence":"medium","title":"A concrete defect, if any", "description":"Trigger, impact, and reasoning", "evidence":[format!("file:{}:{file}",f.commits[0])],"commit":f.commits[0],"path":file,"line":null,"suggested_fix":"A specific correction", "verification":"A targeted check a maintainer could run"}],
        "limitations":["Static review only; no tests or builds executed"]});
    // Triage state and transient API counters should not invalidate the review cache.
    pack["feature"]["status"] = json!("new");
    pack["feature"]["decision_reason"] = Value::Null;
    pack["coverage"]["api_requests"] = json!(0);
    pack["coverage"]["cache_hits"] = json!(0);
    // Only offer additional source citations that are actually present.
    let mut ids = llm::evidence_ids(report, f);
    for source in &sources {
        ids.insert(source["evidence_id"].as_str().unwrap().to_owned());
    }
    pack["evidence_ids"] = json!(ids);
    pack["source_coverage"]["upstream_comparison_available"] = json!(false);
    llm::bound_context(&mut pack, max_bytes)?;
    // Replacing false with true cannot increase the serialized byte size.
    pack["source_coverage"]["upstream_comparison_available"] =
        json!(pack["source_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["role"] == "upstream"
                && !s["content"].as_str().unwrap_or("").trim().is_empty()));
    Ok(pack)
}

pub fn validate(report: &Report, review: &CodeReview) -> Result<()> {
    ensure!(
        review.schema_version.is_none_or(|v| v == SCHEMA_VERSION),
        "unsupported code review schema version"
    );
    let f = report
        .features
        .iter()
        .find(|f| f.id == review.feature_id)
        .context("review references an unknown feature")?;
    ensure!(
        review.base_sha == report.base_sha,
        "review is stale: upstream SHA differs"
    );
    let dimensions: BTreeSet<_> = review
        .dimensions
        .iter()
        .map(|d| d.dimension.clone())
        .collect();
    ensure!(
        review.dimensions.len() == 4 && dimensions.len() == 4,
        "review must cover each of quality, style, security, correctness exactly once"
    );
    ensure!(
        review
            .dimensions
            .iter()
            .all(|d| !d.rationale.trim().is_empty()),
        "every dimension needs a rationale"
    );
    ensure!(
        !review.limitations.is_empty() && review.limitations.iter().all(|s| !s.trim().is_empty()),
        "review must state its limitations"
    );
    let allowed = review_evidence_ids(report, f);
    ensure!(
        !review.summary.is_empty(),
        "review needs a supported summary"
    );
    for d in &review.dimensions {
        ensure!(
            d.evidence.iter().all(|id| allowed.contains(id)),
            "unknown dimension evidence"
        );
        if let Some(verdict) = &d.verdict {
            let valid = match d.dimension {
                Dimension::Quality => matches!(
                    verdict,
                    Verdict::Clean | Verdict::Mixed | Verdict::NeedsCleanup | Verdict::Unknown
                ),
                Dimension::Style => matches!(
                    verdict,
                    Verdict::Fits | Verdict::SomeDifferences | Verdict::Diverges | Verdict::Unknown
                ),
                _ => false,
            };
            ensure!(valid, "verdict does not belong to this review dimension");
            ensure!(
                matches!(d.status, ReviewStatus::InsufficientContext)
                    == (*verdict == Verdict::Unknown),
                "unknown verdict requires insufficient_context; assessed verdict requires reviewed"
            );
            ensure!(
                *verdict == Verdict::Unknown || !d.evidence.is_empty(),
                "assessed dimension needs evidence"
            );
        }
    }
    for claim in &review.summary {
        ensure!(
            !claim.text.trim().is_empty() && !claim.evidence.is_empty(),
            "summary needs text and evidence"
        );
        ensure!(
            claim.evidence.iter().all(|s| allowed.contains(s)),
            "unknown summary evidence"
        );
    }
    for finding in &review.findings {
        ensure!(
            [
                &finding.title,
                &finding.description,
                &finding.suggested_fix,
                &finding.verification
            ]
            .iter()
            .all(|s| !s.trim().is_empty()),
            "finding needs title, impact, fix, and verification"
        );
        ensure!(
            f.commits.contains(&finding.commit),
            "finding must locate a commit in the feature"
        );
        let c = &report.commits[&finding.commit];
        ensure!(
            c.files.iter().any(|file| file.path == finding.path),
            "finding references a file not changed by its commit"
        );
        let location = format!("file:{}:{}", finding.commit, finding.path);
        ensure!(
            finding.evidence.contains(&location),
            "finding must cite its exact file evidence"
        );
        ensure!(
            finding.evidence.iter().all(|s| allowed.contains(s)),
            "unknown finding evidence"
        );
        ensure!(finding.line != Some(0), "line numbers start at 1");
    }
    Ok(())
}

pub fn validate_context(review: &CodeReview, pack: &Value) -> Result<()> {
    let supplied: BTreeSet<_> = pack["evidence_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for id in review
        .dimensions
        .iter()
        .flat_map(|d| &d.evidence)
        .chain(review.summary.iter().flat_map(|c| &c.evidence))
        .chain(review.findings.iter().flat_map(|f| &f.evidence))
    {
        ensure!(
            supplied.contains(id.as_str()),
            "review cites evidence absent from this request"
        );
    }
    if pack["prompt_version"].as_u64().unwrap_or(1) < 2 {
        return Ok(());
    }
    let changed: BTreeSet<_> = pack["feature"]["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for d in &review.dimensions {
        if !matches!(d.dimension, Dimension::Quality | Dimension::Style) {
            continue;
        }
        let verdict = d
            .verdict
            .as_ref()
            .context("quality and style each require a verdict")?;
        if *verdict == Verdict::Unknown {
            continue;
        }
        ensure!(
            d.evidence.iter().any(|id| {
                id.strip_prefix("commit:")
                    .is_some_and(|sha| changed.contains(sha))
                    || id
                        .strip_prefix("file:")
                        .and_then(|v| v.split_once(':'))
                        .is_some_and(|(sha, _)| changed.contains(sha))
            }),
            "assessment must cite changed code"
        );
        if d.dimension == Dimension::Style {
            ensure!(pack["source_files"].as_array().into_iter().flatten().any(|s| {
                s["role"] == "upstream" && s["commit"] == pack["base_sha"]
                    && !s["content"].as_str().unwrap_or("").trim().is_empty()
                    && s["evidence_id"].as_str().is_some_and(|id| d.evidence.iter().any(|e| e == id))
            }), "style assessment needs a cited upstream source excerpt; use unknown when unavailable");
        }
    }
    Ok(())
}

pub fn decode_value(text: &str, format: &OutputFormat) -> Result<Value> {
    let value = match format {
        OutputFormat::Json => llm::parse_json(text)?,
        OutputFormat::ClaudeJson => {
            let value = llm::parse_json(text)?;
            ensure!(value["is_error"] != true, "Claude reported an error");
            if value.get("feature_id").is_some() {
                value
            } else if let Some(output) = value.get("structured_output") {
                output.clone()
            } else {
                llm::parse_json(
                    value["result"]
                        .as_str()
                        .context("Claude result text missing")?,
                )?
            }
        }
        OutputFormat::OpencodeJson => {
            let mut result = String::new();
            for line in text.lines().filter(|s| !s.trim().is_empty()) {
                let event: Value = serde_json::from_str(line).context("invalid OpenCode event")?;
                ensure!(event["type"] != "error", "OpenCode reported an error");
                if event["type"] == "text" {
                    if let Some(text) = event["part"]["text"].as_str() {
                        result.push_str(text);
                    }
                }
            }
            llm::parse_json(&result)?
        }
    };
    Ok(value)
}

pub fn decode(text: &str, format: &OutputFormat) -> Result<CodeReview> {
    serde_json::from_value(decode_value(text, format)?)
        .context("agent must return the code review schema")
}

pub fn invocation(profile: &AgentProfile, input: &Path, output: &Path) -> Result<Vec<String>> {
    let mut args = profile.command.clone();
    ensure!(!args.is_empty(), "agent command is empty");
    if let Some(model) = &profile.model {
        ensure!(
            !model.trim().is_empty() && !profile.model_args.is_empty(),
            "model override unsupported or empty"
        );
        args.extend(profile.model_args.clone());
    }
    if let Some(effort) = &profile.effort {
        ensure!(
            !effort.trim().is_empty() && !profile.effort_args.is_empty(),
            "effort override unsupported or empty"
        );
        args.extend(profile.effort_args.clone());
    }
    for arg in &mut args {
        if arg.contains("{model}") {
            ensure!(profile.model.is_some(), "agent needs --model");
        }
        if arg.contains("{effort}") {
            ensure!(profile.effort.is_some(), "agent needs --effort");
        }
        *arg = arg
            .replace("{input}", &input.to_string_lossy())
            .replace("{output}", &output.to_string_lossy())
            .replace("{model}", profile.model.as_deref().unwrap_or(""))
            .replace("{effort}", profile.effort.as_deref().unwrap_or(""));
    }
    // Resolve before moving to the isolated temporary working directory.
    args[0] = executable(&args[0])
        .with_context(|| format!("agent executable not found: {}", args[0]))?
        .to_string_lossy()
        .into_owned();
    Ok(args)
}

pub struct Request<'a> {
    pub agent: &'a str,
    pub profile: &'a AgentProfile,
    pub pack: &'a Value,
}

impl Request<'_> {
    pub fn key(&self) -> Result<String> {
        let program = self
            .profile
            .command
            .first()
            .context("agent command is empty")?;
        let executable = executable(program);
        let modified = executable
            .as_ref()
            .and_then(|p| p.metadata().ok())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().to_string());
        Ok(hash(serde_json::to_vec(
            &json!({"prompt_version":PROMPT_VERSION,"agent":self.agent,
            "profile":self.profile,"executable":executable,"modified":modified,"pack":self.pack}),
        )?))
    }

    pub fn run_json(&self, timeout: Duration) -> Result<(Value, Option<Value>, u64)> {
        let temp = tempfile::tempdir()?;
        // Grok treats .json prompt files as ACP protocol input, not literal text.
        let input = temp.path().join("context.txt");
        let output = temp.path().join("result.json");
        let bytes = serde_json::to_vec(self.pack)?;
        crate::write_atomic(&input, &bytes)?;
        let command = invocation(self.profile, &input, &output)?;
        let uses_output = self
            .profile
            .command
            .iter()
            .any(|arg| arg.contains("{output}"));
        let start = Instant::now();
        let result = llm::run_process(
            &command,
            &bytes,
            timeout,
            Some(temp.path()),
            &self.profile.environment,
            uses_output.then_some(output.as_path()),
        )?;
        let usage = if matches!(self.profile.output, OutputFormat::ClaudeJson) {
            let envelope = llm::parse_json(&result)?;
            Some(
                json!({"usage": envelope.get("usage"), "model_usage": envelope.get("modelUsage"), "reported_cost_usd": envelope.get("total_cost_usd"), "note": "Provider-reported telemetry; not a subscription invoice"}),
            )
        } else {
            None
        };
        Ok((
            decode_value(&result, &self.profile.output)?,
            usage,
            start.elapsed().as_millis() as u64,
        ))
    }

    pub fn run(&self, report: &Report, timeout: Duration) -> Result<ReviewRecord> {
        let (value, provider_usage, duration_ms) = self.run_json(timeout)?;
        self.record(report, value, provider_usage, duration_ms)
    }

    pub fn run_saved(
        &self,
        report: &Report,
        timeout: Duration,
        cache: &Path,
    ) -> Result<ReviewRecord> {
        let (value, provider_usage, duration_ms) = self.run_json(timeout)?;
        write_json(
            &cache
                .join("review-responses")
                .join(format!("{}.json", self.key()?)),
            &json!({"response":value,"provider_usage":provider_usage,"duration_ms":duration_ms,"input_bytes":crate::json_size(self.pack)?}),
        )?;
        self.record(report, value, provider_usage, duration_ms)
    }

    pub fn record(
        &self,
        report: &Report,
        value: Value,
        provider_usage: Option<Value>,
        duration_ms: u64,
    ) -> Result<ReviewRecord> {
        let bytes = serde_json::to_vec(self.pack)?;
        let mut value = value;
        // Some CLIs echo input metadata. Accept only an exact echo, never model-supplied facts.
        if let Some(coverage) = value.get("source_coverage") {
            ensure!(
                Some(coverage) == self.pack.get("source_coverage"),
                "model changed source coverage metadata"
            );
            value
                .as_object_mut()
                .context("review must be an object")?
                .remove("source_coverage");
        }
        // Keep uncited prose visibly unverified instead of inventing evidence or losing a call.
        let mut notes = Vec::new();
        if let Some(summary) = value.get_mut("summary").and_then(Value::as_array_mut) {
            summary.retain(|claim| {
                if claim.get("evidence").is_none() {
                    if let Some(text) = claim["text"].as_str() {
                        notes.push(Value::String(format!(
                            "Uncited model note (not verified): {text}"
                        )));
                        return false;
                    }
                }
                true
            });
        }
        if !notes.is_empty() {
            value
                .get_mut("limitations")
                .and_then(Value::as_array_mut)
                .context("review limitations missing")?
                .extend(notes);
        }
        let review: CodeReview =
            serde_json::from_value(value).context("agent must return the code review schema")?;
        ensure!(
            Some(review.feature_id.as_str()) == self.pack["feature"]["id"].as_str(),
            "agent reviewed a different feature"
        );
        validate(report, &review)?;
        validate_context(&review, self.pack)?;
        // Validate a claimed line against the supplied post-change source when available.
        for f in &review.findings {
            if let Some(line) = f.line {
                if let Some(source) = self.pack["source_files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|s| s["commit"] == f.commit && s["path"] == f.path)
                {
                    ensure!(
                        line <= source["content"].as_str().unwrap_or("").lines().count(),
                        "finding line is outside the supplied source excerpt"
                    );
                } else {
                    bail!("finding gives a line without a source snapshot; use null for patch-only locations");
                }
            }
        }
        Ok(ReviewRecord {
            request_key: self.key()?,
            context_sha256: hash(&bytes),
            agent: self.agent.into(),
            requested_model: self.profile.model.clone(),
            requested_effort: self.profile.effort.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            duration_ms,
            input_bytes: bytes.len(),
            provider_usage,
            review,
        })
    }
}

pub fn attach(path: &Path, record: ReviewRecord) -> Result<()> {
    scan::update_report(path, |report| {
        validate(report, &record.review)?;
        report
            .reviews
            .retain(|r| r.request_key != record.request_key);
        report.reviews.push(record);
        Ok(())
    })
}

pub fn save_cache(cache: &Path, record: &ReviewRecord) -> Result<()> {
    write_json(
        &cache
            .join("reviews")
            .join(format!("{}.json", record.request_key)),
        record,
    )
}
