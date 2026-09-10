//! Static integration evidence against a pinned target. Never executes fork code.
use crate::{git::Git, hash, metrics::Inventory, model::*, parallel, write_json};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const VERSION: u32 = 1;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Overlap {
    pub source_tip: String,
    pub merge_base: Option<String>,
    pub touched_files: usize,
    pub changed_upstream: Vec<String>,
    pub deleted_upstream: Vec<String>,
    pub renamed_upstream: Vec<(String, String)>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub method_version: u32,
    pub input_key: String,
    pub target_sha: String,
    pub checked_at: String,
    pub status: String,
    pub ordered_commits: Vec<String>,
    pub applied_commits: Vec<String>,
    pub failed_commit: Option<String>,
    pub conflicting_files: Vec<String>,
    pub overlap: Vec<Overlap>,
    pub possible_prerequisites: Vec<String>,
    pub notes: Vec<String>,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub cached: bool,
}
impl Check {
    fn new(target: &str, key: String, f: &Feature) -> Self {
        Self { method_version: VERSION, input_key:key, target_sha:target.into(), checked_at:chrono::Utc::now().to_rfc3339(), status:"unknown".into(), ordered_commits:vec![], applied_commits:vec![], failed_commit:None, conflicting_files:vec![], overlap:vec![], possible_prerequisites:f.context_commits.clone(), notes:vec!["Tests/builds not run. Clean application does not establish correctness. Possible prerequisites are unproven and are not automatically included.".into()], elapsed_ms:0, cached:false }
    }
    pub fn overlap_max(&self) -> Option<usize> {
        if self.overlap.is_empty() || self.overlap.iter().any(|o| o.error.is_some()) {
            return None;
        }
        self.overlap.iter().map(|o| o.changed_upstream.len()).max()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub target_sha: String,
    pub git_version: String,
    pub jobs: usize,
    pub elapsed_ms: u64,
    pub cache_hits: usize,
    pub statuses: BTreeMap<String, usize>,
}

pub fn label(status: &str) -> &str {
    match status {
        "clean" => "Applies cleanly",
        "clean-three-way" => "Applies with a three-way merge",
        "conflicts" => "Conflicting files",
        "not-applicable" => "Did not apply",
        "unknown" => "Could not assess",
        "no-file-changes" => "No file changes",
        _ => "Not checked",
    }
}

// No inherited Git configuration, hooks, replacement refs, lazy fetches, or custom merge drivers.
// Temporary repositories read shared objects but write only to their own object store and index.
struct Sandbox {
    dir: tempfile::TempDir,
}
struct Output {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
}
impl Sandbox {
    fn new(objects: &Path) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let mut command = Self::command(dir.path());
        let output = command
            .args(["init", "--bare", "--quiet", "--template=", "."])
            .output()?;
        ensure!(
            output.status.success(),
            "cannot initialize temporary Git repository"
        );
        ensure!(
            !objects.as_os_str().as_encoded_bytes().contains(&b'\n'),
            "object directory contains a newline"
        );
        fs::write(
            dir.path().join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )?;
        Ok(Self { dir })
    }
    fn command(root: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "core.quotePath=false",
                "-c",
                "merge.renormalize=false",
                "-c",
                "merge.conflictStyle=merge",
            ])
            .current_dir(root);
        cmd
    }
    fn run(&self, args: &[&str], input: &[u8]) -> Result<Output> {
        let input_path = self.dir.path().join("input");
        let stdout_path = self.dir.path().join("stdout");
        let stderr_path = self.dir.path().join("stderr");
        fs::write(&input_path, input)?;
        let mut child = Self::command(self.dir.path())
            .args(args)
            .stdin(Stdio::from(fs::File::open(input_path)?))
            .stdout(Stdio::from(fs::File::create(&stdout_path)?))
            .stderr(Stdio::from(fs::File::create(&stderr_path)?))
            .spawn()?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > Duration::from_secs(60) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Git operation timed out: {}", args[0]);
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        ensure!(
            fs::metadata(&stdout_path)?.len() <= 128 * 1024 * 1024
                && fs::metadata(&stderr_path)?.len() <= 1024 * 1024,
            "Git output exceeds inspection limit"
        );
        Ok(Output {
            success: status.success(),
            stdout: fs::read(stdout_path)?,
            stderr: String::from_utf8_lossy(&fs::read(stderr_path)?).into_owned(),
        })
    }
    fn checked(&self, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
        let out = self.run(args, input)?;
        ensure!(out.success, "{}: {}", args[0], out.stderr.trim());
        Ok(out.stdout)
    }
}
fn text(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).context("non-UTF8 Git evidence is not supported")
}
fn valid_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
}

