//! Per-test line coverage with cargo-llvm-cov's instrumentation, in one
//! nextest run for any number of tests.
//!
//! `cargo llvm-cov show-env` gives the environment of an instrumented build.
//! nextest runs each test in its own process, through a target runner
//! (`CARGO_TARGET_<triple>_RUNNER`); testopt is that runner, and it points
//! `LLVM_PROFILE_FILE` at a directory of the test's own (nextest names the
//! test in `NEXTEST_BINARY_ID` / `NEXTEST_TEST_NAME`). The processes a test
//! spawns inherit it, so their coverage counts as the test's. Each directory
//! is merged with llvm-profdata and exported with llvm-cov as lcov.
//!
//! Lines are of the workspace's own code under test: files under a `tests/`
//! directory, `#[cfg(test)]` modules and `#[test]` functions are left out —
//! a test always covers its own body, which says nothing of overlap.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::nextest::{self, Spec, TestId};
use crate::record::round;
use crate::source::{Index, test_code_lines};
use crate::store::{Store, test_key, write_json};
use crate::workspace::{Workspace, fnv, rust_files};

/// file (relative to the workspace root) → covered lines.
pub type Lines = BTreeMap<String, BTreeSet<u32>>;

/// A branch arm as one value of a `Lines` map: its line, shifted, and the
/// arm's index (lcov's block and branch) below. Two tests that cover one
/// line but take different arms of it do not overlap there.
pub fn arm(line: u32, block: u32, branch: u32) -> u32 {
    (line << 8) | ((block * 16 + branch).min(255))
}

pub fn arm_line(unit: u32) -> u32 {
    unit >> 8
}

/// The cache's format: a tree's coverage measured before it changed is
/// measured again.
const FORMAT: &str = "v3-flags-dir";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Covered {
    pub test: TestId,
    pub tree: String,
    pub status: String,
    pub lines: Lines,
    /// Branch arms taken, as `arm()` units (empty where the toolchain has
    /// no branch coverage).
    #[serde(default)]
    pub branches: Lines,
}

pub fn count(l: &Lines) -> usize {
    l.values().map(|s| s.len()).sum()
}

/// The runner: called by nextest as `testopt __cov-runner <binary> <args>`.
pub fn runner(args: &[String]) -> ! {
    let raw = std::env::var("TESTOPT_COV_RAW").unwrap_or_else(|_| ".".into());
    let key = match (
        std::env::var("NEXTEST_TEST_PHASE").as_deref(),
        std::env::var("NEXTEST_TEST_NAME"),
    ) {
        (Ok("run"), Ok(name)) => test_key(
            &std::env::var("NEXTEST_BINARY_ID").unwrap_or_default(),
            &name,
        ),
        _ => "_list".to_string(),
    };
    let dir = Path::new(&raw).join(&key);
    let _ = std::fs::create_dir_all(&dir);
    if key != "_list" {
        let _ = std::fs::write(
            dir.join("test.json"),
            serde_json::to_vec(&json!({
                "binary_id": std::env::var("NEXTEST_BINARY_ID").unwrap_or_default(),
                "name": std::env::var("NEXTEST_TEST_NAME").unwrap_or_default(),
                "binary": args.first(),
            }))
            .unwrap_or_default(),
        );
    }
    use std::os::unix::process::CommandExt;
    let err = Command::new(&args[0])
        .args(&args[1..])
        .env("LLVM_PROFILE_FILE", dir.join("%p-%m.profraw"))
        .exec();
    eprintln!("testopt __cov-runner: exec {}: {err}", args[0]);
    std::process::exit(127)
}

/// A content hash of the workspace's Rust sources and manifests: coverage
/// is cached per tree, so a test's lines are measured once per state.
pub fn tree_key(ws: &Workspace) -> String {
    let mut files = rust_files(&ws.root);
    for extra in ["Cargo.toml", "Cargo.lock"] {
        files.push(ws.root.join(extra));
    }
    for p in &ws.packages {
        files.push(p.dir.join("Cargo.toml"));
    }
    files.sort();
    files.dedup();
    let mut h = 0xcbf29ce484222325u64;
    for f in files {
        let Ok(bytes) = std::fs::read(&f) else {
            continue;
        };
        h ^= fnv(ws.relative(&f).to_string_lossy().as_bytes());
        h = h.wrapping_mul(0x100000001b3) ^ fnv(&bytes);
    }
    format!("{h:016x}-{FORMAT}")
}

fn host_triple() -> Result<String> {
    let out = Command::new("rustc").arg("-vV").output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_string)
        .context("rustc -vV names no host")
}

fn llvm_tool(ws: &Workspace, tool: &str) -> Result<PathBuf> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(&ws.root)
        .output()?;
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let p = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host_triple()?)
        .join("bin")
        .join(tool);
    if !p.exists() {
        bail!(
            "{} not found: `rustup component add llvm-tools` for this toolchain",
            p.display()
        );
    }
    Ok(p)
}

