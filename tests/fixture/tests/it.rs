use std::process::Command;
use std::time::Duration;

#[test]
fn fast_adds() {
    assert_eq!(fixture::add(2, 3), 5);
}

#[test]
fn sleeps() {
    settle();
    assert_eq!(fixture::add(1, 1), 2);
}

/// A helper the test calls: the wait lives here, not in the test's body.
fn settle() {
    std::thread::sleep(Duration::from_millis(1500));
}

#[test]
fn spawns_processes() {
    for _ in 0..6 {
        let out = Command::new("sh").args(["-c", "sleep 0.2"]).status().unwrap();
        assert!(out.success());
    }
}

#[test]
fn classifies_every_sign() {
    assert_eq!(fixture::classify(-1), "negative");
    assert_eq!(fixture::classify(0), "zero");
    assert_eq!(fixture::classify(1), "positive");
}

#[test]
fn classifies_a_positive() {
    assert_eq!(fixture::classify(7), "positive");
}