struct Graph {
    nodes: BTreeMap<String, (usize, Vec<String>)>,
}
impl Graph {
    fn load(sandbox: &Sandbox, features: &[Feature], target: &str) -> Result<Self> {
        let mut tips: BTreeSet<_> = features
            .iter()
            .flat_map(|f| f.commits.iter().cloned())
            .collect();
        tips.insert(target.into());
        ensure!(tips.iter().all(|s| valid_sha(s)), "invalid commit identity");
        // Missing candidate objects should not prevent checks of other candidates.
        let probe = text(
            sandbox.checked(
                &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
                tips.iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
                    .as_bytes(),
            )?,
        )?;
        let known: Vec<_> = probe
            .lines()
            .filter_map(|l| l.strip_suffix(" commit"))
            .collect();
        let history = text(sandbox.checked(
            &[
                "rev-list",
                "--parents",
                "--topo-order",
                "--reverse",
                "--stdin",
            ],
            known.join("\n").as_bytes(),
        )?)?;
        let nodes = history
            .lines()
            .enumerate()
            .filter_map(|(i, l)| {
                let mut p = l.split_whitespace();
                Some((p.next()?.into(), (i, p.map(str::to_owned).collect())))
            })
            .collect();
        Ok(Self { nodes })
    }
    fn ancestor(&self, older: &str, newer: &str) -> bool {
        let Some((rank, _)) = self.nodes.get(older) else {
            return false;
        };
        let mut stack = vec![newer];
        let mut seen = BTreeSet::new();
        while let Some(sha) = stack.pop() {
            if sha == older {
                return true;
            }
            if !seen.insert(sha) {
                continue;
            }
            if let Some((r, parents)) = self.nodes.get(sha) {
                if r > rank {
                    stack.extend(parents.iter().map(String::as_str));
                }
            }
        }
        false
    }
    fn series(&self, report: &Report, f: &Feature) -> Result<Vec<String>> {
        ensure!(!f.commits.is_empty(), "no candidate commits");
        ensure!(
            f.commits
                .iter()
                .all(|s| self.nodes.contains_key(s) && report.commits.contains_key(s)),
            "missing candidate Git objects or commit records"
        );
        let identity = |s: &String| {
            report.commits[s]
                .patch_id
                .clone()
                .unwrap_or_else(|| s.clone())
        };
        let expected: BTreeSet<_> = f.commits.iter().map(identity).collect();
        let mut candidates = f.commits.clone();
        candidates.sort_by_key(|s| self.nodes[s].0);
        for end in candidates.iter().rev() {
            let mut seen = BTreeSet::new();
            let series: Vec<_> = candidates
                .iter()
                .filter(|s| self.ancestor(s, end))
                .filter(|s| seen.insert(identity(s)))
                .cloned()
                .collect();
            if seen == expected {
                ensure!(
                    series.iter().all(|s| self.nodes[s].1.len() == 1),
                    "root or merge commit requires explicit parent selection"
                );
                return Ok(series);
            }
        }
        bail!("candidate patch identities span incompatible histories; cannot establish one ancestry-consistent series")
    }
}

