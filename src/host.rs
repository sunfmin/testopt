//! Facts about the host a measurement ran on: its load, its cores, its name.

use serde::{Deserialize, Serialize};

/// The host's state at one moment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Load {
    /// 1-minute load average.
    pub load1: f64,
    /// Logical CPUs.
    pub cores: usize,
}

impl Load {
    pub fn now() -> Load {
        let mut avg = [0f64; 3];
        // SAFETY: getloadavg writes at most `nelem` doubles into `avg`.
        let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
        let load1 = if n >= 1 { avg[0] } else { f64::NAN };
        Load { load1, cores: cores() }
    }

    /// Load per logical CPU: above 1.0 the run queue is longer than the host
    /// has CPUs, and a test's time says as much about its neighbours as
    /// about it.
    pub fn per_core(&self) -> f64 {
        self.load1 / self.cores.max(1) as f64
    }
}

pub fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub fn now_rfc3339() -> String {
    jiff::Timestamp::now().round(jiff::Unit::Second).unwrap_or(jiff::Timestamp::now()).to_string()
}
