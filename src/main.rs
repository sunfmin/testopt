//! testopt: find where a cargo-nextest suite's time goes, and check that a
//! faster suite kept its coverage. JSON on stdout; progress on stderr and in
//! the job's log.

mod coverage;
mod dash;
mod env;
mod host;
mod jobs;
mod ledger;
mod nextest;
mod probe;
mod procs;
mod record;
mod schedule;
mod source;
mod store;
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
        /// fixed-wait | poll | process-heavy | redundant | setup | other
        #[arg(long)]
        kind: String,
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
    match run(cli, &args[1..]) {
        Ok((v, code)) => {
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
    let long = matches!(cli.cmd, Cmd::Record { .. } | Cmd::Check { .. });
    if cli.tmux && long {
        let kind = if matches!(cli.cmd, Cmd::Record { .. }) {
            "record"
        } else {
            "check"
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
        let code = if kind == "check" && v["pass"] == false {
            1
        } else {
            0
        };
        return Ok((v, code));
    }
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
        } => Ok((
            probe::why(&ws, &store, &test, &profile, interval_ms, depth)?,
            0,
        )),
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
            profile,
        } => {
            let id = probe::resolve(&ws, &store, &test)?;
            Ok((
                coverage::covers(&ws, &store, &id, against.as_deref(), &profile)?,
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
                if let Some(job) = from_check {
                    return Ok((
                        ledger::add_from_check(&ws, &store, &job, &kind, &status, &change)?,
                        0,
                    ));
                }
                let test =
                    test.ok_or_else(|| anyhow::anyhow!("--test is needed (or --from-check)"))?;
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
                Ok((ledger::add(&ws, &store, entry)?, 0))
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
