//! `dash`: a terminal view for a tmux pane. The newest job's progress, the
//! latest run's top 10, the ledger's totals; redrawn every few seconds.

use anyhow::Result;
use std::fmt::Write as _;
use std::time::Duration;

use crate::host::Load;
use crate::ledger::{Entry, totals};
use crate::nextest::Progress;
use crate::record::Run;
use crate::store::Store;
use crate::workspace::Workspace;

pub fn frame(ws: &Workspace, store: &Store) -> String {
    let mut s = String::new();
    let load = Load::now();
    let _ = writeln!(
        s,
        "testopt dash — {}   {}   load {:.1} / {} cores",
        ws.root.display(),
        jiff::Zoned::now().strftime("%H:%M:%S"),
        load.load1,
        load.cores
    );
    let _ = writeln!(s, "{}", "─".repeat(78));
    match crate::jobs::latest(store) {
        Some(dir) => {
            let id = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let p: Option<Progress> = std::fs::read(dir.join("progress.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            let done = dir.join("result.json").exists();
            let state = if done { "finished" } else { "running" };
            let _ = writeln!(s, "job {id}: {state}");
            if let Some(p) = p {
                let pct = if p.total > 0 {
                    100.0 * p.done as f64 / p.total as f64
                } else {
                    0.0
                };
                let width = 40;
                let filled = (pct / 100.0 * width as f64).round() as usize;
                let eta = if p.done > 0 && p.total > p.done && !done {
                    format!(
                        "  ~{:.0}s left",
                        p.elapsed_s / p.done as f64 * (p.total - p.done) as f64
                    )
                } else {
                    String::new()
                };
                let _ = writeln!(
                    s,
                    "  [{}{}] {}/{} tests, {} failed, {:.0}s{eta}",
                    "#".repeat(filled.min(width)),
                    ".".repeat(width - filled.min(width)),
                    p.done,
                    p.total,
                    p.failed,
                    p.elapsed_s
                );
                if !p.last.is_empty() {
                    let _ = writeln!(
                        s,
                        "  last: {}",
                        p.last.chars().take(110).collect::<String>()
                    );
                }
            } else if let Ok(log) = std::fs::read_to_string(dir.join("log")) {
                for l in log
                    .lines()
                    .rev()
                    .take(3)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    let _ = writeln!(s, "  {}", l.chars().take(110).collect::<String>());
                }
            }
        }
        None => {
            let _ = writeln!(s, "no job yet");
        }
    }
    let _ = writeln!(s);
    let runs: Vec<Run> = store.read("runs.jsonl").unwrap_or_default();
    match runs.last() {
        Some(run) => {
            let total = run.test_seconds();
            let _ = writeln!(
                s,
                "latest run {} — {} tests, {:.0} test-s, {:.0}s wall, {} at a time",
                run.id,
                run.tests.len(),
                total,
                run.wall_s,
                run.threads
            );
            let mut tests: Vec<_> = run.tests.iter().collect();
            tests.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
            let _ = writeln!(s, "  {:>7} {:>6} {:>7}  test", "secs", "share", "wall-s");
            for t in tests.iter().take(10) {
                let name = format!("{} {}", t.binary_id, t.name);
                let name: String = if name.chars().count() > 90 {
                    format!(
                        "…{}",
                        name.chars()
                            .rev()
                            .take(89)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<String>()
                    )
                } else {
                    name
                };
                let _ = writeln!(
                    s,
                    "  {:>7.2} {:>5.1}% {:>7.2}  {name}",
                    t.seconds,
                    100.0 * t.seconds / total.max(1e-9),
                    t.seconds / run.threads.max(1) as f64
                );
            }
        }
        None => {
            let _ = writeln!(s, "no run recorded yet");
        }
    }
    let _ = writeln!(s);
    let entries: Vec<Entry> = store.read("ledger.jsonl").unwrap_or_default();
    let t = totals(&entries);
    let _ = writeln!(
        s,
        "ledger: {} entries — {} kept, {} rejected, {} tried; kept saves {} test-s ≈ {} s wall",
        t["entries"],
        t["kept"],
        t["rejected"],
        t["tried"],
        t["kept_test_seconds_saved"],
        t["kept_est_wall_saving_s"]
    );
    for e in entries.iter().rev().take(5) {
        let _ = writeln!(
            s,
            "  {:<8} {:<13} {}",
            e.status,
            e.kind,
            e.test.chars().take(90).collect::<String>()
        );
    }
    s
}

pub fn run(ws: &Workspace, store: &Store, once: bool, interval: Duration) -> Result<()> {
    loop {
        let f = frame(ws, store);
        if once {
            print!("{f}");
            return Ok(());
        }
        // Clear, home, draw.
        print!("\x1b[2J\x1b[H{f}");
        use std::io::Write;
        std::io::stdout().flush()?;
        std::thread::sleep(interval);
    }
}
