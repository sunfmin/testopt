//! A tiny workspace for testopt's own tests: one fast test, one that sleeps,
//! one that spawns processes, and two whose coverage nests.

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn classify(n: i32) -> &'static str {
    if n < 0 {
        "negative"
    } else if n == 0 {
        "zero"
    } else {
        "positive"
    }
}

/// One line, two arms: tests that take different arms cover the same line.
pub fn larger(a: i32, b: i32) -> i32 { if a >= b { a } else { b } }
