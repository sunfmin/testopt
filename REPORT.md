# testopt — report

Progress and evidence for GOAL.md. Every number below is from a real run
on this host: `macnoMac-Studio.local`, 18 cores, a shared host. The
1-minute load was 25–48 throughout, so load/cores was 1.4–2.7. testopt
flags every timing taken at that load.

- **Target:** afkconvoy at `19f0315960ba12cbd1a88f01e58b6063eba20038`, in a
  detached worktree at `work/afkconvoy-target` (ignored).
- **Toolchain:** afkconvoy's gate exports `RUSTUP_TOOLCHAIN=nightly-2026-09-25`,
  `AFKCONVOY_BUILD_COMMIT` and a deep `TMPDIR`. These are stored once with
  `testopt env set …` and applied to every command.
- **Binary:** testopt built from this branch and installed with
  `cargo install --path . --root work/inst --locked`.

## What was built

`testopt` is a Rust CLI (edition 2024). It prints JSON on stdout and
progress on stderr. Every answer carries a one-line `summary` and `next`,
the commands to run next, so an AI session can act without reading the
whole result.

| command | what it does |
|---|---|
| `env` | the target's gate environment, kept once in the store |
| `record` | runs nextest and stores per-test JUnit times, start order, commit, host, load, threads |
| `rank` | bands, per-file totals, trend, and two wall savings per test: `seconds/threads`, and a replay of the run's own schedule without the test |
| `why` | `syn` static scan of the test and the helpers it calls (sleeps, limits, polls, cited by file:line), plus a 20 ms process-tree sampler while the test runs alone |
| `time` | N runs alone: median, spread, CPU seconds of the process tree, load; flags a loaded host |
| `covers` | per-test line and branch-arm coverage through cargo-llvm-cov; testopt is nextest's target runner and gives each test its own profile dir |
| `check` | the base copied with `git archive` into the store, both trees measured; fails when a line or arm loses its only test; touched tests timed with base and head interleaved |
| `survey` | `why` over the top N, kept in the store |
| `dash` | a terminal view for a tmux pane |
| `ledger` | the only home of optimization state |
| `status` | where an optimization stands, and the next command |
| `job` / `wait` / `tmux-kill` | tmux jobs, never in or against the caller's own session |

The design (why sampling, how the replay works, how coverage is told
apart per test) is in README.md.

testopt's own tests: 24, all passing (11 unit tests, 13 CLI tests on
`tests/fixture`). The latest local run took 13.6 s. CI runs fmt, clippy
`-D warnings` and nextest on ubuntu and macOS.

## Acceptance

### 1. Generic, with a tiny fixture workspace

testopt reads everything from `cargo metadata` and nextest. Nothing is
specific to afkconvoy: the gate's filter file is just `--filter-file`, and
its environment is just `env`. The store is keyed `<repo>-<root commit>`.

`tests/fixture` is a two-file workspace with:

- `fast_adds`
- `sleeps`, whose 1.5 s sleep is in a helper, `settle()`
- `spawns_processes`, with 6 × `sh -c "sleep 0.2"`
- `classifies_a_positive`, whose coverage is a subset of
  `classifies_every_sign`'s
- `larger_keeps_the_first` and `larger_takes_the_second`, which cover one
  line down different branch arms
- a `slow` nextest profile

`tests/cli.rs` runs every command against it:

```
$ cargo nextest run
     Summary [  13.624s] 24 tests run: 24 passed, 0 skipped
```

### 2. `record` of the slow tests on afkconvoy, and `rank` reproduces its totals

```
$ testopt env set RUSTUP_TOOLCHAIN=nightly-2026-09-25 'AFKCONVOY_BUILD_COMMIT={commit}' \
    'TMPDIR={root}/.afkconvoy-tmp/the-tests-of-the-gate-run-in-a-directory-as-deep-as-a-worker-s'
$ testopt --tmux record --filter-file .config/slow-tests --profile slow
```

`--filter-file` builds the same filter `gate.sh` builds. A unit test,
`filter_file_matches_gate_sh`, checks this.