fn overlap(sandbox: &Sandbox, target: &str, tip: &str, paths: &BTreeSet<String>) -> Overlap {
    let mut result = Overlap {
        source_tip: tip.into(),
        merge_base: None,
        touched_files: paths.len(),
        changed_upstream: vec![],
        deleted_upstream: vec![],
        renamed_upstream: vec![],
        error: None,
    };
    let measured = (|| -> Result<()> {
        ensure!(valid_sha(tip), "invalid source tip");
        let bases = text(sandbox.checked(&["merge-base", "--all", target, tip], b"")?)?;
        ensure!(
            bases.lines().count() == 1,
            "missing or multiple common ancestors"
        );
        let base = bases.trim();
        result.merge_base = Some(base.into());
        // Detect renames over the whole upstream diff, then intersect paths. Path-limited
        // rename detection could miss a destination outside the candidate's file set.
        let diff = text(sandbox.checked(
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-status",
                "-z",
                "--find-renames=50%",
                "-l1000",
                base,
                target,
                "--",
            ],
            b"",
        )?)?;
        let mut fields = diff.split('\0').filter(|s| !s.is_empty());
        let mut changed = BTreeSet::new();
        while let Some(status) = fields.next() {
            let path = fields.next().context("missing changed path")?;
            if status.starts_with('R') {
                let dest = fields.next().context("missing rename destination")?;
                if paths.contains(path) || paths.contains(dest) {
                    result.renamed_upstream.push((path.into(), dest.into()));
                }
                if paths.contains(path) {
                    changed.insert(path.into());
                }
                if paths.contains(dest) {
                    changed.insert(dest.into());
                }
            } else if paths.contains(path) {
                changed.insert(path.into());
                if status == "D" {
                    result.deleted_upstream.push(path.into());
                }
            }
        }
        result.changed_upstream = changed.into_iter().collect();
        Ok(())
    })();
    if let Err(e) = measured {
        result.error = Some(format!("{e:#}"));
    }
    result
}

fn inspect(
    sandbox: &Sandbox,
    graph: &Graph,
    report: &Report,
    f: &Feature,
    result: &mut Check,
) -> Result<()> {
    let series = graph.series(report, f)?;
    result.ordered_commits = series.clone();
    let mut paths = BTreeSet::new();
    let mut patches = Vec::new();
    for sha in &series {
        // Full patches from pinned objects, never possibly truncated portable excerpts.
        let patch = sandbox.checked(
            &[
                "diff-tree",
                "--no-commit-id",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--full-index",
                "--binary",
                "-p",
                &format!("{sha}^"),
                sha,
                "--",
            ],
            b"",
        )?;
        ensure!(
            patch.len() <= 8 * 1024 * 1024,
            "candidate patch exceeds 8 MiB inspection limit"
        );
        let names = text(sandbox.checked(
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "--no-renames",
                "-r",
                "-z",
                &format!("{sha}^"),
                sha,
                "--",
            ],
            b"",
        )?)?;
        paths.extend(
            names
                .split('\0')
                .filter(|p| !p.is_empty())
                .map(str::to_owned),
        );
        patches.push(patch);
    }
    for tip in f.sources.iter().map(|s| &s.tip).collect::<BTreeSet<_>>() {
        result
            .overlap
            .push(overlap(sandbox, &result.target_sha, tip, &paths));
    }
    if paths.is_empty() {
        result.status = "no-file-changes".into();
        return Ok(());
    }
    sandbox.checked(&["read-tree", &result.target_sha], b"")?;
    let mut three_way = false;
    for (sha, patch) in series.iter().zip(patches) {
        if patch.is_empty() {
            continue;
        }
        let direct = sandbox.run(
            &["apply", "--cached", "--check", "--whitespace=nowarn", "-"],
            &patch,
        )?;
        if direct.success {
            sandbox.checked(&["apply", "--cached", "--whitespace=nowarn", "-"], &patch)?;
        } else {
            let merged = sandbox.run(
                &["apply", "--cached", "--3way", "--whitespace=nowarn", "-"],
                &patch,
            )?;
            if !merged.success {
                result.failed_commit = Some(sha.clone());
                let unmerged = text(sandbox.checked(&["ls-files", "--unmerged", "-z"], b"")?)?;
                result.conflicting_files = unmerged
                    .split('\0')
                    .filter_map(|l| l.split_once('\t').map(|(_, p)| p.to_owned()))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                result.status = if result.conflicting_files.is_empty() {
                    "not-applicable"
                } else {
                    "conflicts"
                }
                .into();
                result.notes.push(format!("Stopped at the first unsuccessful commit. Later patches were not assessed. Direct check: {} Three-way attempt: {}",direct.stderr.trim(),merged.stderr.trim()));
                result.notes.push("Failure may reflect omitted prerequisites; it does not prove the feature cannot be ported.".into());
                return Ok(());
            }
            three_way = true;
        }
        result.applied_commits.push(sha.clone());
    }
    result.status = if three_way {
        "clean-three-way"
    } else {
        "clean"
    }
    .into();
    Ok(())
}

