//! The store: one directory per target workspace, outside it, holding
//! append-only JSON-lines files. It is the only home of runs, timings,
//! coverage and the ledger; a new session resumes from it.
//!
//! Layout of `<home>/<id>/`:
//! - `runs.jsonl`    one line per `record`
//! - `times.jsonl`   one line per `time`
//! - `ledger.jsonl`  one line per candidate tried
//! - `coverage/<commit>/<test-key>.json`  covered lines of one test at one tree
//! - `jobs/<id>/`    a running or finished job: `progress.json`, `log`, `result.json`
//! - `src/<commit>/` a clean copy of a commit's tree, for `check`

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    /// The store of `ws`: `$TESTOPT_HOME` (default
    /// `~/.local/share/testopt`) / the workspace's identity. Worktrees and
    /// copies of one repository share it, so the ledger follows the repo,
    /// not one checkout.
    pub fn of(ws: &Workspace) -> Result<Store> {
        let home = match std::env::var_os("TESTOPT_HOME") {
            // Absolute: tests and tools run from other directories.
            Some(h) => std::path::absolute(PathBuf::from(h))?,
            None => {
                let home = std::env::var_os("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(".local/share/testopt")
            }
        };
        let dir = home.join(&ws.id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let about = dir.join("workspace.txt");
        if !about.exists() {
            std::fs::write(&about, format!("{}\n", ws.root.display()))?;
        }
        Ok(Store { dir })
    }

    pub fn append<T: Serialize>(&self, file: &str, value: &T) -> Result<()> {
        let path = self.dir.join(file);
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }

    pub fn read<T: DeserializeOwned>(&self, file: &str) -> Result<Vec<T>> {
        read_jsonl(&self.dir.join(file))
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.dir.join("jobs")
    }
}

pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).with_context(|| format!("{}:{}", path.display(), i + 1))
        })
        .collect()
}

/// Writes `value` to `path` atomically (write a sibling, rename).
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// A file-name-safe key for a test.
pub fn test_key(binary_id: &str, name: &str) -> String {
    let raw = format!("{binary_id}__{name}");
    let mut safe: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(150)
        .collect();
    safe.push_str(&format!("-{:08x}", crate::workspace::fnv(raw.as_bytes()) as u32));
    safe
}