Two full runs:

| run | tests | test-seconds | wall | nextest's own wall | load start→end |
|---|---|---|---|---|---|
| `20261116-225302-record-52487` | 993 passed | 4,528.856 | 382.738 s | 382.335 s | 34.5 → 43.1 |
| `20261116-235314-record-62921` (tmux, 6 min 53 s with build) | 993 passed (17 slow) | 4,862.402 | 411.665 s | 411.089 s | 26.4 → 41.1 |

Both ran 12 threads (the `slow` profile).

```
$ testopt rank --top 5          # 0.55 s
"summary": "993 tests, 4862 test-s, 412s wall at 12; slowest 36.0s (saves 2.9s wall);
            ≥20s 17 tests 8%, 10–20s 94 tests 27%, 2–10s 657 tests 59%, <2s 225 tests 6%"
"totals": {"tests":993,"test_seconds":4862.402,"wall_s":411.665,"threads":12,
           "replayed_wall_s":411.076,"replayed_from":"JUnit start order","failed":0}
```

Checked independently: `jq '[.tests[].seconds]|add'` over the stored run
line gives `4862.401999999998`, the same total. The replay of the recorded
start order gives 411.076 s against a measured 411.665 s (0.14 % off). So
`replayed_wall_saving_s` rests on a schedule that reproduces the real run.

The top files by test-seconds:

| file | tests | seconds | est. wall share |
|---|---|---|---|
| crates/cli/tests/host_watch.rs | 53 | 471.8 | 39.3 s |
| crates/cli/tests/candidates.rs | 38 | 403.1 | 33.6 s |
| crates/cli/tests/tick.rs | 54 | 238.4 | 19.9 s |
| crates/landing/tests/standalone.rs | 17 | 234.5 | 19.5 s |
| crates/cli/tests/switch.rs | 40 | 210.4 | 17.5 s |

As GOAL.md measured: the 2–10 s band holds 59–65 % of the time, so the
middle is the lever.

### 3. `why` classifies the two named tests

```
$ testopt why exclusive::a_cut_gates_ahead_of_waiting_workers_and_its_queue_wait_does_not_end_its_gate
                                                                  # 26.1 s
"summary": "…: fixed-wait, 24.5s alone; fixed waits 21.0s
  (crates/cli/tests/exclusive.rs:411 20.0s, crates/cli/tests/exclusive.rs:451 21.0s);
  1141 processes (sleep 980, git 77, sh 36)"
"breakdown": {"wall_s":24.456,"fixed_waits_s":21.0,"with_child_processes_alive_s":24.249,
              "children_cpu_s":3.211,"test_process_cpu_s":0.06}
```

The scan cites the sleep at `exclusive.rs:451` (`std::thread::sleep(Duration::from_secs(21))`)
and the `gate_seconds: 20` limit at `:411`. They count as one wait, the
sleep outlasting the limit, not 41 s. The 980 `sleep` processes come from
a shell poll loop. The scan finds that loop too: the test polls
`host_held` (`common/mod.rs`) and `until` (`crates/patience/src/lib.rs`).

```
$ testopt why consult::a_failed_start_or_trust_gap_escalates_with_or_without_a_named_consult
                                                                  # 19.4 s
"summary": "…: process-heavy, 18.0s alone; fixed waits 0.0s; 802–2051 processes (git 420, sh 89, gh 82)"
"by_program": git 420, sh 89, gh 82, afkconvoy 47, git-receive-pack 39, orca 34
"breakdown": {"wall_s":17.988,"with_child_processes_alive_s":16.722,"children_cpu_s":4.227,
              "test_process_cpu_s":0.399,"fixed_waits_s":0.0}
```

The process count is a range. The sampler saw 802 processes. The pids the
host handed out during the test bound what a 20 ms sample can miss, at
2,051. Child processes were alive for 16.7 s of the 18.0 s.

### 4. `time` gives median and spread with load, and flags a loaded host

