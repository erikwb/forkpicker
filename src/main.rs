use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use forkpicker::{
    analyze,
    git::Git,
    github::{Github, RepoName},
    llm, metrics,
    model::*,
    priority, render, review,
    scan::{self, ScanOptions},
    shortlist, state, write_atomic, write_json,
};
use fs2::FileExt;
mod oneshot;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    name = "forkpicker",
    version,
    about = "Discover reviewable features in forks, with evidence for maintainers and LLMs",
    long_about = "Find feature candidates across fork branches, suppress inherited and patch-equivalent upstream work, and preserve maintainer decisions. All scanning is read-only on GitHub; repository code is never executed."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    #[arg(
        long,
        global = true,
        help = "Git/API cache (default: XDG_CACHE_HOME/forkpicker)"
    )]
    cache_dir: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Maintainer decisions (default: XDG_DATA_HOME/forkpicker)"
    )]
    state_dir: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Agent profiles (default: XDG_CONFIG_HOME/forkpicker/config.json)"
    )]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan forks, inspect promising features, match open issues, and write a dashboard.
    Run(oneshot::RunArgs),
    /// Gather GitHub forks and find feature candidates across their branches.
    Scan(RemoteArgs),
    /// Analyze local Git refs without any GitHub API calls or network access.
    ScanLocal(LocalArgs),
    /// Rank observed project demand and linked fork candidates without an LLM.
    Shortlist(ShortlistArgs),
    /// Export factual measurements and independent effort bands, entirely offline.
    Inventory {
        report: PathBuf,
        #[arg(long, help = "Prior scan for newly observed and changed patch sets")]
        baseline: Option<PathBuf>,
        #[arg(
            long,
            help = "Check affected-file overlap and patch application using cached Git objects"
        )]
        check_apply: bool,
        #[arg(
            long,
            requires = "check_apply",
            help = "Git object store override; never fetches"
        )]
        repo: Option<PathBuf>,
        #[arg(
            long,
            requires = "check_apply",
            help = "Integration target ref/SHA; default: scanned upstream base"
        )]
        target: Option<String>,
        #[arg(long, default_value_t = forkpicker::parallel::local_jobs(), value_parser = clap::value_parser!(u16).range(1..=32), help = "Parallel integration checks (default: available CPUs, capped at 32)")]
        jobs: u16,
        #[arg(
            long,
            help = "JSON threshold overrides; unspecified fields use defaults"
        )]
        policy: Option<PathBuf>,
        #[arg(long, help = "Optional saved issue catalog for vocabulary overlap")]
        demand_snapshot: Option<PathBuf>,
        #[arg(
            long,
            requires = "demand_snapshot",
            help = "Include open discussions in the word histogram"
        )]
        include_discussions: bool,
        #[arg(short, long, default_value = "inventory.json")]
        output: PathBuf,
        #[arg(long)]
        html: Option<PathBuf>,
    },
    /// Match fork behavior to observed demand using bounded, cached CLI model calls.
    Triage(forkpicker::triage::TriageArgs),
    /// Classify coherent fork features using native JSON Schema outputs.
    Classify(forkpicker::classify::ClassifyArgs),
    /// Relate inspected features to open project issues with code-backed evidence.
    MatchIssues(forkpicker::issue_review::IssueReviewArgs),
    /// Refresh project demand and review recommendations without invoking an LLM.
    Demand {
        report: PathBuf,
        #[arg(long)]
        query: Option<String>,
        #[arg(
            long = "priority-label",
            help = "Urgent project labels; repeatable; replaces defaults"
        )]
        priority_labels: Vec<String>,
        #[arg(long, default_value_t = 20)]
        api_budget: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        html: Option<PathBuf>,
    },
    /// Search/render a saved report; works entirely offline.
    Report {
        report: PathBuf,
        /// Render saved classifications with source commit links (HTML only).
        #[arg(long)]
        classification: Option<PathBuf>,
        /// Include explicit issue links from a saved demand shortlist.
        #[arg(long, requires = "classification")]
        shortlist: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "terminal")]
        format: Format,
        #[arg(long)]
        query: Option<String>,
        #[arg(long, help = "Include dismissed and adopted features")]
        all: bool,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Inspect a feature's commits, grouping evidence, and possible prerequisites.
    Show {
        report: PathBuf,
        feature: String,
        #[arg(long)]
        patch: bool,
    },
    /// Export bounded, source-backed JSON context for any LLM.
    Context {
        report: PathBuf,
        feature: String,
        #[arg(long, default_value_t = 100_000)]
        max_bytes: usize,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(
            long,
            help = "Export a code review prompt with available before/after source files"
        )]
        review: bool,
        #[arg(
            long,
            help = "Git repository containing the pinned commits for source context"
        )]
        repo: Option<PathBuf>,
    },
    /// Review quality, style, security, and correctness using an installed agent CLI.
    Review(ReviewArgs),
    /// List CLI adapters or write an editable configuration file.
    Agents {
        #[arg(
            long,
            help = "Write example profiles; refuses to overwrite an existing file"
        )]
        write_config: Option<PathBuf>,
    },
    /// Import an LLM assessment after validating its feature, base SHA, and citations.
    Annotate {
        report: PathBuf,
        assessment: PathBuf,
    },
    /// Pipe context to an explicitly chosen LLM command and import its JSON response.
    Enrich {
        report: PathBuf,
        feature: String,
        #[arg(long, default_value_t = 100_000)]
        max_bytes: usize,
        #[arg(long, default_value_t = 180)]
        timeout: u64,
        #[arg(last=true, required=true, num_args=1..)]
        command: Vec<String>,
    },
    /// Persist a maintainer decision; exact patch sets retain it across rebases.
    Decide {
        report: PathBuf,
        feature: String,
        #[arg(long, value_enum)]
        status: DecisionStatus,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Args)]
