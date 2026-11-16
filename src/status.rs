//! `status`: where an optimization of this target stands, in one answer —
//! the first command of a new session. The latest run and whether it is of
//! this tree, any job running, the survey, the ledger, the environment,
//! the tools, and the command to run next.

use serde_json::{Value, json};
use std::process::Command;

use crate::record::{Run, round};
use crate::store::Store;
use crate::workspace::{Workspace, commit};

fn has(tool: &[&str]) -> bool {
    Command::new(tool[0])
        .args(&tool[1..])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn age_minutes(rfc3339: &str) -> Option<f64> {
    let t: jiff::Timestamp = rfc3339.parse().ok()?;
    Some(round(
        (jiff::Timestamp::now().as_second() - t.as_second()) as f64 / 60.0,
    ))
}

pub fn status(ws: &Workspace, store: &Store) -> Value {
    let (head, dirty) = commit(&ws.root);
    let runs: Vec<Run> = store.read("runs.jsonl").unwrap_or_default();
    let latest = runs
        .iter()
        .rev()
        .find(|r| r.tests.len() > 1)
        .or(runs.last());
    let run = latest.map(|r| {
        json!({
            "id": r.id,
            "age_min": age_minutes(&r.ended),
            "commit": r.commit,
            "of_this_tree": r.commit == head && !r.dirty && !dirty,
            "selection": r.selection,
            "tests": r.tests.len(),
            "test_seconds": round(r.test_seconds()),
            "wall_s": round(r.wall_s),
            "threads": r.threads,
            "start_order_recorded": r.tests.iter().all(|t| t.start.is_some()),
        })
    });
    // Jobs without a result: running, or ended without one (killed).
    let mut jobs = vec![];
    if let Ok(entries) = std::fs::read_dir(store.jobs_dir()) {
        for e in entries.flatten() {
            let dir = e.path();
            if dir.join("result.json").exists() {
                continue;
            }
            let fresh = std::fs::metadata(dir.join("log"))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|d| d.as_secs() < 120);
            let progress: Value = std::fs::read(dir.join("progress.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            let exit = std::fs::read_to_string(dir.join("exit")).ok();
            let state = match (exit, fresh) {
                (Some(code), _) => format!("ended without a result (exit {})", code.trim()),
                (None, Some(true)) => "running".to_string(),
                (None, _) => "silent for 2+ minutes (killed?)".to_string(),
            };
            jobs.push(json!({
                "job": e.file_name().to_string_lossy(),
                "state": state,
                "progress": progress,
            }));
        }
    }
    let survey = crate::survey::latest(store);
    let ledger = crate::ledger::show(store, 5).unwrap_or(Value::Null);
    let env = crate::env::load(store);

    let mut next: Vec<String> = vec![];
    if let Some(j) = jobs.iter().find(|j| j["state"] == "running") {
        next.push(format!("testopt wait {}", j["job"].as_str().unwrap_or("")));
    }
    let run_ok = latest.is_some_and(|r| r.commit == head && r.tests.len() > 1);
    if !run_ok {
        if env.is_empty() && ws.root.join("gate.sh").exists() {
            next.push(
                "record the gate's environment first: testopt env set NAME=value … (see ./gate.sh)"
                    .into(),
            );
        }
        next.push("testopt --tmux record".into());
        for p in nextest_profiles(&ws.root) {
            next.push(format!("testopt --tmux record --profile {p}"));
        }
    } else if survey.is_empty() {
        next.push("testopt --tmux survey --top 20".into());
    }
    if let Some(step) = ledger["candidates"][0]["next"].as_str() {
        next.push(step.to_string());
    }
    let summary = format!(
        "{}; {}; {} survey row(s); ledger {} tests ({} kept)",
        match latest {
            Some(r) => format!(
                "latest run {} tests, {:.0}s wall, {}",
                r.tests.len(),
                r.wall_s,
                if run_ok {
                    "of this commit"
                } else {
                    "of another commit or tree: stale"
                }
            ),
            None => "no run yet".into(),
        },
        if jobs.is_empty() {
            "no job running".to_string()
        } else {
            format!("{} job(s) without a result", jobs.len())
        },
        survey.len(),
        ledger["totals"]["tests"],
        ledger["totals"]["kept"],
    );
    json!({
        "summary": summary,
        "next": next,
        "workspace": ws.root,
        "store": store.dir,
        "head": head,
        "dirty": dirty,
        "env": env,
        "latest_run": run,
        "jobs": jobs,
        "survey": survey.iter().take(10).map(|r| json!({
            "test": r.test, "kind": r.kind, "recorded_s": r.recorded_s, "replayed_wall_saving_s": r.replayed_wall_saving_s,
            "fixed_waits_s": r.fixed_waits_s, "spawned": r.spawned,
        })).collect::<Vec<_>>(),
        "ledger": {"totals": ledger["totals"], "candidates": ledger["candidates"]},
        "tools": {
            "cargo-nextest": has(&["cargo", "nextest", "--version"]),
            "cargo-llvm-cov": has(&["cargo", "llvm-cov", "--version"]),
            "tmux": has(&["tmux", "-V"]),
        },
    })
}

/// The profiles the workspace's `.config/nextest.toml` names, but `default`.
fn nextest_profiles(root: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join(".config/nextest.toml")) else {
        return vec![];
    };
    let Ok(doc) = text.parse::<toml::Table>() else {
        return vec![];
    };
    doc.get("profile")
        .and_then(|p| p.as_table())
        .map(|t| {
            t.keys()
                .filter(|k| *k != "default" && !k.starts_with("default-"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}
