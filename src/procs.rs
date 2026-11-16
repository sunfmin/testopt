//! Sampling a process tree while a test runs, without changing the target:
//! every few milliseconds, list the host's processes, keep the descendants
//! of the root, and note each one's start (exact, from the kernel), last
//! sighting, command line and CPU time. A process that starts and ends
//! between two samples is missed; the interval is reported with the result.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
struct Info {
    ppid: u32,
    /// Start, seconds since the epoch.
    start: f64,
    comm: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Seen {
    pub pid: u32,
    pub ppid: u32,
    pub start: f64,
    pub last_seen: f64,
    pub args: Vec<String>,
    pub cpu_s: f64,
}

impl Seen {
    pub fn duration(&self) -> f64 {
        (self.last_seen - self.start).max(0.0)
    }

    /// `git status`, `afkconvoy tick`, `sh -c sleep`: the program's name and
    /// its first word that is not a flag.
    pub fn label(&self) -> String {
        let prog = self
            .args
            .first()
            .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
            .unwrap_or_default();
        let sub = self
            .args
            .iter()
            .skip(1)
            .find(|a| !a.starts_with('-') && !a.contains('/') && a.len() < 40);
        match sub {
            Some(s) => format!("{prog} {}", s.split_whitespace().next().unwrap_or("")),
            None => prog,
        }
    }

    pub fn program(&self) -> String {
        self.args
            .first()
            .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
            .unwrap_or_default()
    }
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<(Vec<Seen>, usize)>,
}

pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl Sampler {
    /// Samples the descendants of `root` every `interval` until stopped.
    pub fn start(root: u32, interval: Duration) -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut tracked: HashMap<u32, Seen> = HashMap::new();
            let mut tracked_start: HashMap<u32, f64> = HashMap::new();
            let mut tracked_comm: HashMap<u32, String> = HashMap::new();
            let mut samples = 0;
            let mut ours: HashSet<u32> = HashSet::from([root]);
            while !flag.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                let now = now_epoch();
                let all = list_all();
                samples += 1;
                // Children of anything already ours become ours; a process
                // reparented after its parent ended stays ours by pid and start.
                let mut changed = true;
                while changed {
                    changed = false;
                    for (pid, info) in &all {
                        if !ours.contains(pid) && ours.contains(&info.ppid) {
                            ours.insert(*pid);
                            changed = true;
                        }
                    }
                }
                for (pid, info) in &all {
                    if !ours.contains(pid) {
                        continue;
                    }
                    let same = tracked_start
                        .get(pid)
                        .is_some_and(|s| (s - info.start).abs() < 1e-3);
                    if !same {
                        tracked_start.insert(*pid, info.start);
                        let key_old = tracked.remove(pid);
                        if let Some(old) = key_old {
                            // A pid reused: keep the old one under a fresh key.
                            tracked.insert(u32::MAX - tracked.len() as u32, old);
                        }
                        tracked.insert(
                            *pid,
                            Seen {
                                pid: *pid,
                                ppid: info.ppid,
                                start: info.start,
                                last_seen: now,
                                args: args_of(*pid).unwrap_or_else(|| vec![info.comm.clone()]),
                                cpu_s: 0.0,
                            },
                        );
                    }
                    let s = tracked.get_mut(pid).unwrap();
                    s.last_seen = now;
                    // A fork that has since exec'd: its command changed.
                    if tracked_comm.get(pid) != Some(&info.comm) {
                        if let Some(a) = args_of(*pid) {
                            s.args = a;
                        }
                        tracked_comm.insert(*pid, info.comm.clone());
                    }
                    if let Some(c) = cpu_of(*pid) {
                        s.cpu_s = c;
                    }
                }
                // Forget pids no longer alive, so a reused pid is not ours.
                ours.retain(|p| *p == root || all.contains_key(p));
                let spent = t0.elapsed();
                if spent < interval {
                    std::thread::sleep(interval - spent);
                }
            }
            (tracked.into_values().collect(), samples)
        });
        Sampler { stop, handle }
    }

    /// Stops sampling: every process seen, and the number of samples taken.
    pub fn finish(self) -> (Vec<Seen>, usize) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap_or_default()
    }
}

/// Seconds during which at least one of `procs` was alive.
pub fn busy_seconds(procs: &[&Seen]) -> f64 {
    let mut spans: Vec<(f64, f64)> = procs
        .iter()
        .map(|p| (p.start, p.last_seen.max(p.start)))
        .collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = 0.0;
    let mut cur: Option<(f64, f64)> = None;
    for (s, e) in spans {
        match cur {
            Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
            Some((cs, ce)) => {
                total += ce - cs;
                cur = Some((s, e));
            }
            None => cur = Some((s, e)),
        }
    }
    if let Some((cs, ce)) = cur {
        total += ce - cs;
    }
    total
}