struct ShortlistArgs {
    report: PathBuf,
    #[arg(long, help = "Prior scan; compare patch identity, not commit dates")]
    baseline: Option<PathBuf>,
    #[arg(
        long,
        requires = "baseline",
        help = "Select only newly observed patches; prior coverage gaps remain explicit"
    )]
    only_new: bool,
    #[arg(
        long,
        conflicts_with = "demand_snapshot",
        help = "Enumerate the unfiltered project issue/discussion catalog"
    )]
    refresh_demand: bool,
    #[arg(long, help = "Reuse a saved demand catalog offline")]
    demand_snapshot: Option<PathBuf>,
    #[arg(long, requires = "refresh_demand")]
    write_demand_snapshot: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "pr_snapshot",
        help = "Read all available open upstream PRs and their commit membership"
    )]
    with_prs: bool,
    #[arg(long, help = "Reuse a saved PR snapshot offline")]
    pr_snapshot: Option<PathBuf>,
    #[arg(
        long,
        requires = "with_prs",
        help = "Save PR membership for repeatable offline runs"
    )]
    write_pr_snapshot: Option<PathBuf>,
    #[arg(long, default_value_t = 500)]
    api_budget: usize,
    #[arg(
        long,
        default_value_t = 10,
        help = "Maximum suggested candidates; 0 means all eligible; never calls a model"
    )]
    limit: usize,
    #[arg(short, long, default_value = "forkpicker-shortlist.json")]
    output: PathBuf,
    #[arg(long)]
    html: Option<PathBuf>,
}

#[derive(Args)]
struct ReviewArgs {
    report: PathBuf,
    #[arg(required_unless_present_any = ["all", "shortlist", "triage"], conflicts_with_all = ["all", "shortlist", "triage"])]
    feature: Option<String>,
    #[arg(
        long,
        conflicts_with_all = ["shortlist", "triage"],
        help = "Batch review visible candidates in author-recency order (default: at most five new calls)"
    )]
    all: bool,
    #[arg(
        long,
        conflicts_with = "triage",
        help = "Review the explicitly selected IDs in an evidence-matched demand shortlist"
    )]
    shortlist: Option<PathBuf>,
    /// Review model-plausible matches from a validated triage run.
    #[arg(long)]
    triage: Option<PathBuf>,
    #[arg(
        long,
        value_enum,
        requires = "all",
        help = "Opt in to legacy heuristic priority filtering for --all"
    )]
    min_priority: Option<priority::Tier>,
    #[arg(
        long,
        help = "CLI adapter name; otherwise use default_agent from config"
    )]
    agent: Option<String>,
    #[arg(long, help = "Override the CLI's configured model")]
    model: Option<String>,
    #[arg(long, help = "Override the CLI's configured reasoning effort")]
    effort: Option<String>,
    #[arg(
        long,
        default_value_t = 5,
        help = "Maximum new model calls this run; 0 means unlimited"
    )]
    limit: usize,
    #[arg(long, default_value_t = 200_000)]
    max_bytes: usize,
    #[arg(
        long,
        default_value_t = 300,
        help = "Seconds allowed per CLI invocation"
    )]
    timeout: u64,
    #[arg(long, help = "Repeat reviews even when matching cached results exist")]
    force: bool,
    #[arg(
        long,
        help = "Show candidate count, prompt sizes, and cache hits without invoking a model"
    )]
    dry_run: bool,
    #[arg(
        long,
        help = "Local Git repository for source context; otherwise use the scan cache"
    )]
    repo: Option<PathBuf>,
    #[arg(long, help = "Render an HTML report after each completed review")]
    html: Option<PathBuf>,
}

#[derive(Clone, ValueEnum)]
enum Format {
    Terminal,
    Markdown,
    Html,
    Json,
}
#[derive(Clone, ValueEnum)]
enum DecisionStatus {
    New,
    Saved,
    Dismissed,
    NeedsAdopter,
    Adopted,
}
impl DecisionStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Saved => "saved",
            Self::Dismissed => "dismissed",
            Self::NeedsAdopter => "needs_adopter",
            Self::Adopted => "adopted",
        }
    }
}

#[derive(Args)]
struct CommonScan {
    #[arg(
        long,
        default_value_t = 500,
        help = "Non-merge commits inspected per branch; 0 means unlimited"
    )]
    max_commits: usize,
    #[arg(
        long,
        default_value_t = 2000,
        help = "Upstream commits indexed for patch equivalence; 0 means all"
    )]
    upstream_history: usize,
    #[arg(
        short,
        long,
        default_value = "forkpicker-report.json",
        help = "Portable JSON report (includes evidence)"
    )]
    output: PathBuf,
    #[arg(long, help = "Also write a searchable standalone HTML report")]
    html: Option<PathBuf>,
    #[arg(
        long,
        help = "Exit with code 2 after writing a report with incomplete coverage"
    )]
    strict: bool,
}

#[derive(Args)]
struct RemoteArgs {
    repository: RepoName,
    #[arg(
        long = "fork",
        help = "Inspect only these repositories (repeatable); bypass network enumeration"
    )]
    forks: Vec<RepoName>,
    #[arg(
        long,
        default_value_t = 0,
        help = "Forks selected after enumeration by push date; 0 means all"
    )]
    max_forks: usize,
    #[arg(
        long,
        default_value_t = 0,
        help = "Branches per fork; default branch first; 0 means all"
    )]
    max_branches: usize,
    #[arg(long = "branch", help = "Include only these branch names (repeatable)")]
    branches: Vec<String>,
    #[arg(long, help = "Compare only default branches")]
    default_branch_only: bool,
    #[arg(long, help = "Override upstream's default branch")]
    base: Option<String>,
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=32), help = "Concurrent GitHub branch enumeration requests (shared budget; at most 8 requests/sec)")]
    jobs: u16,
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=16), help = "Concurrent incremental Git fetches into the shared object store")]
    fetch_jobs: u16,
    #[arg(long, default_value_t = 500)]
    api_budget: usize,
    #[arg(long, help = "Revalidate cached API responses immediately")]
    refresh: bool,
    #[arg(
        long,
        help = "Prioritize features matching fork commit messages and changed paths"
    )]
    query: Option<String>,
    #[arg(
        long,
        help = "Attach optional upstream issue/PR/discussion context without affecting discovery or priority"
    )]
    with_context: bool,
    #[arg(
        long,
        help = "Use project issue/discussion demand to inform review recommendations"
    )]
    with_demand: bool,
    #[arg(
        long = "priority-label",
        help = "Urgent project labels; repeatable; replaces defaults"
    )]
    priority_labels: Vec<String>,
    #[command(flatten)]
    common: CommonScan,
}

