pub mod analyze;
pub mod classification_html;
pub mod classify;
pub mod discovery;
pub mod discovery_application;
pub mod discovery_html;
pub mod discovery_window;
pub mod forks;
pub mod git;
pub mod github;
pub mod inbox;
pub mod inline_issues;
pub mod integration;
pub mod issue_context;
pub mod issue_review;
pub mod llm;
pub mod metrics;
pub mod model;
pub mod model_defaults;
pub mod parallel;
pub mod priority;
pub mod related;
pub mod render;
pub mod review;
pub mod scan;
pub mod shortlist;
pub mod state;
pub mod structured;
pub mod triage;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

pub fn hash(input: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(input.as_ref()))
}

/// Count compact JSON bytes without allocating a serialized copy of the value.
pub fn json_size(value: &impl Serialize) -> serde_json::Result<usize> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_size_matches_serialized_bytes_and_preserves_errors() {
        for value in [
            json!(null),
            json!({"message": "\"quoted\"\\path\n\t\u{0001} 🦀", "nested": [true, {}, []]}),
            json!([u64::MAX, i64::MIN, 1.25, "<script>&".repeat(1024)]),
        ] {
            assert_eq!(
                json_size(&value).unwrap(),
                serde_json::to_vec(&value).unwrap().len()
            );
        }
        let invalid_keys = std::collections::BTreeMap::from([((1, 2), "value")]);
        assert!(json_size(&invalid_keys).is_err());
    }
}
