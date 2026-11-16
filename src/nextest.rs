//! Running tests with cargo-nextest, without touching the target's config:
//! testopt hands nextest a tool config file (`--tool-config-file`) that
//! defines a profile of its own inheriting the caller's, with JUnit output
//! on, and reads per-test times back from that JUnit file.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::store::write_json;
use crate::workspace::Workspace;

/// A test as nextest names it: binary id and test name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TestId {
    pub binary_id: String,
    pub name: String,
}

impl TestId {
    pub fn filter(&self) -> String {
        format!("binary_id(={}) & test(={})", self.binary_id, self.name)
    }
}

impl fmt::Display for TestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.binary_id, self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestTime {
    pub binary_id: String,
    pub name: String,
    pub seconds: f64,
    /// pass | fail | skip
    pub status: String,
}

impl TestTime {
    pub fn id(&self) -> TestId {
        TestId { binary_id: self.binary_id.clone(), name: self.name.clone() }
    }
}

/// One nextest filter from a file of `<binary-id> <test name>` lines, as
/// afkconvoy's gate.sh builds its slow gate's: `#` comments and blank lines
/// skipped; each line becomes `(binary_id(=B) & test(=N))`, or-ed.
pub fn filter_from_file(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut parts = vec![];
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((binary, name)) = t.split_once(char::is_whitespace) else {
            bail!("{}: line `{t}` is not `<binary-id> <test name>`", path.display());
        };
        parts.push(format!("(binary_id(={binary}) & test(={}))", name.trim()));
    }
    Ok(if parts.is_empty() { "none()".into() } else { parts.join(" | ") })
}

/// The tests nextest runs at once under `profile`: an explicit count, else
/// `NEXTEST_TEST_THREADS`, else the profile's `test-threads` (following
/// `inherits`, then `default`) in `.config/nextest.toml`, else one per core.
pub fn threads(ws: &Workspace, profile: &str, explicit: Option<usize>) -> usize {
    if let Some(n) = explicit {
        return n;
    }
    let cores = crate::host::cores();
    if let Some(n) = std::env::var("NEXTEST_TEST_THREADS").ok().and_then(|v| parse_threads(&v, cores)) {
        return n;
    }
    let config: toml::Table = std::fs::read_to_string(ws.root.join(".config/nextest.toml"))
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or_default();
    let profiles = config.get("profile").and_then(|p| p.as_table());
    let mut name = profile.to_string();
    for _ in 0..8 {
        let Some(p) = profiles.and_then(|ps| ps.get(&name)).and_then(|p| p.as_table()) else { break };
        if let Some(v) = p.get("test-threads") {
            let parsed = match v {
                toml::Value::Integer(i) => parse_threads(&i.to_string(), cores),
                toml::Value::String(s) => parse_threads(s, cores),
                _ => None,
            };
            if let Some(n) = parsed {
                return n;
            }
        }
        match p.get("inherits").and_then(|v| v.as_str()) {
            Some(parent) => name = parent.to_string(),
            None if name != "default" => name = "default".into(),
            None => break,
        }
    }
    cores
}

fn parse_threads(v: &str, cores: usize) -> Option<usize> {
    if v == "num-cpus" {
        return Some(cores);
    }
    let i: i64 = v.trim().parse().ok()?;
    match i {
        0 => None,
        i if i < 0 => Some((cores as i64 + i).max(1) as usize),
        i => Some(i as usize),
    }
}

/// Where nextest keeps its output for this workspace: its `[store] dir`,
/// default `target/nextest` under the workspace root.
fn nextest_store(ws: &Workspace) -> PathBuf {
    let config: Option<toml::Table> =
        std::fs::read_to_string(ws.root.join(".config/nextest.toml")).ok().and_then(|t| t.parse().ok());
    let dir = config
        .as_ref()
        .and_then(|c| c.get("store"))
        .and_then(|s| s.get("dir"))
        .and_then(|d| d.as_str())
        .unwrap_or("target/nextest");
    ws.root.join(dir)
}

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    /// The caller's nextest profile, which testopt's own profile inherits.
    pub profile: String,
    pub filter: Option<String>,
    pub threads: Option<usize>,
    /// Further `cargo nextest run` arguments.
    pub extra: Vec<String>,
    /// Further environment for nextest and the tests.
    pub env: Vec<(String, String)>,
    /// Where nextest's stdout and stderr go, line by line.
    pub log: Option<PathBuf>,
    /// Where live progress is written as JSON.
    pub progress: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub elapsed_s: f64,
    pub last: String,
    pub finished: bool,
}

pub struct Running {
    child: Child,
    started: Instant,
    junit: PathBuf,
    tool_dir: PathBuf,
    reader: std::thread::JoinHandle<Option<String>>,
    progress: Arc<Mutex<Progress>>,
    progress_path: Option<PathBuf>,
}