/// The environment of an instrumented build, from `cargo llvm-cov show-env`,
/// and the dir it builds into: a dir under `target_dir` named for its
/// compiler flags. cargo-llvm-cov passes them through its rustc wrapper, which
/// cargo does not fingerprint, so a build made with other flags (lines only,
/// say) would look fresh and keep its old instrumentation.
fn cov_env(ws: &Workspace, target_dir: &Path) -> Result<(Vec<(String, String)>, PathBuf)> {
    // Branch coverage where the toolchain has it (nightly), lines alone otherwise.
    let show = |branch: bool| {
        let mut c = Command::new("cargo");
        c.args(["llvm-cov", "show-env", "--sh"]);
        if branch {
            c.arg("--branch");
        }
        c.current_dir(&ws.root)
            .env("CARGO_TARGET_DIR", target_dir)
            .env("CARGO_LLVM_COV_TARGET_DIR", target_dir)
            .output()
            .context("running cargo llvm-cov (is cargo-llvm-cov installed?)")
    };
    let mut out = show(true)?;
    if !out.status.success() {
        out = show(false)?;
    }
    if !out.status.success() {
        bail!(
            "cargo llvm-cov show-env failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut env = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some(rest) = line.strip_prefix("export ") else {
            continue;
        };
        let Some((k, v)) = rest.split_once('=') else {
            continue;
        };
        env.push((k.to_string(), unquote(v)));
    }
    env.retain(|(k, _)| k != "LLVM_PROFILE_FILE");
    let flags: String = env
        .iter()
        .filter(|(k, _)| k.contains("RUSTFLAGS"))
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect();
    let dir = target_dir.join(format!(
        "{:012x}",
        crate::workspace::fnv(flags.as_bytes()) & 0xffff_ffff_ffff
    ));
    env.retain(|(k, _)| k != "CARGO_TARGET_DIR" && k != "CARGO_LLVM_COV_TARGET_DIR");
    env.push(("CARGO_TARGET_DIR".into(), dir.display().to_string()));
    env.push((
        "CARGO_LLVM_COV_TARGET_DIR".into(),
        dir.display().to_string(),
    ));
    Ok((env, dir))
}

/// Undoes sh single-quoting: `'a'\''b'` → `a'b`.
fn unquote(v: &str) -> String {
    let mut out = String::new();
    let mut in_q = false;
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => in_q = !in_q,
            '\\' if !in_q => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Where coverage of tree `tree` is cached.
fn cache_dir(store: &Store, tree: &str) -> PathBuf {
    store.dir.join("coverage").join(tree)
}

/// Which tests coverage is measured for.
pub enum Scope<'a> {
    /// A nextest filter (None: every test).
    Filter(Option<&'a str>),
    /// Tests whose function is in one of `files` (relative to the
    /// workspace root), and the tests `ids`.
    Auto {
        files: BTreeSet<String>,
        ids: BTreeSet<TestId>,
    },
}

impl Scope<'_> {
    fn filter(&self) -> Option<&str> {
        match self {
            Scope::Filter(f) => *f,
            Scope::Auto { .. } => None,
        }
    }
}

