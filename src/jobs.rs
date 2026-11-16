//! Jobs: a long command's progress, log and result, kept in the store so
//! `dash` and a waiting caller can read them, and run in tmux on request.
//!
//! tmux layout: one session, `testopt`. Window `dash` runs `testopt dash`
//! for the workspace; each long job gets a window `run-<job id>` of its own,
//! which stays open after the job ends until a key is pressed. The caller
//! never reads a pane: it waits on the job's `result.json`.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::store::{Store, write_json};

pub struct Job {
    pub id: String,
    pub dir: PathBuf,
    /// Echo notes to stderr too (not when running in a tmux window).
    pub quiet: bool,
}

pub fn new_id(kind: &str) -> String {
    let ts = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string();
    format!("{ts}-{kind}-{}", std::process::id())
}

impl Job {
    pub fn open(store: &Store, id: &str) -> Result<Job> {
        let dir = store.jobs_dir().join(id);
        std::fs::create_dir_all(&dir)?;
        Ok(Job {
            id: id.to_string(),
            dir,
            quiet: false,
        })
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("log")
    }
    pub fn progress_path(&self) -> PathBuf {
        self.dir.join("progress.json")
    }
    pub fn result_path(&self) -> PathBuf {
        self.dir.join("result.json")
    }

    pub fn note(&self, msg: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
        {
            let _ = writeln!(f, "[testopt] {msg}");
        }
        if !self.quiet {
            eprintln!("[testopt] {msg}");
        }
    }

    pub fn finish(&self, result: &Result<Value>) -> Result<()> {
        let v = match result {
            Ok(v) => serde_json::json!({"ok": true, "result": v}),
            Err(e) => serde_json::json!({"ok": false, "error": format!("{e:#}")}),
        };
        write_json(&self.result_path(), &v)
    }
}

/// The newest job directory, by name (ids start with a timestamp).
pub fn latest(store: &Store) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(store.jobs_dir())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.pop()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The tmux session this process runs inside, if any.
pub fn own_session() -> Option<String> {
    std::env::var_os("TMUX")?;
    let out = Command::new("tmux")
        .args(["display-message", "-p", "#S"])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// Refuses to touch `session` when it is the one this process runs in: a
/// caller inside tmux (an agent, a human's shell) must never have its own
/// session killed, replaced, or filled with job windows by testopt.
pub fn guard_session(session: &str) -> Result<()> {
    if let Some(own) = own_session()
        && own == session
    {
        bail!(
            "refusing to use tmux session `{session}`: testopt is running inside it, and killing or reusing it would take the caller down; pass --tmux-session <another name>"
        );
    }
    Ok(())
}

/// Kills the job session `session`, never the one this process runs in.
pub fn kill_session(session: &str) -> Result<serde_json::Value> {
    guard_session(session)?;
    let st = Command::new("tmux")
        .args(["kill-session", "-t", &format!("={session}")])
        .output()?;
    Ok(serde_json::json!({"session": session, "killed": st.status.success()}))
}

/// Starts this same command, without `--tmux`, in a new window of the job
/// tmux session `session` (making it, with its `dash` window, if there is
/// none), and returns the job directory to wait on. Refuses when `session`
/// is the session this process runs in.
pub fn start_in_tmux(
    store: &Store,
    ws_root: &Path,
    session: &str,
    kind: &str,
    args: &[String],
) -> Result<Job> {
    guard_session(session)?;
    let id = new_id(kind);
    let job = Job::open(store, &id)?;
    // The window runs under the tmux server's environment, not ours: carry
    // ours over in a script the window sources first.
    let mut env = String::new();
    for (k, v) in std::env::vars() {
        if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.starts_with("TMUX") {
            env.push_str(&format!("export {k}={}\n", shell_quote(&v)));
        }
    }
    std::fs::write(job.dir.join("env.sh"), env)?;
    // The job runs its own copy of testopt: a new file, so a rebuild or
    // reinstall while it runs replaces nothing it is executing (a binary
    // overwritten in place is killed by macOS as it next runs).
    let exe = job.dir.join("testopt");
    std::fs::copy(std::env::current_exe()?, &exe)?;
    let mut cmd = vec![
        shell_quote(&exe.to_string_lossy()),
        "--job".into(),
        shell_quote(&id),
    ];
    cmd.extend(job_args(args).iter().map(|a| shell_quote(a)));
    let script = format!(
        ". {}; cd {}; {}; rc=$?; echo $rc > {}; echo; if [ $rc -eq 0 ]; then echo \"[testopt] job {id} ended (exit 0); this window closes in 60s\"; sleep 60; else echo \"[testopt] job {id} ended (exit $rc); press enter to close\"; read _; fi",
        shell_quote(&job.dir.join("env.sh").to_string_lossy()),
        shell_quote(&ws_root.to_string_lossy()),
        cmd.join(" "),
        shell_quote(&job.dir.join("exit").to_string_lossy())
    );
    let has = Command::new("tmux")
        .args(["has-session", "-t", &format!("={session}")])
        .output()
        .context("running tmux")?;
    if !has.status.success() {
        let dash = format!(
            ". {}; cd {}; {} dash",
            shell_quote(&job.dir.join("env.sh").to_string_lossy()),
            shell_quote(&ws_root.to_string_lossy()),
            shell_quote(&exe.to_string_lossy())
        );
        let st = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                session,
                "-n",
                "dash",
                "-x",
                "160",
                "-y",
                "48",
                "sh",
                "-c",
                &dash,
            ])
            .status()?;
        if !st.success() {
            bail!("tmux new-session failed");
        }
    }
    let st = Command::new("tmux")
        .args([
            "new-window",
            "-d",
            "-t",
            &format!("={session}:"),
            "-n",
            &format!("run-{id}"),
            "sh",
            "-c",
            &script,
        ])
        .status()?;
    if !st.success() {
        bail!("tmux new-window failed");
    }
    Ok(job)
}