```
$ testopt time four_clean_changes_are_one_group_with_one_fast_gate_and_land_in_one_fast_forward -n 5
                                                                  # 40.4 s
"summary": "…: median 7.334s, spread 1.913s, cpu 1.376s over 5 runs; FLAGGED: load 1.944/core — compare cpu, not wall"
"runs": [8.572, 8.369, 6.659, 6.707, 7.334]     "cpu_runs": [1.682, 1.672, 1.365, 1.311, 1.376]
"loads": [35.0, 34.1, 34.1, 32.9, 32.9, 30.8, 30.8, 29.9, 29.9, 27.6]
"reliable": false
"flag": "unreliable: load reached 1.94 per core (threshold 1); this time cannot be compared with one taken on a quiet host"
```

With `--strict`, the command exits 3 instead of returning the flagged
number. Wall time spread 26 % of its median here, CPU time 27 %, so on
this host neither is a precise before/after.

### 5. `covers` and `check` on afkconvoy

**The overlap of one standalone / candidates pair:**

```
$ testopt covers four_clean_changes_are_one_group_with_one_fast_gate_and_land_in_one_fast_forward \
    --peers binary --against 'test(=candidates::four_ready_prs_are_one_group_gated_fast_once_and_land_in_one_fast_forward)'
                                                                  # 77.5 s, instrumented build included
"summary": "…four_clean…: 1842 lines, 347 arms; 0 lines and 0 arms no other compared test covers; within 1 other test(s)"
"next": ["delete it (every line and arm is also covered by afkconvoy-landing::standalone
          a_change_conflicting_with_one_of_a_group_goes_up_alone_and_carries_it_next), then: testopt check HEAD"]
```

| compared with | lines | arms | shared lines | shared arms | share of this test | this within it |
|---|---|---|---|---|---|---|
| standalone `a_change_conflicting_with_one_of_a_group…` | 1,912 | 386 | 1,842 | 347 | 100 % | yes |
| cli `candidates::four_ready_prs_are_one_group…` | 6,697 | 1,052 | 1,015 | 220 | 55 % | no |

The CLI test reaches the same landing code through the CLI. It shares 55 %
of the standalone test's lines, but takes a different set of arms. The
redundancy GOAL.md suspected is real, but inside `standalone.rs` rather
than across the layers: every line and arm of `four_clean…` is also
covered by its neighbour `a_change_conflicting…`.

**`check` passes when a fully covered test is deleted, and fails when the
only cover of a line goes.** Both cases ran in the scratch worktree, which
was restored after each (`git status` clean). Nothing was committed to
afkconvoy.

*Pass:* delete `standalone four_clean_changes_are_one_group…` (39 lines).
`covers` above shows that every line and arm of it is also covered by its
neighbour.

```
$ perl work/delete-test.pl four_clean_changes_are_one_group_with_one_fast_gate_and_land_in_one_fast_forward crates/landing/tests/standalone.rs
$ testopt --tmux check HEAD          # 104.7 s; job 20261117-001531-check-80808 in tmux window run-…
"summary": "PASS; lost 0 lines, 0 arms; touched tests change -6.984s"
"verdict": "pass: every line and branch arm covered at base and still in the code is covered"
"scope": {"auto":true,"test_files":["crates/landing/tests/standalone.rs"],"covering_tests":0}
"coverage": {"base_tests":19,"head_tests":18,"base_lines":2391,"head_lines":2391,
             "base_branch_arms":571,"head_branch_arms":571,"lost_lines":0,"lost_arms":0,"unattributed_profiles":0}
"timing": deleted, base runs [6.803, 6.984, 9.086], median 6.984 s, cpu 1.491 s, flagged (load 1.59/core)
exit 0
$ git checkout -- crates/landing/tests/standalone.rs && git status --short     # (empty)
```

The scope came from the diff, with no `-E`: the 19 tests of the changed
file. The base side came from the `covers` cache of the same tree, so only
the working tree was built and run.

*Fail:* delete `standalone a_change_whose_source_the_remote_has_not_is_gone_and_the_rest_lands`
(24 lines).