/// Covered lines of every test `filter` selects in `ws`, measured in one
/// instrumented nextest run (cached per tree: tests measured before are not
/// run again). Coverage builds go to `cov_target`, apart from normal builds.
pub fn collect(
    ws: &Workspace,
    store: &Store,
    scope: &Scope,
    profile: &str,
    cov_target: &Path,
    note: &dyn Fn(&str),
) -> Result<HashMap<TestId, Covered>> {
    let tree = tree_key(ws);
    let cache = cache_dir(store, &tree);
    std::fs::create_dir_all(&cache)?;
    let (mut env, cov_target) = cov_env(ws, cov_target)?;
    let cov_target = cov_target.as_path();
    // Every instrumented process writes a profile: build scripts, listings,
    // anything not run as a test. Those go to the store, not the target's
    // tree (the per-test runner overrides this for each test's own).
    env.push((
        "LLVM_PROFILE_FILE".into(),
        cache
            .join("other")
            .join("%p-%m.profraw")
            .display()
            .to_string(),
    ));
    note(&format!(
        "coverage: listing tests (instrumented build into {})",
        cov_target.display()
    ));
    let mut listed = nextest::list(ws, scope.filter(), &env)?;
    if let Scope::Auto { files, ids } = scope {
        let mut loc = crate::record::Locator::new(ws);
        listed.retain(|l| {
            ids.contains(&l.id) || loc.locate(&l.id).is_some_and(|(f, _)| files.contains(&f))
        });
    }
    let mut have: HashMap<TestId, Covered> = HashMap::new();
    let mut missing = vec![];
    for l in &listed {
        let f = cache.join(format!("{}.json", test_key(&l.id.binary_id, &l.id.name)));
        match std::fs::read(&f)
            .ok()
            .and_then(|b| serde_json::from_slice::<Covered>(&b).ok())
        {
            Some(c) => {
                have.insert(l.id.clone(), c);
            }
            None => missing.push(l.clone()),
        }
    }
    note(&format!(
        "coverage: {} tests, {} cached, {} to run (tree {tree})",
        listed.len(),
        have.len(),
        missing.len()
    ));
    if missing.is_empty() {
        return Ok(have);
    }
    let raw = cache.join(format!("raw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&raw);
    let _ = std::fs::remove_dir_all(cache.join("other"));
    std::fs::create_dir_all(&raw)?;
    let exe = std::env::current_exe()?;
    let triple = host_triple()?.to_uppercase().replace('-', "_");
    let mut run_env = env.clone();
    run_env.push((
        format!("CARGO_TARGET_{triple}_RUNNER"),
        format!("{} __cov-runner", exe.display()),
    ));
    run_env.push(("TESTOPT_COV_RAW".into(), raw.display().to_string()));
    let filter = missing
        .iter()
        .map(|l| format!("({})", l.id.filter()))
        .collect::<Vec<_>>()
        .join(" | ");
    let began = std::time::SystemTime::now();
    let out = nextest::run(
        ws,
        &Spec {
            profile: profile.to_string(),
            filter: Some(filter),
            env: run_env,
            log: Some(raw.join("nextest.log")),
            ..Default::default()
        },
    )?;
    // A process that dropped LLVM_PROFILE_FILE (started with a cleared
    // environment) wrote `default_*.profraw` where it ran: no test can be
    // told from it. It is moved out of the target's tree and counted, so
    // the loss is said, not silent.
    let unattributed = sweep_profiles(&ws.root, began, &cache.join("unattributed"))?;
    if unattributed > 0 {
        note(&format!(
            "coverage: {unattributed} profile(s) written by processes that dropped LLVM_PROFILE_FILE; their lines count for no test"
        ));
    }
    let prior = unattributed_of(&cache);
    write_json(&cache.join("unattributed.json"), &(prior + unattributed))?;
    let status: HashMap<TestId, String> = out
        .tests
        .iter()
        .map(|t| (t.id(), t.status.clone()))
        .collect();
    note(&format!(
        "coverage: ran {} tests in {:.1}s; exporting each test's lines",
        out.tests.len(),
        out.wall_s
    ));

    let profdata = llvm_tool(ws, "llvm-profdata")?;
    let llvm_cov = llvm_tool(ws, "llvm-cov")?;
    // Binaries the tests spawn: every bin target of the workspace, built
    // into the coverage target dir.
    let mut bins: Vec<PathBuf> = vec![];
    for p in &ws.packages {
        for t in p.targets.iter().filter(|t| t.kind == "bin") {
            let b = cov_target.join("debug").join(&t.name);
            if b.exists() {
                bins.push(b);
            }
        }
    }
    let test_files = TestCode::new(ws);
    let jobs: Vec<&nextest::Listed> = missing.iter().collect();
    let results = std::sync::Mutex::new(vec![]);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = crate::host::cores().clamp(1, 6);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(l) = jobs.get(i) else { break };
                    let dir = raw.join(test_key(&l.id.binary_id, &l.id.name));
                    let r = export_one(
                        &dir,
                        &profdata,
                        &llvm_cov,
                        &l.binary_path,
                        &bins,
                        ws,
                        &test_files,
                    );
                    results.lock().unwrap().push((l.id.clone(), r));
                }
            });
        }
    });
    for (id, r) in results.into_inner().unwrap() {
        let (lines, branches) = r.with_context(|| format!("coverage of {id}"))?;
        let c = Covered {
            test: id.clone(),
            tree: tree.clone(),
            status: status.get(&id).cloned().unwrap_or("missing".into()),
            lines,
            branches,
        };
        write_json(
            &cache.join(format!("{}.json", test_key(&id.binary_id, &id.name))),
            &c,
        )?;
        have.insert(id, c);
    }
    let _ = std::fs::remove_dir_all(&raw);
    Ok(have)
}

fn export_one(
    dir: &Path,
    profdata: &Path,
    llvm_cov: &Path,
    binary: &Path,
    bins: &[PathBuf],
    ws: &Workspace,
    test_code: &TestCode,
) -> Result<(Lines, Lines)> {
    let raws: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "profraw"))
                .collect()
        })
        .unwrap_or_default();
    if raws.is_empty() {
        return Ok((Lines::new(), Lines::new()));
    }
    let merged = dir.join("merged.profdata");
    let st = Command::new(profdata)
        .args(["merge", "-sparse", "-o"])
        .arg(&merged)
        .args(&raws)
        .output()?;
    if !st.status.success() {
        bail!(
            "llvm-profdata merge: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }
    let mut cmd = Command::new(llvm_cov);
    cmd.args(["export", "-format=lcov", "-skip-functions"])
        .arg(format!("-instr-profile={}", merged.display()))
        .arg("-ignore-filename-regex=(/\\.cargo/registry/|/\\.cargo/git/|/rustc/|/\\.rustup/)")
        .arg(binary);
    for b in bins {
        cmd.arg("-object").arg(b);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("llvm-cov export: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(parse_lcov(
        &String::from_utf8_lossy(&out.stdout),
        ws,
        test_code,
    ))
}

/// Which lines of the workspace are test code.
pub struct TestCode {
    root: PathBuf,
    ranges: std::sync::Mutex<HashMap<String, Vec<(usize, usize)>>>,
}

impl TestCode {
    pub fn new(ws: &Workspace) -> TestCode {
        TestCode {
            root: ws.root.clone(),
            ranges: Default::default(),
        }
    }

    pub fn is_test(&self, rel: &str, line: u32) -> bool {
        if Path::new(rel).components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some("tests" | "benches" | "examples")
            )
        }) {
            return true;
        }
        let mut cache = self.ranges.lock().unwrap();
        let ranges = cache
            .entry(rel.to_string())
            .or_insert_with(|| test_code_lines(&self.root.join(rel)));
        ranges
            .iter()
            .any(|(a, b)| (*a..=*b).contains(&(line as usize)))
    }
}