/// Waits for a job's result file, saying its progress on stderr now and then.
pub fn wait(job_dir: &Path, timeout: Duration) -> Result<Value> {
    let result = job_dir.join("result.json");
    let began = Instant::now();
    let mut last_say = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(&result)
            && let Ok(v) = serde_json::from_str::<Value>(&text)
        {
            if v["ok"] == true {
                return Ok(v["result"].clone());
            }
            bail!(
                "job failed: {}",
                v["error"].as_str().unwrap_or("unknown error")
            );
        }
        // The job's window writes its exit code when testopt ends there. Ended
        // with no result (it failed before it could write one, or was
        // killed): say so now rather than wait out the timeout.
        if let Ok(code) = std::fs::read_to_string(job_dir.join("exit"))
            && !result.exists()
        {
            let log = std::fs::read_to_string(job_dir.join("log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(5).collect();
            bail!(
                "job {} ended (exit {}) without a result; its window in tmux shows why{}",
                job_dir.display(),
                code.trim(),
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; its log ends: {}",
                        tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
                    )
                }
            );
        }
        if began.elapsed() > timeout {
            bail!(
                "job {} not done after {}s; it keeps running in tmux; `testopt job` shows it",
                job_dir.display(),
                timeout.as_secs()
            );
        }
        if last_say.elapsed() > Duration::from_secs(30) {
            if let Some(line) = std::fs::read(job_dir.join("progress.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<crate::nextest::Progress>(&b).ok())
                .map(|p| progress_line(&p))
            {
                eprintln!("[testopt] waiting: {line}");
            }
            last_say = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The newest job without a result.
pub fn unfinished(store: &Store) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(store.jobs_dir())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|d| !d.join("result.json").exists())
        .collect();
    dirs.sort();
    dirs.pop()
}

/// One line of a job's progress: `538/993, 0 failed, 209s, ~176s left — last: …`.
pub fn progress_line(p: &crate::nextest::Progress) -> String {
    let eta = (p.done > 0 && p.done < p.total)
        .then(|| p.elapsed_s / p.done as f64 * (p.total - p.done) as f64);
    format!(
        "{}/{}, {} failed, {:.0}s{} — last: {}",
        p.done,
        p.total,
        p.failed,
        p.elapsed_s,
        eta.map(|e| format!(", ~{e:.0}s left")).unwrap_or_default(),
        p.last
    )
}

/// The command a job ran, from its id (`<date>-<time>-<command>-<pid>`).
pub fn kind_of(job_dir: &Path) -> String {
    job_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('-').nth(2))
        .unwrap_or_default()
        .to_string()
}

/// The arguments a job runs with: the caller's, less `--tmux`, and less the
/// workspace (`-C`): the job runs in the workspace root, where a relative
/// `-C` would name another directory.
fn job_args(args: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        match a.as_str() {
            "--tmux" => {}
            "-C" | "--workspace" => skip = true,
            _ if a.starts_with("--workspace=") || (a.starts_with("-C") && a.len() > 2) => {}
            _ => out.push(a.clone()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_one_line_with_an_eta() {
        let p = crate::nextest::Progress {
            done: 538,
            total: 993,
            failed: 0,
            elapsed_s: 209.4,
            last: "PASS lander::a_landing".into(),
            finished: false,
        };
        assert_eq!(
            progress_line(&p),
            "538/993, 0 failed, 209s, ~177s left — last: PASS lander::a_landing"
        );
        let start = crate::nextest::Progress {
            total: 20,
            ..Default::default()
        };
        assert_eq!(progress_line(&start), "0/20, 0 failed, 0s — last: ");
    }

    #[test]
    fn a_job_runs_without_tmux_or_the_workspace() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            job_args(&a(&[
                "-C", "rel/ws", "--tmux", "covers", "x", "--peers", "file"
            ])),
            a(&["covers", "x", "--peers", "file"])
        );
        assert_eq!(
            job_args(&a(&[
                "--workspace=w",
                "-Cw",
                "--workspace",
                "w",
                "why",
                "t"
            ])),
            a(&["why", "t"])
        );
    }

    #[test]
    fn wait_ends_when_the_job_ended_without_a_result() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exit"), "2\n").unwrap();
        std::fs::write(
            dir.path().join("log"),
            "running cargo metadata: no such file\n",
        )
        .unwrap();
        let t = Instant::now();
        let e = wait(dir.path(), Duration::from_secs(60))
            .unwrap_err()
            .to_string();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(e.contains("ended (exit 2) without a result"), "{e}");
        assert!(e.contains("running cargo metadata"), "{e}");
    }
}
