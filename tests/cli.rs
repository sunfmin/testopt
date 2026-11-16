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
    assert_eq!(rec["tests"], 7, "{rec:#}");
    assert_eq!(rec["threads"], 4);
    assert!(
        rec["nextest_summary"]
            .as_str()
            .unwrap()
            .contains("7 tests run"),
        "{rec:#}"
    );
    let rank = fx.ok(&["rank"]);
    assert_eq!(rank["totals"]["tests"], 7);
    assert_eq!(f(&rank["totals"]["test_seconds"]), f(&rec["test_seconds"]));
    // Slowest first. `sleeps` (1.5 s) and `spawns_processes` (6 × 0.2 s and
    // spawn costs) are close on a slow runner, so either may lead.
    let secs: Vec<f64> = rank["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| f(&t["seconds"]))
        .collect();
    assert!(secs.windows(2).all(|w| w[0] >= w[1]), "{rank:#}");
    let sleeps = rank["tests"]
        .as_array()
        .unwrap()
        .iter()
        .position(|t| t["name"] == "sleeps")
        .expect("sleeps ranked");
    assert!(sleeps < 2, "{rank:#}");
    assert!(f(&rank["tests"][sleeps]["seconds"]) >= 1.5);
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
    // The replayed schedule: 7 tests on 4 slots end with the 1.5s sleep;
    // without it the run ends with the next longest, so it saves the gap.
    assert_eq!(rank["totals"]["replayed_from"], "JUnit start order");
    let replayed = f(&rank["totals"]["replayed_wall_s"]);
    assert!(
        replayed >= 1.5 && replayed <= f(&rank["totals"]["test_seconds"]),
        "{rank:#}"
    );
    let saving = f(&rank["tests"][0]["replayed_wall_saving_s"]);
    assert!(saving > 0.0 && saving < 1.5, "{rank:#}");
    // No test saves more than its own time, nor less than nothing.
    for t in rank["tests"].as_array().unwrap() {
        let s = f(&t["replayed_wall_saving_s"]);
        assert!((0.0..=f(&t["seconds"]) + 1e-3).contains(&s), "{t:#}");
    }

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
    // Each of the six `sleep`s once. The test process may start a
    // short-lived helper of its own besides (seen under a stress run, as a
    // copy of the test binary living under one sample), so the total is at
    // least six, and the range [spawned, spawned_at_most] holds six.
    // (Linux's sh does not exec its last command: there the six `sh`
    // count too, so `sleep` is found by name, not by place.)
    let sleeps = w["processes"]["by_program"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["program"] == "sleep")
        .unwrap();
    assert_eq!(
        (sleeps["program"].as_str(), sleeps["count"].as_u64()),
        (Some("sleep"), Some(6)),
        "{w:#}"
    );
    let seen = w["processes"]["spawned"].as_u64().unwrap();
    assert!(
        seen >= 6 && w["processes"]["spawned_at_most"].as_u64().unwrap() >= seen,
        "{w:#}"
    );
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
    // A second timing of `sleeps`: the ledger takes before and after from
    // the two, flagged when either was.
    fx.ok(&["time", "sleeps", "-n", "1", "--max-load-per-core", "0"]);
    let l = fx.ok(&[
        "ledger",
        "add",
        "--test",
        "fixture::it sleeps",
        "--from-times",
        "--kind",
        "fixed-wait",
        "--status",
        "tried",
    ]);
    let e = &l["added"];
    assert!(f(&e["before_s"]) >= 1.5 && f(&e["after_s"]) >= 1.5, "{l:#}");
    assert!(e["reason"].as_str().unwrap().contains("flagged"), "{l:#}");
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

    // The ledger takes the check's numbers and verdict as they are.
    let l = fx.ok(&[
        "ledger",
        "add",
        "--from-check",
        "latest",
        "--kind",
        "redundant",
        "--status",
        "kept",
    ]);
    let e = &l["added"][0]["added"];
    assert_eq!(e["test"], "fixture::it classifies_a_positive", "{l:#}");
    assert_eq!(f(&e["after_s"]), 0.0);
    assert!(f(&e["before_s"]) >= 0.0);
    assert!(
        e["reason"]
            .as_str()
            .unwrap()
            .contains(": pass (0 lost lines"),
        "{l:#}"
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
    let next: Vec<&str> = l["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["test"].as_str().unwrap())
        .collect();
    assert!(
        !next.contains(&"fixture::it sleeps") && !next.contains(&"fixture::it spawns_processes"),
        "{next:?}"
    );
    assert_eq!(next.len(), 5);

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
         {exe} -C {ws} --tmux --tmux-session jobs --wait-timeout 120 record -E 'test(fast_adds)' > {d}/job.json 2>/dev/null; echo $? > {d}/job.code; \
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
    // A job in another session runs, from its own copy of testopt.
    assert_eq!(read("job.code").trim(), "0", "{}", read("job.json"));
    let job: Value = serde_json::from_str(&read("job.json")).unwrap();
    assert_eq!(job["tests"], 1, "{job:#}");
    let jobs = std::fs::read_dir(
        fx.home
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("jobs"),
    )
    .unwrap();
    let copies = jobs
        .flatten()
        .filter(|j| j.path().join("testopt").is_file())
        .count();
    assert_eq!(copies, 1);
}

#[test]
fn the_target_s_environment_is_kept_once_and_applied_to_every_command() {
    let fx = Fixture::new();
    let set = fx.ok(&[
        "env",
        "set",
        "NEXTEST_TEST_THREADS=2",
        "TMPDIR={root}/../deep/tmp",
    ]);
    assert_eq!(set["env"]["TMPDIR"], "{root}/../deep/tmp");
    let shown = fx.ok(&["env"]);
    let tmp = shown["resolved"]["TMPDIR"].as_str().unwrap();
    // {root} is the workspace root as cargo says it (/private/var, not /var).
    let root = fx.root.canonicalize().unwrap();
    assert_eq!(tmp, format!("{}/../deep/tmp", root.display()));
    // Applied: nextest's own variable sets the threads, and the TMPDIR the
    // tests get exists.
    let rec = fx.ok(&["record", "-E", "test(fast_adds)"]);
    assert_eq!(rec["threads"], 2, "{rec:#}");
    assert!(Path::new(tmp).is_dir());
    fx.ok(&["env", "unset", "NEXTEST_TEST_THREADS"]);
    assert_eq!(
        fx.ok(&["record", "-E", "test(fast_adds)", "--test-threads", "3"])["threads"],
        3
    );
}

#[test]
fn a_changed_test_is_timed_on_both_trees_interleaved_with_its_cpu() {
    let fx = Fixture::new();
    let it = fx.root.join("tests/it.rs");
    let src = std::fs::read_to_string(&it).unwrap();
    std::fs::write(
        &it,
        src.replace("    for _ in 0..6 {", "    for _ in 0..3 {"),
    )
    .unwrap();
    let c = fx.ok(&["check", "HEAD", "-E", "test(spawns_processes)", "-n", "2"]);
    assert_eq!(
        c["tests"]["changed"],
        serde_json::json!(["fixture::it spawns_processes"]),
        "{c:#}"
    );
    let t = &c["tests"]["timing"][0];
    assert_eq!(t["interleaved"], true, "{c:#}");
    assert_eq!(t["base"]["runs"].as_array().unwrap().len(), 2);
    assert_eq!(t["head"]["runs"].as_array().unwrap().len(), 2);
    // Half the processes: about half the time, and both sides used CPU.
    assert!(f(&t["delta_s"]) < -0.3, "{c:#}");
    assert!(
        f(&t["base"]["cpu_median_s"]) > 0.0 && f(&t["head"]["cpu_median_s"]) > 0.0,
        "{c:#}"
    );

    let time = fx.ok(&["time", "spawns_processes", "-n", "1"]);
    assert_eq!(time["cpu_runs"].as_array().unwrap().len(), 1, "{time:#}");
    assert!(f(&time["cpu_median_s"]) > 0.0, "{time:#}");
}

#[test]
fn tests_on_one_line_down_different_arms_do_not_overlap_and_check_keeps_each_arm() {
    let fx = Fixture::new();
    let c = fx.ok(&[
        "covers",
        "larger_keeps_the_first",
        "--against",
        "test(larger_)",
    ]);
    let other = &c["others"][0];
    assert_eq!(other["test"], "fixture::it larger_takes_the_second");
    // The same line...
    assert_eq!(f(&other["share_of_this"]), 1.0, "{c:#}");
    // ...down another arm: neither is within the other.
    assert_eq!(other["shared_arms"], 0, "{c:#}");
    assert_eq!(other["this_within_it"], false);
    assert_eq!(c["arms_taken_by_no_other"], 1);
    assert_eq!(c["unattributed_profiles"], 0);

    fx.delete_test("larger_takes_the_second");
    let (c, code) = fx.run(&["check", "HEAD", "-E", "test(larger_)", "-n", "1"]);
    assert_eq!(code, 1, "{c:#}");
    assert_eq!(c["coverage"]["lost_lines"], 0, "{c:#}");
    assert_eq!(c["coverage"]["lost_arms"], 1, "{c:#}");
    assert_no_profiles(&fx.root);
}

#[test]
fn check_picks_its_scope_from_the_diff_and_names_what_it_cannot_see() {
    let fx = Fixture::new();
    // Code under test changed, no coverage cached yet: named, not guessed.
    let lib = fx.root.join("src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(
        &lib,
        src.replace("pub fn classify", "// moved\npub fn classify"),
    )
    .unwrap();
    let c = fx.ok(&["check", "HEAD", "-n", "1"]);
    assert_eq!(
        c["scope_unknown_for"],
        serde_json::json!(["src/lib.rs"]),
        "{c:#}"
    );
    git(&fx.root, &["checkout", "-q", "src/lib.rs"]);

    // A test file changed: its tests are the scope.
    fx.delete_test("classifies_every_sign");
    let (c, code) = fx.run(&["check", "HEAD", "-n", "1"]);
    assert_eq!(code, 1, "{c:#}");
    assert_eq!(
        c["scope"]["test_files"],
        serde_json::json!(["tests/it.rs"]),
        "{c:#}"
    );
    assert_eq!(c["coverage"]["base_tests"], 7, "{c:#}");
    assert_eq!(
        c["coverage"]["lost"],
        serde_json::json!(["src/lib.rs:10", "src/lib.rs:12"])
    );
    git(&fx.root, &["checkout", "-q", "tests/it.rs"]);

    // Code under test changed, the base's coverage now cached: the tests
    // that covered it are the scope, and nothing is unknown.
    std::fs::write(
        &lib,
        src.replace("pub fn classify", "// moved\npub fn classify"),
    )
    .unwrap();
    let c = fx.ok(&["check", "HEAD", "-n", "1"]);
    assert_eq!(c["scope_unknown_for"], serde_json::json!([]), "{c:#}");
    assert!(c["scope"]["covering_tests"].as_u64().unwrap() >= 2, "{c:#}");
    assert_eq!(c["pass"], true, "{c:#}");
}

/// What an AI session needs first: where things stand, one line per
/// result, and the next command to run.
#[test]
fn status_summaries_and_next_steps_lead_a_session() {
    let fx = Fixture::new();
    let s = fx.ok(&["status"]);
    assert!(
        s["summary"].as_str().unwrap().starts_with("no run yet"),
        "{s:#}"
    );
    assert!(
        s["next"][0]
            .as_str()
            .unwrap()
            .contains("testopt --tmux record"),
        "{s:#}"
    );

    let rec = fx.ok(&["record"]);
    assert!(
        rec["summary"].as_str().unwrap().starts_with("7 tests"),
        "{rec:#}"
    );
    assert_eq!(rec["next"][0], "testopt rank");
    let rank = fx.ok(&["rank"]);
    assert_eq!(
        rank["next"][0], "testopt why 'fixture::it sleeps'",
        "{rank:#}"
    );

    // The slowest two, surveyed in one job; kept in the store.
    let sv = fx.ok(&["survey", "--top", "2"]);
    assert_eq!(sv["rows"].as_array().unwrap().len(), 2, "{sv:#}");
    assert_eq!(sv["rows"][0]["test"], "fixture::it sleeps");
    assert_eq!(sv["rows"][0]["kind"], "fixed-wait");
    assert_eq!(sv["rows"][1]["kind"], "process-heavy");
    assert!(
        sv["summary"].as_str().unwrap().contains("fixed-wait 1"),
        "{sv:#}"
    );
    // `wait` answers at once for a job that has ended.
    let job = fx.ok(&["job"]);
    let id = job["job"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    let w = fx.ok(&["wait", &id, "--timeout", "5"]);
    assert_eq!(w["tests"], 2);

    let s = fx.ok(&["status"]);
    assert_eq!(s["latest_run"]["of_this_tree"], true, "{s:#}");
    assert_eq!(s["survey"][0]["kind"], "fixed-wait", "{s:#}");
    assert!(
        s["summary"].as_str().unwrap().contains("of this commit"),
        "{s:#}"
    );

    // A why answer is short by default: what weighs, with the next step.
    let why = fx.ok(&["why", "sleeps"]);
    assert!(
        why["summary"].as_str().unwrap().contains("fixed-wait, "),
        "{why:#}"
    );
    assert!(
        why["next"][0]
            .as_str()
            .unwrap()
            .starts_with("replace the wait at tests/it.rs:17"),
        "{why:#}"
    );
    assert!(why["processes"].get("by_command").is_none());
    assert!(
        fx.ok(&["why", "sleeps", "--full"])["processes"]
            .get("by_command")
            .is_some()
    );

    // Peers by file, no filter to write.
    let c = fx.ok(&["covers", "classifies_a_positive", "--peers", "file"]);
    assert_eq!(c["compared_with"], 6, "{c:#}");
    assert!(
        c["next"][0].as_str().unwrap().starts_with("delete it"),
        "{c:#}"
    );

    // A commit later, the run is stale and status says to record again.
    std::fs::write(fx.root.join("NOTE"), "x").unwrap();
    git(&fx.root, &["add", "NOTE"]);
    git(&fx.root, &["commit", "-qm", "later"]);
    let s = fx.ok(&["status"]);
    assert_eq!(s["latest_run"]["of_this_tree"], false, "{s:#}");
    assert!(s["next"][0].as_str().unwrap().contains("record"), "{s:#}");
}