/// Most processes alive at once.
pub fn peak_concurrency(procs: &[&Seen]) -> usize {
    let mut events: Vec<(f64, i32)> = vec![];
    for p in procs {
        events.push((p.start, 1));
        events.push((p.last_seen.max(p.start), -1));
    }
    events.sort_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)));
    let (mut cur, mut peak) = (0i32, 0i32);
    for (_, d) in events {
        cur += d;
        peak = peak.max(cur);
    }
    peak as usize
}

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub command: String,
    pub count: usize,
    /// Sum of the processes' lifetimes.
    pub seconds: f64,
    pub cpu_s: f64,
}

/// Processes grouped by `key`, most first.
pub fn group_by(procs: &[&Seen], key: impl Fn(&Seen) -> String) -> Vec<Group> {
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for p in procs {
        let k = key(p);
        let g = groups.entry(k.clone()).or_insert(Group {
            command: k,
            count: 0,
            seconds: 0.0,
            cpu_s: 0.0,
        });
        g.count += 1;
        g.seconds += p.duration();
        g.cpu_s += p.cpu_s;
    }
    let mut v: Vec<Group> = groups.into_values().collect();
    v.sort_by(|a, b| b.count.cmp(&a.count).then(b.seconds.total_cmp(&a.seconds)));
    v
}

#[cfg(target_os = "macos")]
fn list_all() -> HashMap<u32, Info> {
    let mut out = HashMap::new();
    let mut pids = vec![0i32; 8192];
    // SAFETY: the buffer's size in bytes is passed; the call fills at most that.
    let n = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    if n <= 0 {
        return out;
    }
    for &pid in pids.iter().take(n as usize) {
        if pid <= 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: `info` is a proc_bsdinfo of `size` bytes.
        let got = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if got != size {
            continue;
        }
        let comm: Vec<u8> = info
            .pbi_comm
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        out.insert(
            pid as u32,
            Info {
                ppid: info.pbi_ppid,
                start: info.pbi_start_tvsec as f64 + info.pbi_start_tvusec as f64 / 1e6,
                comm: String::from_utf8_lossy(&comm).into_owned(),
            },
        );
    }
    out
}

#[cfg(target_os = "macos")]
fn args_of(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query, then a read into a buffer of that size.
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        buf.truncate(size);
        if buf.len() < 4 {
            return None;
        }
        let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
        // argc, the executable path, NUL padding, then argc NUL-ended args.
        let rest = &buf[4..];
        let exe_end = rest.iter().position(|b| *b == 0)?;
        let mut i = exe_end;
        while i < rest.len() && rest[i] == 0 {
            i += 1;
        }
        let args: Vec<String> = rest[i..]
            .split(|b| *b == 0)
            .take(argc)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        Some(args)
    }
}

#[cfg(target_os = "macos")]
fn cpu_of(pid: u32) -> Option<f64> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    // SAFETY: `info` is a proc_taskinfo of `size` bytes.
    let got = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if got != size {
        return None;
    }
    // Times are in mach absolute units.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    let mut tb = Timebase { numer: 1, denom: 1 };
    // SAFETY: fills `tb`.
    unsafe { mach_timebase_info(&mut tb) };
    let ticks = (info.pti_total_user + info.pti_total_system) as f64;
    Some(ticks * tb.numer as f64 / tb.denom.max(1) as f64 / 1e9)
}

#[cfg(target_os = "linux")]
fn list_all() -> HashMap<u32, Info> {
    let mut out = HashMap::new();
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let boot = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("btime"))
                .and_then(|l| l[6..].trim().parse::<f64>().ok())
        })
        .unwrap_or(0.0);
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in dir.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let (Some(l), Some(r)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let comm = stat[l + 1..r].to_string();
        let fields: Vec<&str> = stat[r + 2..].split_whitespace().collect();
        let ppid = fields.get(1).and_then(|f| f.parse().ok()).unwrap_or(0);
        let start = fields
            .get(19)
            .and_then(|f| f.parse::<f64>().ok())
            .unwrap_or(0.0)
            / ticks
            + boot;
        out.insert(pid, Info { ppid, start, comm });
    }
    out
}

#[cfg(target_os = "linux")]
fn args_of(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn cpu_of(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let r = stat.rfind(')')?;
    let f: Vec<&str> = stat[r + 2..].split_whitespace().collect();
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / ticks)
}
