use crate::{hash, model::*, write_json};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone)]
pub struct Git {
    pub path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PatchIndex {
    pub patches: BTreeMap<String, Vec<String>>,
    pub files: BTreeMap<String, Vec<String>>,
    pub indexed: usize,
    pub available: usize,
}

impl Git {
    pub fn source_file(
        &self,
        revision: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<(String, bool)>> {
        anyhow::ensure!(
            matches!(revision.len(), 40 | 64) && revision.bytes().all(|b| b.is_ascii_hexdigit()),
            "source revision must be an exact SHA"
        );
        let object = format!("{revision}:{path}");
        let size = match self.run(&["cat-file", "-s", &object]) {
            Ok(size) => size.trim().parse::<usize>()?,
            Err(_) => {
                self.run(&["cat-file", "-e", revision])?;
                return Ok(None);
            }
        };
        let mut child = self
            .command()
            .args(["cat-file", "blob", &object])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut bytes = Vec::new();
        let read = child
            .stdout
            .take()
            .context("missing blob output")?
            .take(max_bytes as u64)
            .read_to_end(&mut bytes);
        if size > max_bytes || read.is_err() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        read?;
        anyhow::ensure!(
            size > max_bytes || status.success(),
            "cannot read source blob"
        );
        anyhow::ensure!(!bytes.contains(&0), "binary source blob omitted");
        let truncated = size > bytes.len();
        Ok(Some((
            String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        )))
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn command(&self) -> Command {
        let mut c = Command::new("git");
        c.args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.quotePath=false",
            "-c",
            "diff.renames=false",
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=30",
            "-C",
        ])
        .arg(&self.path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
        c
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
            .output()
            .context("start git (Git must be installed)")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn init_bare(path: &Path) -> Result<Self> {
        if !path.join("HEAD").exists() {
            std::fs::create_dir_all(path)?;
            let result = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(path)
                .output()?;
            if !result.status.success() {
                bail!(
                    "cannot initialize Git cache: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
        }
        Ok(Self::new(path))
    }

    pub fn fetch(&self, repository: &str, specs: &[String]) -> Result<()> {
        let url = format!("https://github.com/{repository}.git");
        // URL comes exclusively from the validated GitHub owner/repository type.
        self.fetch_url(&url, specs)
    }

    fn fetch_url(&self, url: &str, specs: &[String]) -> Result<()> {
        let mut args = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
            "--prune",
            "--force",
            "--",
            url,
        ];
        args.extend(specs.iter().map(String::as_str));
        self.run(&args)?;
        Ok(())
    }

    pub fn resolve(&self, reference: &str) -> Result<String> {
        let sha = self.run(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ])?;
        Ok(sha.trim().to_owned())
    }

    pub fn refs(&self, prefix: &str) -> Result<BTreeMap<String, String>> {
        let data = self.run(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "--",
            prefix,
        ])?;
        Ok(data
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(r, s)| (r.to_owned(), s.to_owned()))
            .collect())
    }

    pub fn count(&self, args: &[String]) -> Result<usize> {
        let mut all = vec!["rev-list", "--count"];
        all.extend(args.iter().map(String::as_str));
        Ok(self.run(&all)?.trim().parse()?)
    }

    /// Read the complete diff, bypassing the portable commit cache's size limit.
    pub fn full_patch(&self, sha: &str) -> Result<String> {
        anyhow::ensure!(
            matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "patch revision must be an exact SHA"
        );
        self.run(&[
            "show",
            "--format=",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--binary",
            sha,
            "--",
        ])
    }

    pub fn commit(&self, sha: &str, cache: &Path) -> Result<Commit> {
        let file = cache.join("commits-v1").join(format!("{sha}.json"));
        if let Ok(data) = std::fs::read(&file) {
            if let Ok(value) = serde_json::from_slice::<Commit>(&data) {
                return Ok(value);
            }
        }
        let meta = self.run(&[
            "show",
            "-s",
            "--format=%H%x00%P%x00%an%x00%aI%x00%B",
            sha,
            "--",
        ])?;
        let parts: Vec<_> = meta.splitn(5, '\0').collect();
        if parts.len() != 5 {
            bail!("invalid commit metadata for {sha}");
        }
        let patch = self.run(&[
            "show",
            "--format=",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--binary",
            sha,
            "--",
        ])?;
        let ids = self.patch_ids(&format!("commit {sha}\n{patch}"))?;
        let mut value = Commit {
            sha: sha.into(),
            parents: parts[1].split_whitespace().map(String::from).collect(),
            author: parts[2].into(),
            date: parts[3].into(),
            subject: parts[4].lines().next().unwrap_or("").into(),
            message: parts[4].trim().into(),
            files: parse_files(&patch),
            patch_id: ids.keys().next().cloned(),
            patch,
            patch_truncated: false,
        };
        // Fingerprints are computed on the full patch. The portable evidence payload is bounded.
        const MAX_PATCH: usize = 256 * 1024;
        if value.patch.len() > MAX_PATCH {
            let mut end = MAX_PATCH;
            while !value.patch.is_char_boundary(end) {
                end -= 1;
            }
            value.patch.truncate(end);
            value.patch_truncated = true;
        }
        write_json(&file, &value)?;
        Ok(value)
    }