```
$ perl work/delete-test.pl a_change_whose_source_the_remote_has_not_is_gone_and_the_rest_lands crates/landing/tests/standalone.rs
$ testopt --tmux check HEAD          # 16.9 s (both trees' coverage cached by tree hash); job 20261117-002844-check-61797
"summary": "FAIL; lost 2 lines, 1 arms; touched tests change -2.933s"
"verdict": "FAIL: lines or branch arms covered at base are covered by no test now"
"coverage": {"base_lines":2391,"head_lines":2389,"base_branch_arms":571,"head_branch_arms":570,
             "lost":["crates/landing/src/land.rs:752-753"],"lost_arms_sample":["crates/landing/src/land.rs:751 (arm 1)"]}
"lost_detail": [{"at":"crates/landing/src/land.rs:752",
                 "covered_at_base_by":["afkconvoy-landing::standalone a_change_whose_source_the_remote_has_not_is_gone_and_the_rest_lands"]}, …]
exit 1
$ git checkout -- crates/landing/tests/standalone.rs && git status --short     # (empty)
```

The lost code is the `else` arm of `land.rs:751`:

```rust
let Some(head) = self.fetched(&source_ref(&current.source))? else {
    outcomes.push((change.number, Outcome::Gone));
    continue;
};
```

Only that test takes it.


### 6. `dash` runs in a tmux pane during a `record`

`--tmux` puts the job in window `run-<job id>` of session `testopt`.
Window 0, `dash`, runs `testopt dash`. This capture was taken with
`tmux capture-pane -p -t testopt:dash` 209 s into run 1
(`work/dash-capture-2.txt`):

```
testopt dash — /Users/felix/Developments/testopt/work/afkconvoy-target   22:56:32   load 32.0 / 18 cores
──────────────────────────────────────────────────────────────────────────────
job 20261116-225302-record-52487: running
  [######################..................] 538/993 tests, 0 failed, 209s  ~176s left
  last: PASS [  26.351s] (538/993) afkconvoy::cli lander::a_landing_blocked_verdict_parks_and_resumes_landing
latest run 20261116-224902-record-38997 — 1 tests, 7 test-s, 8s wall, 12 at a time
…
ledger: 0 entries — 0 kept, 0 rejected, 0 tried; kept saves 0.0 test-s ≈ 0.0 s wall
```

The run finished in 382.7 s, so the ETA at 209 s (≈176 s left) was 2 s
off. Captures during run 2 (`work/dash-run2-early.txt`) and the survey
(`work/dash-survey.txt`) show the top 10 and the ledger's entries.

### 7. afkconvoy's top 20 candidates

From `testopt --tmux survey --top 20`, which ran in 353.3 s: `why` on each
of the latest run's 20 slowest tests, run alone.

- **seconds:** the time in the recorded run.
- **alone:** the time when run alone.
- **replayed:** the wall time the run loses without the test, by replay.
- **est.:** seconds / 12 threads.

