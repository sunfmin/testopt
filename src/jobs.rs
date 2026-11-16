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
    let exe = std::env::current_exe()?;
    let mut cmd = vec![
        shell_quote(&exe.to_string_lossy()),
        "--job".into(),
        shell_quote(&id),
    ];
    cmd.extend(
        args.iter()
            .filter(|a| *a != "--tmux")
            .map(|a| shell_quote(a)),
    );
    let script = format!(
        ". {}; cd {}; {}; echo; echo \"[testopt] job {id} ended (exit $?); press enter to close\"; read _",
        shell_quote(&job.dir.join("env.sh").to_string_lossy()),
        shell_quote(&ws_root.to_string_lossy()),
        cmd.join(" ")
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
        if began.elapsed() > timeout {
            bail!(
                "job {} not done after {}s; it keeps running in tmux; `testopt job` shows it",
                job_dir.display(),
                timeout.as_secs()
            );
        }
        if last_say.elapsed() > Duration::from_secs(30) {
            if let Ok(p) = std::fs::read_to_string(job_dir.join("progress.json")) {
                eprintln!("[testopt] waiting: {}", p.replace('\n', " "));
            }
            last_say = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
