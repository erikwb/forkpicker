//! One-command orchestration of the existing scan and evidence-review stages.
use crate::{Commands, CommonScan, RemoteArgs, ShortlistArgs};
use anyhow::{ensure, Context, Result};
use clap::Args;
use forkpicker::{
    classify::ClassifyArgs,
    discovery::Discovery,
    discovery_application::Policy,
    github::{Github, RepoName},
    issue_review::IssueReviewArgs,
    review,
    structured::Agent,
    write_json,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Args)]
pub struct RunArgs {
    pub repository: RepoName,
    /// Directory for this run's HTML and evidence (default: reports/OWNER-REPO/TIMESTAMP).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Use this CLI; otherwise use configured default, then installed Codex, Claude, or Grok.
    #[arg(long, value_enum, conflicts_with = "no_llm")]
    agent: Option<Agent>,
    #[arg(long, conflicts_with = "no_llm")]
    model: Option<String>,
    #[arg(long, conflicts_with = "no_llm")]
    effort: Option<String>,
    /// Gather forks and integration facts without calling a model.
    #[arg(long)]
    no_llm: bool,
    /// Open the completed dashboard in your default browser.
    #[arg(long)]
    open: bool,
    /// Total model-call ceiling across discovery and issue matching; failures count. 0 makes no calls.
    #[arg(long, default_value_t = 30)]
    limit: usize,
    /// Maximum mapping calls; remaining discovery calls inspect nominated feature sets.
    #[arg(long, default_value_t = 5)]
    map_calls: usize,
    /// Total serialized model input allowance across all stages (bytes).
    #[arg(long, default_value_t = 4_000_000)]
    total_bytes: usize,
    /// Maximum input bytes per model call; complete diffs are never shortened to fit.
    #[arg(long, default_value_t = 256_000)]
    max_bytes: usize,
    /// Seconds allowed per model invocation.
    #[arg(long, default_value_t = 180)]
    timeout: u64,
    /// Time ceiling for the discovery stage, in seconds.
    #[arg(long, default_value_t = 3600)]
    max_seconds: u64,
    /// Maximum GitHub requests per collection stage; includes all forks and branches by default.
    #[arg(long, default_value_t = 5000)]
    api_budget: usize,
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=32))]
    jobs: u16,
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=16))]
    fetch_jobs: u16,
    /// Concurrent independent code inspections (model calls, not Git workers).
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=8))]
    review_jobs: u16,
    /// Always match issues in a separate pass instead of including small catalogs in inspections.
    #[arg(long)]
    separate_issues: bool,
    /// Screen all history instead of starting one month before the latest stable release.
    #[arg(long, conflicts_with = "release_snapshot")]
    all_history: bool,
    /// Reuse a raw scan instead of collecting forks again; cached Git objects must be available.
    #[arg(long)]
    from_scan: Option<PathBuf>,
    /// Reuse saved raw issue/discussion context.
    #[arg(long)]
    issue_cache: Option<PathBuf>,
    /// Reuse verified open-PR membership.
    #[arg(long)]
    pr_snapshot: Option<PathBuf>,
    /// Reuse a raw stable GitHub release record.
    #[arg(long)]
    release_snapshot: Option<PathBuf>,
}

impl RunArgs {
    pub fn uses_llm(&self) -> bool {
        !self.no_llm && self.limit > 0
    }
}

fn choose_agent(args: &RunArgs, config: &review::Config) -> Result<Agent> {
    let agent = if let Some(agent) = &args.agent {
        agent.clone()
    } else if let Some(name) = &config.default_agent {
        Agent::parse(name)?
    } else {
        [Agent::Codex, Agent::Claude, Agent::Grok].into_iter().find(|a| {
            config.agents.get(a.name()).and_then(|p| p.command.first())
                .and_then(|p| review::executable(p)).is_some()
        }).context("No supported agent CLI found. Install/sign into Codex, Claude, or Grok, or use --no-llm")?
    };
    let profile = forkpicker::model_defaults::resolve_classification(
        config,
        agent.name(),
        args.model.as_deref(),
        args.effort.as_deref(),
    )?;
    let executable = profile.command.first().context("agent command is empty")?;
    ensure!(
        review::executable(executable).is_some(),
        "{} CLI executable not found: {}; use --agent or --no-llm",
        agent.name(),
        executable
    );
    Ok(agent)
}

// Reserve up to five calls and one quarter of bytes for issue matching. Unspent
// discovery allowance is also available there, still under the shared ceilings.
fn discovery_budget(limit: usize, bytes: usize) -> (usize, usize) {
    (limit - (limit / 6).min(5), bytes - bytes / 4)
}

