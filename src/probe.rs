//! `why` and `time`: one test run alone, measured.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

use crate::host::{Load, hostname, now_rfc3339};
use crate::nextest::{self, Spec, TestId};
use crate::procs::{self, Sampler, Seen};
use crate::record::{Run, round};
use crate::source;
use crate::store::Store;
use crate::workspace::{Workspace, commit};

/// Finds the one test `query` names: `<binary-id> <name>`, a whole name, or
/// the end of one at a `::` boundary. Looks in the latest recorded run
/// first, then asks nextest.
pub fn resolve(ws: &Workspace, store: &Store, query: &str) -> Result<TestId> {
    let query = query.trim();
    if let Some((b, n)) = query.split_once(' ') {
        return Ok(TestId {
            binary_id: b.to_string(),
            name: n.trim().to_string(),
        });
    }
    let matches = |name: &str| name == query || name.ends_with(&format!("::{query}"));
    let runs: Vec<Run> = store.read("runs.jsonl")?;
    if let Some(run) = runs.last() {
        let found: Vec<TestId> = run
            .tests
            .iter()
            .filter(|t| matches(&t.name))
            .map(|t| t.id())
            .collect();
        if found.len() == 1 {
            return Ok(found[0].clone());
        }
    }
    let listed = nextest::list(ws, None, &[])?;
    let found: Vec<TestId> = listed
        .into_iter()
        .map(|l| l.id)
        .filter(|id| matches(&id.name))
        .collect();
    match found.len() {
        1 => Ok(found[0].clone()),
        0 => bail!("no test named `{query}`"),
        _ => bail!(
            "`{query}` names {} tests; give `<binary-id> <name>`: {}",
            found.len(),
            found
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Builds what running `id` needs, so a timed run does not time a build.
fn build_for(ws: &Workspace, id: &TestId, env: &[(String, String)]) -> Result<()> {
    let listed = nextest::list(ws, Some(&id.filter()), env)?;
    if listed.is_empty() {
        bail!("nextest finds no test {id}");
    }
    Ok(())
}

fn alone(profile: &str, id: &TestId, env: &[(String, String)]) -> Spec {
    Spec {
        profile: profile.to_string(),
        filter: Some(id.filter()),
        threads: Some(1),
        env: env.to_vec(),
        ..Default::default()
    }
}

pub fn why(
    ws: &Workspace,
    store: &Store,
    query: &str,
    profile: &str,
    interval_ms: u64,
    depth: usize,
) -> Result<Value> {
    let id = resolve(ws, store, query)?;
    let scan = source::scan(ws, &id, depth);
    build_for(ws, &id, &[])?;
    let load_start = Load::now();
    let running = nextest::start(ws, &alone(profile, &id, &[]))?;
    let sampler = Sampler::start(running.pid(), Duration::from_millis(interval_ms));
    let out = running.wait()?;
    let (seen, census) = sampler.finish();
    let samples = census.samples;
    let load_end = Load::now();
    let Some(t) = out
        .tests
        .iter()
        .find(|t| t.name == id.name && t.binary_id == id.binary_id)
    else {
        bail!("nextest ran no test {id}");
    };
    let wall = t.seconds;

    // The test's own process: the one nextest started with `--exact <name>`.
    let test_proc = seen
        .iter()
        .filter(|p| p.args.iter().any(|a| a == "--exact") && p.args.contains(&id.name))
        .max_by(|a, b| a.duration().total_cmp(&b.duration()));
    let spawned: Vec<&Seen> = match test_proc {
        Some(tp) => descendants(&seen, tp.pid),
        None => vec![],
    };
    let in_children = procs::busy_seconds(&spawned);
    let children_cpu: f64 = spawned.iter().map(|p| p.cpu_s).sum();
    let test_cpu = test_proc.map(|p| p.cpu_s).unwrap_or(0.0);
    let by_program = procs::group_by(&spawned, Seen::program);
    let by_command = procs::group_by(&spawned, Seen::label);
    let mut longest: Vec<&&Seen> = spawned.iter().collect();
    longest.sort_by(|a, b| b.duration().total_cmp(&a.duration()));
    let t0 = test_proc.map(|p| p.start).unwrap_or(0.0);
    let first_spawn = spawned
        .iter()
        .map(|p| p.start)
        .fold(f64::INFINITY, f64::min);

    let all_waits = scan.as_ref().map(|s| s.waits.clone()).unwrap_or_default();
    let (fixed, cited) = fixed_wait(&all_waits, wall);
    let polls = scan.as_ref().map(|s| s.polls.len()).unwrap_or(0);
    let mut kinds = vec![];
    if fixed >= 0.25 * wall && fixed >= 0.5 {
        kinds.push(json!({"kind": "fixed-wait", "seconds": round(fixed), "cited": cited,
            "evidence": "from the source: sleeps outside loops no longer than the test, and limits it ran up to"}));
    }
    if spawned.len() >= 20 || (in_children >= 0.5 * wall && !spawned.is_empty()) {
        kinds.push(json!({"kind": "process-heavy", "seconds": round(in_children), "processes": spawned.len(),
            "evidence": "processes the test spawned, sampled; seconds with at least one alive"}));
    }
    if polls > 0 {
        kinds.push(json!({"kind": "poll", "helpers": polls,
            "evidence": "loops holding a sleep, reached from the test: each round waits"}));
    }
    if test_cpu >= 0.5 * wall {
        kinds.push(json!({"kind": "cpu", "seconds": round(test_cpu), "evidence": "the test process's own CPU time"}));
    }
    let setup = if first_spawn.is_finite() {
        (first_spawn - t0).max(0.0)
    } else {
        0.0
    };
    if kinds.is_empty() {
        kinds.push(json!({"kind": "setup-or-other", "seconds": round(wall)}));
    }
    // The primary kind: a fixed wait is the cheapest lever, so it leads when
    // it holds a quarter of the time; otherwise the larger measured share.
    let primary = kinds[0]["kind"].clone();

    let waits = scan.as_ref().map(|s| s.waits.clone()).unwrap_or_default();
    Ok(json!({
        "test": id.to_string(),
        "at": scan.as_ref().map(|s| s.test_at.clone()),
        "kind": primary,
        "kinds": kinds,
        "seconds": round(wall),
        "host": hostname(),
        "load_start": load_start.load1,
        "load_end": load_end.load1,
        "cores": load_start.cores,
        "breakdown": {
            "wall_s": round(wall),
            "fixed_waits_s": round(fixed),
            "with_child_processes_alive_s": round(in_children),
            "test_process_cpu_s": round(test_cpu),
            "children_cpu_s": round(children_cpu),
            "before_first_spawn_s": round(setup),
        },
        "processes": {
            "spawned": spawned.len(),
            // The test started at least what was seen, and at most that plus
            // every process the host started that no sample saw.
            "spawned_at_most": census.missed().map(|m| spawned.len() + m),
            "host_started_unseen": census.missed(),
            "peak_concurrent": procs::peak_concurrency(&spawned),
            "sample_interval_ms": interval_ms,
            "samples": samples,
            "note": "sampled: a process shorter than the interval can be missed; spawned is a floor and spawned_at_most a ceiling (pids are handed out in order, so the host's unseen starts bound the misses)",
            "by_program": by_program.iter().take(15).map(|g| json!({"program": g.command, "count": g.count, "seconds": round(g.seconds), "cpu_s": round(g.cpu_s)})).collect::<Vec<_>>(),
            "by_command": by_command.iter().take(15).map(|g| json!({"command": g.command, "count": g.count, "seconds": round(g.seconds)})).collect::<Vec<_>>(),
            "longest": longest.iter().take(8).map(|p| json!({"command": p.args.iter().take(6).cloned().collect::<Vec<_>>().join(" "), "seconds": round(p.duration()), "starts_at_s": round(p.start - t0), "pid": p.pid, "ppid": p.ppid})).collect::<Vec<_>>(),
        },
        "static": {
            "functions_read": scan.as_ref().map(|s| s.functions_read),
            "waits": waits.iter().filter(|w| w.seconds.is_some_and(|s| s >= 0.05) || w.in_loop).take(40).collect::<Vec<_>>(),
            "polls": scan.as_ref().map(|s| s.polls.clone()),
        },
    }))
}

fn descendants(seen: &[Seen], root: u32) -> Vec<&Seen> {
    let mut out: Vec<&Seen> = vec![];
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for s in seen.iter().filter(|s| s.ppid == p && s.pid != root) {
            if !out.iter().any(|o| o.pid == s.pid && o.start == s.start) {
                out.push(s);
                frontier.push(s.pid);
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timing {
    pub at: String,
    pub test: String,
    pub commit: Option<String>,
    pub dirty: bool,
    pub host: String,
    pub cores: usize,
    pub runs: Vec<f64>,
    pub loads: Vec<f64>,
    pub median_s: f64,
    pub min_s: f64,
    pub max_s: f64,
    pub spread_s: f64,
    pub max_load_per_core: f64,
    pub threshold: f64,
    pub reliable: bool,
    /// CPU seconds of the test's process and every process it started, per
    /// run: what the test costs the host, which load changes far less than
    /// its wall time.
    #[serde(default)]
    pub cpu_runs: Vec<f64>,
    #[serde(default)]
    pub cpu_median_s: f64,
}

/// Where a test is timed: a workspace, and the environment to run it with.
pub struct Side<'a> {
    pub ws: &'a Workspace,
    pub env: &'a [(String, String)],
}

/// One timed run: wall seconds, CPU seconds of its process tree, and the
/// load before and after.
struct Shot {
    seconds: f64,
    cpu_s: f64,
    loads: [f64; 2],
}

fn one_run(side: &Side, profile: &str, id: &TestId) -> Result<Shot> {
    let before = Load::now().load1;
    let running = nextest::start(side.ws, &alone(profile, id, side.env))?;
    let sampler = Sampler::start(running.pid(), Duration::from_millis(50));
    let out = running.wait()?;
    let (seen, _) = sampler.finish();
    let after = Load::now().load1;
    let Some(t) = out
        .tests
        .iter()
        .find(|t| t.name == id.name && t.binary_id == id.binary_id)
    else {
        bail!("nextest ran no test {id}");
    };
    if t.status == "fail" {
        bail!("{id} failed");
    }
    let cpu_s = seen
        .iter()
        .filter(|p| p.args.iter().any(|a| a == "--exact") && p.args.contains(&id.name))
        .map(|tp| {
            tp.cpu_s
                + descendants(&seen, tp.pid)
                    .iter()
                    .map(|p| p.cpu_s)
                    .sum::<f64>()
        })
        .fold(0.0, f64::max);
    eprintln!(
        "[testopt] {id} in {}: {:.3}s, {:.3} cpu-s (load {:.1})",
        side.ws.root.display(),
        t.seconds,
        cpu_s,
        after
    );
    Ok(Shot {
        seconds: t.seconds,
        cpu_s,
        loads: [before, after],
    })
}

fn summarize(side: &Side, id: &TestId, shots: &[Shot], threshold: f64) -> Timing {
    let runs: Vec<f64> = shots.iter().map(|s| s.seconds).collect();
    let cpu: Vec<f64> = shots.iter().map(|s| s.cpu_s).collect();
    let loads: Vec<f64> = shots.iter().flat_map(|s| s.loads).collect();
    let cores = crate::host::cores();
    let min = runs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = runs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let max_load_per_core = loads.iter().cloned().fold(0.0, f64::max) / cores as f64;
    let (c, dirty) = commit(&side.ws.root);
    Timing {
        at: now_rfc3339(),
        test: id.to_string(),
        commit: c,
        dirty,
        host: hostname(),
        cores,
        median_s: round(median(&runs)),
        min_s: round(min),
        max_s: round(max),
        spread_s: round(max - min),
        runs: runs.iter().map(|r| round(*r)).collect(),
        loads,
        max_load_per_core: round(max_load_per_core),
        threshold,
        reliable: max_load_per_core <= threshold,
        cpu_median_s: round(median(&cpu)),
        cpu_runs: cpu.iter().map(|r| round(*r)).collect(),
    }
}

/// Times `id` in two trees, interleaved — base, head, head, base, … — so
/// both sides meet the same load, and a load that rises or falls during the
/// timing weighs on both alike.
pub fn time_pair(
    base: &Side,
    head: &Side,
    profile: &str,
    id: &TestId,
    n: usize,
    threshold: f64,
) -> Result<(Timing, Timing)> {
    build_for(base.ws, id, base.env)?;
    build_for(head.ws, id, head.env)?;
    let (mut b, mut h) = (vec![], vec![]);
    for i in 0..n {
        if i % 2 == 0 {
            b.push(one_run(base, profile, id)?);
            h.push(one_run(head, profile, id)?);
        } else {
            h.push(one_run(head, profile, id)?);
            b.push(one_run(base, profile, id)?);
        }
    }
    Ok((
        summarize(base, id, &b, threshold),
        summarize(head, id, &h, threshold),
    ))
}

pub fn median(xs: &[f64]) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return f64::NAN;
    }
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

/// Runs `id` alone `n` times; loads are read before and after each.
pub fn time_runs(
    ws: &Workspace,
    profile: &str,
    id: &TestId,
    n: usize,
    threshold: f64,
    env: &[(String, String)],
) -> Result<Timing> {
    build_for(ws, id, env)?;
    let side = Side { ws, env };
    let mut shots = vec![];
    for _ in 0..n {
        shots.push(one_run(&side, profile, id)?);
    }
    Ok(summarize(&side, id, &shots, threshold))
}

pub fn time(
    ws: &Workspace,
    store: &Store,
    query: &str,
    profile: &str,
    n: usize,
    threshold: f64,
) -> Result<Value> {
    let id = resolve(ws, store, query)?;
    let before = Load::now();
    if before.per_core() > threshold * 2.0 {
        eprintln!(
            "[testopt] warning: load {:.1} on {} cores is {:.2} per core, past twice the threshold {threshold}; timing anyway, flagged",
            before.load1,
            before.cores,
            before.per_core()
        );
    }
    let t = time_runs(ws, profile, &id, n, threshold, &[])?;
    store.append("times.jsonl", &t)?;
    let mut v = serde_json::to_value(&t)?;
    if !t.reliable {
        v["flag"] = json!(format!(
            "unreliable: load reached {:.2} per core (threshold {threshold}); this time cannot be compared with one taken on a quiet host",
            t.max_load_per_core
        ));
    }
    Ok(v)
}

/// The fixed wait a run of `wall` seconds can have spent, and the waits
/// counted: a sleep outside a loop no longer than the test (a longer one was
/// cut short, e.g. by a limit), times the call sites reaching it; and a
/// limit the test ran up to, one from half to all of its time. Sleeps run
/// one after another and add up; a limit runs beside them (a sleep is often
/// there to outlast it), so the wait is the larger of the two, not the sum.
pub fn fixed_wait(waits: &[source::Wait], wall: f64) -> (f64, Vec<Value>) {
    let mut cited = vec![];
    let mut slept = 0.0f64;
    let mut limit = 0.0f64;
    for w in waits {
        let Some(s) = w.seconds else { continue };
        let total = s * w.reached as f64;
        let is_sleep = (w.kind == "sleep" || w.kind == "shell-sleep") && !w.in_loop;
        if is_sleep && s >= 0.05 && total <= wall * 1.05 {
            slept += total;
            cited.push(json!({"at": w.at, "seconds": round(total), "code": w.code, "why": "sleep outside a loop"}));
        } else if w.kind == "limit" && s >= 0.5 * wall && s <= wall * 1.05 {
            limit = limit.max(s);
            cited.push(json!({"at": w.at, "seconds": round(s), "code": w.code, "why": "a limit the test ran up to"}));
        }
    }
    (slept.max(limit).min(wall), cited)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(kind: &str, seconds: f64, in_loop: bool) -> source::Wait {
        source::Wait {
            kind: kind.into(),
            seconds: Some(seconds),
            in_loop,
            repeated: false,
            at: "t.rs:1".into(),
            code: String::new(),
            via: vec![],
            reached: 1,
        }
    }

    #[test]
    fn a_limit_and_the_sleep_that_outlasts_it_are_one_wait() {
        // exclusive.rs: gate_seconds: 20 and sleep(21) in a 24.5s test.
        let (s, cited) = fixed_wait(
            &[wait("limit", 20.0, false), wait("sleep", 21.0, false)],
            24.5,
        );
        assert_eq!(s, 21.0);
        assert_eq!(cited.len(), 2);
        // Sleeps in sequence add up; one in a loop is a poll, not counted.
        let (s, _) = fixed_wait(
            &[
                wait("sleep", 2.0, false),
                wait("sleep", 3.0, false),
                wait("sleep", 1.0, true),
            ],
            10.0,
        );
        assert_eq!(s, 5.0);
        // A sleep longer than the test was cut short: not counted.
        let (s, cited) = fixed_wait(
            &[
                wait("shell-sleep", 600.0, false),
                wait("limit", 20.0, false),
            ],
            21.3,
        );
        assert_eq!((s, cited.len()), (20.0, 1));
    }
}
