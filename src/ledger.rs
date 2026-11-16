//! The ledger: every candidate tried on a target, kept or rejected, with
//! its numbers and reason. The only home of optimization state: a new
//! session reads it (`testopt ledger`) to see what was done and what is next.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::host::now_rfc3339;
use crate::record::{Run, round};
use crate::store::Store;
use crate::workspace::{Workspace, commit};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub at: String,
    pub commit: Option<String>,
    /// `<binary-id> <name>`, or a file, or a short name for a change.
    pub test: String,
    /// fixed-wait | poll | process-heavy | redundant | setup | other
    pub kind: String,
    /// tried | kept | rejected
    pub status: String,
    pub change: String,
    pub before_s: Option<f64>,
    pub after_s: Option<f64>,
    /// Threads the suite runs at: a test second saved is 1/threads of wall.
    pub threads: Option<usize>,
    pub reason: String,
}

impl Entry {
    pub fn saved_s(&self) -> Option<f64> {
        Some(self.before_s? - self.after_s?)
    }
}

pub struct Add {
    pub test: String,
    pub kind: String,
    pub status: String,
    pub change: String,
    pub before_s: Option<f64>,
    pub after_s: Option<f64>,
    pub threads: Option<usize>,
    pub reason: String,
}

pub fn add(ws: &Workspace, store: &Store, a: Add) -> Result<Value> {
    if !["tried", "kept", "rejected"].contains(&a.status.as_str()) {
        bail!("--status is tried, kept or rejected, not {}", a.status);
    }
    let threads = a.threads.or_else(|| store.read::<Run>("runs.jsonl").ok()?.last().map(|r| r.threads));
    let e = Entry {
        at: now_rfc3339(),
        commit: commit(&ws.root).0,
        test: a.test,
        kind: a.kind,
        status: a.status,
        change: a.change,
        before_s: a.before_s,
        after_s: a.after_s,
        threads,
        reason: a.reason,
    };
    store.append("ledger.jsonl", &e)?;
    Ok(json!({"added": e, "saved_s": e.saved_s().map(round)}))
}

pub fn totals(entries: &[Entry]) -> Value {
    let kept: Vec<&Entry> = entries.iter().filter(|e| e.status == "kept").collect();
    let saved: f64 = kept.iter().filter_map(|e| e.saved_s()).sum();
    let wall: f64 = kept.iter().filter_map(|e| Some(e.saved_s()? / e.threads? as f64)).sum();
    json!({
        "entries": entries.len(),
        "tried": entries.iter().filter(|e| e.status == "tried").count(),
        "kept": kept.len(),
        "rejected": entries.iter().filter(|e| e.status == "rejected").count(),
        "kept_test_seconds_saved": round(saved),
        "kept_est_wall_saving_s": round(wall),
    })
}

/// The ledger, its totals, and the next candidates: the latest run's
/// slowest tests the ledger does not name yet.
pub fn show(store: &Store, next: usize) -> Result<Value> {
    let entries: Vec<Entry> = store.read("ledger.jsonl")?;
    let named: HashSet<&str> = entries.iter().map(|e| e.test.as_str()).collect();
    let runs: Vec<Run> = store.read("runs.jsonl")?;
    let mut upcoming = vec![];
    if let Some(run) = runs.last() {
        let mut tests: Vec<_> = run.tests.iter().filter(|t| t.status != "skip").collect();
        tests.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
        for t in tests {
            let id = format!("{} {}", t.binary_id, t.name);
            if named.contains(id.as_str()) {
                continue;
            }
            upcoming.push(json!({"test": id, "seconds": round(t.seconds), "est_wall_saving_s": round(t.seconds / run.threads.max(1) as f64)}));
            if upcoming.len() >= next {
                break;
            }
        }
    }
    Ok(json!({
        "ledger": store.dir.join("ledger.jsonl"),
        "totals": totals(&entries),
        "entries": entries.iter().map(|e| {
            let mut v = serde_json::to_value(e).unwrap_or_default();
            v["saved_s"] = json!(e.saved_s().map(round));
            v
        }).collect::<Vec<_>>(),
        "next": upcoming,
    }))
}