pub fn parse_lcov(text: &str, ws: &Workspace, test_code: &TestCode) -> (Lines, Lines) {
    let mut out = Lines::new();
    let mut arms = Lines::new();
    let mut file: Option<String> = None;
    for line in text.lines() {
        if let Some(f) = line.strip_prefix("SF:") {
            let p = Path::new(f);
            file = p
                .strip_prefix(&ws.root)
                .ok()
                .map(|r| r.display().to_string());
        } else if let Some(da) = line.strip_prefix("DA:") {
            let Some(f) = &file else { continue };
            let mut parts = da.split(',');
            let (Some(l), Some(c)) = (parts.next(), parts.next()) else {
                continue;
            };
            let (Ok(l), Ok(c)) = (l.parse::<u32>(), c.parse::<u64>()) else {
                continue;
            };
            if c > 0 && !test_code.is_test(f, l) {
                out.entry(f.clone()).or_default().insert(l);
            }
        } else if let Some(br) = line.strip_prefix("BRDA:") {
            // BRDA:line,block,branch,taken (taken is `-` for never reached)
            let Some(f) = &file else { continue };
            let p: Vec<&str> = br.split(',').collect();
            let [l, b, n, taken] = p.as_slice() else {
                continue;
            };
            let (Ok(l), Ok(b), Ok(n)) = (l.parse::<u32>(), b.parse::<u32>(), n.parse::<u32>())
            else {
                continue;
            };
            if taken.parse::<u64>().is_ok_and(|t| t > 0) && !test_code.is_test(f, l) {
                arms.entry(f.clone()).or_default().insert(arm(l, b, n));
            }
        } else if line == "end_of_record" {
            file = None;
        }
    }
    out.retain(|_, v| !v.is_empty());
    arms.retain(|_, v| !v.is_empty());
    (out, arms)
}

fn intersect(a: &Lines, b: &Lines) -> usize {
    a.iter()
        .map(|(f, la)| b.get(f).map(|lb| la.intersection(lb).count()).unwrap_or(0))
        .sum()
}

fn minus(a: &Lines, b: &Lines) -> Lines {
    let mut out = Lines::new();
    for (f, la) in a {
        let d: BTreeSet<u32> = match b.get(f) {
            Some(lb) => la.difference(lb).cloned().collect(),
            None => la.clone(),
        };
        if !d.is_empty() {
            out.insert(f.clone(), d);
        }
    }
    out
}

fn sample(l: &Lines, n: usize) -> Vec<String> {
    let mut out = vec![];
    for (f, ls) in l {
        // Runs of consecutive lines, as `file:a-b`.
        let mut it = ls.iter().peekable();
        while let Some(&a) = it.next() {
            let mut b = a;
            while it.peek().is_some_and(|&&n| n == b + 1) {
                b = *it.next().unwrap();
            }
            out.push(if a == b {
                format!("{f}:{a}")
            } else {
                format!("{f}:{a}-{b}")
            });
            if out.len() >= n {
                return out;
            }
        }
    }
    out
}

pub fn cov_target(ws: &Workspace) -> PathBuf {
    ws.target_dir.join("testopt-cov")
}

/// `covers <test>`: the test's lines, and every other measured test's overlap.
pub fn covers(
    ws: &Workspace,
    store: &Store,
    id: &TestId,
    against: Option<&str>,
    peers: Option<&str>,
    profile: &str,
) -> Result<Value> {
    // `--peers binary`: every test of its binary; `--peers file`: every
    // test whose function is in its file. Both beside `--against`.
    let mut filter = id.filter();
    if let Some(a) = against {
        filter = format!("({filter}) | ({a})");
    }
    if peers == Some("binary") {
        filter = format!("({filter}) | binary_id(={})", id.binary_id);
    }
    let scope = match peers {
        Some("file") => {
            let (file, _) = crate::record::Locator::new(ws)
                .locate(id)
                .ok_or_else(|| anyhow::anyhow!("cannot find the file of {id}"))?;
            if against.is_some() {
                bail!(
                    "--peers file and --against do not go together; use --peers binary or -E-style --against"
                );
            }
            Scope::Auto {
                files: [file].into(),
                ids: [id.clone()].into(),
            }
        }
        None | Some("binary") => Scope::Filter(Some(&filter)),
        Some(other) => bail!("--peers is file or binary, not {other}"),
    };
    let note = |m: &str| eprintln!("[testopt] {m}");
    let started = std::time::Instant::now();
    let mut all = collect(ws, store, &scope, profile, &cov_target(ws), &note)?;
    // Any other test measured on this same tree counts too.
    let cache = cache_dir(store, &tree_key(ws));
    for e in std::fs::read_dir(&cache).into_iter().flatten().flatten() {
        if let Some(c) = std::fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Covered>(&b).ok())
        {
            all.entry(c.test.clone()).or_insert(c);
        }
    }
    let Some(me) = all.get(id).cloned() else {
        bail!("no coverage measured for {id}")
    };
    let mine = &me.lines;
    let n = count(mine);
    let my_arms = &me.branches;
    let n_arms = count(my_arms);
    let mut others = vec![];
    let mut union_others = Lines::new();
    let mut union_arms = Lines::new();
    for (oid, c) in &all {
        if oid == id {
            continue;
        }
        for (f, ls) in &c.lines {
            union_others.entry(f.clone()).or_default().extend(ls);
        }
        for (f, a) in &c.branches {
            union_arms.entry(f.clone()).or_default().extend(a);
        }
        let shared = intersect(mine, &c.lines);
        if shared == 0 {
            continue;
        }
        let theirs = count(&c.lines);
        // Within another only when its lines and its branch arms both are:
        // one line taken down a different arm is not the same coverage.
        let shared_arms = intersect(my_arms, &c.branches);
        let their_arms = count(&c.branches);
        others.push(json!({
            "test": oid.to_string(),
            "lines": theirs,
            "shared": shared,
            "share_of_this": round(shared as f64 / n.max(1) as f64),
            "branch_arms": their_arms,
            "shared_arms": shared_arms,
            "this_within_it": shared == n && shared_arms == n_arms,
            "it_within_this": shared == theirs && shared_arms == their_arms,
        }));
    }
    let unique_arms = minus(my_arms, &union_arms);
    others.sort_by(|a, b| b["shared"].as_u64().cmp(&a["shared"].as_u64()));
    let unique = minus(mine, &union_others);
    let by_file: Vec<Value> = mine
        .iter()
        .map(|(f, ls)| json!({"file": f, "lines": ls.len()}))
        .collect();
    Ok(json!({
        "test": id.to_string(),
        "status": me.status,
        "tree": me.tree,
        "lines": n,
        "files": by_file,
        "compared_with": all.len() - 1,
        "covered_by_no_other": count(&unique),
        "covered_by_no_other_sample": sample(&unique, 15),
        "branch_arms": n_arms,
        "arms_taken_by_no_other": count(&unique_arms),
        "arms_taken_by_no_other_sample": sample_arms(&unique_arms, 15),
        "unattributed_profiles": unattributed_of(&cache),
        "others": others,
        "seconds": round(started.elapsed().as_secs_f64()),
    }))
}

