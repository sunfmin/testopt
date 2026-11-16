//! testopt: find where a cargo-nextest suite's time goes, and check that a
//! faster suite kept its coverage. JSON on stdout; progress on stderr and in
//! the job's log.

mod coverage;
mod dash;
mod env;
mod hints;
mod host;
mod jobs;
mod ledger;
mod nextest;
mod probe;
mod procs;
mod record;
mod schedule;
mod source;
mod status;
mod store;
mod survey;
mod workspace;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

use record::Selection;
use store::Store;
use workspace::Workspace;

#[derive(Parser)]
#[command(
    name = "testopt",
    version,
    about = "Find where a cargo-nextest suite's time goes"
)]
struct Cli {
    /// The target workspace (any directory in it).
    #[arg(short = 'C', long = "workspace", global = true, default_value = ".")]
    workspace: PathBuf,
    /// Run a long job (record, check) in a window of the `testopt` tmux
    /// session and wait on its result file.
    #[arg(long, global = true)]
    tmux: bool,
    /// The tmux session --tmux puts jobs in (never the one testopt runs inside).
    #[arg(long, global = true, default_value = "testopt")]
    tmux_session: String,
    /// With --tmux: how long to wait for the result, in seconds.
    #[arg(long, global = true, default_value_t = 3 * 3600)]
    wait_timeout: u64,
    /// With --tmux: return the job id at once instead of waiting.
    #[arg(long, global = true)]
    detach: bool,
    /// The job this process runs as (set by --tmux).
    #[arg(long, global = true, hide = true)]
    job: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the tests with nextest and keep each one's time.
    Record {
        /// A file of `<binary-id> <test name>` lines (afkconvoy's .config/slow-tests).
        #[arg(long)]
        filter_file: Option<PathBuf>,
        /// A nextest filter expression.
        #[arg(short = 'E', long = "expr")]
        expr: Option<String>,
        /// The nextest profile to run under.
        #[arg(long, default_value = "default")]
        profile: String,
        #[arg(long)]
        test_threads: Option<usize>,
        /// Further `cargo nextest run` arguments.
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// Rank the latest run's tests by time, share and estimated wall saving.
    Rank {
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// A run id other than the latest.
        #[arg(long)]
        run: Option<String>,
    },
    /// Say where one test's time goes: fixed waits, polls, processes, setup.
    Why {
        test: String,
        #[arg(long, default_value = "default")]
        profile: String,
        /// Process sampling interval.
        #[arg(long, default_value_t = 20)]
        interval_ms: u64,
        /// How many calls deep to follow helpers for waits.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Every wait and process, not only those that weigh.
        #[arg(long)]
        full: bool,
    },
    /// Run one test alone N times: median, spread and load.
    Time {
        test: String,
        #[arg(short = 'n', default_value_t = 5)]
        n: usize,
        #[arg(long, default_value = "default")]
        profile: String,
        /// Load per core above which a time is flagged unreliable.
        #[arg(long, default_value_t = 1.0)]
        max_load_per_core: f64,
        /// Refuse (exit 3) instead of flagging an unreliable time.
        #[arg(long)]
        strict: bool,
    },
    /// Lines one test covers, and which other tests cover the same lines.
    Covers {
        test: String,
        /// Also measure these tests (a nextest filter) to compare with.
        #[arg(long)]
        against: Option<String>,
        /// Also measure its peers: `file` (tests in its file) or `binary`.
        #[arg(long)]
        peers: Option<String>,
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Compare the working tree with a base: touched tests' times, and
    /// whether any line covered at base is covered no more.
    Check {
        base: String,
        #[arg(long)]
        filter_file: Option<PathBuf>,
        #[arg(short = 'E', long = "expr")]
        expr: Option<String>,
        #[arg(long, default_value = "default")]
        profile: String,
        /// Timed runs of each touched test, per side.
        #[arg(short = 'n', default_value_t = 3)]
        n: usize,
        #[arg(long, default_value_t = 1.0)]
        max_load_per_core: f64,
    },
    /// A terminal view for a tmux pane: job progress, top 10, ledger.
    Dash {
        /// Print one frame and exit.
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
    /// Read the ledger, or add an entry to it.
    Ledger {
        #[command(subcommand)]
        add: Option<LedgerCmd>,
        /// How many next candidates to list.
        #[arg(long, default_value_t = 10)]
        next: usize,
    },
    /// Where the optimization stands: run, jobs, survey, ledger, and what to
    /// run next. The first command of a session.
    Status,
    /// `why` on the latest run's slowest N tests, one at a time: one table.
    Survey {
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Wait for a job (the newest without a result, or the one named) and
    /// print its result.
    Wait {
        job: Option<String>,
        /// Seconds to wait at most.
        #[arg(long, default_value_t = 3 * 3600)]
        timeout: u64,
    },
    /// A job's state: the newest, or the one named.
    Job { id: Option<String> },
    /// The target's environment, applied to every command: show it, or
    /// `set NAME=value...` (values may name {root} and {commit}) or `unset NAME...`.
    Env {
        #[command(subcommand)]
        change: Option<EnvCmd>,
    },
    /// Kill the job tmux session (--tmux-session); refuses the one testopt runs in.
    TmuxKill,
}

#[derive(Subcommand)]
enum EnvCmd {
    Set { pairs: Vec<String> },
    Unset { names: Vec<String> },
}

#[derive(Subcommand)]
enum LedgerCmd {
    Add {
        /// `<binary-id> <name>` of the test (or a name for the change).
        #[arg(long)]
        test: Option<String>,
        /// fixed-wait | poll | process-heavy | redundant | setup | other.
        /// Default: the kind `survey` found, or `redundant` for a deleted test.
        #[arg(long)]
        kind: Option<String>,
        /// tried | kept | rejected
        #[arg(long)]
        status: String,
        #[arg(long, default_value = "")]
        change: String,
        #[arg(long)]
        before: Option<f64>,
        #[arg(long)]
        after: Option<f64>,
        #[arg(long)]
        threads: Option<usize>,
        #[arg(long)]
        reason: Option<String>,
        /// One entry per test a check job touched, its numbers and verdict
        /// from the job (a job id, or `latest`).
        #[arg(long)]
        from_check: Option<String>,
        /// Before and after from the test's newest two `time` records.
        #[arg(long)]
        from_times: bool,
    },
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__cov-runner") {
        coverage::runner(&args[2..]);
    }
    let cli = Cli::parse();
    let name = match &cli.cmd {
        Cmd::Record { .. } => "record",
        Cmd::Rank { .. } => "rank",
        Cmd::Why { .. } => "why",
        Cmd::Time { .. } => "time",
        Cmd::Covers { .. } => "covers",
        Cmd::Check { .. } => "check",
        Cmd::Survey { .. } => "survey",
        Cmd::Ledger { add: None, .. } => "ledger",
        Cmd::Ledger { add: Some(_), .. } => "ledger-add",
        Cmd::Status => "status",
        _ => "",
    };
    match run(cli, &args[1..]) {
        Ok((mut v, code)) => {
            hints::annotate(name, &mut v);
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            std::process::exit(code);
        }
        Err(e) => {
            eprintln!("testopt: {e:#}");
            println!("{}", serde_json::json!({"error": format!("{e:#}")}));
            std::process::exit(2);
        }
    }
}

fn run(cli: Cli, raw_args: &[String]) -> Result<(Value, i32)> {
    let ws = Workspace::find(&cli.workspace)?;
    let store = Store::of(&ws)?;
    // The target's own environment first: cargo metadata itself may depend
    // on it (a toolchain), so the workspace is read again under it.
    let ws = if env::apply(&store, &ws)? > 0 {
        Workspace::find(&cli.workspace)?
    } else {
        ws
    };
    // The commands that run tests, and so can take minutes: each can run as
    // a tmux job. A short one asked to is refused, not run in the foreground.
    let kind = match cli.cmd {
        Cmd::Record { .. } => Some("record"),
        Cmd::Check { .. } => Some("check"),
        Cmd::Survey { .. } => Some("survey"),
        Cmd::Covers { .. } => Some("covers"),
        Cmd::Time { .. } => Some("time"),
        Cmd::Why { .. } => Some("why"),
        _ => None,
    };
    let strict = matches!(cli.cmd, Cmd::Time { strict: true, .. });
    if cli.tmux {
        let Some(kind) = kind else {
            anyhow::bail!(
                "--tmux runs a command that runs tests (record, check, survey, covers, time, why); this one returns at once"
            );
        };
        let job = jobs::start_in_tmux(&store, &ws.root, &cli.tmux_session, kind, raw_args)?;
        eprintln!(
            "[testopt] job {} started in tmux window {}:run-{}; waiting on {}",
            job.id,
            cli.tmux_session,
            job.id,
            job.result_path().display()
        );
        if cli.detach {
            return Ok((
                serde_json::json!({"job": job.id, "result": job.result_path()}),
                0,
            ));
        }
        let v = jobs::wait(&job.dir, Duration::from_secs(cli.wait_timeout))?;
        return Ok((v.clone(), exit_code(kind, &v, strict)));
    }
    // Run as a job (by --tmux): commands that keep no job of their own get
    // one here, so the waiting caller finds their result.
    let own_job = match (&cli.job, kind) {
        (Some(id), Some("covers" | "time" | "why")) => Some(jobs::Job::open(&store, id)?),
        _ => None,
    };
    let r = run_cmd(cli, ws, store);
    if let Some(job) = own_job {
        job.finish(
            &r.as_ref()
                .map(|(v, _)| v.clone())
                .map_err(|e| anyhow::anyhow!("{e:#}")),
        )?;
    }
    r
}

/// The exit code of a result: 1 for a failed check, 3 for a flagged time
/// under --strict, 0 otherwise.
fn exit_code(kind: &str, v: &Value, strict: bool) -> i32 {
    match kind {
        "check" if v["pass"] == false => 1,
        "time" if strict && v["reliable"] == false => 3,
        _ => 0,
    }
}

fn run_cmd(cli: Cli, ws: Workspace, store: Store) -> Result<(Value, i32)> {
    match cli.cmd {
        Cmd::Record {
            filter_file,
            expr,
            profile,
            test_threads,
            extra,
        } => {
            let job = jobs::Job::open(
                &store,
                &cli.job.clone().unwrap_or_else(|| jobs::new_id("record")),
            )?;
            let sel = Selection { filter_file, expr };
            let r = record::record(&ws, &store, &sel, &profile, test_threads, extra, &job);
            job.finish(&r)?;
            Ok((r?, 0))
        }
        Cmd::Rank { top, run } => Ok((record::rank(&ws, &store, top, run.as_deref())?, 0)),
        Cmd::Why {
            test,
            profile,
            interval_ms,
            depth,
            full,
        } => {
            let mut v = probe::why(&ws, &store, &test, &profile, interval_ms, depth)?;
            if !full {
                hints::compact_why(&mut v);
            }
            Ok((v, 0))
        }
        Cmd::Status => Ok((status::status(&ws, &store), 0)),
        Cmd::Survey { top, profile } => {
            let job = jobs::Job::open(
                &store,
                &cli.job.clone().unwrap_or_else(|| jobs::new_id("survey")),
            )?;
            let r = survey::survey(&ws, &store, top, &profile, &job);
            job.finish(&r)?;
            Ok((r?, 0))
        }
        Cmd::Wait { job, timeout } => {
            let dir = match job {
                Some(j) => store.jobs_dir().join(j),
                None => jobs::unfinished(&store)
                    .ok_or_else(|| anyhow::anyhow!("no job without a result"))?,
            };
            let mut v = jobs::wait(&dir, Duration::from_secs(timeout))?;
            // The answer of the command the job ran: its summary and next steps.
            let kind = jobs::kind_of(&dir);
            hints::annotate(&kind, &mut v);
            Ok((v.clone(), exit_code(&kind, &v, false)))
        }
        Cmd::Time {
            test,
            n,
            profile,
            max_load_per_core,
            strict,
        } => {
            let v = probe::time(&ws, &store, &test, &profile, n, max_load_per_core)?;
            let code = if strict && v["reliable"] == false {
                3
            } else {
                0
            };
            Ok((v, code))
        }
        Cmd::Covers {
            test,
            against,
            peers,
            profile,
        } => {
            let id = probe::resolve(&ws, &store, &test)?;
            Ok((
                coverage::covers(
                    &ws,
                    &store,
                    &id,
                    against.as_deref(),
                    peers.as_deref(),
                    &profile,
                )?,
                0,
            ))
        }
        Cmd::Check {
            base,
            filter_file,
            expr,
            profile,
            n,
            max_load_per_core,
        } => {
            let job = jobs::Job::open(
                &store,
                &cli.job.clone().unwrap_or_else(|| jobs::new_id("check")),
            )?;
            let sel = Selection { filter_file, expr };
            let opts = coverage::CheckOpts {
                base: &base,
                filter: sel.filter()?,
                profile: &profile,
                runs: n,
                threshold: max_load_per_core,
            };
            let r = coverage::check(&ws, &store, &opts, &|m| job.note(m));
            job.finish(&r)?;
            let v = r?;
            let code = if v["pass"] == true { 0 } else { 1 };
            Ok((v, code))
        }
        Cmd::Dash { once, interval } => {
            dash::run(&ws, &store, once, Duration::from_secs(interval))?;
            Ok((Value::Null, 0))
        }
        Cmd::Ledger { add, next } => match add {
            None => Ok((ledger::show(&store, next)?, 0)),
            Some(LedgerCmd::Add {
                test,
                kind,
                status,
                change,
                before,
                after,
                threads,
                reason,
                from_check,
                from_times,
            }) => {
                let mut v = if let Some(job) = from_check {
                    ledger::add_from_check(
                        &ws,
                        &store,
                        &job,
                        kind.as_deref(),
                        &status,
                        &change,
                        reason.as_deref(),
                    )?
                } else {
                    let test =
                        test.ok_or_else(|| anyhow::anyhow!("--test is needed (or --from-check)"))?;
                    // One name per test, as `record` and `check` write it.
                    let test = probe::resolve(&ws, &store, &test)?.to_string();
                    let (mut before, mut after, mut reason) = (before, after, reason);
                    if from_times {
                        let (b, a, note) = ledger::from_times(&store, &test)?;
                        before = before.or(Some(b));
                        after = after.or(Some(a));
                        reason = match (reason, note) {
                            (Some(r), Some(n)) => Some(format!("{r}; {n}")),
                            (r, n) => r.or(n),
                        };
                    }
                    let reason = reason.ok_or_else(|| anyhow::anyhow!("--reason is needed"))?;
                    let kind = ledger::kind_for(&store, &test, kind.as_deref(), &change)?;
                    let entry = ledger::Add {
                        test,
                        kind,
                        status,
                        change,
                        before_s: before,
                        after_s: after,
                        threads,
                        reason,
                    };
                    ledger::add(&ws, &store, entry)?
                };
                // Where the ledger stands now, and what to try next.
                let shown = ledger::show(&store, 3)?;
                v["totals"] = shown["totals"].clone();
                v["candidates"] = shown["candidates"].clone();
                Ok((v, 0))
            }
        },
        Cmd::Env { change } => Ok((
            match change {
                None => env::show(&store, &ws),
                Some(EnvCmd::Set { pairs }) => env::set(&store, &pairs)?,
                Some(EnvCmd::Unset { names }) => env::unset(&store, &names)?,
            },
            0,
        )),
        Cmd::TmuxKill => Ok((jobs::kill_session(&cli.tmux_session)?, 0)),
        Cmd::Job { id } => {
            let dir = match id {
                Some(i) => store.jobs_dir().join(i),
                None => jobs::latest(&store).ok_or_else(|| anyhow::anyhow!("no job yet"))?,
            };
            let read = |f: &str| -> Value {
                std::fs::read(dir.join(f))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or(Value::Null)
            };
            Ok((
                serde_json::json!({"job": dir, "progress": read("progress.json"), "result": read("result.json")}),
                0,
            ))
        }
    }
}
