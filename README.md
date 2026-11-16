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

Install or update testopt with `cargo install`, which writes a new file. Do
not `cp` over an installed binary: macOS kills a binary overwritten in place
the next time it runs. A `--tmux` job runs its own copy of testopt, so a
rebuild while it runs leaves it alone.

`tmux` is needed only for `--tmux`. Branch coverage needs a nightly
toolchain; on stable, `covers` and `check` compare lines only.

## Commands

Every command prints JSON on stdout and progress on stderr. `-C <dir>`
names the target workspace (default: the current directory). An answer
starts with `summary`, one line, and `next`, the commands that follow
from it, ready to run. An error is `{"error": …}` with exit 2.

| command | does |
|---|---|
| `testopt status` | Where the optimization stands: the latest run (and whether it is of this tree), jobs without a result, the survey, the ledger, the environment, the tools, and the next command. The first command of a session. |
| `testopt env [set NAME=value… \| unset NAME…]` | The target's environment: what its own gate exports before its tests run. It is kept once in the store and applied to every command. Values may name `{root}` (the workspace root) and `{commit}` (its HEAD). A `TMPDIR` is created if it is missing. |
| `testopt record [--filter-file F \| -E EXPR] [--profile P] [--test-threads N] [-- nextest args]` | Builds, then runs the tests under the profile and appends the run to the store: per-test JUnit times and start order, commit, host, load at start and end, threads, and nextest's own summary. `--filter-file` reads `<binary-id> <test name>` lines and builds the same filter afkconvoy's `gate.sh` builds. |
| `testopt rank [--top N] [--run ID]` | The latest run's totals, time bands, tests by time and share, per-file totals, each test's previous time, and the trend across runs. Each test gets two wall savings: `est_wall_saving_s` (seconds / threads) and `replayed_wall_saving_s`. The replayed one comes from running the run's schedule again with and without the test (see below). |
| `testopt why <test>` | Runs the test alone and says where its time goes (see below). |
| `testopt time <test> [-n N] [--max-load-per-core X] [--strict]` | Runs it alone N times. Gives the median, min, max and spread of wall time, the CPU seconds of its process tree, and the load before and after each run. When load per core passes X (default 1.0), the result is `"reliable": false` with a `flag`; `--strict` exits 3 instead. CPU seconds move far less with load than wall time does. |
| `testopt survey [--top N]` | `why` on the latest run's N slowest tests, one at a time, as one job: each test's kind, time alone, fixed waits, process range and cited lines. Rows are kept in the store, so the ledger's candidates know their kind. |
| `testopt covers <test> [--against EXPR] [--peers file\|binary]` | `--peers` also measures the tests of its file or binary, with no filter to write. The lines and branch arms the test covers. For every other test measured on the same tree, and those `--against` selects: shared lines and arms, and whether either test is within the other (lines and arms both). Also what no other test covers, and how many profiles no test could be told from. |
| `testopt check <base-ref> [--filter-file F \| -E EXPR] [-n N]` | Compares the working tree with base (see below). Exits 1 when a line or branch arm still in the code, covered by some test at base, is covered by none now. Times the deleted, added and changed tests, base and working tree interleaved. |
| `testopt dash [--once] [--interval S]` | A terminal view for a tmux pane: the newest job's progress and ETA, the latest run's top 10, and the ledger's totals. Redraws itself. |
| `testopt ledger` | The ledger: every entry, its numbers and reason; totals over each test's newest entry; and the next candidates (the latest run's slowest tests not yet in it), each with its survey kind and its `next` step. |
| `testopt ledger add --test T --status kept\|rejected\|tried [--kind K] [--before S --after S \| --from-times] --reason R` | Adds an entry, under the test's full id. `--kind` defaults to the kind `survey` found (or `redundant` for a deleted test). Answers with the ledger's totals and the next candidate. `--from-times` takes before and after from the test's newest two `time` records, and says so when either was flagged. |
| `testopt ledger add --from-check JOB\|latest --status S [--kind K] [--reason R]` | One entry per test a `check` job touched, with the check's timings and verdict after your reason. |
| `testopt job [ID]` | A job's progress and result. |
| `testopt wait [ID] [--timeout S]` | Waits for a job (the newest without a result, by default), with a progress line every 30 s, and answers as the command it ran. |
| `testopt tmux-kill [--tmux-session NAME]` | Kills the job session, but never the session testopt runs inside. |

