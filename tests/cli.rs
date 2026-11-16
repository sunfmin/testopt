//! testopt run against a copy of tests/fixture: a fast test, one that
//! sleeps (in a helper), one that spawns processes, and two whose coverage
//! nests (`classifies_a_positive` ⊂ `classifies_every_sign`).

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let name = e.file_name();
        if name == "target" || name == "Cargo.lock" {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(&name));
        } else {
            std::fs::copy(&p, to.join(&name)).unwrap();
        }
    }
}

fn git(root: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&st.stderr)
    );
}

impl Fixture {
    /// A copy of the fixture in its own git repository, with its own store.
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture"),
            &root,
        );
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        let home = dir.path().join("home");
        Fixture {
            _dir: dir,
            root,
            home,
        }
    }

    fn run(&self, args: &[&str]) -> (Value, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_testopt"))
            .args(["-C", self.root.to_str().unwrap()])
            .args(args)
            .env("TESTOPT_HOME", &self.home)
            // These tests run under nextest, which says its own threads here.
            .env_remove("NEXTEST_TEST_THREADS")
            .output()
            .unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "testopt {args:?}: {e}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (v, out.status.code().unwrap_or(-1))
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (v, code) = self.run(args);
        assert_eq!(code, 0, "testopt {args:?}: {v:#}");
        v
    }

    fn delete_test(&self, name: &str) {
        let f = self.root.join("tests/it.rs");
        let src = std::fs::read_to_string(&f).unwrap();
        let start = src
            .find(&format!("#[test]\nfn {name}()"))
            .expect("test present");
        let end = start + src[start..].find("\n}\n").unwrap() + 3;
        std::fs::write(&f, format!("{}{}", &src[..start], &src[end..])).unwrap();
    }
}

/// Coverage leaves nothing in the target's tree: no stray `.profraw`.
fn assert_no_profiles(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                if e.file_name() != "target" {
                    stack.push(p);
                }
            } else {
                assert!(
                    p.extension().is_none_or(|x| x != "profraw"),
                    "left in the workspace: {}",
                    p.display()
                );
            }
        }
    }
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