#[derive(Args)]
struct LocalArgs {
    path: PathBuf,
    #[arg(long, default_value = "main")]
    base: String,
    #[arg(
        long = "upstream-ref",
        help = "Additional known upstream refs, e.g. a release branch (repeatable)"
    )]
    upstream_refs: Vec<String>,
    #[arg(
        long = "ref",
        required = true,
        help = "Fork/feature refs to inspect (repeatable)"
    )]
    refs: Vec<String>,
    #[command(flatten)]
    common: CommonScan,
}

fn directory(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
        .unwrap_or_else(|| PathBuf::from(".forkpicker"))
        .join("forkpicker")
}

fn emit(text: &str, output: Option<&Path>) -> Result<()> {
    if let Some(path) = output {
        write_atomic(path, text.as_bytes())?;
    } else {
        std::io::stdout().lock().write_all(text.as_bytes())?;
    }
    Ok(())
}

fn save_scan(
    mut report: Report,
    common: &CommonScan,
    decisions: &Path,
    query: Option<&str>,
) -> Result<bool> {
    state::apply(&mut report, decisions)?;
    priority::refresh(&mut report);
    if let Some(query) = query {
        report.features.sort_by(|a, b| {
            analyze::search_score(b, &report.commits, query)
                .cmp(&analyze::search_score(a, &report.commits, query))
                .then(a.id.cmp(&b.id))
        });
    }
    write_json(&common.output, &report)?;
    if let Some(path) = &common.html {
        write_atomic(path, render::html(&report, None, false).as_bytes())?;
    }
    emit(&render::terminal(&report, None, false), None)?;
    eprintln!("Saved {}", common.output.display());
    Ok(common.strict
        && (!report.coverage.warnings.is_empty()
            || report.coverage.forks_omitted > 0
            || report.coverage.branches_omitted > 0))
}

fn remote(args: RemoteArgs, cache: &Path, decisions: &Path) -> Result<bool> {
    let started = Instant::now();
    let mut api = Github::new(cache.into(), args.refresh, args.api_budget)?;
    let root = api.repository(&args.repository)?;
    let mut coverage = Coverage {
        scope: if args.forks.is_empty() {
            "recursive GitHub fork network"
        } else {
            "explicitly selected forks"
        }
        .into(),
        ..Default::default()
    };
    eprintln!("Discovering {}…", root.full_name);
    let mut forks = if args.forks.is_empty() {
        api.forks(&root, &mut coverage)
    } else {
        let mut selected = Vec::new();
        let mut seen = BTreeSet::new();
        for name in &args.forks {
            match api.repository(name) {
                Ok(fork) => {
                    if seen.insert(fork.id) {
                        selected.push(fork);
                    }
                }
                Err(e) => coverage
                    .warnings
                    .push(format!("Cannot inspect {}: {e}", name.0)),
            }
        }
        coverage.forks_discovered = selected.len();
        selected
    };
    forks.retain(|fork| {
        if fork.disabled {
            coverage
                .warnings
                .push(format!("Skipped disabled fork {}", fork.full_name));
            false
        } else {
            true
        }
    });
    if args.max_forks > 0 {
        forks.truncate(args.max_forks);
    }
    coverage.forks_selected = forks.len();
    eprintln!(
        "Discovered {} forks; inspecting {}.",
        coverage.forks_discovered, coverage.forks_selected
    );
    coverage.forks_omitted = coverage.forks_discovered.saturating_sub(forks.len());
    if coverage.forks_omitted > 0 {
        coverage.warnings.push(format!(
            "{} discovered forks were not selected; use --max-forks 0 to inspect all",
            coverage.forks_omitted
        ));
    }
    let repo_cache = cache
        .join("repositories")
        .join(forkpicker::hash(root.full_name.to_lowercase()));
    std::fs::create_dir_all(&repo_cache)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(repo_cache.join("scan.lock"))?;
    lock.lock_exclusive()?;
    let git = Git::init_bare(&repo_cache.join("git"))?;
    eprintln!("Fetching upstream branches and tags into the shared Git cache…");
    git.fetch(
        &root.full_name,
        &[
            "+refs/heads/*:refs/remotes/upstream/*".into(),
            "+refs/tags/*:refs/tags/upstream/*".into(),
        ],
    )?;
    let base_branch = args.base.as_deref().unwrap_or(&root.default_branch);
    let base_ref = format!("refs/remotes/upstream/{base_branch}");
    let mut known = git.refs("refs/remotes/upstream/")?;
    known.extend(git.refs("refs/tags/upstream/")?);
    let metadata_started = Instant::now();
    eprintln!("Enumerating branches with {} API workers…", args.jobs);
    let branch_results = forkpicker::parallel::map(&forks, args.jobs as usize, |i, fork| {
        let result = api.clone().branches(fork);
        if (i + 1) % 25 == 0 || i + 1 == forks.len() {
            eprintln!("[branches {}/{}] {}", i + 1, forks.len(), fork.full_name);
        }
        result
    });
    coverage.performance.branch_enumeration_ms = metadata_started.elapsed().as_millis() as u64;
    let mut selected = Vec::new();
    for (fork, result) in forks.iter().zip(branch_results) {
        let branches = match result {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Branch enumeration failed for {}: {e}", fork.full_name);
                coverage.warnings.push(format!(
                    "Branch enumeration failed for {}: {e}",
                    fork.full_name
                ));
                continue;
            }
        };
        coverage.branches_discovered += branches.len();
        let count = branches.len();
        let mut branches: Vec<_> = branches
            .into_iter()
            .filter(|b| {
                (!args.default_branch_only || b.name == fork.default_branch)
                    && (args.branches.is_empty() || args.branches.contains(&b.name))
            })
            .collect();
        if args.max_branches > 0 {
            branches.truncate(args.max_branches);
        }
        coverage.branches_omitted += count.saturating_sub(branches.len());
        if branches.is_empty() {
            coverage
                .warnings
                .push(format!("No selected branches in {}", fork.full_name));
            continue;
        }
        selected.push((fork, branches));
    }
    let fetch_started = Instant::now();
    eprintln!(
        "Fetching missing branch tips with {} Git workers…",
        args.fetch_jobs
    );
    let results = forkpicker::parallel::map(
        &selected,
        args.fetch_jobs as usize,
        |i, (fork, branches)| {
            let specs: Vec<_> = branches
                .iter()
                .enumerate()
                .filter(|(_, b)| git.resolve(&b.commit.sha).is_err())
                .map(|(j, b)| format!("+{}:refs/forkpicker/{}/{j}", b.commit.sha, fork.id))
                .collect();
            eprintln!(
                "[fork {}/{}] {} · {} branches · {} tips to fetch",
                i + 1,
                selected.len(),
                fork.full_name,
                branches.len(),
                specs.len()
            );
            let fetched = if specs.is_empty() {
                Ok(())
            } else {
                git.fetch(&fork.full_name, &specs)
            };
            (specs.len(), fetched)
        },
    );
    let mut sources = Vec::new();
    for ((fork, branches), (missing, result)) in selected.into_iter().zip(results) {
        coverage.performance.cached_branch_tips += branches.len() - missing;
        coverage.performance.fetched_branch_tips += missing;
        coverage.performance.git_fetches += usize::from(missing > 0);
        let fetch_failed = result.is_err();
        if let Err(error) = result {
            eprintln!("Fetch failed for {}: {error}", fork.full_name);
            coverage
                .warnings
                .push(format!("Fetch failed for {}: {error}", fork.full_name));
        }
        // A failed fetch must not hide healthy branches already in the cache.
        for branch in branches {
            if fetch_failed && git.resolve(&branch.commit.sha).is_err() {
                coverage.warnings.push(format!(
                    "Branch unavailable after failed fetch: {}:{} at {}",
                    fork.full_name, branch.name, branch.commit.sha
                ));
                continue;
            }
            sources.push(Source {
                repository: fork.full_name.clone(),
                branch: branch.name,
                tip: branch.commit.sha,
                url: Some(format!("https://github.com/{}", fork.full_name)),
            });
        }
    }
    coverage.performance.fork_fetch_ms = fetch_started.elapsed().as_millis() as u64;
    if coverage.branches_omitted > 0 {
        coverage.warnings.push(format!(
            "{} branch(es) omitted by filters or --max-branches",
            coverage.branches_omitted
        ));
    }
    let analysis_started = Instant::now();
    let mut report = scan::inspect(
        &git,
        &root.full_name,
        &base_ref,
        known,
        sources,
        cache,
        &ScanOptions {
            max_commits: args.common.max_commits,
            upstream_history: args.common.upstream_history,
        },
    )?;
    coverage.performance.analysis_ms = analysis_started.elapsed().as_millis() as u64;
    coverage.upstream_commits_indexed = report.coverage.upstream_commits_indexed;
    coverage.upstream_commits_available = report.coverage.upstream_commits_available;
    coverage.warnings.extend(report.coverage.warnings);
    if args.with_context {
        report.issues = api.issues(&root.full_name, args.query.as_deref(), &mut coverage);
        report.issues.extend(api.discussions(
            &root.full_name,
            args.query.as_deref(),
            &mut coverage,
        ));
        analyze::link_issues(&mut report.features, &report.issues, &report.commits);
        let urls = report
            .features
            .iter()
            .flat_map(|f| f.issue_links.iter().cloned())
            .collect();
        api.comments(&root.full_name, &mut report.issues, &urls, &mut coverage);
    }
    if args.with_demand {
        let labels = if args.priority_labels.is_empty() {
            priority::default_priority_labels()
        } else {
            args.priority_labels
        };
        report.demand = Some(api.demand(&root.full_name, args.query.as_deref(), labels));
    }
    coverage.api_requests = api.requests();
    coverage.cache_hits = api.hits();
    coverage.performance.total_ms = started.elapsed().as_millis() as u64;
    coverage.performance.api_jobs = args.jobs as usize;
    coverage.performance.fetch_jobs = args.fetch_jobs as usize;
    eprintln!("Scan: {:.1}s total · {:.1}s branch enumeration · {:.1}s fork fetch · {:.1}s analysis · {} fork fetches · {} cached tips · {} API requests",
        coverage.performance.total_ms as f64 / 1000.0,
        coverage.performance.branch_enumeration_ms as f64 / 1000.0,
        coverage.performance.fork_fetch_ms as f64 / 1000.0,
        coverage.performance.analysis_ms as f64 / 1000.0,
        coverage.performance.git_fetches, coverage.performance.cached_branch_tips, coverage.api_requests);
    report.coverage = coverage;
    save_scan(report, &args.common, decisions, args.query.as_deref())
}