| # | test | kind | seconds | alone | replayed wall saving | est. wall saving | evidence |
|---|---|---|---|---|---|---|---|
| 1 | `cli host_watch::a_hung_human_notification_times_out_and_retries_without_holding_back_looks_or_rounds` | process-heavy | 36.048 | 36.145 | 2.949 | 3.004 | 386–565 procs: git 169, sh 70, orca 34 |
| 2 | `cli host_watch::human_notifications_send_each_condition_once_despite_reworded_reports_and_restart` | process-heavy | 32.217 | 30.713 | 2.653 | 2.685 | 579–756 procs: git 273, sh 89, orca 45 |
| 3 | `cli lander::a_landing_blocked_verdict_parks_and_resumes_landing` | process-heavy | 27.57 | 22.526 | 2.276 | 2.298 | 1178 procs: git 638, gh 193, sh 83 |
| 4 | `cli exclusive::a_cut_gates_ahead_of_waiting_workers_and_its_queue_wait_does_not_end_its_gate` | fixed-wait | 25.437 | 23.716 | 2.098 | 2.12 | fixed 21.0s: crates/cli/tests/exclusive.rs:411 |
| 5 | `cli host_watch::resident_rounds_queue_while_busy_revalidate_recover_and_keep_reports_separate` | process-heavy | 24.898 | 24.103 | 2.07 | 2.075 | 348 procs: git 158, sh 67, orca 24 |
| 6 | `cli candidates::a_red_group_has_its_last_pr_carry_the_others_and_the_rest_lands` | process-heavy | 24.614 | 13.555 | 2.019 | 2.051 | 1059 procs: git 595, sleep 172, git-upload-pack 83 |
| 7 | `cli walk::running_again_after_a_park_counts_as_no_rework` | process-heavy | 22.54 | 12.657 | 1.68 | 1.878 | 585 procs: git 304, gh 84, afkconvoy 40 |
| 8 | `cli joining::landing_retires_existing_health_even_when_its_recovery_window_has_expired` | process-heavy | 22.326 | 14.775 | 1.844 | 1.861 | 684–2005 procs: git 417, sh 66, git-receive-pack 55 |
| 9 | `cli exclusive::a_fast_gate_s_wait_for_its_lane_uses_none_of_its_limit` | fixed-wait | 22.05 | 22.406 | 1.802 | 1.838 | fixed 21.0s: crates/cli/tests/exclusive.rs:670 |
| 10 | `cli consult::a_failed_start_or_trust_gap_escalates_with_or_without_a_named_consult` | process-heavy | 22.008 | 12.198 | 1.814 | 1.834 | 642 procs: git 308, gh 72, sh 64 |
| 11 | `cli switch::ambiguous_normal_work_and_failed_judgments_never_authorize_a_switch` | process-heavy | 21.938 | 16.868 | 1.824 | 1.828 | 816 procs: git 437, sh 135, git-receive-pack 60 |
| 12 | `cli candidates::a_round_takes_the_prs_that_asked_and_one_asking_during_it_waits_for_the_next` | process-heavy | 21.479 | 14.466 | 1.779 | 1.79 | 1234 procs: git 740, sleep 176, sh 98 |
| 13 | `cli exclusive::a_gate_timeout_still_ends_a_nested_exclusive_command_and_its_descendants` | fixed-wait | 21.286 | 21.26 | 1.749 | 1.774 | fixed 20.0s: crates/cli/tests/exclusive.rs:105 |
| 14 | `cli candidates::a_pr_ready_while_a_round_s_whole_gate_runs_stacks_on_its_root` | process-heavy | 21.04 | 11.791 | 1.74 | 1.753 | 752 procs: git 402, sleep 129, git-receive-pack 72 |
| 15 | `cli park::a_blocker_nothing_will_resolve_escalates` | process-heavy | 20.91 | 10.887 | 1.727 | 1.743 | 555 procs: git 301, sh 75, git-receive-pack 51 |
| 16 | `standalone a_change_conflicting_with_one_of_a_group_goes_up_alone_and_carries_it_next` | process-heavy | 20.907 | 7.45 | 0.497 | 1.742 | 379–1550 procs: git 301, git-receive-pack 27, sh 25 |
| 17 | `cli candidates::with_one_lane_a_round_s_one_group_gates_and_the_round_lands_on_one_slow_gate` | process-heavy | 20.782 | 9.283 | 1.708 | 1.732 | 640 procs: git 401, sleep 60, git-receive-pack 50 |
| 18 | `standalone a_change_asked_while_a_round_s_whole_gate_runs_stacks_on_its_root_and_runs_only_its_slow_gate` | process-heavy | 19.8 | 6.606 | 0.497 | 1.65 | 361 procs: git 267, git-upload-pack 28, sh 24 |
| 19 | `cli candidates::four_ready_prs_are_one_group_gated_fast_once_and_land_in_one_fast_forward` | process-heavy | 19.768 | 10.069 | 1.633 | 1.647 | 892 procs: git 568, sleep 80, sh 70 |
| 20 | `cli tracker::two_fleets_of_one_repo_working_two_trackers_each_claim_land_and_close_only_their_own_issue` | process-heavy | 19.674 | 7.145 | 1.474 | 1.64 | 348 procs: git 228, git-receive-pack 35, sh 25 |