fn copy_snapshot(source: &Path, destination: &Path) -> Result<()> {
    // Validate JSON before publishing a copied raw snapshot.
    let value: Value = serde_json::from_slice(&std::fs::read(source)?)?;
    write_json(destination, &value)
}

fn open_browser(path: &Path) -> Result<()> {
    let path = path.canonicalize()?;
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    ensure!(
        command
            .arg(path)
            .status()
            .context("open dashboard")?
            .success(),
        "browser opener failed"
    );
    Ok(())
}

struct Progress {
    directory: PathBuf,
    manifest: Value,
}
impl Progress {
    fn save(&self) -> Result<()> {
        write_json(&self.directory.join("run.json"), &self.manifest)
    }
    fn stage<T>(&mut self, name: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
        eprintln!("\n{name}…");
        self.manifest["status"] = json!("running");
        self.manifest["stage"] = json!(name);
        self.save()?;
        let start = Instant::now();
        let result = work();
        self.manifest["stages"].as_array_mut().unwrap().push(json!({
            "name":name, "elapsed_ms":start.elapsed().as_millis(),
            "error":result.as_ref().err().map(|e|format!("{e:#}"))
        }));
        if result.is_err() {
            self.manifest["status"] = json!("failed");
        }
        self.save()?;
        result.with_context(|| format!("{name}; saved run: {}", self.directory.display()))
    }
}

