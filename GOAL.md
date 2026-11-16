# Goal: testopt — a tool that finds where a Rust test suite's time goes

Build `testopt`, a CLI that measures a cargo-nextest workspace's tests, ranks
them by the wall time they cost, says where each one's time goes, and checks
that a change to the tests made them faster without losing coverage. One
Claude session (not a fleet) uses it to optimize a suite; long runs happen in
tmux so they survive the session and the human can watch.

You are done when every item under **Acceptance** holds, each shown by a real
run whose output is in `REPORT.md`.

## Why (measured 2026-11-16 on afkconvoy, FelixMBP14.local, 18 cores)

- afkconvoy's slow gate: 991 tests, 12 at a time, 435 s wall, 4,119
  test-seconds, load 34–47. Wall ≈ test-seconds / threads, so a finding is
  worth (seconds saved) / threads of wall time.
- Share by test time: ≥20 s 12 tests (7 %), 10–20 s 59 (19 %), 2–10 s 646
  (65 %), <2 s 274 (9 %). The middle is the lever, not the top.
- Where time goes, three kinds seen:
  1. fixed waits — e.g. `crates/cli/tests/exclusive.rs:451` `sleep(21s)`,
     `:670`, `:105` `gate_seconds: 20` (each ~21 s);
  2. watch/poll rounds — `host_watch.rs` tests restart a runner 5× and wait
     for looks;
  3. many processes — every CLI call spawns `afkconvoy`, fake gh/orca, git;
     `consult::a_failed_start_or_trust_gap…` runs 4 worlds × ~9 ticks.
- Redundancy exists: `crates/landing/tests/standalone.rs` and
  `crates/cli/tests/candidates.rs` test the same landing scenarios at two
  layers (e.g. `four_clean_changes_are_one_group…` 7.6 s vs
  `four_ready_prs_are_one_group…` 16.6 s).
- afkconvoy's `gate.sh` deletes nextest's output after each run, so no
  per-test history exists; getting numbers meant rerunning everything.

## Commands (JSON on stdout; progress on stderr / a log)

- `testopt record [--filter-file F | -E EXPR] [--profile P]` — run the tests
  with nextest, keep per-test times (nextest JUnit) with commit, host, load at
  start/end, thread count, into one append-only store per target workspace.
- `testopt rank` — from the latest run: tests by time, share, and estimated
  wall saving; per-file totals; trend vs earlier runs when there are any.
- `testopt why <test>` — classify where its time goes: fixed waits (static
  scan of its body and helpers it calls for `sleep`, timeouts/limits, waits
  not scaled), processes spawned (command + duration), setup. Must work
  without editing the target repo: e.g. sample the test's process tree
  (`ps`, short interval) while running it alone. Your choice; justify it.
- `testopt time <test> [-n N]` — run it alone N times, median and spread,
  load recorded; refuse or flag as unreliable when load / cores is above a
  threshold, rather than return a number that cannot be compared.
- `testopt covers <test>` — lines it covers (cargo-llvm-cov) and which other
  tests cover the same lines.
- `testopt check <base-ref>` — compare working tree vs base: time change of
  the touched tests, and whether the union of covered lines shrank (a
  deleted or changed test that loses a line fails the check).
- `testopt dash` — a terminal view for a tmux pane: running job's progress,
  current top 10, ledger totals; refreshes itself.
- Ledger: one file per target (`testopt ledger` to read/append) — each
  candidate tried, kept or rejected, with before/after numbers and reason. It
  is the only home of optimization state; a new session resumes from it.

tmux: `testopt` starts long jobs (`record`, full `check`) in a tmux window
of a `testopt` session when asked (`--tmux`) and the caller waits on the
result file, not on the pane. Document the layout (run / dash windows).

## Acceptance

1. Generic: works on any nextest workspace. Its own tests use a tiny fixture
   workspace (a fast test, a test that sleeps, a test that spawns processes,
   two tests where one's coverage is a subset of the other's).
2. On afkconvoy (see Boundaries for where): `record` of the slow tests
   (filter built from `.config/slow-tests` as afkconvoy's `gate.sh` builds
   it) stores a run; `rank` reproduces its totals.
3. `why` on afkconvoy classifies at least:
   `exclusive::a_cut_gates_ahead_of_waiting_workers_and_its_queue_wait_does_not_end_its_gate`
   as a fixed wait citing the `sleep`, and
   `consult::a_failed_start_or_trust_gap_escalates_with_or_without_a_named_consult`
   as process-heavy with its process counts.
4. `time` gives median/spread with load, and flags a loaded host.
5. `covers` and `check` on afkconvoy: show the landing standalone /
   candidates overlap for one pair; in a scratch copy, deleting a test that
   is the only cover of some line makes `check` fail, deleting a fully
   covered one passes. Nothing is committed to afkconvoy.
6. `dash` runs in a tmux pane during a `record`.
7. `REPORT.md`: what was built, how each acceptance item was shown (commands
   + output excerpts + measured seconds), afkconvoy's top 20 candidates with
   evidence (kind, seconds, estimated wall saving), and what is not done.
8. README: install, commands, the one-session optimization loop
   (record → rank → why → change → time/check → ledger).

## Boundaries

- Do not modify, commit to, or push afkconvoy. Measure in your own detached
  worktree of it (`git -C ~/Developments/afkconvoy worktree add --detach
  <testopt work dir>/afkconvoy-target 19f0315960ba12cbd1a88f01e58b6063eba20038`),
  under this project's ignored work dir; remove it when done. Another
  session works in `~/Developments/afkconvoy` itself — never touch that
  checkout's files or its `target/`.
- The host is shared and loaded. Run at most one full afkconvoy suite at a
  time; a full slow-test run is ~7 min plus build. Use small filters while
  developing.
- Rust, edition 2024, clippy clean, `cargo nextest` for this project's own
  tests. Installing cargo-llvm-cov (pinned version, into `$HOME`) is allowed.
- Work on branches, commit per meaningful stage, push, open PRs to `main`
  and merge your own when green (global CLAUDE.md rules apply).
- Stuck on a decision: consult an independent subagent, take the
  conservative path, note it in REPORT.md. Do not stop to wait for a human.