/// For each file, base line → head line, for lines the diff leaves alone.
/// Lines it removes or changes map to nothing.
pub struct LineMap {
    /// file → sorted (base_start, head_start, len) runs of unchanged lines
    /// are implied by hunks: here, hunks as (base_start, base_len, head_start, head_len).
    hunks: HashMap<String, Vec<(u32, u32, u32, u32)>>,
    deleted_files: BTreeSet<String>,
    renamed: HashMap<String, String>,
}

impl LineMap {
    /// From `git diff <base>` in `root` (the working tree against base).
    pub fn from_git(root: &Path, base: &str) -> Result<LineMap> {
        let out = Command::new("git")
            .args([
                "diff",
                "--no-ext-diff",
                "--no-color",
                "-U0",
                "-M",
                base,
                "--",
            ])
            .current_dir(root)
            .output()?;
        if !out.status.success() {
            bail!("git diff {base}: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(LineMap::parse(&String::from_utf8_lossy(&out.stdout)))
    }

    pub fn parse(diff: &str) -> LineMap {
        let mut hunks: HashMap<String, Vec<(u32, u32, u32, u32)>> = HashMap::new();
        let mut deleted_files = BTreeSet::new();
        let mut renamed = HashMap::new();
        let mut old: Option<String> = None;
        for line in diff.lines() {
            if let Some(p) = line.strip_prefix("--- ") {
                old = p.strip_prefix("a/").map(str::to_string);
            } else if let Some(p) = line.strip_prefix("+++ ") {
                let new = p.strip_prefix("b/").map(str::to_string);
                match (&old, &new) {
                    (Some(o), None) => {
                        deleted_files.insert(o.clone());
                    }
                    (Some(o), Some(n)) if o != n => {
                        renamed.insert(o.clone(), n.clone());
                    }
                    _ => {}
                }
            } else if let Some(h) = line.strip_prefix("@@ ") {
                let Some(o) = &old else { continue };
                let mut parts = h.split_whitespace();
                let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let range = |s: &str| -> (u32, u32) {
                    let s = &s[1..];
                    match s.split_once(',') {
                        Some((x, y)) => (x.parse().unwrap_or(0), y.parse().unwrap_or(0)),
                        None => (s.parse().unwrap_or(0), 1),
                    }
                };
                let (bs, bl) = range(a);
                let (hs, hl) = range(b);
                hunks.entry(o.clone()).or_default().push((bs, bl, hs, hl));
            }
        }
        LineMap {
            hunks,
            deleted_files,
            renamed,
        }
    }

    /// The head file and line of base `file:line`, if the diff kept it.
    pub fn map(&self, file: &str, line: u32) -> Option<(String, u32)> {
        if self.deleted_files.contains(file) {
            return None;
        }
        let head_file = self
            .renamed
            .get(file)
            .cloned()
            .unwrap_or_else(|| file.to_string());
        let mut shift: i64 = 0;
        for &(bs, bl, _hs, hl) in self.hunks.get(file).map(|v| v.as_slice()).unwrap_or(&[]) {
            // With -U0, a pure insertion has bl == 0 and sits after line bs.
            let (lo, hi) = if bl == 0 {
                (bs + 1, bs)
            } else {
                (bs, bs + bl - 1)
            };
            if bl > 0 && line >= lo && line <= hi {
                return None;
            }
            if line > hi || (bl == 0 && line > bs) {
                shift += hl as i64 - bl as i64;
            }
        }
        Some((head_file, (line as i64 + shift) as u32))
    }
}

/// A clean copy of `commit`'s tree, from `git archive` (the target repo is
/// not written to: no worktree, no checkout).
fn base_tree(store: &Store, root: &Path, commit: &str) -> Result<PathBuf> {
    let dir = store.dir.join("src").join(commit);
    if dir.join(".testopt-complete").exists() {
        return Ok(dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let archive = Command::new("git")
        .args(["archive", "--format=tar", commit])
        .current_dir(root)
        .output()?;
    if !archive.status.success() {
        bail!(
            "git archive {commit}: {}",
            String::from_utf8_lossy(&archive.stderr)
        );
    }
    let mut tar = Command::new("tar")
        .args(["-x", "-f", "-", "-C"])
        .arg(&dir)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    std::io::Write::write_all(tar.stdin.as_mut().unwrap(), &archive.stdout)?;
    drop(tar.stdin.take());
    if !tar.wait()?.success() {
        bail!("extracting {commit}");
    }
    std::fs::write(dir.join(".testopt-complete"), "")?;
    Ok(dir)
}

fn union(all: &HashMap<TestId, Covered>) -> Lines {
    let mut u = Lines::new();
    for c in all.values() {
        for (f, ls) in &c.lines {
            u.entry(f.clone()).or_default().extend(ls);
        }
    }
    u
}

pub struct CheckOpts<'a> {
    pub base: &'a str,
    pub filter: Option<String>,
    pub profile: &'a str,
    pub runs: usize,
    pub threshold: f64,
}

/// `check <base>`: the working tree against base. Fails when a line some
/// test covered at base, and still in the code, is covered by none now.
pub fn check(ws: &Workspace, store: &Store, o: &CheckOpts, note: &dyn Fn(&str)) -> Result<Value> {
    let started = std::time::Instant::now();
    let rev = Command::new("git")
        .args(["rev-parse", "--verify", &format!("{}^{{commit}}", o.base)])
        .current_dir(&ws.root)
        .output()?;
    if !rev.status.success() {
        bail!("{} is not a commit here", o.base);
    }
    let commit = String::from_utf8_lossy(&rev.stdout).trim().to_string();
    let base_dir = base_tree(store, &ws.root, &commit)?;
    // The base copy sits in the store; its paths say where the workspace
    // root is within it.
    let rel_root = {
        let top = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&ws.root)
            .output()?;
        let top = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim());
        ws.root
            .strip_prefix(&top)
            .map(Path::to_path_buf)
            .unwrap_or_default()
    };
    let base_ws = Workspace::find(&base_dir.join(&rel_root))?;
    // The base builds into target dirs of its own: cargo names a workspace
    // crate's artifacts the same whatever its path, so a shared dir would
    // find one tree's build fresh for the other.
    let cov = cov_target(ws);
    let cov_base = ws.target_dir.join("testopt-cov-base");
    let plain_base = ws.target_dir.join("testopt-base");
    note(&format!(
        "check: base {commit} copied to {}",
        base_ws.root.display()
    ));
    let (scope, unknown) = match &o.filter {
        Some(f) => (Scope::Filter(Some(f.as_str())), vec![]),
        None => auto_scope(ws, store, &base_ws, &commit, note)?,
    };
    let base_cov = collect(&base_ws, store, &scope, o.profile, &cov_base, note)?;
    note("check: base coverage done; now the working tree");
    let head_cov = collect(ws, store, &scope, o.profile, &cov, note)?;
    let base_u = union(&base_cov);
    let head_u = union(&head_cov);
    let map = LineMap::from_git(&ws.root, &commit)?;
    let prefix = |f: &str| -> String {
        if rel_root.as_os_str().is_empty() {
            f.to_string()
        } else {
            format!("{}/{f}", rel_root.display())
        }
    };
    let unprefix = |f: &str| -> String {
        if rel_root.as_os_str().is_empty() {
            f.to_string()
        } else {
            f.strip_prefix(&format!("{}/", rel_root.display()))
                .unwrap_or(f)
                .to_string()
        }
    };
    let mut lost = Lines::new();
    let mut gone = 0usize;
    for (f, ls) in &base_u {
        for &l in ls {
            match map.map(&prefix(f), l) {
                Some((hf, hl)) => {
                    let hf = unprefix(&hf);
                    if !head_u.get(&hf).is_some_and(|s| s.contains(&hl)) {
                        lost.entry(hf).or_default().insert(hl);
                    }
                }
                None => gone += 1,
            }
        }
    }
    // Branch arms the same way: an arm taken at base, on a line the diff
    // kept, must be taken by some test now.
    let base_arms = union_arms(&base_cov);
    let head_arms = union_arms(&head_cov);
    let mut lost_arms = Lines::new();
    for (f, units) in &base_arms {
        for &u in units {
            if let Some((hf, hl)) = map.map(&prefix(f), arm_line(u)) {
                let hf = unprefix(&hf);
                let moved = (hl << 8) | (u & 0xff);
                if !head_arms.get(&hf).is_some_and(|s| s.contains(&moved)) {
                    lost_arms.entry(hf).or_default().insert(moved);
                }
            }
        }
    }
    // Which base tests covered each lost line (by its base position).
    let mut lost_detail = vec![];
    for (f, ls) in &base_u {
        for &l in ls {
            if let Some((hf, hl)) = map.map(&prefix(f), l)
                && lost.get(&unprefix(&hf)).is_some_and(|s| s.contains(&hl))
            {
                let by: Vec<String> = base_cov
                    .values()
                    .filter(|c| c.lines.get(f).is_some_and(|s| s.contains(&l)))
                    .map(|c| c.test.to_string())
                    .collect();
                if lost_detail.len() < 30 {
                    lost_detail.push(json!({"at": format!("{}:{hl}", unprefix(&hf)), "base_at": format!("{f}:{l}"), "covered_at_base_by": by}));
                }
            }
        }
    }

    // Touched tests: gone, new, or with a different body.
    let base_tests: BTreeSet<TestId> = base_cov.keys().cloned().collect();
    let head_tests: BTreeSet<TestId> = head_cov.keys().cloned().collect();
    let deleted: Vec<&TestId> = base_tests.difference(&head_tests).collect();
    let added: Vec<&TestId> = head_tests.difference(&base_tests).collect();
    let mut changed = vec![];
    let mut base_src: HashMap<String, Index> = HashMap::new();
    let mut head_src: HashMap<String, Index> = HashMap::new();
    for id in base_tests.intersection(&head_tests) {
        let b = base_src
            .entry(id.binary_id.clone())
            .or_insert_with(|| Index::of_binary(&base_ws, &id.binary_id));
        let h = head_src
            .entry(id.binary_id.clone())
            .or_insert_with(|| Index::of_binary(ws, &id.binary_id));
        // Compared as tokens: a change of layout alone is no change.
        let body = |s: &Index| s.test_fn(id).map(Index::body_tokens);
        if body(b) != body(h) {
            changed.push(id.clone());
        }
    }
    let plain_base_env = vec![(
        "CARGO_TARGET_DIR".to_string(),
        plain_base.display().to_string(),
    )];
    let shown = |t: &crate::probe::Timing| -> Value {
        json!({"median_s": t.median_s, "spread_s": t.spread_s, "runs": t.runs, "cpu_median_s": t.cpu_median_s, "reliable": t.reliable, "max_load_per_core": t.max_load_per_core})
    };
    let times_of = |w: &Workspace, env: &[(String, String)], id: &TestId| -> Option<Value> {
        crate::probe::time_runs(w, o.profile, id, o.runs, o.threshold, env)
            .map(|t| shown(&t))
            .map_err(|e| note(&format!("timing {id}: {e:#}")))
            .ok()
    };
    let mut timed = vec![];
    for id in &changed {
        note(&format!(
            "check: timing changed test {id}, base and working tree interleaved"
        ));
        let base_side = crate::probe::Side {
            ws: &base_ws,
            env: &plain_base_env,
        };
        let head_side = crate::probe::Side { ws, env: &[] };
        match crate::probe::time_pair(&base_side, &head_side, o.profile, id, o.runs, o.threshold) {
            Ok((b, h)) => timed.push(
                json!({"test": id.to_string(), "change": "changed", "interleaved": true,
                "base": shown(&b), "head": shown(&h), "delta_s": round(h.median_s - b.median_s),
                "cpu_delta_s": round(h.cpu_median_s - b.cpu_median_s)}),
            ),
            Err(e) => {
                note(&format!("timing {id}: {e:#}"));
                timed.push(json!({"test": id.to_string(), "change": "changed", "base": null, "head": null, "delta_s": null}));
            }
        }
    }
    for id in &deleted {
        note(&format!("check: timing deleted test {id} at base"));
        let before = times_of(&base_ws, &plain_base_env, id);
        let delta = before
            .as_ref()
            .map(|b| round(-b["median_s"].as_f64().unwrap_or(0.0)));
        timed.push(json!({"test": id.to_string(), "change": "deleted", "base": before, "head": null, "delta_s": delta}));
    }
    for id in &added {
        note(&format!("check: timing new test {id}"));
        let after = times_of(ws, &[], id);
        let delta = after
            .as_ref()
            .map(|a| round(a["median_s"].as_f64().unwrap_or(0.0)));
        timed.push(json!({"test": id.to_string(), "change": "added", "base": null, "head": after, "delta_s": delta}));
    }
    let total_delta: f64 = timed.iter().filter_map(|t| t["delta_s"].as_f64()).sum();
    let pass = lost.is_empty() && lost_arms.is_empty();
    Ok(json!({
        "pass": pass,
        "verdict": if pass { "pass: every line and branch arm covered at base and still in the code is covered" } else { "FAIL: lines or branch arms covered at base are covered by no test now" },
        "base": commit,
        "scope": match &scope {
            Scope::Filter(Some(f)) => json!(if f.len() > 200 { format!("{}…", &f[..200]) } else { f.to_string() }),
            Scope::Filter(None) => json!("all tests"),
            Scope::Auto { files, ids } => json!({"auto": true, "test_files": files, "covering_tests": ids.len()}),
        },
        "scope_unknown_for": unknown,
        "coverage": {
            "base_tests": base_cov.len(),
            "head_tests": head_cov.len(),
            "base_lines": count(&base_u),
            "head_lines": count(&head_u),
            "base_lines_removed_by_the_change": gone,
            "lost_lines": count(&lost),
            "lost": sample(&lost, 30),
            "lost_detail": lost_detail,
            "base_branch_arms": count(&base_arms),
            "head_branch_arms": count(&head_arms),
            "lost_arms": count(&lost_arms),
            "lost_arms_sample": sample_arms(&lost_arms, 30),
            "unattributed_profiles": unattributed_of(&cache_dir(store, &tree_key(ws))),
        },
        "tests": {
            "deleted": deleted.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            "added": added.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            "changed": changed.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            "timing": timed,
            "total_delta_s": round(total_delta),
        },
        "seconds": round(started.elapsed().as_secs_f64()),
    }))
}

/// Moves every `.profraw` under `root` (but `target`) written since `since`
/// into `dest`; how many there were.
fn sweep_profiles(root: &Path, since: std::time::SystemTime, dest: &Path) -> Result<usize> {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            if p.is_dir() {
                if name != "target" && name != ".git" {
                    stack.push(p);
                }
                continue;
            }
            let fresh = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m >= since);
            if p.extension().is_some_and(|x| x == "profraw") && fresh {
                std::fs::create_dir_all(dest)?;
                std::fs::rename(&p, dest.join(&name))?;
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Profiles of tree `cache` no test could be told from, over its runs.
fn unattributed_of(cache: &Path) -> usize {
    std::fs::read(cache.join("unattributed.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(0)
}

/// Branch arms as `file:line(arm N)`.
fn sample_arms(arms: &Lines, n: usize) -> Vec<String> {
    arms.iter()
        .flat_map(|(f, units)| {
            units
                .iter()
                .map(move |u| format!("{f}:{} (arm {})", arm_line(*u), u & 0xff))
        })
        .take(n)
        .collect()
}

fn union_arms(all: &HashMap<TestId, Covered>) -> Lines {
    let mut u = Lines::new();
    for c in all.values() {
        for (f, a) in &c.branches {
            u.entry(f.clone()).or_default().extend(a);
        }
    }
    u
}

/// The scope `check` picks itself, from the diff against `commit`: tests
/// whose function is in a changed file (each side located in its own
/// tree), and tests that covered a changed file of code under test in any
/// coverage measured on the base tree before. A changed file no cached
/// coverage reaches is named: the scope may miss its tests.
fn auto_scope<'a>(
    ws: &Workspace,
    store: &Store,
    base_ws: &Workspace,
    commit: &str,
    note: &dyn Fn(&str),
) -> Result<(Scope<'a>, Vec<String>)> {
    let git = |args: &[&str]| -> Result<Vec<String>> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&ws.root)
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    };
    // `--relative`: paths relative to the workspace root, as the scope's are.
    let mut changed: BTreeSet<String> = BTreeSet::new();
    for p in git(&["diff", "--name-only", "--relative", commit, "--"])?
        .into_iter()
        .chain(git(&["ls-files", "--others", "--exclude-standard"])?)
    {
        if p.ends_with(".rs") {
            changed.insert(p);
        }
    }
    let is_test_file = |f: &str| {
        f.split('/')
            .any(|c| matches!(c, "tests" | "benches" | "examples"))
    };
    let mut ids = BTreeSet::new();
    let mut unknown = vec![];
    let cached: Vec<Covered> = std::fs::read_dir(cache_dir(store, &tree_key(base_ws)))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            std::fs::read(e.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<Covered>(&b).ok())
        })
        .collect();
    for f in &changed {
        let mut reached = false;
        for c in &cached {
            if c.lines.contains_key(f) {
                ids.insert(c.test.clone());
                reached = true;
            }
        }
        // Test code (a tests/ file, or a file holding test modules) is
        // scoped by where tests are; code under test only by coverage.
        let holds_tests = is_test_file(f)
            || source_holds_tests(&ws.root.join(f))
            || source_holds_tests(&base_ws.root.join(f));
        if !reached && !holds_tests {
            unknown.push(f.clone());
        }
    }
    note(&format!(
        "check: auto scope: tests in {} changed file(s), {} test(s) that covered changed code at base{}",
        changed.len(),
        ids.len(),
        if unknown.is_empty() {
            String::new()
        } else {
            format!(
                "; no coverage cached for {} (give -E to widen)",
                unknown.join(", ")
            )
        }
    ));
    Ok((
        Scope::Auto {
            files: changed,
            ids,
        },
        unknown,
    ))
}

fn source_holds_tests(path: &Path) -> bool {
    !test_code_lines(path).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_lines_across_a_diff() {
        let diff = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -3,2 +2,0 @@\n-a\n-b\n@@ -10,0 +9,3 @@\n+c\n+d\n+e\n";
        let m = LineMap::parse(diff);
        assert_eq!(m.map("x.rs", 1), Some(("x.rs".into(), 1)));
        assert_eq!(m.map("x.rs", 3), None);
        assert_eq!(m.map("x.rs", 4), None);
        assert_eq!(m.map("x.rs", 5), Some(("x.rs".into(), 3)));
        assert_eq!(m.map("x.rs", 10), Some(("x.rs".into(), 8)));
        assert_eq!(m.map("x.rs", 11), Some(("x.rs".into(), 12)));
        assert_eq!(m.map("y.rs", 7), Some(("y.rs".into(), 7)));
    }

    #[test]
    fn unquotes_sh() {
        assert_eq!(unquote("'a b'"), "a b");
        assert_eq!(unquote("'it'\\''s'"), "it's");
    }
}
