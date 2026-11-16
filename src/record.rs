//! `record` and `rank`: one run of the suite kept with where and when it ran;
//! the latest run ranked by what each test costs the wall clock.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crate::host::{Load, hostname, now_rfc3339};
use crate::jobs::Job;
use crate::nextest::{self, Spec, TestTime};
use crate::source::Index;
use crate::store::Store;
use crate::workspace::{Workspace, commit};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub started: String,
    pub ended: String,
    pub commit: Option<String>,
    pub dirty: bool,
    pub host: String,
    pub cores: usize,
    pub load_start: f64,
    pub load_end: f64,
    pub threads: usize,
    pub profile: String,
    /// What selected the tests: `-E <expr>`, `--filter-file <path>`, or all.
    pub selection: String,
    pub wall_s: f64,
    pub success: bool,
    /// nextest's own summary line, for checking the totals against.
    pub nextest_summary: Option<String>,
    pub tests: Vec<TestTime>,
}

impl Run {
    pub fn test_seconds(&self) -> f64 {
        self.tests
            .iter()
            .filter(|t| t.status != "skip")
            .map(|t| t.seconds)
            .sum()
    }
}

pub struct Selection {
    pub filter_file: Option<PathBuf>,
    pub expr: Option<String>,
}

impl Selection {
    pub fn filter(&self) -> Result<Option<String>> {
        match (&self.filter_file, &self.expr) {
            (Some(_), Some(_)) => bail!("give --filter-file or -E, not both"),
            (Some(f), None) => Ok(Some(nextest::filter_from_file(f)?)),
            (None, Some(e)) => Ok(Some(e.clone())),
            (None, None) => Ok(None),
        }
    }

    pub fn describe(&self) -> String {
        match (&self.filter_file, &self.expr) {
            (Some(f), _) => format!("--filter-file {}", f.display()),
            (_, Some(e)) if e.len() > 200 => format!("-E {}…", &e[..200]),
            (_, Some(e)) => format!("-E {e}"),
            _ => "all tests".into(),
        }
    }
}

pub fn record(
    ws: &Workspace,
    store: &Store,
    sel: &Selection,
    profile: &str,
    threads: Option<usize>,
    extra: Vec<String>,
    job: &Job,
) -> Result<Value> {
    let filter = sel.filter()?;
    let threads = nextest::threads(ws, profile, threads);
    let (commit, dirty) = commit(&ws.root);
    let load_start = Load::now();
    let started = now_rfc3339();
    job.note(&format!(
        "record: {} in {}, {} at a time, load {:.1}/{} cores",
        sel.describe(),
        ws.root.display(),
        threads,
        load_start.load1,
        load_start.cores
    ));
    // Build first: the run's wall time is the tests', not the compiler's.
    job.note("record: building the tests");
    let listed = nextest::list(ws, filter.as_deref(), &[])?;
    job.note(&format!("record: {} tests selected; running", listed.len()));
    let spec = Spec {
        profile: profile.to_string(),
        filter,
        threads: Some(threads),
        extra,
        env: vec![],
        log: Some(job.log_path()),
        progress: Some(job.progress_path()),
    };
    let out = nextest::run(ws, &spec)?;
    let load_end = Load::now();
    let run = Run {
        id: job.id.clone(),
        started,
        ended: now_rfc3339(),
        commit,
        dirty,
        host: hostname(),
        cores: load_start.cores,
        load_start: load_start.load1,
        load_end: load_end.load1,
        threads,
        profile: profile.to_string(),
        selection: sel.describe(),
        wall_s: out.wall_s,
        success: out.success,
        nextest_summary: out.summary,
        tests: out.tests,
    };
    store.append("runs.jsonl", &run)?;
    let failed: Vec<String> = run
        .tests
        .iter()
        .filter(|t| t.status == "fail")
        .map(|t| format!("{} {}", t.binary_id, t.name))
        .collect();
    Ok(json!({
        "run": run.id,
        "store": store.dir.join("runs.jsonl"),
        "tests": run.tests.len(),
        "failed": failed,
        "test_seconds": round(run.test_seconds()),
        "wall_s": round(run.wall_s),
        "threads": run.threads,
        "load_start": run.load_start,
        "load_end": run.load_end,
        "nextest_summary": run.nextest_summary,
    }))
}

pub fn round(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0 + 0.0
}

/// Maps tests to the file and line of their function.
pub struct Locator<'a> {
    ws: &'a Workspace,
    by_binary: HashMap<String, Index>,
}

impl<'a> Locator<'a> {
    pub fn new(ws: &'a Workspace) -> Self {
        Locator {
            ws,
            by_binary: HashMap::new(),
        }
    }

    pub fn locate(&mut self, t: &nextest::TestId) -> Option<(String, usize)> {
        let ws = self.ws;
        let sources = self
            .by_binary
            .entry(t.binary_id.clone())
            .or_insert_with(|| Index::of_binary(ws, &t.binary_id));
        let (path, line, _generated) = sources.locate(t)?;
        Some((ws.relative(path).display().to_string(), line))
    }
}