fn local(args: LocalArgs, cache: &Path, decisions: &Path) -> Result<bool> {
    let path = std::fs::canonicalize(&args.path)?;
    let git = Git::new(&path);
    let repo = format!("local:{}", path.display());
    let mut known = BTreeMap::from([(args.base.clone(), git.resolve(&args.base)?)]);
    for reference in &args.upstream_refs {
        known.insert(reference.clone(), git.resolve(reference)?);
    }
    let mut sources = Vec::new();
    for reference in &args.refs {
        sources.push(Source {
            repository: repo.clone(),
            branch: reference.clone(),
            tip: git.resolve(reference)?,
            url: None,
        });
    }
    let mut report = scan::inspect(
        &git,
        &repo,
        &args.base,
        known,
        sources,
        cache,
        &ScanOptions {
            max_commits: args.common.max_commits,
            upstream_history: args.common.upstream_history,
        },
    )?;
    report.coverage.scope = "explicit local refs; no network access".into();
    report.coverage.forks_discovered = 1;
    report.coverage.forks_selected = 1;
    save_scan(report, &args.common, decisions, None)
}

fn run_reviews(
    args: ReviewArgs,
    config: review::Config,
    cache: &Path,
    decisions: &Path,
) -> Result<bool> {
    let agent = args.agent.or(config.default_agent).context("choose --agent codex|claude|grok|muse|opencode|pi, or set default_agent in the config file")?;
    let mut profile = config
        .agents
        .get(&agent)
        .with_context(|| format!("unknown agent {agent}; use agents to list profiles"))?
        .clone();
    if args.model.is_some() {
        profile.model = args.model;
    }
    if args.effort.is_some() {
        profile.effort = args.effort;
    }
    let mut report = scan::load_report(&args.report)?;
    state::apply(&mut report, decisions)?;
    let triage_context: Option<forkpicker::triage::Experiment> = args
        .triage
        .as_ref()
        .map(|path| -> Result<_> { Ok(serde_json::from_slice(&std::fs::read(path)?)?) })
        .transpose()?;
    let ids: Vec<String> =
        if let Some(path) = &args.shortlist {
            let selected: shortlist::Shortlist = serde_json::from_slice(&std::fs::read(path)?)?;
            shortlist::validate_selection(&selected, &report)?
                .into_iter()
                .filter(|id| {
                    report.features.iter().any(|f| {
                        &f.id == id && !["dismissed", "adopted"].contains(&f.status.as_str())
                    })
                })
                .collect()
        } else if let Some(experiment) = &triage_context {
            forkpicker::triage::validate_selection(experiment, &report)?
                .into_iter()
                .filter(|id| {
                    report.features.iter().any(|f| {
                        &f.id == id && !["dismissed", "adopted"].contains(&f.status.as_str())
                    })
                })
                .collect()
        } else if let Some(id) = args.feature {
            anyhow::ensure!(
                report.features.iter().any(|f| f.id == id),
                "feature not found"
            );
            vec![id]
        } else {
            args.min_priority
                .map_or_else(
                    || render::selected(&report, None, false),
                    |tier| priority::selected(&report, tier),
                )
                .iter()
                .map(|f| f.id.clone())
                .collect()
        };
    let filtered = if args.all {
        render::selected(&report, None, false)
            .len()
            .saturating_sub(ids.len())
    } else {
        0
    };
    if args.all {
        if let Some(tier) = args.min_priority {
            eprintln!("Legacy batch policy: {} priority or higher; {filtered} lower-priority candidates skipped", tier.label());
        } else {
            eprintln!("Batch order: author recency; no quality or impact ranking");
        }
        eprintln!(
            "New-call limit: {}",
            if args.limit == 0 {
                "unlimited".into()
            } else {
                args.limit.to_string()
            }
        );
    }
    let git = review::source_git(&report, cache, args.repo.as_deref());
    let mut calls = 0;
    let mut reused = 0;
    let mut deferred = 0;
    let mut failed = 0;
    eprintln!(
        "{} candidates · agent {} · model {} · effort {}",
        ids.len(),
        render::clean(&agent),
        render::clean(profile.model.as_deref().unwrap_or("CLI default")),
        render::clean(profile.effort.as_deref().unwrap_or("CLI default"))
    );
    for (i, id) in ids.iter().enumerate() {
        let mut pack = review::context(&report, id, args.max_bytes, git.as_ref())?;
        if let Some(experiment) = &triage_context {
            let card = experiment
                .cards
                .iter()
                .find(|c| &c.feature_id == id)
                .context("triage card missing")?;
            let assessment = experiment
                .batches
                .iter()
                .flat_map(|b| &b.response.assessments)
                .find(|a| &a.feature_id == id)
                .context("triage assessment missing")?;
            pack["triage_hypotheses"] = serde_json::json!({"assessment": assessment, "requests": card.requests, "instructions": "These are unverified hypotheses from a cheaper model, not established facts. Independently check whether the supplied code supports the claimed behavior and request fit. Include unsupported connections in summary or limitations. Cite only the review pack's original evidence_ids in the review response. Do not repeat a hypothesis as a fact."});
            llm::bound_context(&mut pack, args.max_bytes)?;
        }
        let request = review::Request {
            agent: &agent,
            profile: &profile,
            pack: &pack,
        };
        let key = request.key()?;
        let cache_path = cache.join("reviews").join(format!("{key}.json"));
        let cached = if args.force {
            None
        } else {
            report
                .reviews
                .iter()
                .find(|r| r.request_key == key)
                .cloned()
                .or_else(|| {
                    std::fs::read(&cache_path)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<review::ReviewRecord>(&b).ok())
                })
                .or_else(|| {
                    let raw: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(cache.join("review-responses").join(format!("{key}.json")))
                            .ok()?,
                    )
                    .ok()?;
                    request
                        .record(
                            &report,
                            raw["response"].clone(),
                            raw.get("provider_usage").cloned(),
                            raw["duration_ms"].as_u64()?,
                        )
                        .ok()
                })
                .filter(|r| {
                    r.request_key == key
                        && r.review.feature_id == *id
                        && review::validate(&report, &r.review).is_ok()
                })
        };
        let bytes = forkpicker::json_size(&pack)?;
        if let Some(record) = cached {
            reused += 1;
            if !args.dry_run {
                review::attach(&args.report, record)?;
            }
            eprintln!("[{}/{}] {id} · cached", i + 1, ids.len());
        } else if args.limit > 0 && calls >= args.limit {
            deferred += 1;
        } else {
            calls += 1;
            eprintln!(
                "[{}/{}] {id} · {bytes} prompt bytes{}",
                i + 1,
                ids.len(),
                if args.dry_run {
                    " · would review"
                } else {
                    " · reviewing"
                }
            );
            if !args.dry_run {
                match request.run_saved(&report, Duration::from_secs(args.timeout), cache) {
                    Ok(record) => {
                        let count = record.review.findings.len();
                        review::save_cache(cache, &record)?;
                        review::attach(&args.report, record)?;
                        eprintln!("Saved {count} unverified findings.");
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!(
                            "Review failed for {id}: {}",
                            render::clean(&format!("{e:#}"))
                        );
                        // Authentication, quota, and adapter errors should not exhaust a subscription
                        // by automatically retrying across the rest of a large fork network.
                        deferred += ids.len() - i - 1;
                        break;
                    }
                }
            }
        }
        if !args.dry_run {
            if let Some(path) = &args.html {
                let mut latest = scan::load_report(&args.report)?;
                state::apply(&mut latest, decisions)?;
                write_atomic(path, render::html(&latest, None, false).as_bytes())?;
            }
        }
    }
    eprintln!(
        "{}: {calls} {}, {reused} cached, {deferred} deferred, {failed} failed.{}",
        if args.dry_run { "Plan" } else { "Review run" },
        if args.dry_run {
            "planned calls"
        } else {
            "calls"
        },
        if deferred > 0 || failed > 0 {
            " Rerun to resume completed work."
        } else {
            ""
        }
    );
    Ok(deferred > 0 || failed > 0)
}

