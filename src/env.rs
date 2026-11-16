//! The target's environment: what its own gate exports before its tests run
//! (a toolchain, a TMPDIR, a build stamp), kept once per target in the store
//! and applied to every testopt command, so a measurement runs the tests as
//! the gate does without each caller copying the gate's exports.
//!
//! Values may name `{root}` (the workspace root) and `{commit}` (its HEAD),
//! so one entry serves every worktree of the repo. A `TMPDIR` is created if
//! it is missing, as a gate creates its own.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::store::{Store, write_json};
use crate::workspace::{Workspace, commit};

const FILE: &str = "env.json";

pub fn load(store: &Store) -> BTreeMap<String, String> {
    std::fs::read(store.dir.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn expand(value: &str, ws: &Workspace) -> String {
    let mut v = value.replace("{root}", &ws.root.display().to_string());
    if v.contains("{commit}") {
        v = v.replace("{commit}", &commit(&ws.root).0.unwrap_or_default());
    }
    v
}

/// The environment to run with, expanded for `ws`.
pub fn resolved(store: &Store, ws: &Workspace) -> Vec<(String, String)> {
    load(store)
        .iter()
        .map(|(k, v)| (k.clone(), expand(v, ws)))
        .collect()
}

/// Sets the target's environment in this process, so every command it
/// runs inherits it. Called once at startup, before any thread starts.
pub fn apply(store: &Store, ws: &Workspace) -> Result<usize> {
    let vars = resolved(store, ws);
    for (k, v) in &vars {
        if k == "TMPDIR" {
            std::fs::create_dir_all(v)?;
        }
        // SAFETY: called at startup, before testopt starts any thread.
        unsafe { std::env::set_var(k, v) };
    }
    Ok(vars.len())
}

pub fn set(store: &Store, pairs: &[String]) -> Result<Value> {
    let mut env = load(store);
    for p in pairs {
        let Some((k, v)) = p.split_once('=') else {
            bail!("`{p}` is not NAME=value")
        };
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("`{k}` is not a variable name");
        }
        env.insert(k.to_string(), v.to_string());
    }
    write_json(&store.dir.join(FILE), &env)?;
    Ok(json!({"env": env}))
}

pub fn unset(store: &Store, names: &[String]) -> Result<Value> {
    let mut env = load(store);
    for n in names {
        env.remove(n);
    }
    write_json(&store.dir.join(FILE), &env)?;
    Ok(json!({"env": env}))
}

pub fn show(store: &Store, ws: &Workspace) -> Value {
    let env = load(store);
    let resolved: BTreeMap<String, String> = resolved(store, ws).into_iter().collect();
    json!({"file": store.dir.join(FILE), "env": env, "resolved": resolved})
}
