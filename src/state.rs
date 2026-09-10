use crate::{hash, model::*, write_json};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub status: String,
    pub reason: String,
    pub patch_ids: Vec<String>,
    pub recorded_at: String,
}

fn location(root: &Path, repository: &str) -> PathBuf {
    root.join(format!("{}.json", hash(repository.to_lowercase())))
}

fn read(path: &Path) -> Result<BTreeMap<String, Decision>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("invalid decision store; refusing to replace it")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn apply(report: &mut Report, root: &Path) -> Result<()> {
    let decisions = read(&location(root, &report.repository))?;
    for feature in &mut report.features {
        if let Some(decision) = decisions.get(&feature.id) {
            feature.status = decision.status.clone();
            feature.decision_reason = Some(decision.reason.clone());
        } else if let Some((_, prior)) = decisions
            .iter()
            .find(|(_, d)| d.patch_ids.iter().any(|p| feature.patch_ids.contains(p)))
        {
            feature.status = "updated".into();
            feature.decision_reason = Some(format!(
                "Overlaps previously {} work, but the patch set changed. Previous reason: {}",
                prior.status, prior.reason
            ));
        }
    }
    Ok(())
}

pub fn decide(
    report: &Report,
    feature_id: &str,
    status: &str,
    reason: &str,
    root: &Path,
) -> Result<()> {
    anyhow::ensure!(
        ["new", "saved", "dismissed", "needs_adopter", "adopted"].contains(&status),
        "invalid decision status"
    );
    anyhow::ensure!(
        !reason.trim().is_empty(),
        "record a reason so future scans preserve the decision's context"
    );
    let feature = report
        .features
        .iter()
        .find(|f| f.id == feature_id)
        .context("feature not found")?;
    std::fs::create_dir_all(root)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("decisions.lock"))?;
    lock.lock_exclusive()?;
    let path = location(root, &report.repository);
    let mut decisions = read(&path)?;
    decisions.insert(
        feature_id.into(),
        Decision {
            status: status.into(),
            reason: reason.into(),
            patch_ids: feature.patch_ids.clone(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    write_json(&path, &decisions)?;
    Ok(())
}
