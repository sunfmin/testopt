//! The target: a cargo workspace whose tests nextest runs. Found from a
//! directory, described by `cargo metadata`, never written to (testopt keeps
//! its state in the store, and nextest's own output in the target dir).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub target_dir: PathBuf,
    /// Identity of the repository: `<name>-<root commit>` for git, so every
    /// worktree and copy of one repo shares a store; a path hash otherwise.
    pub id: String,
    pub packages: Vec<Package>,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub dir: PathBuf,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub kind: String,
    pub src_path: PathBuf,
}

impl Workspace {
    pub fn find(dir: &Path) -> Result<Workspace> {
        let out = Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(dir)
            .output()
            .context("running cargo metadata")?;
        if !out.status.success() {
            bail!("cargo metadata in {} failed: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
        }
        let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        let root = PathBuf::from(meta["workspace_root"].as_str().context("no workspace_root")?);
        let target_dir = PathBuf::from(meta["target_directory"].as_str().context("no target_directory")?);
        let mut packages = vec![];
        for p in meta["packages"].as_array().into_iter().flatten() {
            let manifest = PathBuf::from(p["manifest_path"].as_str().unwrap_or_default());
            let targets = p["targets"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| Target {
                    name: t["name"].as_str().unwrap_or_default().to_string(),
                    kind: t["kind"][0].as_str().unwrap_or_default().to_string(),
                    src_path: PathBuf::from(t["src_path"].as_str().unwrap_or_default()),
                })
                .collect();
            packages.push(Package {
                name: p["name"].as_str().unwrap_or_default().to_string(),
                dir: manifest.parent().map(Path::to_path_buf).unwrap_or_default(),
                targets,
            });
        }
        let id = identity(&root);
        Ok(Workspace { root, target_dir, id, packages })
    }

    /// The package a nextest binary id belongs to: `pkg`, `pkg::test`,
    /// `pkg::bin/name`.
    pub fn package_of(&self, binary_id: &str) -> Option<&Package> {
        let name = binary_id.split("::").next()?;
        self.packages.iter().find(|p| p.name == name)
    }

    /// Source files of the target a binary id builds, then the rest of its
    /// package: where a test's function and its helpers are looked for.
    pub fn sources_of(&self, binary_id: &str) -> Vec<PathBuf> {
        let Some(pkg) = self.package_of(binary_id) else { return vec![] };
        let mut files = rust_files(&pkg.dir);
        // The binary's own target directory first, so a name defined in
        // several targets resolves to the one the test is built from.
        let rest = binary_id.split_once("::").map(|(_, r)| r);
        let own_dir = match rest {
            Some(r) => {
                let target = r.trim_start_matches("bin/");
                pkg.targets.iter().find(|t| t.name == target).and_then(|t| t.src_path.parent().map(Path::to_path_buf))
            }
            None => pkg.targets.iter().find(|t| t.kind == "lib" || t.kind == "proc-macro").and_then(|t| t.src_path.parent().map(Path::to_path_buf)),
        };
        if let Some(own) = own_dir {
            files.sort_by_key(|f| !f.starts_with(&own));
        }
        files
    }

    pub fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }
}

/// Every `.rs` file under `dir`, skipping `target` and hidden directories.
pub fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                if name == "target" || name.starts_with('.') {
                    continue;
                }
                // A nested workspace or package is not this package's code.
                if p != dir && p.join("Cargo.toml").exists() {
                    continue;
                }
                stack.push(p);
            } else if name.ends_with(".rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn identity(root: &Path) -> String {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).current_dir(root).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if let Some(roots) = git(&["rev-list", "--max-parents=0", "HEAD"]) {
        let first = roots.lines().last().unwrap_or_default();
        let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]).unwrap_or_default();
        let common = Path::new(&common);
        let name = if common.file_name().is_some_and(|n| n == ".git") { common.parent() } else { Some(common) }
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into());
        return format!("{name}-{}", &first[..first.len().min(12)]);
    }
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "ws".into());
    format!("{name}-{:016x}", fnv(root.to_string_lossy().as_bytes()))
}

pub fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The commit `root` is at, and whether its tree has changes.
pub fn commit(root: &Path) -> (Option<String>, bool) {
    let rev = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let dirty = rev.is_some()
        && Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=no"])
            .current_dir(root)
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);
    (rev, dirty)
}
