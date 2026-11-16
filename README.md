# testopt

Find where a cargo-nextest suite's time goes, and check that a change made
it faster without losing coverage. It works on any nextest workspace and
never writes to it: its state lives in a store outside the workspace.

## Install

```sh
cargo install --locked --path .            # this checkout
cargo install cargo-nextest --locked       # needed by every command
cargo install cargo-llvm-cov --version 0.9.1 --locked   # needed by covers / check
rustup component add llvm-tools            # for the toolchain the target builds with
```

`tmux` is needed only for `--tmux`.

## Commands

Every command prints JSON on stdout and progress on stderr. `-C <dir>`
names the target workspace (default: the current directory). The target's
own environment is the caller's: run testopt with whatever the target's
gate exports (e.g. `RUSTUP_TOOLCHAIN`, `TMPDIR`).

| command | does |
|---|---|
| `testopt record [--filter-file F \| -E EXPR] [--profile P] [--test-threads N] [-- nextest args]` | Builds, then runs the tests with nextest and appends the run (per-test JUnit times, commit, host, load at start and end, threads, nextest's own summary) to the store. `--filter-file` reads `<binary-id> <test name>` lines and builds the same filter afkconvoy's `gate.sh` builds. |
| `testopt rank [--top N] [--run ID]` | The latest run's totals, time bands, tests by time with share and estimated wall saving (seconds / threads), per-file totals, each test's previous time, and the trend across runs. |
| `testopt why <test>` | Runs the test alone and says where its time goes (see below). |
| `testopt time <test> [-n N] [--max-load-per-core X] [--strict]` | Runs it alone N times: median, min, max, spread, and the load before and after each run. When load per core passes X (default 1.0), the result is `"reliable": false` with a `flag`; `--strict` exits 3 instead. |
| `testopt covers <test> [--against EXPR]` | The lines it covers, and for every other test measured on the same tree (and those `--against` selects): shared lines, and whether either is within the other. Also the lines no other test covers. |
| `testopt check <base-ref> [--filter-file F \| -E EXPR] [-n N]` | Working tree against base. Measures coverage on both, maps base lines to working-tree lines through the diff, and fails (exit 1) when a line still in the code that some test covered at base is covered by none now. Times the deleted, added and changed tests on both sides. |
| `testopt dash [--once] [--interval S]` | A terminal view for a tmux pane: the newest job's progress and ETA, the latest run's top 10, and the ledger's totals. Redraws itself. |
| `testopt ledger` / `testopt ledger add --test T --kind K --status kept\|rejected\|tried --before S --after S --reason R` | The ledger: every candidate tried, its numbers and reason, its totals, and the next candidates (the latest run's slowest tests not yet in it). |
| `testopt job [ID]` | A job's progress and result. |
| `testopt tmux-kill [--tmux-session NAME]` | Kills the job session, but never the session testopt runs inside. |

A test is named `<binary-id> <name>` as nextest lists it, or by its name
alone or the end of its name (`exclusive::a_cut_…`) when that names exactly
one test.

### How `why` works, and why that way

`why` must not edit the target, so it measures from outside:

- **Static scan** of the test's function and of the helpers it calls, up to
  3 calls deep. Comments and strings are masked so braces and calls match.
  It cites every `sleep(Duration::…)` with its seconds, every `sleep N` in
  a shell string, every limit written as `*_seconds: N` or `timeout…`, and
  every `recv_timeout`-style wait. Each one is marked as a **poll** (inside
  `loop` / `while`, so it waits until something holds), **repeated**
  (inside `for`), or **fixed**. Fixed seconds are multiplied by the number
  of call sites that reach them.
- **Process sampling** while the test runs alone. Every 20 ms the sampler
  lists the host's processes (libproc on macOS, `/proc` on Linux) and keeps
  the descendants of the test's own process: pid, exact start time from the
  kernel, last sighting, command line, and CPU time. That gives a count per
  program and per command, the seconds with any child alive, peak
  concurrency, and the longest processes.

Sampling was chosen over the alternatives because they all fail the
"no edits, no privileges" rule. A `PATH` shim misses absolute paths.
dtrace and eBPF need root or a SIP change. Editing the test or wrapping
`Command` changes the code under study. Sampling costs about 1–3 ms of CPU
per sample. It can miss a process that lives for less than the interval,
so its counts are a floor, and the output says so.

The kind it reports first: **fixed-wait** when fixed sleeps hold at least a
quarter of the test's time; otherwise **process-heavy** (≥ 20 processes,
or children alive for at least half the time); then **poll** and **cpu**.
All the kinds that apply are listed with their evidence.

### How `covers` and `check` measure coverage per test

`cargo llvm-cov show-env` gives the instrumented build's environment, and
those builds go to `<target>/testopt-cov`. nextest runs each test in its
own process, through a target runner. testopt is that runner: it gives each
test its own `LLVM_PROFILE_FILE` directory (nextest names the test in
`NEXTEST_BINARY_ID` / `NEXTEST_TEST_NAME`). Processes the test spawns
inherit that directory, so one nextest run measures many tests at once.
Each directory is merged with `llvm-profdata` and exported with `llvm-cov`.
Coverage counts the code under test only: files under `tests/`,
`#[cfg(test)]` modules and `#[test]` functions are left out. Results are
cached per tree, keyed by a hash of the sources.

`check` copies the base with `git archive` into the store, so the target
repository gains no worktree and no checkout. It builds that copy in target
dirs of its own: cargo names a workspace crate's artifacts the same
whatever its path, so a shared dir would find one tree's build fresh for
the other.

## Store

The store lives at `$TESTOPT_HOME` (default `~/.local/share/testopt`), in
`<repo>-<root commit>/`. Every worktree and copy of one repository shares
it.

- `runs.jsonl`: one line per `record` (append-only)
- `times.jsonl`: one line per `time`
- `ledger.jsonl`: the ledger, the only home of optimization state
- `coverage/<tree>/`: per-test covered lines
- `jobs/<id>/`: `log`, `progress.json`, `result.json` of a job
- `src/<commit>/`: base copies for `check`

## tmux

`--tmux` (on `record` and `check`) runs the job in a window of the tmux
session `testopt` and waits on the job's `result.json`, never on the pane:

- window `dash`: `testopt dash` for the workspace, made with the session.
- window `run-<job id>`: the job. It stays open after the job ends, until
  Enter is pressed.

The window gets the caller's environment through a script in the job's
directory. `--detach` returns the job id at once, and `testopt job` reads
it later. `--wait-timeout` bounds the wait (default 3 h). After that the
job keeps running and the caller gets an error naming it.

```sh
tmux attach -t testopt     # watch: window 0 is the dash, the rest are jobs
```

`--tmux-session NAME` picks another session (default `testopt`). testopt
never uses or kills the session it runs inside. When `$TMUX` says it runs
in a session of that name, `--tmux` and `testopt tmux-kill` refuse with an
error, so an agent working in tmux cannot end itself. Use `testopt
tmux-kill`, not a raw `tmux kill-session`, to clear the job session.

## The one-session optimization loop

1. **Record** the suite once, in tmux:
   `testopt --tmux record --filter-file .config/slow-tests --profile slow`
2. **Rank**: `testopt rank --top 30`. A second saved on a test is worth
   1/threads of wall time. Look at the middle bands, not only the top.
3. **Resume**: `testopt ledger` shows what was tried and the next
   untried candidates.
4. **Why**: `testopt why <test>`, for its kind, seconds and the cited
   lines.
5. **Change** the test: replace a fixed sleep with a wait on the condition,
   reduce poll rounds, spawn fewer processes, or delete one of two tests
   that cover the same lines (`testopt covers <test> --against …`).
6. **Measure**:
   - `testopt time <test> -n 5`, before and after. Compare only times that
     are not flagged.
   - `testopt check <base> -E '<the touched tests' binary or file>'`.
     It must pass: no line loses its only test.
7. **Ledger**:
   `testopt ledger add --test … --kind … --status kept|rejected --before … --after … --reason …`.
   Then go to 3.

## Develop

```sh
cargo clippy --all-targets -- -D warnings
cargo nextest run          # unit tests, and tests/cli.rs against tests/fixture
```

`tests/fixture` is a tiny workspace with:

- a fast test
- a test that sleeps in a helper
- a test that spawns processes
- two tests where one's coverage is a subset of the other's