A test is named `<binary-id> <name>` as nextest lists it, or by its name
alone or the end of its name (`exclusive::a_cut_…`) when that names exactly
one test.

### How `rank` estimates a wall saving

`seconds / threads` treats the suite as perfectly packed. That is wrong at
both ends. A long test that starts last sets the end of the run alone. A
short test in a busy middle saves nothing.

nextest's JUnit has each test's start time. `rank` replays the run: the
tests in their start order, each on the first of the run's threads to come
free. It replays once with every test and once without each ranked test.
The difference is that test's `replayed_wall_saving_s`. `replayed_wall_s`
is the replay's own wall time, so you can check the replay against the
measured run.

### How `why` works, and why that way

`why` must not edit the target, so it measures from outside.

**Static scan.** It parses the test's target with `syn` and follows its
module tree from the root file, through `mod x;` and inline modules. A
test's function is found by its exact module path. A test that a macro
generates is located at the line that names it.

Calls are followed up to 3 deep:

- An unqualified name resolves to this module, then each enclosing module.
- A path resolves as a module path, through `crate::`, `super::` and the
  workspace's other crates.
- A method, or a name found only because it is unique, resolves only to
  code the caller can see: its own file, its enclosing modules, or a
  module its file names in a `use`.

The scan cites:

- every `sleep(Duration::…)`, with its seconds
- every `sleep N` in a shell string, macro arguments included
- every limit written as `*_seconds: N`, `timeout…`, or in a config string
- every `recv_timeout`-style wait

Each one is marked as a **poll** (inside `loop` / `while`), **repeated**
(inside `for` / `for_each`), or **fixed**. Files that do not parse are
listed.

The fixed wait of a test is the larger of two things:

- its fixed sleeps, added up and multiplied by the call sites that reach
  them, counting only sleeps no longer than the test;
- the largest limit the test ran up to, meaning one between half and all of
  its time.

The two are not summed, because a limit runs beside a sleep: the sleep is
often there to outlast it.

**Process sampling.** While the test runs alone, the sampler lists the
host's processes every 20 ms (libproc on macOS, `/proc` on Linux). It keeps
the descendants of the test's own process: pid, kernel start time, last
sighting, command line, and CPU time. That gives:

- a count per program and per command
- the seconds with any child alive
- peak concurrency
- the longest processes

A process that lives for less than the interval can be missed. pids are
handed out in order, so the pids the host handed out during the test that
no sample saw bound the misses. The count is reported as a range:
`spawned` is a floor and `spawned_at_most` a ceiling.

Sampling was chosen over the alternatives because they all fail the "no
edits, no privileges" rule:

- a `PATH` shim misses absolute paths;
- dtrace and eBPF need root or a SIP change;
- editing the test or wrapping `Command` changes the code under study.

The kind `why` reports first:

1. **fixed-wait**, when the fixed wait holds at least a quarter of the
   test's time;
2. otherwise **process-heavy** (≥ 20 processes, or children alive for at
   least half the time);
3. then **poll** and **cpu**.

All the kinds that apply are listed with their evidence.

### How `covers` and `check` measure coverage per test