pub fn rank(ws: &Workspace, store: &Store, top: usize, run_id: Option<&str>) -> Result<Value> {
    let runs: Vec<Run> = store.read("runs.jsonl")?;
    let Some(run) = (match run_id {
        Some(id) => runs.iter().find(|r| r.id == id),
        None => runs.last(),
    }) else {
        bail!("no run recorded for this workspace yet; run `testopt record` first");
    };
    let total = run.test_seconds();
    let threads = run.threads.max(1) as f64;
    // Runs are appended in order: each test's time in the newest run before this one.
    let position = runs.iter().position(|r| r.id == run.id).unwrap_or(0);
    let previous: HashMap<(String, String), f64> = runs[..position]
        .iter()
        .flat_map(|r| r.tests.iter())
        .filter(|t| t.status != "skip")
        .map(|t| ((t.binary_id.clone(), t.name.clone()), t.seconds))
        .collect();
    let mut tests: Vec<&TestTime> = run.tests.iter().filter(|t| t.status != "skip").collect();
    // The run's schedule, replayed: tests in the order they started (JUnit
    // timestamps; the file's order where a run has none).
    let mut order: Vec<&TestTime> = tests.clone();
    order.sort_by(|a, b| a.start.unwrap_or(0.0).total_cmp(&b.start.unwrap_or(0.0)));
    let has_starts = order.iter().all(|t| t.start.is_some());
    let durations: Vec<f64> = order.iter().map(|t| t.seconds).collect();
    let replayed_wall = crate::schedule::wall(&durations, run.threads);
    let position: HashMap<(&str, &str), usize> = order
        .iter()
        .enumerate()
        .map(|(i, t)| ((t.binary_id.as_str(), t.name.as_str()), i))
        .collect();
    tests.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
    let mut loc = Locator::new(ws);
    let top_idx: Vec<usize> = tests
        .iter()
        .take(top)
        .map(|t| position[&(t.binary_id.as_str(), t.name.as_str())])
        .collect();
    let replayed_savings = crate::schedule::savings(&durations, run.threads, &top_idx);

    let mut bands = vec![];
    for (label, lo, hi) in [
        ("≥20s", 20.0, f64::INFINITY),
        ("10–20s", 10.0, 20.0),
        ("2–10s", 2.0, 10.0),
        ("<2s", 0.0, 2.0),
    ] {
        let inb: Vec<&&TestTime> = tests
            .iter()
            .filter(|t| t.seconds >= lo && t.seconds < hi)
            .collect();
        let secs: f64 = inb.iter().map(|t| t.seconds).sum();
        bands.push(json!({"band": label, "tests": inb.len(), "seconds": round(secs), "share": round(secs / total.max(1e-9))}));
    }

    let mut files: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut ranked = vec![];
    for (i, t) in tests.iter().enumerate() {
        let at = loc.locate(&t.id());
        let file = at
            .as_ref()
            .map(|a| a.0.clone())
            .unwrap_or_else(|| format!("({})", t.binary_id));
        let e = files.entry(file).or_default();
        e.0 += 1;
        e.1 += t.seconds;
        if i < top {
            let prev = previous.get(&(t.binary_id.clone(), t.name.clone()));
            ranked.push(json!({
                "rank": i + 1,
                "binary_id": t.binary_id,
                "name": t.name,
                "seconds": round(t.seconds),
                "share": round(t.seconds / total.max(1e-9)),
                "est_wall_saving_s": round(t.seconds / threads),
                "replayed_wall_saving_s": round(replayed_savings[i]),
                "at": at.map(|(f, l)| format!("{f}:{l}")),
                "status": t.status,
                "previous_s": prev.map(|p| round(*p)),
                "delta_s": prev.map(|p| round(t.seconds - p)),
            }));
        }
    }
    let mut by_file: Vec<Value> = files
        .into_iter()
        .map(|(f, (n, s))| {
            json!({"file": f, "tests": n, "seconds": round(s), "share": round(s / total.max(1e-9)), "est_wall_saving_s": round(s / threads)})
        })
        .collect();
    by_file.sort_by(|a, b| {
        b["seconds"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["seconds"].as_f64().unwrap_or(0.0))
    });

    let trend: Vec<Value> = runs
        .iter()
        .map(|r| {
            json!({"run": r.id, "started": r.started, "commit": r.commit.as_deref().map(|c| &c[..c.len().min(9)]),
                   "selection": r.selection, "tests": r.tests.len(), "test_seconds": round(r.test_seconds()),
                   "wall_s": round(r.wall_s), "threads": r.threads, "load_start": r.load_start})
        })
        .collect();

    Ok(json!({
        "run": {
            "id": run.id, "started": run.started, "commit": run.commit, "dirty": run.dirty, "host": run.host,
            "cores": run.cores, "threads": run.threads, "load_start": run.load_start, "load_end": run.load_end,
            "selection": run.selection, "nextest_summary": run.nextest_summary,
        },
        "totals": {
            "tests": tests.len(),
            "failed": tests.iter().filter(|t| t.status == "fail").count(),
            "test_seconds": round(total),
            "wall_s": round(run.wall_s),
            "threads": run.threads,
            "test_seconds_over_threads": round(total / threads),
            "replayed_wall_s": round(replayed_wall),
            "replayed_from": if has_starts { "JUnit start order" } else { "file order (no start times recorded)" },
        },
        "bands": bands,
        "tests": ranked,
        "files": by_file.into_iter().take(top).collect::<Vec<_>>(),
        "trend": if runs.len() > 1 { json!(trend) } else { json!([]) },
    }))
}
