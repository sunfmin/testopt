//! What a caller — most often an AI session — needs first from each
//! command: one `summary` line, and `next`, the commands that follow from the
//! result, ready to run. Kept here, in one place, so every command answers
//! the same way.

use serde_json::{Value, json};

fn q(test: &str) -> String {
    format!("'{}'", test.replace('\'', "'\\''"))
}

fn n(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// Adds `summary` and `next` to the result of command `cmd`.
pub fn annotate(cmd: &str, v: &mut Value) {
    let (summary, next): (String, Vec<String>) = match cmd {
        "record" => (
            format!(
                "{} tests, {:.0} test-s, {:.0}s wall at {} threads (load {:.0}→{:.0}); {} failed",
                v["tests"],
                n(&v["test_seconds"]),
                n(&v["wall_s"]),
                v["threads"],
                n(&v["load_start"]),
                n(&v["load_end"]),
                v["failed"].as_array().map(|a| a.len()).unwrap_or(0)
            ),
            vec![
                "testopt rank".into(),
                "testopt --tmux survey --top 20".into(),
            ],
        ),
        "rank" => {
            let top = &v["tests"][0];
            let name = format!(
                "{} {}",
                top["binary_id"].as_str().unwrap_or(""),
                top["name"].as_str().unwrap_or("")
            );
            let bands: Vec<String> = v["bands"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| {
                    format!(
                        "{} {} tests {:.0}%",
                        b["band"].as_str().unwrap_or(""),
                        b["tests"],
                        100.0 * n(&b["share"])
                    )
                })
                .collect();
            (
                format!(
                    "{} tests, {:.0} test-s, {:.0}s wall at {}; slowest {:.1}s (saves {:.1}s wall); {}",
                    v["totals"]["tests"],
                    n(&v["totals"]["test_seconds"]),
                    n(&v["totals"]["wall_s"]),
                    v["totals"]["threads"],
                    n(&top["seconds"]),
                    n(&top["replayed_wall_saving_s"]),
                    bands.join(", ")
                ),
                vec![
                    format!("testopt why {}", q(&name)),
                    "testopt --tmux survey --top 20".into(),
                ],
            )
        }
        "why" => {
            let test = v["test"].as_str().unwrap_or("").to_string();
            let kind = v["kind"].as_str().unwrap_or("?");
            let cited: Vec<String> = v["kinds"][0]["cited"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| format!("{} {}s", c["at"].as_str().unwrap_or(""), c["seconds"]))
                .collect();
            let procs = &v["processes"];
            let range = match procs["spawned_at_most"].as_u64() {
                Some(m) if m != procs["spawned"].as_u64().unwrap_or(0) => {
                    format!("{}–{m}", procs["spawned"])
                }
                _ => procs["spawned"].to_string(),
            };
            let mut next = vec![];
            match kind {
                "fixed-wait" => next.push(format!(
                    "replace the wait at {} with a wait on its condition, then: testopt time {} -n 5",
                    cited.first().map(String::as_str).unwrap_or("the cited line"),
                    q(&test)
                )),
                "process-heavy" | "poll" => {
                    next.push(format!("testopt covers {} --peers file", q(&test)));
                    next.push(format!("testopt time {} -n 5", q(&test)));
                }
                _ => next.push(format!("testopt time {} -n 5", q(&test))),
            }
            (
                format!(
                    "{test}: {kind}, {:.1}s alone; fixed waits {:.1}s{}; {range} processes ({})",
                    n(&v["seconds"]),
                    n(&v["breakdown"]["fixed_waits_s"]),
                    if cited.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", cited.join(", "))
                    },
                    procs["by_program"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .take(3)
                        .map(|g| format!("{} {}", g["program"].as_str().unwrap_or(""), g["count"]))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                next,
            )
        }
        "time" => {
            let test = v["test"].as_str().unwrap_or("");
            (
                format!(
                    "{test}: median {:.3}s, spread {:.3}s, cpu {:.3}s over {} runs{}",
                    n(&v["median_s"]),
                    n(&v["spread_s"]),
                    n(&v["cpu_median_s"]),
                    v["runs"].as_array().map(|r| r.len()).unwrap_or(0),
                    if v["reliable"] == false {
                        format!(
                            "; FLAGGED: load {}/core — compare cpu, not wall",
                            v["max_load_per_core"]
                        )
                    } else {
                        String::new()
                    }
                ),
                vec![format!(
                    "after a change, time it again, then: testopt ledger add --test {} --from-times --status kept|rejected --reason '…'",
                    q(test)
                )],
            )
        }
        "covers" => {
            let test = v["test"].as_str().unwrap_or("");
            let within: Vec<String> = v["others"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o["this_within_it"] == true)
                .filter_map(|o| o["test"].as_str().map(str::to_string))
                .collect();
            let own = v["covered_by_no_other"].as_u64().unwrap_or(0)
                + v["arms_taken_by_no_other"].as_u64().unwrap_or(0);
            let mut next = vec![];
            if own == 0 && !within.is_empty() {
                next.push(format!("delete it (every line and arm is also covered by {}), then: testopt --tmux check HEAD", within[0]));
            }
            (
                format!(
                    "{test}: {} lines, {} arms; {} lines and {} arms no other compared test covers; within {} other test(s)",
                    v["lines"],
                    v["branch_arms"],
                    v["covered_by_no_other"],
                    v["arms_taken_by_no_other"],
                    within.len()
                ),
                next,
            )
        }
        "check" => (
            format!(
                "{}; lost {} lines, {} arms; touched tests change {:+.3}s",
                if v["pass"] == true { "PASS" } else { "FAIL" },
                v["coverage"]["lost_lines"],
                v["coverage"]["lost_arms"],
                n(&v["tests"]["total_delta_s"])
            ),
            vec![format!(
                "testopt ledger add --from-check latest --status {}",
                if v["pass"] == true {
                    "kept"
                } else {
                    "rejected"
                }
            )],
        ),
        "survey" => {
            let kinds: Vec<String> = v["by_kind"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|k| {
                    format!(
                        "{} {} ({:.1}s wall)",
                        k["kind"].as_str().unwrap_or(""),
                        k["tests"],
                        n(&k["replayed_wall_saving_s"])
                    )
                })
                .collect();
            // What to do about the first fixed wait (the cheapest to remove),
            // else about the slowest test.
            let first = v["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["kind"] == "fixed-wait")
                .or_else(|| v["rows"].get(0))
                .and_then(|r| serde_json::from_value::<crate::survey::Row>(r.clone()).ok())
                .map(|r| crate::survey::step_for(&r.test, Some(&r)));
            (
                format!("{} tests surveyed: {}", v["tests"], kinds.join(", ")),
                first.into_iter().collect(),
            )
        }
        "ledger" => {
            let t = &v["totals"];
            let next = v["candidates"][0]["next"]
                .as_str()
                .map(str::to_string)
                .into_iter()
                .collect();
            (
                format!(
                    "{} entries on {} tests, each at its newest: {} kept ({} test-s, ≈{}s wall), {} rejected, {} tried",
                    t["entries"],
                    t["tests"],
                    t["kept"],
                    t["kept_test_seconds_saved"],
                    t["kept_est_wall_saving_s"],
                    t["rejected"],
                    t["tried"]
                ),
                next,
            )
        }
        "ledger-add" => {
            // One entry (`added` an object) or one per test a check touched
            // (`added` a list of `{added, saved_s}`).
            let entries: Vec<&Value> = match &v["added"] {
                Value::Array(a) => a.iter().map(|e| &e["added"]).collect(),
                e => vec![e],
            };
            let added: Vec<String> = entries
                .iter()
                .map(|e| {
                    format!(
                        "{} {} ({})",
                        e["status"].as_str().unwrap_or(""),
                        e["test"].as_str().unwrap_or(""),
                        e["kind"].as_str().unwrap_or("")
                    )
                })
                .collect();
            let t = &v["totals"];
            (
                format!(
                    "added {}; ledger: {} tests, {} kept ({} test-s, ≈{}s wall), {} rejected, {} tried",
                    added.join("; "),
                    t["tests"],
                    t["kept"],
                    t["kept_test_seconds_saved"],
                    t["kept_est_wall_saving_s"],
                    t["rejected"],
                    t["tried"]
                ),
                v["candidates"][0]["next"]
                    .as_str()
                    .map(str::to_string)
                    .into_iter()
                    .collect(),
            )
        }
        _ => return,
    };
    if let Some(o) = v.as_object_mut() {
        o.insert("summary".into(), json!(summary));
        o.insert("next".into(), json!(next));
    }
}

/// `why` without `--full`: what a decision needs, not every sample.
pub fn compact_why(v: &mut Value) {
    if let Some(p) = v["processes"].as_object_mut() {
        p.remove("by_command");
        if let Some(l) = p.get_mut("longest").and_then(|l| l.as_array_mut()) {
            l.truncate(3);
        }
        if let Some(b) = p.get_mut("by_program").and_then(|b| b.as_array_mut()) {
            b.truncate(6);
        }
    }
    if let Some(s) = v["static"].as_object_mut()
        && let Some(w) = s.get_mut("waits").and_then(|w| w.as_array_mut())
    {
        // The waits that weigh: half a second or more over every call site
        // reaching them, or any in a loop (a poll, or a repeat).
        w.retain(|w| {
            let total = w["seconds"].as_f64().unwrap_or(0.0) * w["reached"].as_f64().unwrap_or(1.0);
            total >= 0.5 || w["in_loop"] == true || w["repeated"] == true
        });
        w.truncate(10);
    }
}
