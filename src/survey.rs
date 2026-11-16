//! `survey`: `why` on the latest run's slowest tests, one at a time, as one
//! job — the table an optimizing session starts from, without a shell loop.
//! Each row is kept in the store (`surveys.jsonl`), so `status` and the
//! ledger's next candidates can say each test's kind.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::host::now_rfc3339;
use crate::jobs::Job;
use crate::nextest::Progress;
use crate::record::round;
use crate::store::{Store, write_json};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub at: String,
    pub run: String,
    pub rank: usize,
    pub test: String,
    pub location: Option<String>,
    /// Its time in the recorded run, and the wall time removing it saves.
    pub recorded_s: f64,
    pub replayed_wall_saving_s: f64,
    pub est_wall_saving_s: f64,
    /// What `why` found, run alone.
    pub alone_s: Option<f64>,
    pub kind: Option<String>,
    pub kinds: Vec<String>,
    pub fixed_waits_s: Option<f64>,
    pub spawned: Option<u64>,
    pub spawned_at_most: Option<u64>,
    pub top_programs: Vec<String>,
    pub cited: Vec<String>,
    pub polls: Vec<String>,
    pub load_per_core: Option<f64>,
    pub error: Option<String>,
}

pub fn survey(
    ws: &Workspace,
    store: &Store,
    top: usize,
    profile: &str,
    job: &Job,
) -> Result<Value> {
    let ranked = crate::record::rank(ws, store, top, None)?;
    let run = ranked["run"]["id"].as_str().unwrap_or_default().to_string();
    let tests = ranked["tests"].as_array().cloned().unwrap_or_default();
    let mut rows = vec![];
    let started = std::time::Instant::now();
    for (i, t) in tests.iter().enumerate() {
        let test = format!(
            "{} {}",
            t["binary_id"].as_str().unwrap_or(""),
            t["name"].as_str().unwrap_or("")
        );
        let progress = Progress {
            done: i,
            total: tests.len(),
            failed: rows.iter().filter(|r: &&Row| r.error.is_some()).count(),
            elapsed_s: started.elapsed().as_secs_f64(),
            last: format!("why {test}"),
            finished: false,
        };
        let _ = write_json(&job.progress_path(), &progress);
        job.note(&format!("survey {}/{}: {test}", i + 1, tests.len()));
        let w = crate::probe::why(ws, store, &test, profile, 20, 3);
        let mut row = Row {
            at: now_rfc3339(),
            run: run.clone(),
            rank: i + 1,
            test: test.clone(),
            location: t["at"].as_str().map(str::to_string),
            recorded_s: t["seconds"].as_f64().unwrap_or(0.0),
            replayed_wall_saving_s: t["replayed_wall_saving_s"].as_f64().unwrap_or(0.0),
            est_wall_saving_s: t["est_wall_saving_s"].as_f64().unwrap_or(0.0),
            alone_s: None,
            kind: None,
            kinds: vec![],
            fixed_waits_s: None,
            spawned: None,
            spawned_at_most: None,
            top_programs: vec![],
            cited: vec![],
            polls: vec![],
            load_per_core: None,
            error: None,
        };
        match w {
            Ok(w) => {
                row.alone_s = w["seconds"].as_f64();
                row.kind = w["kind"].as_str().map(str::to_string);
                row.kinds = w["kinds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|k| k["kind"].as_str().map(str::to_string))
                    .collect();
                row.fixed_waits_s = w["breakdown"]["fixed_waits_s"].as_f64();
                row.spawned = w["processes"]["spawned"].as_u64();
                row.spawned_at_most = w["processes"]["spawned_at_most"].as_u64();
                row.top_programs = w["processes"]["by_program"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(4)
                    .map(|g| format!("{} {}", g["program"].as_str().unwrap_or(""), g["count"]))
                    .collect();
                row.cited = w["kinds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|k| k["cited"].as_array().cloned().unwrap_or_default())
                    .map(|c| {
                        format!(
                            "{} {}s: {}",
                            c["at"].as_str().unwrap_or(""),
                            c["seconds"],
                            c["code"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                row.polls = w["static"]["polls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect();
                let cores = w["cores"].as_f64().unwrap_or(1.0);
                row.load_per_core = w["load_end"].as_f64().map(|l| round(l / cores));
            }
            Err(e) => row.error = Some(format!("{e:#}")),
        }
        store.append("surveys.jsonl", &row)?;
        rows.push(row);
    }
    let _ = write_json(
        &job.progress_path(),
        &Progress {
            done: rows.len(),
            total: rows.len(),
            failed: rows.iter().filter(|r| r.error.is_some()).count(),
            elapsed_s: started.elapsed().as_secs_f64(),
            last: "survey done".into(),
            finished: true,
        },
    );
    let mut by_kind: std::collections::BTreeMap<String, (usize, f64)> = Default::default();
    for r in &rows {
        let e = by_kind
            .entry(r.kind.clone().unwrap_or_else(|| "error".into()))
            .or_default();
        e.0 += 1;
        e.1 += r.replayed_wall_saving_s;
    }
    Ok(json!({
        "run": run,
        "tests": rows.len(),
        "seconds": round(started.elapsed().as_secs_f64()),
        "by_kind": by_kind.iter().map(|(k, (n, s))| json!({"kind": k, "tests": n, "replayed_wall_saving_s": round(*s)})).collect::<Vec<_>>(),
        "rows": rows,
    }))
}

/// The newest survey row of each test.
pub fn latest(store: &Store) -> Vec<Row> {
    let rows: Vec<Row> = store.read("surveys.jsonl").unwrap_or_default();
    let mut newest: std::collections::HashMap<String, Row> = Default::default();
    for r in rows {
        newest.insert(r.test.clone(), r);
    }
    let mut v: Vec<Row> = newest.into_values().collect();
    v.sort_by(|a, b| {
        b.replayed_wall_saving_s
            .total_cmp(&a.replayed_wall_saving_s)
            .then(b.recorded_s.total_cmp(&a.recorded_s))
    });
    v
}