#[test]
fn record_keeps_every_test_and_rank_reproduces_its_totals() {
    let fx = Fixture::new();
    let rec = fx.ok(&["record", "--test-threads", "4"]);
    assert_eq!(rec["tests"], 5, "{rec:#}");
    assert_eq!(rec["threads"], 4);
    assert!(
        rec["nextest_summary"]
            .as_str()
            .unwrap()
            .contains("5 tests run"),
        "{rec:#}"
    );
    let rank = fx.ok(&["rank"]);
    assert_eq!(rank["totals"]["tests"], 5);
    assert_eq!(f(&rank["totals"]["test_seconds"]), f(&rec["test_seconds"]));
    assert_eq!(rank["tests"][0]["name"], "sleeps", "{rank:#}");
    assert!(f(&rank["tests"][0]["seconds"]) >= 1.5);
    let share: f64 = rank["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| f(&t["share"]))
        .sum();
    assert!((share - 1.0).abs() < 0.01, "shares sum to {share}");
    assert_eq!(
        f(&rank["tests"][0]["est_wall_saving_s"]),
        (f(&rank["tests"][0]["seconds"]) / 4.0 * 1000.0).round() / 1000.0
    );
    assert_eq!(rank["tests"][0]["at"], "tests/it.rs:10");
    assert_eq!(rank["files"][0]["file"], "tests/it.rs");
    // The replayed schedule: 5 tests on 4 slots end with the 1.5s sleep;
    // without it the run ends with the next longest, so it saves the gap.
    assert_eq!(rank["totals"]["replayed_from"], "JUnit start order");
    let replayed = f(&rank["totals"]["replayed_wall_s"]);
    assert!(
        replayed >= 1.5 && replayed <= f(&rank["totals"]["test_seconds"]),
        "{rank:#}"
    );
    let saving = f(&rank["tests"][0]["replayed_wall_saving_s"]);
    assert!(saving > 0.0 && saving < 1.5, "{rank:#}");
    assert_eq!(
        f(&rank["tests"][4]["replayed_wall_saving_s"]),
        0.0,
        "{rank:#}"
    );

    // Under a profile of the repo's own: its thread count, its settings.
    let slow = fx.ok(&["record", "--profile", "slow", "-E", "test(fast_adds)"]);
    assert_eq!(slow["threads"], 3, "{slow:#}");
    assert_eq!(slow["tests"], 1, "{slow:#}");

    // Later runs give a trend and each test's previous time.
    fx.ok(&["record", "-E", "test(sleeps)"]);
    let rank = fx.ok(&["rank"]);
    assert_eq!(rank["trend"].as_array().unwrap().len(), 3);
    assert!(rank["tests"][0]["previous_s"].is_number(), "{rank:#}");
}

#[test]
fn why_finds_a_fixed_wait_in_a_helper_and_counts_spawned_processes() {
    let fx = Fixture::new();
    let w = fx.ok(&["why", "sleeps"]);
    assert_eq!(w["kind"], "fixed-wait", "{w:#}");
    let wait = &w["static"]["waits"][0];
    assert_eq!(wait["at"], "tests/it.rs:17", "{w:#}");
    assert_eq!(f(&wait["seconds"]), 1.5);
    assert_eq!(wait["via"], serde_json::json!(["sleeps", "settle"]));
    assert_eq!(w["processes"]["spawned"], 0);

    let w = fx.ok(&["why", "fixture::it spawns_processes"]);
    assert_eq!(w["kind"], "process-heavy", "{w:#}");
    assert_eq!(w["processes"]["spawned"], 6, "{w:#}");
    // A range: at least what was seen, at most that plus what the host
    // started unseen.
    assert!(
        w["processes"]["spawned_at_most"].as_u64().unwrap() >= 6,
        "{w:#}"
    );
    assert_eq!(w["processes"]["by_program"][0]["program"], "sleep", "{w:#}");
    assert!(
        f(&w["breakdown"]["with_child_processes_alive_s"]) >= 1.0,
        "{w:#}"
    );
    // A sleep in a `for` repeats; it is not a poll.
    assert_eq!(w["static"]["waits"][0]["repeated"], true);
    assert_eq!(w["static"]["polls"], serde_json::json!([]));
}

#[test]
fn time_gives_median_and_spread_and_flags_a_loaded_host() {
    let fx = Fixture::new();
    let t = fx.ok(&["time", "sleeps", "-n", "3", "--max-load-per-core", "1000"]);
    assert_eq!(t["runs"].as_array().unwrap().len(), 3);
    assert!(f(&t["median_s"]) >= 1.5);
    assert!(f(&t["spread_s"]) >= 0.0);
    assert_eq!(t["reliable"], true);
    assert_eq!(t["loads"].as_array().unwrap().len(), 6);
    // Any load is past a threshold of zero: flagged, and refused with --strict.
    let (t, code) = fx.run(&[
        "time",
        "fast_adds",
        "-n",
        "1",
        "--max-load-per-core",
        "0",
        "--strict",
    ]);
    assert_eq!(t["reliable"], false);
    assert!(
        t["flag"].as_str().unwrap().starts_with("unreliable"),
        "{t:#}"
    );
    assert_eq!(code, 3);
}

#[test]
fn covers_finds_a_test_whose_lines_another_covers() {
    let fx = Fixture::new();
    let c = fx.ok(&[
        "covers",
        "classifies_a_positive",
        "--against",
        "test(classifies_)",
    ]);
    assert_eq!(c["files"][0]["file"], "src/lib.rs", "{c:#}");
    assert_eq!(c["covered_by_no_other"], 0, "{c:#}");
    let other = &c["others"][0];
    assert_eq!(other["test"], "fixture::it classifies_every_sign");
    assert_eq!(other["this_within_it"], true);
    assert_eq!(other["it_within_this"], false);

    let c = fx.ok(&["covers", "classifies_every_sign"]);
    // The other test was measured on the same tree: it is compared without
    // running again, and the negative and zero branches are this test's alone.
    assert_eq!(c["compared_with"], 1, "{c:#}");
    assert_eq!(c["covered_by_no_other"], 2, "{c:#}");
    assert_no_profiles(&fx.root);
}

#[test]
fn check_fails_when_a_line_loses_its_only_test_and_passes_otherwise() {
    let fx = Fixture::new();
    fx.delete_test("classifies_a_positive");
    let (c, code) = fx.run(&["check", "HEAD", "-E", "test(classifies_)", "-n", "1"]);
    assert_eq!(code, 0, "{c:#}");
    assert_eq!(c["pass"], true);
    assert_eq!(
        c["tests"]["deleted"],
        serde_json::json!(["fixture::it classifies_a_positive"])
    );
    assert!(
        f(&c["tests"]["timing"][0]["base"]["median_s"]) >= 0.0,
        "{c:#}"
    );

    git(&fx.root, &["checkout", "-q", "tests/it.rs"]);
    fx.delete_test("classifies_every_sign");
    let (c, code) = fx.run(&["check", "HEAD", "-E", "test(classifies_)", "-n", "1"]);
    assert_eq!(code, 1, "{c:#}");
    assert_eq!(c["pass"], false);
    assert_eq!(
        c["coverage"]["lost"],
        serde_json::json!(["src/lib.rs:10", "src/lib.rs:12"]),
        "{c:#}"
    );
    assert_eq!(
        c["coverage"]["lost_detail"][0]["covered_at_base_by"][0],
        "fixture::it classifies_every_sign"
    );
    assert_no_profiles(&fx.root);
}

#[test]
fn check_maps_lines_moved_by_an_edit_above_them() {
    let fx = Fixture::new();
    // Two lines added above `classify`: every covered line moves down, and
    // none is lost.
    let lib = fx.root.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(
        &lib,
        src.replace("pub fn classify", "// moved\n// down\npub fn classify"),
    )
    .unwrap();
    let c = fx.ok(&["check", "HEAD", "-E", "test(classifies_)", "-n", "1"]);
    assert_eq!(c["pass"], true, "{c:#}");
    assert_eq!(c["coverage"]["base_lines"], c["coverage"]["head_lines"]);
}

#[test]
fn ledger_keeps_entries_and_names_the_next_candidates() {
    let fx = Fixture::new();
    fx.ok(&["record"]);
    fx.ok(&[
        "ledger",
        "add",
        "--test",
        "fixture::it sleeps",
        "--kind",
        "fixed-wait",
        "--status",
        "kept",
        "--change",
        "settle waits on a condition",
        "--before",
        "1.5",
        "--after",
        "0.1",
        "--reason",
        "no lost lines",
    ]);
    fx.ok(&[
        "ledger",
        "add",
        "--test",
        "fixture::it spawns_processes",
        "--kind",
        "process-heavy",
        "--status",
        "rejected",
        "--reason",
        "needs the processes",
    ]);
    let l = fx.ok(&["ledger"]);
    assert_eq!(l["totals"]["kept"], 1);
    assert_eq!(l["totals"]["rejected"], 1);
    assert_eq!(f(&l["totals"]["kept_test_seconds_saved"]), 1.4);
    let next: Vec<&str> = l["next"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["test"].as_str().unwrap())
        .collect();
    assert!(
        !next.contains(&"fixture::it sleeps") && !next.contains(&"fixture::it spawns_processes"),
        "{next:?}"
    );
    assert_eq!(next.len(), 3);

    let out = Command::new(env!("CARGO_BIN_EXE_testopt"))
        .args(["-C", fx.root.to_str().unwrap(), "dash", "--once"])
        .env("TESTOPT_HOME", &fx.home)
        .output()
        .unwrap();
    let frame = String::from_utf8_lossy(&out.stdout);
    assert!(frame.contains("fixture::it sleeps"), "{frame}");
    assert!(frame.contains("1 kept, 1 rejected"), "{frame}");
    assert!(frame.contains("finished"), "{frame}");
}

/// testopt run from inside a tmux session never kills, replaces or fills
/// that session (on 2026-11-16 a `tmux kill-session -t testopt` run from
/// inside the agent's own session `testopt` ended the agent). On a private
/// tmux server, so no real session is touched.
#[test]
fn refuses_to_use_or_kill_the_tmux_session_it_runs_inside() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed: skipped");
        return;
    }
    let fx = Fixture::new();
    let dir = tempfile::Builder::new()
        .prefix("tx")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = dir.path().join("s");
    let tmux = |args: &[&str]| {
        Command::new("tmux")
            .arg("-S")
            .arg(&sock)
            .args(args)
            .output()
            .unwrap()
    };
    let exe = env!("CARGO_BIN_EXE_testopt");
    let d = dir.path().display();
    let ws = fx.root.display();
    let script = format!(
        "export TESTOPT_HOME={home}; \
         {exe} -C {ws} --tmux --tmux-session host record > {d}/use.json 2>&1; echo $? > {d}/use.code; \
         {exe} -C {ws} --tmux-session host tmux-kill > {d}/kill.json 2>&1; echo $? > {d}/kill.code; \
         {exe} -C {ws} --tmux-session other tmux-kill > {d}/other.json 2>&1; echo $? > {d}/other.code; \
         sleep 60",
        home = fx.home.display()
    );
    assert!(
        tmux(&["new-session", "-d", "-s", "other", "sleep 60"])
            .status
            .success()
    );
    assert!(
        tmux(&["new-session", "-d", "-s", "host", "sh", "-c", &script])
            .status
            .success()
    );
    let other_code = dir.path().join("other.code");
    for _ in 0..300 {
        if other_code.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let read = |f: &str| std::fs::read_to_string(dir.path().join(f)).unwrap_or_default();
    let host_alive = tmux(&["has-session", "-t", "=host"]).status.success();
    let other_alive = tmux(&["has-session", "-t", "=other"]).status.success();
    let windows =
        String::from_utf8_lossy(&tmux(&["list-windows", "-t", "=host", "-F", "#W"]).stdout)
            .into_owned();
    tmux(&["kill-server"]);

    assert_eq!(read("use.code").trim(), "2", "{}", read("use.json"));
    assert!(
        read("use.json").contains("refusing to use tmux session `host`"),
        "{}",
        read("use.json")
    );
    assert_eq!(read("kill.code").trim(), "2", "{}", read("kill.json"));
    assert!(
        read("kill.json").contains("refusing"),
        "{}",
        read("kill.json")
    );
    assert!(host_alive, "the session testopt ran in was killed");
    assert!(
        !windows.contains("run-"),
        "a job window was put in the caller's session: {windows}"
    );
    // Another session it may kill.
    assert_eq!(read("other.code").trim(), "0", "{}", read("other.json"));
    assert!(
        read("other.json").contains("\"killed\": true"),
        "{}",
        read("other.json")
    );
    assert!(!other_alive);
}