By kind: **fixed-wait 3** (5.6 s of replayed wall saving) and
**process-heavy 17** (30.2 s).

- **The three fixed waits** are the `exclusive.rs` gate-limit tests named
  in GOAL.md: `:411`, `:670` and `:105`. Each waits a 20–21 s limit out.
  Replacing the wait with a wait on the condition saves up to ~20 s of
  each test's time.
- **The rest** are dominated by `git`, which makes up roughly half of
  every test's processes. Next come `gh`, `sh` and `orca` fakes.
- **The `candidates.rs` tests** also run 60–176 `sleep` processes from
  shell poll loops.

A test that is slow in the suite but fast alone is slow from contention,
not from its own work. Examples: `walk::running_again…` (22.5 s vs 12.7 s
alone) and `tracker::two_fleets…` (19.7 s vs 7.1 s).

The replayed saving of a single top-20 test is 1.5–3.0 s of wall time.
The sum over all 20 is about 36 s of 412 s.

### 8. README

README.md has install (with the cargo-install-not-cp warning), every
command, how `rank`, `why`, `covers` and `check` work and why that way,
the store layout, the tmux layout (dash and run windows), and the
one-session loop: env → record → rank → ledger → why → change →
time/check → ledger.

## Ledger state left for the next session

```
$ testopt ledger add --from-check latest --kind redundant --status rejected \
    --reason "deleting it would leave land.rs:752-753 and the arm at :751 untested"
$ testopt ledger           # 0.11 s
"summary": "5 entries: 0 kept (0.0 test-s, ≈0.0s wall), 1 rejected, 1 tried"
"totals": {"entries":5,"tests":2,"kept":0,"rejected":1,"tried":1}
$ testopt status
"summary": "latest run 993 tests, 412s wall, of this commit; no job running; 20 survey row(s); ledger 5 entries (0 kept)"
"next": ["testopt why 'afkconvoy::cli host_watch::a_hung_human_notification_times_out_and_retries_without_holding_back_looks_or_rounds'"]
```

The ledger names each test's newest state.

- **`four_clean…`: tried.** Its deletion passes `check`. It was not
  applied, because this work does not change afkconvoy. The next session
  can apply it there and run `check` again.
- **`a_change_whose_source…`: rejected.** Deleting it loses coverage.

The next candidates are the survey's top rows that the ledger does not
name yet.


## Changes made at the user's request during the work

The user asked for the tool to be made convenient for an AI session. Done:

- **Every answer** carries `summary` and `next`.
- **`status`** is the first command of a session.
- **`survey`** runs `why` over the top N as one job.
- **`wait`** waits on a job.
- **`why`** is compact by default; `--full` gives everything.
- **`covers --peers file|binary`** compares a test with its peers.
- **`ledger add`** takes `--from-check` and `--from-times`, and the ledger
  names the next candidates.
- **`env`** keeps the gate's environment once, instead of in a shell
  script.
- **`rank`** gives replayed wall savings and per-file totals.
- **`why`** got a syn-based scan with module-exact lookup and
  cross-crate calls, a process count range, and CPU time.
- **Coverage** got branch arms.
- **`check`** got an automatic scope from the diff, and times base and
  head interleaved.
- **A job runs its own copy of testopt**, so a rebuild cannot kill it.

## Incidents and the mechanisms they left

**The agent killed its own tmux session.** While starting the afkconvoy
record in tmux, the agent ran `tmux kill-session -t testopt` to clear an
old job session. The agent was itself running inside a tmux session named
`testopt`, so that ended the agent's own session mid-task. The pushed
commits were safe, and the work resumed in session `testopt-agent`.

- **The invariant that broke:** a tool must never kill, replace, or fill
  the tmux session its caller runs inside.