fn run(cli: Cli) -> Result<bool> {
    let config_explicit = cli.config.is_some();
    let config_path = cli
        .config
        .unwrap_or_else(|| directory("XDG_CONFIG_HOME", ".config").join("config.json"));
    let cache = cli
        .cache_dir
        .unwrap_or_else(|| directory("XDG_CACHE_HOME", ".cache"));
    let decisions = cli
        .state_dir
        .unwrap_or_else(|| directory("XDG_DATA_HOME", ".local/share"));
    match cli.command {
        Commands::Run(args) => {
            let config = if !args.uses_llm() {
                review::Config::default()
            } else {
                review::load_config(&config_path, config_explicit)?
            };
            return oneshot::run(args, &config, &cache, |command| {
                run(Cli {
                    command,
                    cache_dir: Some(cache.clone()),
                    state_dir: Some(decisions.clone()),
                    config: config_explicit.then(|| config_path.clone()),
                })
            });
        }
        Commands::MatchIssues(args) => {
            return forkpicker::issue_review::run(
                args,
                review::load_config(&config_path, config_explicit)?,
            )
        }
        Commands::Classify(args) => {
            return forkpicker::classify::run(
                args,
                review::load_config(&config_path, config_explicit)?,
                &cache,
                &decisions,
            )
        }
        Commands::Triage(args) => {
            return forkpicker::triage::run(
                args,
                review::load_config(&config_path, config_explicit)?,
                &cache,
                &decisions,
            )
        }
        Commands::Shortlist(args) => {
            let mut protected = vec![args.report.clone()];
            protected.extend(args.baseline.clone());
            protected.extend(args.pr_snapshot.clone());
            protected.extend(args.demand_snapshot.clone());
            let mut destinations = vec![args.output.clone()];
            destinations.extend(args.html.clone());
            destinations.extend(args.write_pr_snapshot.clone());
            destinations.extend(args.write_demand_snapshot.clone());
            for path in &destinations {
                anyhow::ensure!(
                    !protected.iter().any(|p| p == path
                        || (std::fs::canonicalize(p).ok().is_some()
                            && std::fs::canonicalize(p).ok() == std::fs::canonicalize(path).ok())),
                    "shortlist output must not overwrite an input snapshot"
                );
            }
            anyhow::ensure!(
                destinations.iter().collect::<BTreeSet<_>>().len() == destinations.len(),
                "shortlist output paths must be distinct"
            );
            let mut report = scan::load_report(&args.report)?;
            state::apply(&mut report, &decisions)?;
            let baseline = args
                .baseline
                .as_deref()
                .map(scan::load_report)
                .transpose()?;
            let mut prs: Option<shortlist::PullSnapshot> = args
                .pr_snapshot
                .as_ref()
                .map(|p| -> Result<_> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) })
                .transpose()?;
            if let Some(path) = &args.demand_snapshot {
                report.demand = Some(serde_json::from_slice(&std::fs::read(path)?)?);
            }
            if args.refresh_demand || args.with_prs {
                let repo: RepoName = report.repository.parse()?;
                let mut api = Github::new(cache.clone(), false, args.api_budget)?;
                if args.refresh_demand {
                    let labels = report
                        .demand
                        .as_ref()
                        .map(|d| d.priority_labels.clone())
                        .unwrap_or_else(priority::default_priority_labels);
                    report.demand = Some(api.demand_catalog(&repo.0, labels)?);
                }
                if args.with_prs {
                    prs = Some(shortlist::collect_pulls(&mut api, &repo.0)?);
                }
                eprintln!(
                    "Shortlist evidence: {} API requests; no model calls",
                    api.requests()
                );
            }
            if let Some(path) = &args.write_demand_snapshot {
                write_json(path, report.demand.as_ref().context("no demand snapshot")?)?;
            }
            if let Some(path) = &args.write_pr_snapshot {
                write_json(path, prs.as_ref().context("no PR snapshot")?)?;
            }
            let selection = shortlist::build(
                &report,
                baseline.as_ref(),
                prs.as_ref(),
                args.only_new,
                args.limit,
            )?;
            write_json(&args.output, &selection)?;
            if let Some(path) = &args.html {
                write_atomic(path, shortlist::html(&selection).as_bytes())?;
            }
            emit(&shortlist::terminal(&selection), None)?;
            eprintln!("Saved {}", args.output.display());
        }
        Commands::Demand {
            report: path,
            query,
            priority_labels,
            api_budget,
            refresh,
            html,
        } => {
            let report = scan::load_report(&path)?;
            let repo: RepoName = report
                .repository
                .parse()
                .context("demand retrieval requires a GitHub repository report")?;
            let mut api = Github::new(cache.clone(), refresh, api_budget)?;
            let labels = if priority_labels.is_empty() {
                priority::default_priority_labels()
            } else {
                priority_labels
            };
            let snapshot = api.demand(&repo.0, query.as_deref(), labels);
            let incomplete = !snapshot.warnings.is_empty();
            scan::update_report(&path, |report| {
                report.demand = Some(snapshot);
                Ok(())
            })?;
            let mut report = scan::load_report(&path)?;
            state::apply(&mut report, &decisions)?;
            if let Some(html) = html {
                write_atomic(&html, render::html(&report, None, false).as_bytes())?;
            }
            emit(&render::terminal(&report, None, false), None)?;
            eprintln!(
                "Updated demand recommendations using {} GitHub requests; no model invoked.",
                api.requests()
            );
            return Ok(incomplete);
        }
        Commands::Agents { write_config } => {
            let config = review::load_config(&config_path, config_explicit)?;
            if let Some(path) = write_config {
                use std::io::Write;
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(&serde_json::to_vec_pretty(&config)?)?;
                eprintln!(
                    "Wrote {}. Set default_agent and optional model/effort preferences.",
                    path.display()
                );
            } else {
                println!(
                    "CLI adapters · authentication and billing follow each CLI's configuration"
                );
                for (name, profile) in &config.agents {
                    let installed = profile
                        .command
                        .first()
                        .and_then(|p| review::executable(p))
                        .is_some();
                    println!(
                        "{name:12} {:12} model={} effort={}{}",
                        if installed { "installed" } else { "not found" },
                        profile.model.as_deref().unwrap_or("CLI default"),
                        profile.effort.as_deref().unwrap_or("CLI default"),
                        if config.default_agent.as_deref() == Some(name) {
                            " [default]"
                        } else {
                            ""
                        }
                    );
                    let classification = forkpicker::model_defaults::resolve_classification(
                        &config, name, None, None,
                    )?;
                    println!(
                        "             {} model={} effort={}",
                        if forkpicker::structured::Agent::parse(name).is_ok() {
                            "classification/triage"
                        } else {
                            "triage only (no native schema)"
                        },
                        render::clean(classification.model.as_deref().unwrap_or("CLI default")),
                        render::clean(classification.effort.as_deref().unwrap_or("CLI default"))
                    );
                }
            }
        }
        Commands::Review(args) => {
            let config = review::load_config(&config_path, config_explicit)?;
            return run_reviews(args, config, &cache, &decisions);
        }
        Commands::Scan(args) => return remote(args, &cache, &decisions),
        Commands::ScanLocal(args) => return local(args, &cache, &decisions),
        Commands::Inventory {
            report,
            baseline,
            check_apply,
            repo,
            target,
            jobs,
            policy,
            demand_snapshot,
            include_discussions,
            output,
            html,
        } => {
            let protected: Vec<_> = std::iter::once(&report)
                .chain(baseline.iter())
                .chain(policy.iter())
                .chain(demand_snapshot.iter())
                .collect();
            for dest in std::iter::once(&output).chain(html.iter()) {
                anyhow::ensure!(
                    !protected.iter().any(|source| *source == dest
                        || (dest.exists()
                            && source.canonicalize().ok() == dest.canonicalize().ok())),
                    "inventory output must not overwrite an input snapshot or policy"
                );
            }
            if let Some(html) = &html {
                // Canonicalize the parent even for new destinations, so aliases cannot collide.
                let destination = |p: &Path| -> Result<PathBuf> {
                    if p.exists() {
                        return Ok(p.canonicalize()?);
                    }
                    let parent = p
                        .parent()
                        .filter(|v| !v.as_os_str().is_empty())
                        .unwrap_or(Path::new("."));
                    std::fs::create_dir_all(parent)?;
                    Ok(parent
                        .canonicalize()?
                        .join(p.file_name().context("output needs a filename")?))
                };
                anyhow::ensure!(
                    destination(html)? != destination(&output)?,
                    "inventory JSON and HTML outputs must differ"
                );
            }
            let report_path = report.canonicalize()?.to_string_lossy().into_owned();
            let mut report = scan::load_report(&report)?;
            state::apply(&mut report, &decisions)?;
            let policy: metrics::Policy = policy
                .map(|p| -> Result<_> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) })
                .transpose()?
                .unwrap_or_default();
            policy.validate()?;
            let mut inventory = metrics::measure(&report, &policy);
            inventory.report_path = Some(report_path);
            inventory.decision_state_dir = Some(
                std::env::current_dir()?
                    .join(&decisions)
                    .to_string_lossy()
                    .into_owned(),
            );
            if let Some(path) = baseline {
                forkpicker::inbox::compare(&report, &mut inventory, &scan::load_report(&path)?)?;
            }
            if let Some(path) = demand_snapshot {
                let catalog: priority::DemandSnapshot =
                    serde_json::from_slice(&std::fs::read(path)?)?;
                metrics::add_histogram(&mut inventory, &report, &catalog, include_discussions);
            }
            if check_apply {
                let git = review::source_git(&report, &cache, repo.as_deref())
                    .context("Git objects unavailable; pass --repo PATH or omit --check-apply")?;
                let summary = forkpicker::integration::run(
                    &report,
                    &mut inventory,
                    &git,
                    &cache,
                    target.as_deref(),
                    jobs as usize,
                )?;
                println!(
                    "Integration: {} · {} cache hits · {:.2}s",
                    serde_json::to_string(&summary.statuses)?,
                    summary.cache_hits,
                    summary.elapsed_ms as f64 / 1000.0
                );
            }
            write_json(&output, &inventory)?;
            if let Some(path) = html {
                write_atomic(
                    &path,
                    render::html_with_inventory(&report, None, false, &inventory).as_bytes(),
                )?;
            }
            println!(
                "{} candidates measured offline; saved {}",
                inventory.candidates.len(),
                output.display()
            );
            for (name, values) in [
                (
                    "Change size",
                    inventory
                        .candidates
                        .iter()
                        .map(|f| f.change_size.as_str())
                        .collect::<Vec<_>>(),
                ),
                (
                    "File spread",
                    inventory
                        .candidates
                        .iter()
                        .map(|f| f.file_spread.as_str())
                        .collect(),
                ),
                (
                    "Patch series",
                    inventory
                        .candidates
                        .iter()
                        .map(|f| f.patch_series.as_str())
                        .collect(),
                ),
            ] {
                let mut counts = BTreeMap::new();
                for value in values {
                    *counts.entry(value).or_insert(0usize) += 1;
                }
                println!(
                    "{name}: {}",
                    counts
                        .iter()
                        .map(|(k, v)| format!("{k} {v}"))
                        .collect::<Vec<_>>()
                        .join(" · ")
                );
            }
        }
        Commands::Report {
            report,
            classification,
            shortlist: demand_path,
            format,
            query,
            all,
            output,
        } => {
            if let Some(path) = classification {
                anyhow::ensure!(
                    matches!(format, Format::Html) && query.is_none() && !all,
                    "--classification requires --format html and does not use --query/--all"
                );
                if let Some(dest) = &output {
                    for input in [&report, &path].into_iter().chain(demand_path.as_ref()) {
                        anyhow::ensure!(
                            input != dest
                                && !(dest.exists()
                                    && input.canonicalize()? == dest.canonicalize()?),
                            "output must not overwrite its input"
                        );
                    }
                }
                let source: Report = serde_json::from_slice(&std::fs::read(&report)?)?;
                let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                let run: forkpicker::classify::Run = serde_json::from_value(value.clone())?;
                anyhow::ensure!(
                    run.repository.eq_ignore_ascii_case(&source.repository)
                        && run.base_sha == source.base_sha
                        && run.source_fingerprint == shortlist::fingerprint(&source),
                    "classification does not match this scan"
                );
                let exploration: Vec<forkpicker::related::Exploration> = value
                    .get("exploration")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                let demand: Option<shortlist::Shortlist> = demand_path
                    .as_ref()
                    .map(|p| -> Result<_> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) })
                    .transpose()?;
                if let Some(d) = &demand {
                    shortlist::validate_selection(d, &source)?;
                }
                let text = if value.get("discovery_version").is_some() {
                    let discovery = serde_json::from_value(value)?;
                    forkpicker::discovery_html::html(&discovery, &source)
                } else {
                    forkpicker::classification_html::html(
                        &run,
                        &source,
                        &exploration,
                        demand.as_ref(),
                    )
                };
                emit(&text, output.as_deref())?;
                return Ok(false);
            }
            let mut report = scan::load_report(&report)?;
            state::apply(&mut report, &decisions)?;
            let text = match format {
                Format::Terminal => render::terminal(&report, query.as_deref(), all),
                Format::Markdown => render::markdown(&report, query.as_deref(), all),
                Format::Html => render::html(&report, query.as_deref(), all),
                Format::Json => {
                    let features = render::selected(&report, query.as_deref(), all)
                        .into_iter()
                        .cloned()
                        .collect();
                    report.features = features;
                    report
                        .assessments
                        .retain(|id, _| report.features.iter().any(|f| &f.id == id));
                    report
                        .reviews
                        .retain(|r| report.features.iter().any(|f| f.id == r.review.feature_id));
                    report
                        .recommendations
                        .retain(|id, _| report.features.iter().any(|f| &f.id == id));
                    serde_json::to_string_pretty(&report)?
                }
            };
            emit(&text, output.as_deref())?;
        }
        Commands::Show {
            report,
            feature,
            patch,
        } => {
            let mut report = scan::load_report(&report)?;
            state::apply(&mut report, &decisions)?;
            let f = report
                .features
                .iter()
                .find(|f| f.id == feature)
                .context("feature not found")?;
            emit(&render::feature_markdown(&report, f), None)?;
            if patch {
                for sha in &f.commits {
                    if let Some(c) = report.commits.get(sha) {
                        emit(
                            &format!("\ncommit {}\n{}\n", sha, render::clean(&c.patch)),
                            None,
                        )?;
                    }
                }
            }
        }
        Commands::Context {
            report,
            feature,
            max_bytes,
            output,
            review: code_review,
            repo,
        } => {
            let report = scan::load_report(&report)?;
            // Compact JSON preserves the exact byte budget promised by this command.
            emit(
                &serde_json::to_string(&if code_review {
                    review::context(
                        &report,
                        &feature,
                        max_bytes,
                        review::source_git(&report, &cache, repo.as_deref()).as_ref(),
                    )?
                } else {
                    llm::context(&report, &feature, max_bytes)?
                })?,
                output.as_deref(),
            )?;
        }
        Commands::Annotate {
            report: path,
            assessment,
        } => {
            let assessment: Assessment = serde_json::from_slice(&std::fs::read(assessment)?)?;
            scan::update_report(&path, |report| {
                llm::validate(report, &assessment)?;
                report
                    .assessments
                    .insert(assessment.feature_id.clone(), assessment);
                Ok(())
            })?;
            eprintln!("Imported unverified LLM assessment; deterministic findings unchanged.");
        }
        Commands::Enrich {
            report: path,
            feature,
            max_bytes,
            timeout,
            command,
        } => {
            let report = scan::load_report(&path)?;
            let pack = llm::context(&report, &feature, max_bytes)?;
            let assessment = llm::run_command(
                &command,
                &serde_json::to_vec(&pack)?,
                Duration::from_secs(timeout),
            )?;
            anyhow::ensure!(
                assessment.feature_id == feature,
                "LLM returned a different feature"
            );
            scan::update_report(&path, |report| {
                llm::validate(report, &assessment)?;
                report.assessments.insert(feature, assessment);
                Ok(())
            })?;
            eprintln!("Imported unverified LLM assessment; deterministic findings unchanged.");
        }
        Commands::Decide {
            report: path,
            feature,
            status,
            reason,
        } => {
            scan::update_report(&path, |report| {
                state::decide(report, &feature, status.as_str(), &reason, &decisions)?;
                state::apply(report, &decisions)
            })?;
            eprintln!("Recorded {}: {}", feature, status.as_str());
        }
    }
    Ok(false)
}

