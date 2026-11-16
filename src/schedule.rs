//! Replaying a run's schedule: nextest starts tests in an order, each on the
//! first of `threads` slots to come free. Replayed with each test's time,
//! that gives the run's wall time; replayed without one test, the wall time
//! the run would have had without it. That is a test's wall saving, which
//! seconds / threads only approximates: a long test that starts last sets
//! the run's end alone, and a short one in a busy middle saves nothing.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Wall time of running `durations`, in order, on `threads` slots.
pub fn wall(durations: &[f64], threads: usize) -> f64 {
    let mut slots: BinaryHeap<Reverse<u64>> = (0..threads.max(1)).map(|_| Reverse(0)).collect();
    let mut end = 0u64;
    for d in durations {
        let Reverse(free) = slots.pop().unwrap_or(Reverse(0));
        let done = free + (d.max(0.0) * 1e6) as u64;
        end = end.max(done);
        slots.push(Reverse(done));
    }
    end as f64 / 1e6
}

/// For each index in `of`, the wall time saved by not running that test.
pub fn savings(durations: &[f64], threads: usize, of: &[usize]) -> Vec<f64> {
    let full = wall(durations, threads);
    of.iter()
        .map(|&i| {
            let without: Vec<f64> = durations
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, d)| *d)
                .collect();
            (full - wall(&without, threads)).max(0.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_slots() {
        // Two slots: 4 and 1 start together, then 1 and 1 follow the 1.
        assert_eq!(wall(&[4.0, 1.0, 1.0, 1.0], 2), 4.0);
        // Dropping the 4 leaves 1+1 on one slot and 1 on the other: 2.
        assert_eq!(savings(&[4.0, 1.0, 1.0, 1.0], 2, &[0, 1]), vec![2.0, 0.0]);
        // A long test started last sets the end alone: all of it is saved
        // past what the others need.
        assert_eq!(wall(&[1.0, 1.0, 5.0], 2), 6.0);
        assert_eq!(savings(&[1.0, 1.0, 5.0], 2, &[2]), vec![5.0]);
    }
}