pub fn run(
    report: &Report,
    inventory: &mut Inventory,
    git: &Git,
    cache: &Path,
    target: Option<&str>,
    jobs: usize,
) -> Result<Summary> {
    let started = Instant::now();
    let objects = PathBuf::from(
        git.run(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])?
        .trim(),
    )
    .canonicalize()?;
    let target = match target {
        Some(reference) => git.resolve(reference)?,
        None => report.base_sha.clone(),
    };
    ensure!(valid_sha(&target), "invalid integration target");
    // A shallow graph cannot establish complete ancestry. Copying only its objects
    // without its shallow boundary would also give misleading traversal failures.
    ensure!(
        git.run(&["rev-parse", "--is-shallow-repository"])?.trim() == "false",
        "integration checks require a non-shallow object store"
    );
    let master = Sandbox::new(&objects)?;
    master.checked(&["cat-file", "-e", &format!("{target}^{{commit}}")], b"")?;
    let version = text(master.checked(&["--version"], b"")?)?
        .trim()
        .to_owned();
    eprintln!(
        "Integration: building ancestry index; target {} · {jobs} workers",
        &target[..12]
    );
    let graph = Graph::load(&master, &report.features, &target)?;
    let directory = cache.join("integration-v1");
    fs::create_dir_all(&directory)?;
    let results = parallel::map(&report.features, jobs, |i, f| {
        let begun = Instant::now();
        let ids: Vec<_> = f
            .commits
            .iter()
            .map(|s| (s, report.commits.get(s).and_then(|c| c.patch_id.as_deref())))
            .collect();
        let key = hash(
            serde_json::to_vec(&(
                VERSION,
                &version,
                &target,
                &ids,
                &f.sources,
                &f.context_commits,
            ))
            .expect("serializable check identity"),
        );
        let path = directory.join(format!("{key}.json"));
        let mut result = Check::new(&target, key.clone(), f);
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(mut saved) = serde_json::from_slice::<Check>(&bytes) {
                if saved.input_key == key
                    && saved.method_version == VERSION
                    && saved.target_sha == target
                    && saved.status != "unknown"
                    && saved.overlap.iter().all(|o| o.error.is_none())
                {
                    saved.cached = true;
                    return saved;
                }
            }
        }
        let inspected = Sandbox::new(&objects)
            .and_then(|sandbox| inspect(&sandbox, &graph, report, f, &mut result));
        if let Err(e) = inspected {
            result.status = "unknown".into();
            result.notes.push(format!("{e:#}"));
        }
        result.elapsed_ms = begun.elapsed().as_millis() as u64;
        if result.status != "unknown" && result.overlap.iter().all(|o| o.error.is_none()) {
            if let Err(e) = write_json(&path, &result) {
                result.notes.push(format!("Could not cache check: {e}"));
            }
        }
        if (i + 1) % 100 == 0 || i + 1 == report.features.len() {
            eprintln!(
                "Integration [{}/{}] {}",
                i + 1,
                report.features.len(),
                result.status
            );
        }
        result
    });
    let mut summary = Summary {
        target_sha: target,
        git_version: version,
        jobs,
        elapsed_ms: started.elapsed().as_millis() as u64,
        cache_hits: 0,
        statuses: BTreeMap::new(),
    };
    let facts: BTreeMap<_, _> = report
        .features
        .iter()
        .zip(results)
        .map(|(f, result)| {
            *summary.statuses.entry(result.status.clone()).or_default() += 1;
            summary.cache_hits += usize::from(result.cached);
            (f.id.as_str(), result)
        })
        .collect();
    for f in &mut inventory.candidates {
        f.integration = facts.get(f.feature_id.as_str()).cloned();
    }
    inventory.integration = Some(summary.clone());
    inventory.forks = crate::forks::summarize(report, inventory);
    inventory.definitions.push("Integration checks replay one ancestry-consistent candidate patch series against the pinned target in a temporary Git index. Full cached patches are used, with direct application then a three-way attempt. Stop at the first failure; conflict paths describe that step, not the whole remaining series. Prerequisites are not automatically included. No fork code, hooks, custom merge drivers, builds or tests run.".into());
    inventory.definitions.push("Upstream overlap is the net file change from each source branch's common ancestor to the integration target, intersected with candidate paths. It includes upstream deletions and approximate rename matches (50% similarity, exhaustive rename search capped at 1000 paths). Counts are not conflict predictions. Missing/multiple merge bases remain unknown.".into());
    Ok(summary)
}