pub fn run(
    args: RunArgs,
    config: &review::Config,
    cache: &Path,
    mut execute: impl FnMut(Commands) -> Result<bool>,
) -> Result<bool> {
    let use_llm = args.uses_llm();
    ensure!(
        !use_llm
            || (args.total_bytes >= 16_000
                && args.max_bytes >= 8000
                && args.timeout > 0
                && args.max_seconds > 0),
        "model runs require --total-bytes >= 16000, --max-bytes >= 8000, and positive time limits"
    );
    // Fail before downloading forks if the selected CLI cannot be invoked.
    let agent = if use_llm {
        Some(choose_agent(&args, config)?)
    } else {
        None
    };
    let source_report = args
        .from_scan
        .as_ref()
        .map(|p| forkpicker::scan::load_report(p))
        .transpose()?;
    if let Some(report) = &source_report {
        ensure!(
            report.repository.eq_ignore_ascii_case(&args.repository.0),
            "--from-scan belongs to another repository"
        );
    }
    for path in [&args.issue_cache, &args.pr_snapshot, &args.release_snapshot]
        .into_iter()
        .flatten()
    {
        let _: Value = serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
        )?;
    }
    let directory = args.output.clone().unwrap_or_else(|| {
        PathBuf::from("reports")
            .join(args.repository.0.replace('/', "-"))
            .join(chrono::Utc::now().format("%Y%m%d-%H%M%S-%f").to_string())
    });
    std::fs::create_dir_all(&directory)?;
    ensure!(std::fs::read_dir(&directory)?.next().is_none(), "output directory is not empty: {}; choose a new --output directory to preserve earlier runs", directory.display());
    // Claim the run directory before any stage; concurrent writers cannot share it.
    let _claim = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("run.json"))?;
    let network = directory.join("network.json");
    let issues = directory.join("issues.json");
    let prs = directory.join("prs.json");
    let inventory = directory.join("inventory.json");
    let discovery = directory.join("discovery.json");
    let html = directory.join("index.html");
    let issue_review = directory.join("issue-review.json");
    let mut progress = Progress {
        directory: directory.clone(),
        manifest: json!({
            "repository":args.repository.0, "started_at":chrono::Utc::now().to_rfc3339(),
            "status":"starting", "stages":[], "agent":agent.as_ref().map(Agent::name),
            "model":args.model, "effort":args.effort, "no_llm":!use_llm,
            "call_limit":if use_llm {args.limit} else {0},
            "input_limit":if use_llm {args.total_bytes} else {0}, "attempted_calls":0, "input_bytes":0,
            "dashboard":"index.html"
        }),
    };
    progress.save()?;
    eprintln!("Run directory: {}", directory.display());
    if let Some(agent) = &agent {
        eprintln!(
            "Agent: {} · at most {} calls across all model stages",
            agent.name(),
            args.limit
        );
    }
    progress.stage("Gather forks", || {
        if let Some(report) = &source_report {
            write_json(&network, report)?;
            forkpicker::write_atomic(
                &html,
                forkpicker::render::html(report, None, false).as_bytes(),
            )?;
            Ok(false)
        } else {
            execute(Commands::Scan(RemoteArgs {
                repository: args.repository.clone(),
                forks: vec![],
                max_forks: 0,
                max_branches: 0,
                branches: vec![],
                default_branch_only: false,
                base: None,
                jobs: args.jobs,
                fetch_jobs: args.fetch_jobs,
                api_budget: args.api_budget,
                refresh: true,
                query: None,
                with_context: false,
                with_demand: false,
                priority_labels: vec![],
                common: CommonScan {
                    max_commits: 0,
                    upstream_history: 0,
                    output: network.clone(),
                    html: Some(html.clone()),
                    strict: false,
                },
            }))
        }
    })?;
    progress.stage("Gather issues and open PRs", || {
        if let Some(path) = &args.issue_cache {
            copy_snapshot(path, &issues)?;
        }
        if let Some(path) = &args.pr_snapshot {
            copy_snapshot(path, &prs)?;
        }
        execute(Commands::Shortlist(ShortlistArgs {
            report: network.clone(),
            baseline: None,
            only_new: false,
            refresh_demand: args.issue_cache.is_none(),
            demand_snapshot: args.issue_cache.as_ref().map(|_| issues.clone()),
            write_demand_snapshot: args.issue_cache.is_none().then(|| issues.clone()),
            with_prs: args.pr_snapshot.is_none(),
            pr_snapshot: args.pr_snapshot.as_ref().map(|_| prs.clone()),
            write_pr_snapshot: args.pr_snapshot.is_none().then(|| prs.clone()),
            api_budget: args.api_budget,
            limit: 0,
            output: directory.join("shortlist.json"),
            html: None,
        }))
    })?;
    progress.stage("Check patch application", || {
        execute(Commands::Inventory {
            report: network.clone(),
            baseline: None,
            check_apply: true,
            repo: None,
            target: None,
            jobs: forkpicker::parallel::local_jobs(),
            policy: None,
            demand_snapshot: Some(issues.clone()),
            include_discussions: false,
            output: inventory.clone(),
            html: Some(html.clone()),
        })
    })?;
    let mut incomplete = false;
    if let Some(agent) = agent {
        let release = if args.all_history {
            None
        } else {
            progress.stage("Read release window", || {
                let path = directory.join("release.json");
                if let Some(source) = &args.release_snapshot {
                    copy_snapshot(source, &path)?;
                    return Ok(Some(path));
                }
                let report = forkpicker::scan::load_report(&network)?;
                let endpoint = format!("repos/{}/releases/latest", report.repository);
                let mut api = Github::new(cache.to_owned(), true, args.api_budget)?;
                match api.get(&endpoint) {
                    Ok((value, _)) => {
                        write_json(&path, &value)?;
                        Ok(Some(path))
                    }
                    Err(e)
                        if e.downcast_ref::<reqwest::Error>()
                            .and_then(reqwest::Error::status)
                            == Some(reqwest::StatusCode::NOT_FOUND) =>
                    {
                        eprintln!("No published GitHub release; screening all history.");
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            })?
        };
        progress.manifest["window"] = json!(if release.is_some() {
            "latest release minus one calendar month"
        } else {
            "all history"
        });
        let (discovery_calls, discovery_bytes) = discovery_budget(args.limit, args.total_bytes);
        incomplete |= progress.stage("Discover and inspect features", || {
            execute(Commands::Classify(ClassifyArgs {
                report: network.clone(),
                inventory: Some(inventory),
                forks: vec![],
                discover: true,
                fresh: true,
                continue_run: false,
                extend_budget: false,
                reinspect: false,
                since_release: false,
                release_snapshot: release,
                release_overlap_days: None,
                application: Some(Policy::Clean),
                resume_discovery: None,
                map_calls: args.map_calls.min(discovery_calls),
                screen_size: 200,
                total_bytes: discovery_bytes,
                explore_related: false,
                candidates: vec![],
                shortlist: None,
                issue_cache: Some(issues.clone()),
                inline_issues: !args.separate_issues,
                pr_snapshot: Some(prs),
                with_prs: false,
                write_pr_snapshot: None,
                api_budget: args.api_budget,
                seed_limit: 10,
                related_limit: 5,
                agent: Some(agent.clone()),
                model: args.model.clone(),
                effort: args.effort.clone(),
                max_forks: 0,
                batch_size: 10,
                limit: discovery_calls,
                jobs: args.review_jobs,
                max_bytes: args.max_bytes,
                timeout: args.timeout,
                max_seconds: args.max_seconds,
                dry_run: false,
                plan_dir: None,
                output: discovery.clone(),
                html: Some(html.clone()),
            }))
        })?;
        let d: Discovery = serde_json::from_slice(&std::fs::read(&discovery)?)?;
        progress.manifest["attempted_calls"] = json!(d.run.attempted_calls);
        progress.manifest["input_bytes"] = json!(d.new_input_bytes);
        progress.manifest["discovery"] = json!({
            "screened":d.screened().len(), "inspected":d.inspected().len(),
            "stop_reason":d.stop_reason, "errors":d.run.errors
        });
        let remaining_calls = args.limit.saturating_sub(d.run.attempted_calls).min(5);
        let remaining_bytes = args.total_bytes.saturating_sub(d.new_input_bytes);
        let needs_issue_review =
            forkpicker::inline_issues::needs_review(&d, &forkpicker::hash(std::fs::read(&issues)?));
        // Never keep spending after a CLI failure. The discovery dashboard is already saved.
        if !incomplete
            && !d.inspections.is_empty()
            && needs_issue_review
            && remaining_calls > 0
            && remaining_bytes >= 8000
        {
            incomplete |= progress.stage("Match features to open issues", || {
                execute(Commands::MatchIssues(IssueReviewArgs {
                    report: network,
                    classification: discovery,
                    issue_cache: issues,
                    agent: Some(agent),
                    model: args.model,
                    effort: args.effort,
                    limit: remaining_calls,
                    max_bytes: args.max_bytes,
                    total_bytes: remaining_bytes,
                    timeout: args.timeout,
                    continue_run: false,
                    dry_run: false,
                    output: issue_review.clone(),
                    html: Some(html.clone()),
                }))
            })?;
            let r: forkpicker::issue_review::Review =
                serde_json::from_slice(&std::fs::read(issue_review)?)?;
            progress.manifest["attempted_calls"] = json!(d.run.attempted_calls + r.attempted_calls);
            progress.manifest["input_bytes"] = json!(d.new_input_bytes + r.input_bytes);
            progress.manifest["issue_matching"] = json!({"complete":r.complete,"errors":r.errors});
        } else if !incomplete && !d.inspections.is_empty() && !needs_issue_review {
            eprintln!(
                "Open issues assessed during code inspection; no separate model calls needed."
            );
            progress.manifest["issue_matching"] =
                json!({"mode":"during_inspection","complete":true,"additional_calls":0});
        } else {
            let reason = if incomplete {
                "Discovery failed; no further model calls made"
            } else if d.inspections.is_empty() {
                "No inspected features to match"
            } else {
                "Model budget exhausted"
            };
            eprintln!("Issue matching skipped: {reason}");
            progress.manifest["issue_matching"] = json!({"skipped":reason});
        }
    }
    progress.manifest["status"] = json!(if incomplete { "incomplete" } else { "finished" });
    progress.manifest["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
    progress.save()?;
    eprintln!("\nDashboard: {}", html.canonicalize()?.display());
    if args.open {
        open_browser(&html)?;
    }
    Ok(incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    fn args(extra: &[&str]) -> RunArgs {
        let cli = crate::Cli::try_parse_from(
            ["forkpicker", "run", "owner/repo"]
                .into_iter()
                .chain(extra.iter().copied()),
        )
        .unwrap();
        let Commands::Run(args) = cli.command else {
            unreachable!()
        };
        args
    }
    #[test]
    fn oneshot_missing_cli_fails_before_scanning_or_creating_output() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        let args = args(&["--agent", "codex", "--output", output.to_str().unwrap()]);
        let mut config = review::Config::default();
        config.agents.get_mut("codex").unwrap().command = vec![dir
            .path()
            .join("missing-cli")
            .to_string_lossy()
            .into_owned()];
        let result = run(args, &config, dir.path(), |_| {
            panic!("must not start a stage")
        });
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("executable not found"));
        assert!(!output.exists());
    }
    #[test]
    fn oneshot_scans_all_history_and_records_collection_failure() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("new run");
        let args = args(&[
            "--no-llm",
            "--output",
            output.to_str().unwrap(),
            "--jobs",
            "1",
            "--fetch-jobs",
            "2",
        ]);
        let result = run(args, &review::Config::default(), dir.path(), |command| {
            let Commands::Scan(scan) = command else {
                panic!("scan must be first")
            };
            assert_eq!(scan.repository.0, "owner/repo");
            assert!(scan.forks.is_empty() && scan.branches.is_empty());
            assert_eq!(
                (
                    scan.max_forks,
                    scan.max_branches,
                    scan.common.max_commits,
                    scan.common.upstream_history
                ),
                (0, 0, 0, 0)
            );
            assert_eq!((scan.jobs, scan.fetch_jobs), (1, 2));
            assert!(scan.refresh);
            anyhow::bail!("fixture collection failure")
        });
        assert!(result.is_err());
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(output.join("run.json")).unwrap()).unwrap();
        assert_eq!(manifest["status"], "failed");
        assert_eq!(manifest["attempted_calls"], 0);
        assert!(manifest["stages"][0]["error"]
            .as_str()
            .unwrap()
            .contains("fixture collection failure"));
    }
}