`cargo llvm-cov show-env --branch` gives the instrumented build's
environment, and those builds go to `<target>/testopt-cov/<hash of the compiler flags>` (cargo does not fingerprint flags passed through cargo-llvm-cov's rustc wrapper, so a build made with other flags would otherwise look fresh). nextest runs
each test in its own process, through a target runner. testopt is that
runner: it gives each test its own `LLVM_PROFILE_FILE` directory (nextest
names the test in `NEXTEST_BINARY_ID` / `NEXTEST_TEST_NAME`). Processes the
test spawns inherit that directory, so one nextest run measures many tests
at once. Each directory is merged with `llvm-profdata` and exported with
`llvm-cov`.

Coverage is measured in two units: lines, and the branch arms taken (lcov's
`BRDA`). Two tests that cover one line down different arms do not overlap
there. Coverage counts the code under test only: files under `tests/`,
`#[cfg(test)]` modules and `#[test]` functions are left out.

Some processes never write a profile under a test's directory:

- **Build scripts and listings** write to a catch-all directory in the
  store, never into the target's tree.
- **A child started with a cleared environment** writes
  `default_*.profraw` where it runs. testopt moves those files out of the
  target's tree and counts them as `unattributed_profiles`. Their lines
  count for no test, and the count says so.

Results are cached per tree, keyed by a hash of the sources and the cache
format.

`check` copies the base with `git archive` into the store, so the target
repository gains no worktree and no checkout. It builds that copy in target
dirs of its own: cargo names a workspace crate's artifacts the same
whatever its path, so a shared dir would find one tree's build fresh for
the other. Base lines map to working-tree lines through `git diff -U0`.
Lines the diff removes are not counted as lost.

**Scope.** With `-E` or `--filter-file`, `check` measures those tests.
Without them, it picks the scope from the diff:

- every test whose function is in a changed file, each side located in its
  own tree;
- every test that covered a changed file of code under test in coverage
  already cached for the base.

A changed file that no cached coverage reaches is listed in
`scope_unknown_for`, and the scope may miss its tests. Give `-E`, or run
`covers` first, to widen it.

**Timing.** A changed test is timed in both trees interleaved (base, head,
head, base, …), so both sides meet the same load.

## Store

The store lives at `$TESTOPT_HOME` (default `~/.local/share/testopt`), in
`<repo>-<root commit>/`. Every worktree and copy of one repository shares
it.

- `env.json`: the target's environment (`testopt env`)
- `runs.jsonl`: one line per `record` (append-only)
- `times.jsonl`: one line per `time`
- `ledger.jsonl`: the ledger, the only home of optimization state
- `coverage/<tree>/`: per-test covered lines and arms, and unattributed
  profiles
- `jobs/<id>/`: `log`, `progress.json`, `result.json` and the testopt
  binary of a job
- `src/<commit>/`: base copies for `check`

## tmux

`--tmux`, on any command that runs tests (`record`, `check`, `survey`,
`covers`, `time`, `why`), runs it as a job in a window of the job
session (`--tmux-session`, default `testopt`). The caller waits on the
job's `result.json`, never on the pane:

- window `dash`: `testopt dash` for the workspace, made with the session.
- window `run-<job id>`: the job, run by its own copy of testopt. It stays
  open for 60 s after the job succeeds, and until Enter after it fails.
  The exit code is shown.

`--tmux` on a command that returns at once (`rank`, `ledger`, …) is an
error, not a silent foreground run.

The window gets the caller's environment through a script in the job's
directory. `--detach` returns the job id at once, and `testopt job` reads
it later; `testopt wait` waits for it. `--wait-timeout` bounds the wait (default 3 h). After that the
job keeps running and the caller gets an error naming it.

```sh
tmux attach -t testopt     # watch: window 0 is the dash, the rest are jobs
```

testopt never uses or kills the session it runs inside. When `$TMUX` says
it runs in the target session, `--tmux` and `testopt tmux-kill` refuse with
an error, so an agent working in tmux cannot end itself. Use
`testopt tmux-kill`, not a raw `tmux kill-session`, to clear the job
session.

## The one-session optimization loop

0. **Once per target**, record its gate's environment:
   `testopt env set RUSTUP_TOOLCHAIN=… TMPDIR={root}/…`
1. **Record** the suite, in tmux:
   `testopt --tmux record --filter-file .config/slow-tests --profile slow`
2. **Rank**: `testopt rank --top 30`. Sort by `replayed_wall_saving_s`.
   Look at the middle bands too, not only the top.
3. **Resume**: `testopt status`, then `testopt ledger`: what was tried,
   and the next untried candidates with their `next` step.
4. **Why**: `testopt --tmux survey --top 20` once, or `testopt why <test>`,
   for each test's kind, seconds, cited lines and process range.
5. **Change** the test:
   - replace a fixed sleep with a wait on the condition;
   - reduce poll rounds;
   - spawn fewer processes;
   - or delete one of two tests that cover the same lines and arms
     (`testopt --tmux covers <test> --peers file`).
6. **Measure**:
   - `testopt time <test> -n 5`, before and after. Compare CPU seconds when
     the host is loaded, and wall times only when neither is flagged.
   - `testopt check <base>`. It must pass: no line or arm loses its only
     test.
7. **Ledger**: `testopt ledger add --from-check latest --status kept|rejected`.
   Then go to 3.

## Develop

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo nextest run          # unit tests, and tests/cli.rs against tests/fixture
```

CI (`.github/workflows/ci.yml`) runs the same on Linux and macOS.

`tests/fixture` is a tiny workspace with:

- a fast test
- a test that sleeps in a helper
- a test that spawns processes
- two tests where one's coverage is a subset of the other's
- two tests that cover one line down different branch arms
- a `slow` nextest profile of its own