fn main() {
    match run(Cli::parse()) {
        Ok(incomplete) => {
            if incomplete {
                std::process::exit(2);
            }
        }
        Err(error) => {
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
            {
                return;
            }
            eprintln!("error: {}", render::clean(&format!("{error:#}")));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn discovery_defaults_widen_screening_without_raising_spending_limits() {
        let cli =
            Cli::try_parse_from(["forkpicker", "classify", "scan.json", "--discover"]).unwrap();
        let Commands::Classify(args) = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(args.screen_size, 200);
        assert_eq!(args.max_forks, 0);
        assert_eq!(args.limit, 5);
        assert_eq!(args.total_bytes, 240_000);
    }

    #[test]
    fn scan_defaults_include_all_forks_and_branches_without_conversation_requests() {
        let cli = Cli::try_parse_from(["forkpicker", "scan", "owner/repo"]).unwrap();
        let Commands::Scan(args) = cli.command else {
            panic!("wrong command")
        };
        assert_eq!(args.max_forks, 0);
        assert_eq!(args.max_branches, 0);
        assert_eq!(args.jobs, 4);
        assert_eq!(args.fetch_jobs, 4);
        assert!(!args.with_context);
    }

    #[test]
    fn scan_concurrency_is_bounded_and_serial_mode_is_available() {
        for (flag, value) in [
            ("--jobs", "0"),
            ("--jobs", "33"),
            ("--fetch-jobs", "0"),
            ("--fetch-jobs", "17"),
        ] {
            assert!(
                Cli::try_parse_from(["forkpicker", "scan", "owner/repo", flag, value]).is_err()
            );
        }
        assert!(Cli::try_parse_from([
            "forkpicker",
            "scan",
            "owner/repo",
            "--jobs",
            "1",
            "--fetch-jobs",
            "1"
        ])
        .is_ok());
    }

    #[test]
    fn review_requires_an_explicit_scope_and_preserves_cli_model_defaults() {
        assert!(
            Cli::try_parse_from(["forkpicker", "review", "report.json", "--agent", "codex"])
                .is_err()
        );
        let cli = Cli::try_parse_from([
            "forkpicker",
            "review",
            "report.json",
            "--all",
            "--agent",
            "codex",
        ])
        .unwrap();
        let Commands::Review(args) = cli.command else {
            panic!("wrong command")
        };
        assert!(args.model.is_none());
        assert!(args.effort.is_none());
        assert!(!args.force);
        assert_eq!(args.min_priority, None);
        assert_eq!(args.limit, 5);
    }
}