/// Check the exact union nominated by the model, without assuming it is a complete
/// feature or inventing an order between unrelated histories. No model calls.
pub fn check_set(report: &Report, ids: &[String], git: &Git) -> Result<Check> {
    let wanted: BTreeSet<_> = ids.iter().collect();
    let members: Vec<_> = report
        .features
        .iter()
        .filter(|f| wanted.contains(&f.id))
        .collect();
    ensure!(
        !members.is_empty() && members.len() == wanted.len(),
        "unknown or empty assembled candidate set"
    );
    let mut set = members[0].clone();
    set.commits = members
        .iter()
        .flat_map(|f| f.commits.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    set.patch_ids = members
        .iter()
        .flat_map(|f| f.patch_ids.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    set.files = members
        .iter()
        .flat_map(|f| f.files.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    set.context_commits = members
        .iter()
        .flat_map(|f| f.context_commits.clone())
        .filter(|s| !set.commits.contains(s))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    set.sources = members.iter().flat_map(|f| f.sources.clone()).collect();
    set.sources.sort_by(|a, b| {
        (&a.repository, &a.branch, &a.tip).cmp(&(&b.repository, &b.branch, &b.tip))
    });
    set.sources
        .dedup_by(|a, b| a.repository == b.repository && a.branch == b.branch && a.tip == b.tip);
    let objects = PathBuf::from(
        git.run(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])?
        .trim(),
    )
    .canonicalize()?;
    ensure!(
        git.run(&["rev-parse", "--is-shallow-repository"])?.trim() == "false",
        "assembled checks require non-shallow history"
    );
    let sandbox = Sandbox::new(&objects)?;
    sandbox.checked(
        &["cat-file", "-e", &format!("{}^{{commit}}", report.base_sha)],
        b"",
    )?;
    let graph = Graph::load(&sandbox, std::slice::from_ref(&set), &report.base_sha)?;
    let begun = Instant::now();
    let mut result = Check::new(
        &report.base_sha,
        hash(serde_json::to_vec(&(
            "assembled-v1",
            &report.base_sha,
            &set.commits,
            &set.sources,
        ))?),
        &set,
    );
    if let Err(error) = inspect(&sandbox, &graph, report, &set, &mut result) {
        result.status = "unknown".into();
        result.notes.push(format!("{error:#}"));
    }
    result.notes.push("Checked exactly the nominated union. Semantic dependencies and alternative implementations have not been resolved. Unrelated histories remain unknown rather than being arbitrarily ordered.".into());
    result.elapsed_ms = begun.elapsed().as_millis() as u64;
    Ok(result)
}