    pub fn patch_ids(&self, data: &str) -> Result<BTreeMap<String, String>> {
        let mut child = self
            .command()
            .args(["patch-id", "--stable"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().context("git stdin")?;
        let input = data.as_bytes().to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let out = child.wait_with_output()?;
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("patch-id writer failed"))??;
        if !out.status.success() {
            bail!(
                "git patch-id failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(a, b)| (a.into(), b.into()))
            .collect())
    }

    pub fn patch_index(
        &self,
        refs: &BTreeMap<String, String>,
        limit: usize,
        cache: &Path,
    ) -> Result<PatchIndex> {
        let key = hash(serde_json::to_vec(&(refs, limit))?);
        let file = cache.join("upstream-v1").join(format!("{key}.json"));
        if let Ok(bytes) = std::fs::read(&file) {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                return Ok(value);
            }
        }
        let mut count_args: Vec<String> = vec!["--no-merges".into()];
        count_args.extend(refs.values().cloned());
        count_args.push("--".into());
        let mut index = PatchIndex {
            available: self.count(&count_args)?,
            ..Default::default()
        };
        let mut args = vec![
            "log".to_owned(),
            "--no-merges".into(),
            "--format=%x1e%H%x1f".into(),
            "--patch".into(),
            "--binary".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-renames".into(),
        ];
        if limit > 0 {
            args.push(format!("--max-count={limit}"));
        }
        args.extend(refs.values().cloned());
        args.push("--".into());
        let log = self.run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        let mut standard = String::new();
        for record in log.split('\u{1e}').skip(1) {
            let Some((sha, patch)) = record.split_once('\u{1f}') else {
                continue;
            };
            index.indexed += 1;
            standard.push_str(&format!("commit {sha}\n{patch}\n"));
            for f in parse_files(patch) {
                if let Some(id) = f.patch_fingerprint {
                    index.files.entry(id).or_default().push(sha.into());
                }
            }
        }
        for (id, sha) in self.patch_ids(&standard)? {
            index.patches.entry(id).or_default().push(sha);
        }
        write_json(&file, &index)?;
        Ok(index)
    }
}

pub fn parse_files(patch: &str) -> Vec<FileChange> {
    let mut files = Vec::new();
    let prefixed = format!("\n{patch}");
    for section in prefixed.split("\ndiff --git ").skip(1) {
        let header = section.lines().next().unwrap_or("");
        let mut path = header
            .rsplit_once(" b/")
            .map(|(_, p)| p)
            .unwrap_or(header)
            .to_owned();
        let mut additions = 0;
        let mut deletions = 0;
        let mut symbols = Vec::new();
        let mut normalized = String::new();
        let mut binary = false;
        for line in section.lines().skip(1) {
            if let Some(name) = line.strip_prefix("+++ b/") {
                path = name.into();
            }
            if let Some(name) = line.strip_prefix("--- a/") {
                if section.contains("+++ /dev/null") {
                    path = name.into();
                }
            }
            if line.starts_with("GIT binary patch") || line.starts_with("Binary files ") {
                binary = true;
            }
            if let Some(hunk) = line.strip_prefix("@@") {
                if let Some((_, signature)) = hunk.split_once("@@") {
                    let signature = signature.trim();
                    if !signature.is_empty() {
                        symbols.push(signature.to_owned());
                    }
                }
            } else if (line.starts_with('+') && !line.starts_with("+++"))
                || (line.starts_with('-') && !line.starts_with("---"))
            {
                if line.starts_with('+') {
                    additions += 1;
                } else {
                    deletions += 1;
                }
                normalized.extend(line.chars().filter(|c| !c.is_whitespace()));
                normalized.push('\n');
            } else if line.starts_with("old mode")
                || line.starts_with("new mode")
                || line.starts_with("new file mode")
                || line.starts_with("deleted file mode")
            {
                normalized.push_str(line);
                normalized.push('\n');
            }
        }
        symbols.sort();
        symbols.dedup();
        let fingerprint = if normalized.is_empty() || binary {
            None
        } else {
            Some(hash(format!("{path}\n{normalized}")))
        };
        files.push(FileChange {
            is_test: is_test(&path),
            is_documentation: is_doc(&path),
            is_routine: is_routine(&path),
            path,
            additions,
            deletions,
            symbols,
            patch_fingerprint: fingerprint,
            binary,
        });
    }
    files
}

#[cfg(test)]
mod fetch_tests {
    use super::*;

    #[test]
    fn concurrent_fetches_share_objects_without_ref_or_fetch_head_contention() {
        let fixture = tempfile::tempdir().unwrap();
        let source = Git::new(fixture.path().join("source"));
        std::fs::create_dir_all(&source.path).unwrap();
        source.run(&["init", "-q", "-b", "main"]).unwrap();
        source.run(&["config", "user.name", "Fixture"]).unwrap();
        source
            .run(&["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        source
            .run(&["commit", "--allow-empty", "-qm", "base"])
            .unwrap();
        let base = source.resolve("HEAD").unwrap();
        let mut tips = Vec::new();
        for i in 0..8 {
            source.run(&["checkout", "--detach", &base]).unwrap();
            std::fs::write(source.path.join("change"), format!("feature {i}")).unwrap();
            source.run(&["add", "change"]).unwrap();
            source
                .run(&["commit", "-qm", &format!("feature {i}")])
                .unwrap();
            tips.push(source.resolve("HEAD").unwrap());
        }
        let cache = Git::init_bare(&fixture.path().join("cache")).unwrap();
        let url = format!("file://{}", source.path.display());
        crate::parallel::map(&tips, 4, |i, tip| {
            let reference = format!("refs/forkpicker/{i}/0");
            cache
                .fetch_url(&url, &[format!("+{tip}:{reference}")])
                .unwrap();
            assert_eq!(cache.resolve(&reference).unwrap(), *tip);
        });
        assert!(!cache.path.join("FETCH_HEAD").exists());
        cache.run(&["fsck", "--full", "--no-dangling"]).unwrap();
        assert_eq!(cache.refs("refs/forkpicker/").unwrap().len(), 8);
    }
}