pub struct Outcome {
    pub tests: Vec<TestTime>,
    pub success: bool,
    pub wall_s: f64,
    /// nextest's own `Summary [...]` line.
    pub summary: Option<String>,
}

impl Running {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn wait(mut self) -> Result<Outcome> {
        let status = self.child.wait()?;
        let wall_s = self.started.elapsed().as_secs_f64();
        let summary = self.reader.join().unwrap_or(None);
        {
            let mut p = self.progress.lock().unwrap();
            p.finished = true;
            p.elapsed_s = wall_s;
            if let Some(path) = &self.progress_path {
                let _ = write_json(path, &*p);
            }
        }
        let tests = if self.junit.exists() {
            let xml = std::fs::read_to_string(&self.junit)?;
            parse_junit(&xml)?
        } else {
            vec![]
        };
        let _ = std::fs::remove_dir_all(&self.tool_dir);
        if let Some(dir) = self.junit.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        if tests.is_empty() && !status.success() {
            bail!("nextest failed before running any test (exit {status}); see its log");
        }
        Ok(Outcome { tests, success: status.success(), wall_s, summary })
    }
}

/// Starts `cargo nextest run` in `ws` per `spec`.
pub fn start(ws: &Workspace, spec: &Spec) -> Result<Running> {
    // A profile per process, so concurrent testopt runs do not share a JUnit file.
    let profile = format!("testopt:r{}", std::process::id());
    let tool_dir = std::env::temp_dir().join(format!("testopt-tool-{}", std::process::id()));
    std::fs::create_dir_all(&tool_dir)?;
    let tool_config = tool_dir.join("nextest.toml");
    let base = if spec.profile.is_empty() { "default" } else { &spec.profile };
    std::fs::write(
        &tool_config,
        format!(
            "[profile.\"{profile}\"]\ninherits = \"{base}\"\nfail-fast = false\n\n[profile.\"{profile}\".junit]\npath = \"junit.xml\"\n"
        ),
    )?;
    let junit = nextest_store(ws).join(&profile).join("junit.xml");
    let _ = std::fs::remove_file(&junit);

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&ws.root)
        .args(["nextest", "run", "--workspace", "--no-fail-fast", "--no-tests=pass"])
        .arg("--tool-config-file")
        .arg(format!("testopt:{}", tool_config.display()))
        .args(["--profile", &profile]);
    if let Some(f) = &spec.filter {
        cmd.args(["-E", f]);
    }
    if let Some(n) = spec.threads {
        cmd.args(["--test-threads", &n.to_string()]);
    }
    cmd.args(&spec.extra);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("starting cargo nextest (is cargo-nextest installed?)")?;
    let started = Instant::now();

    let progress = Arc::new(Mutex::new(Progress::default()));
    let log = match &spec.log {
        Some(p) => Some(Arc::new(Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(p)?))),
        None => None,
    };
    // stdout is copied to the log as it comes; stderr carries the progress.
    let stdout = child.stdout.take().unwrap();
    let out_log = log.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(l) = &out_log {
                let _ = writeln!(l.lock().unwrap(), "{line}");
            }
        }
    });
    let stderr = child.stderr.take().unwrap();
    let prog = progress.clone();
    let progress_path = spec.progress.clone();
    let path_for_thread = progress_path.clone();
    let reader = std::thread::spawn(move || {
        let mut summary = None;
        let mut last_write = Instant::now();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(l) = &log {
                let _ = writeln!(l.lock().unwrap(), "{line}");
            }
            let t = line.trim();
            let mut p = prog.lock().unwrap();
            if let Some(n) = starting_total(t) {
                p.total = n;
            }
            if let Some((word, done, total)) = status_line(t) {
                p.done = p.done.max(done);
                p.total = p.total.max(total);
                if matches!(word, "FAIL" | "TIMEOUT" | "SIGSEGV" | "SIGABRT" | "SIGKILL" | "ABORT" | "LEAK-FAIL") {
                    p.failed += 1;
                }
                p.last = t.to_string();
            }
            if t.starts_with("Summary [") {
                summary = Some(t.to_string());
            }
            p.elapsed_s = started.elapsed().as_secs_f64();
            if let Some(path) = &path_for_thread
                && last_write.elapsed().as_millis() >= 500
            {
                let _ = write_json(path, &*p);
                last_write = Instant::now();
            }
        }
        summary
    });
    Ok(Running { child, started, junit, tool_dir, reader, progress, progress_path })
}

pub fn run(ws: &Workspace, spec: &Spec) -> Result<Outcome> {
    start(ws, spec)?.wait()
}

/// `Starting 5 tests across 2 binaries` → 5.
fn starting_total(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("Starting ")?;
    rest.split_whitespace().next()?.parse().ok()
}