- **The mechanism now:**
  - `--tmux` and the new `testopt tmux-kill` read the caller's session
    from `$TMUX` (`tmux display-message -p '#S'`).
  - They refuse with an error when it is the target session.
  - `--tmux-session` picks the job session.
  - `testopt tmux-kill` replaces raw `kill-session` in the workflow.
- **The test:** `refuses_to_use_or_kill_the_tmux_session_it_runs_inside`
  runs testopt inside a session on a private tmux server. It checks that
  both the use and the kill are refused, that the session survives, and
  that another session can still be killed. With the guard disabled (a
  mutation), the test fails; restored, it passes.

**A binary overwritten in place was killed.**
- **What happened:** `cp` over the installed testopt made macOS SIGKILL
  it at its next start (exit 137), mid-job.
- **Mechanism:**
  - A `--tmux` job now runs its own copy of testopt, kept in its job
    directory.
  - The README says to install with `cargo install`.

**Stray coverage profiles.**
- **What happened:** a child process the target starts with a cleared
  environment writes `default_*.profraw` into the directory it runs in.
  An early `check` left such files in the afkconvoy worktree.
- **Mechanism:**
  - Coverage now sweeps them out of the target's tree and counts them as
    `unattributed_profiles`.
  - Build scripts and listings profile into a catch-all directory in the
    store.

**Branch coverage silently missing.**
- **What happened:** the first `covers` with the final binary reported
  0 branch arms on afkconvoy. cargo-llvm-cov passes
  `-Zcoverage-options=branch` through its rustc wrapper's environment,
  and cargo does not fingerprint that. So the instrumented build left
  over from the lines-only format looked fresh.
- **Mechanism:**
  - Coverage builds now go to a directory named for a hash of their
    compiler flags.
  - The cache format was bumped, so the wrong entries cannot be reused.
- **After the fix:** the rerun gives 347 arms for `four_clean…`.

**The ledger counted a superseded entry.**
- **What happened:** after the two `check` runs, `ledger add --test four_clean…`
  stored the short name as given, so it did not match the full id.
  The totals also counted every entry, so a later "tried" did not
  replace an earlier "kept".
- **Fix:**
  - `ledger add` now resolves the name to its full id.
  - Totals count each test's newest entry.
  - `--from-check` now keeps `--reason`; it used to drop it.
  - Each fix has a test that fails without it.
- **The one short-named line already in the afkconvoy ledger** was
  rewritten to the full id. That is testopt's own store; the old file
  is kept in `work/s/ledger.before-fix.jsonl`.

**Other fixes.**
- A relative `TESTOPT_HOME` broke jobs, which run in another directory.
  It is now absolutized.
- The sampler split one process into two when its pid was reused.
- Linux CI failures:
  - CPU time now comes from `/proc/<pid>/schedstat`.
  - nextest's colour output under CI is now handled.
- On the macOS CI runner, one fixture test assumed which of two close
  tests ranked first. It now checks the ordering, not the winner.

## What is not done, and limits

- **No afkconvoy test was optimized.** The goal is the tool, and
  afkconvoy must not be modified. The ledger holds the two
  redundancy experiments, both judged by `check`.
- **Every timing is flagged.** The host's load per core never fell
  below 1. Before/after comparisons on this host should use CPU seconds,
  and even those spread about 27 % here.
- **Process counts are a range, not exact.** Processes that live for
  less than 20 ms can be missed. The pid bound caps the misses only while
  the host is otherwise idle; on this host the ceiling is loose
  (802–2,051 for `consult`).
- **Children with a cleared environment are counted, not attributed.**
  Their coverage cannot be tied to a test. It is counted
  (`unattributed_profiles`, 0 in the runs above), not used.
- **Branch coverage needs nightly.** On stable, `covers` and `check`
  compare lines only.
- **Static scan limits:**
  - The `why` scan follows calls 3 deep.
  - It does not resolve trait dispatch or closures passed as arguments.
  - It reads `sleep N` in shell strings, but not waits computed at run
    time.
- **No consult was needed.** No decision blocked on a judgment call; the
  conservative choices (scratch worktree, one heavy job at a time) were
  the defaults.
