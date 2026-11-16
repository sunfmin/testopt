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
    let threads = a.threads.or_else(|| {
        store
            .read::<Run>("runs.jsonl")
            .ok()?
            .last()
            .map(|r| r.threads)
    });
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

/// Totals over each test's newest entry: a later entry (a kept change
/// undone, a rejection reconsidered) replaces what came before it.
pub fn totals(all: &[Entry]) -> Value {
    let mut newest: Vec<&Entry> = vec![];
    for e in all.iter().rev() {
        if !newest.iter().any(|n| n.test == e.test) {
            newest.push(e);
        }
    }
    let entries = &newest;
    let kept: Vec<&&Entry> = entries.iter().filter(|e| e.status == "kept").collect();
    let saved: f64 = kept.iter().filter_map(|e| e.saved_s()).sum();
    let wall: f64 = kept
        .iter()
        .filter_map(|e| Some(e.saved_s()? / e.threads? as f64))
        .sum();
    json!({
        "entries": all.len(),
        "tests": entries.len(),
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
        "candidates": upcoming,
    }))
}

/// One entry per test a `check` job touched: its numbers are the check's
/// timings (base median before; working-tree median after, 0 for a deleted
/// test), its reason the check's verdict. `job` is a job id or `latest`.
pub fn add_from_check(
    ws: &Workspace,
    store: &Store,
    job: &str,
    kind: &str,
    status: &str,
    change: &str,
    note: Option<&str>,
) -> Result<Value> {
    let dir = if job == "latest" {
        let mut checks: Vec<std::path::PathBuf> = std::fs::read_dir(store.jobs_dir())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains("-check-"))
            })
            .collect();
        checks.sort();
        checks
            .pop()
            .ok_or_else(|| anyhow::anyhow!("no check job yet"))?
    } else {
        store.jobs_dir().join(job)
    };
    let result: Value = serde_json::from_slice(&std::fs::read(dir.join("result.json"))?)?;
    if result["ok"] != true {
        bail!(
            "check job {} did not finish: {}",
            dir.display(),
            result["error"]
        );
    }
    let r = &result["result"];
    let verdict = format!(
        "check {} vs {}: {} ({} lost lines{})",
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        r["base"]
            .as_str()
            .map(|b| &b[..b.len().min(9)])
            .unwrap_or("?"),
        if r["pass"] == true { "pass" } else { "FAIL" },
        r["coverage"]["lost_lines"],
        r["coverage"]["lost"]
            .as_array()
            .filter(|l| !l.is_empty())
            .map(|l| format!(
                ": {}",
                l.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .unwrap_or_default()
    );
    let mut added = vec![];
    for t in r["tests"]["timing"].as_array().into_iter().flatten() {
        let median = |side: &str| t[side]["median_s"].as_f64();
        let after = match t["change"].as_str() {
            Some("deleted") => Some(0.0),
            _ => median("head"),
        };
        let unreliable = [&t["base"], &t["head"]]
            .iter()
            .any(|s| s["reliable"] == false);
        let mut reason = if unreliable {
            format!("{verdict}; timed on a loaded host (flagged)")
        } else {
            verdict.clone()
        };
        if let Some(n) = note {
            reason = format!("{n}; {reason}");
        }
        added.push(add(
            ws,
            store,
            Add {
                test: t["test"].as_str().unwrap_or_default().to_string(),
                kind: kind.to_string(),
                status: status.to_string(),
                change: if change.is_empty() {
                    t["change"].as_str().unwrap_or_default().to_string()
                } else {
                    change.to_string()
                },
                before_s: median("base"),
                after_s: after,
                threads: None,
                reason,
            },
        )?);
    }
    if added.is_empty() {
        bail!("check job {} touched no test", dir.display());
    }
    Ok(json!({"from": dir, "added": added}))
}

/// Before and after from the newest two `time` records of `test`: the
/// older is before, the newer after; a note when either was flagged.
pub fn from_times(store: &Store, test: &str) -> Result<(f64, f64, Option<String>)> {
    let times: Vec<crate::probe::Timing> = store.read("times.jsonl")?;
    let mine: Vec<&crate::probe::Timing> = times.iter().filter(|t| t.test == test).collect();
    let [.., before, after] = mine.as_slice() else {
        bail!(
            "`testopt time` has {} record(s) of {test}; --from-times needs two (before, then after)",
            mine.len()
        );
    };
    let note = (!before.reliable || !after.reliable).then(|| {
        format!(
            "timed on a loaded host (load per core {} / {}), flagged",
            before.max_load_per_core, after.max_load_per_core
        )
    });
    Ok((before.median_s, after.median_s, note))
}