/// `PASS [   1.270s] (4/5) fixture::it spawns` → ("PASS", 4, 5).
fn status_line(line: &str) -> Option<(&str, usize, usize)> {
    let (word, rest) = line.split_once(char::is_whitespace)?;
    if !word.chars().all(|c| c.is_ascii_uppercase() || c == '-') || word.is_empty() {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('[')?;
    let (_, rest) = rest.split_once(']')?;
    let rest = rest.trim_start().strip_prefix('(')?;
    let (count, _) = rest.split_once(')')?;
    let (done, total) = count.split_once('/')?;
    Some((word, done.trim().parse().ok()?, total.trim().parse().ok()?))
}

enum Kind {
    Start,
    Other,
}

pub fn parse_junit(xml: &str) -> Result<Vec<TestTime>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out: Vec<TestTime> = vec![];
    let mut in_case = false;
    loop {
        let event = reader.read_event()?;
        let event_kind = if matches!(event, Event::Start(_)) { Kind::Start } else { Kind::Other };
        match event {
            Event::Start(ref e) | Event::Empty(ref e) if e.name().as_ref() == b"testcase" => {
                let has_children = matches!(event_kind, Kind::Start);
                let mut name = String::new();
                let mut class = String::new();
                let mut time = 0.0;
                for a in e.attributes().flatten() {
                    let v = a.unescape_value()?.into_owned();
                    match a.key.as_ref() {
                        b"name" => name = v,
                        b"classname" => class = v,
                        b"time" => time = v.parse().unwrap_or(0.0),
                        _ => {}
                    }
                }
                out.push(TestTime { binary_id: class, name, seconds: time, status: "pass".into() });
                in_case = has_children;
            }
            Event::Start(e) | Event::Empty(e) if in_case => {
                let status = match e.name().as_ref() {
                    b"failure" | b"error" => Some("fail"),
                    b"skipped" => Some("skip"),
                    _ => None,
                };
                if let (Some(s), Some(last)) = (status, out.last_mut()) {
                    last.status = s.into();
                }
            }
            Event::End(e) if e.name().as_ref() == b"testcase" => in_case = false,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// A test nextest lists, with the binary that runs it.
#[derive(Debug, Clone)]
pub struct Listed {
    pub id: TestId,
    pub binary_path: PathBuf,
}

/// `cargo nextest list` of `filter` (building what it needs), matching tests only.
pub fn list(ws: &Workspace, filter: Option<&str>, env: &[(String, String)]) -> Result<Vec<Listed>> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&ws.root).args(["nextest", "list", "--workspace", "--message-format", "json"]);
    if let Some(f) = filter {
        cmd.args(["-E", f]);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.stderr(Stdio::inherit()).output().context("running cargo nextest list")?;
    if !out.status.success() {
        bail!("cargo nextest list failed (exit {})", out.status);
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let mut tests = vec![];
    for (bid, suite) in v["rust-suites"].as_object().into_iter().flatten() {
        let path = PathBuf::from(suite["binary-path"].as_str().unwrap_or_default());
        for (name, case) in suite["testcases"].as_object().into_iter().flatten() {
            if case["filter-match"]["status"] == "matches" {
                tests.push(Listed { id: TestId { binary_id: bid.clone(), name: name.clone() }, binary_path: path.clone() });
            }
        }
    }
    tests.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tests)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nextest_status_lines() {
        assert_eq!(status_line("PASS [   1.270s] (4/5) fixture::it spawns"), Some(("PASS", 4, 5)));
        assert_eq!(status_line("LEAK-FAIL [ 0.1s] (1/9) a b"), Some(("LEAK-FAIL", 1, 9)));
        assert_eq!(status_line("Summary [ 1.5s] 5 tests run"), None);
        assert_eq!(starting_total("Starting 991 tests across 9 binaries"), Some(991));
    }

    #[test]
    fn junit_times_and_failures() {
        let xml = r#"<testsuites><testsuite name="a::b" tests="2">
            <testcase name="x::y" classname="a::b" time="1.5"></testcase>
            <testcase name="z" classname="a::b" time="0.25"><failure type="test failure"/></testcase>
            <testcase name="s" classname="a::b" time="0"/>
        </testsuite></testsuites>"#;
        let t = parse_junit(xml).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!((t[0].name.as_str(), t[0].seconds, t[0].status.as_str()), ("x::y", 1.5, "pass"));
        assert_eq!(t[1].status, "fail");
        assert_eq!(t[2].status, "pass");
    }

    #[test]
    fn filter_file_matches_gate_sh() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("slow");
        std::fs::write(&f, "# c\n\nafk::cli a::b\n  core x y \n").unwrap();
        assert_eq!(
            filter_from_file(&f).unwrap(),
            "(binary_id(=afk::cli) & test(=a::b)) | (binary_id(=core) & test(=x y))"
        );
    }
}
