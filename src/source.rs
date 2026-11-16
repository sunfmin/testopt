//! Static reading of Rust source, enough to find a test's function, the
//! helpers it calls, and the waits written in them. Not a parser: comments
//! and string contents are masked out so braces and calls can be matched,
//! and waits are read from the raw lines.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::nextest::TestId;
use crate::workspace::Workspace;

/// A source file, raw and masked (same length, same newlines).
pub struct SourceFile {
    pub path: PathBuf,
    pub raw: String,
    pub masked: String,
    line_starts: Vec<usize>,
}

impl SourceFile {
    pub fn load(path: &Path) -> Option<SourceFile> {
        let raw = std::fs::read_to_string(path).ok()?;
        let masked = mask(&raw);
        let mut line_starts = vec![0];
        line_starts.extend(raw.match_indices('\n').map(|(i, _)| i + 1));
        Some(SourceFile {
            path: path.to_path_buf(),
            raw,
            masked,
            line_starts,
        })
    }

    /// 1-based line of byte offset `pos`.
    pub fn line_of(&self, pos: usize) -> usize {
        match self.line_starts.binary_search(&pos) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }

    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line - 1];
        let end = self
            .line_starts
            .get(line)
            .map(|e| e - 1)
            .unwrap_or(self.raw.len());
        &self.raw[start..end]
    }
}

/// Replaces comments and the contents of string and char literals with
/// spaces, keeping newlines and every byte offset.
pub fn mask(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in out.iter_mut().take(to).skip(from) {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = src[i..].find('\n').map(|e| i + e).unwrap_or(b.len());
                blank(&mut out, i, end);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                let mut j = i;
                while j < b.len() {
                    if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                        depth += 1;
                        j += 2;
                    } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, i, j);
                i = j;
            }
            b'r' if (b.get(i + 1) == Some(&b'"') || b.get(i + 1) == Some(&b'#'))
                && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let close: String = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let end = src[j + 1..]
                    .find(&close)
                    .map(|e| j + 1 + e)
                    .unwrap_or(b.len());
                blank(&mut out, j + 1, end);
                i = end + close.len();
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                blank(&mut out, i + 1, j.min(b.len()));
                i = j + 1;
            }
            b'\'' => {
                // A char literal ('x', '\n', '\u{..}') or a lifetime ('a).
                if b.get(i + 1) == Some(&b'\\') {
                    let end = src[i + 2..]
                        .find('\'')
                        .map(|e| i + 2 + e)
                        .unwrap_or(b.len());
                    blank(&mut out, i + 1, end);
                    i = end + 1;
                } else if b.get(i + 2) == Some(&b'\'') {
                    blank(&mut out, i + 1, i + 2);
                    i += 3;
                } else if let Some(c) = src[i + 1..].chars().next()
                    && !c.is_ascii()
                    && src[i + 1 + c.len_utf8()..].starts_with('\'')
                {
                    blank(&mut out, i + 1, i + 1 + c.len_utf8());
                    i += 2 + c.len_utf8();
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// A function found in a file: where its name is and its body's braces.
#[derive(Debug, Clone)]
pub struct FnDef {
    pub file: usize,
    pub name: String,
    pub line: usize,
    pub body: (usize, usize),
}

/// Every source of one package, with an index of function names.
pub struct Sources {
    pub files: Vec<SourceFile>,
    pub fns: HashMap<String, Vec<FnDef>>,
}

impl Sources {
    pub fn of(paths: &[PathBuf]) -> Sources {
        let files: Vec<SourceFile> = paths.iter().filter_map(|p| SourceFile::load(p)).collect();
        let mut fns: HashMap<String, Vec<FnDef>> = HashMap::new();
        for (fi, f) in files.iter().enumerate() {
            for def in functions(fi, f) {
                fns.entry(def.name.clone()).or_default().push(def);
            }
        }
        Sources { files, fns }
    }

    /// The definition of test `id`: a `fn` named as the test's last segment,
    /// preferring the one whose file and enclosing modules match the test's
    /// module path and that carries a test attribute.
    pub fn test_fn(&self, id: &TestId) -> Option<&FnDef> {
        let mut segs: Vec<&str> = id.name.split("::").collect();
        let leaf = segs.pop()?;
        let defs = self.fns.get(leaf)?;
        defs.iter().max_by_key(|d| {
            let f = &self.files[d.file];
            let path = f.path.to_string_lossy();
            let mut score = 0i32;
            for s in &segs {
                if path.contains(&format!("/{s}.rs")) || path.contains(&format!("/{s}/")) {
                    score += 3;
                }
                if f.masked.contains(&format!("mod {s}")) {
                    score += 1;
                }
            }
            let before = &f.raw[f.line_starts[d.line.saturating_sub(4)]..f.line_starts[d.line - 1]];
            if before.contains("#[test]") || before.contains("test]") {
                score += 5;
            }
            score
        })
    }

    pub fn body_text(&self, d: &FnDef) -> &str {
        &self.files[d.file].masked[d.body.0..d.body.1]
    }
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Every `fn name ... { body }` in a file (bodies only; declarations without
/// a body are skipped).
fn functions(fi: usize, f: &SourceFile) -> Vec<FnDef> {
    let m = f.masked.as_bytes();
    let mut out = vec![];
    let mut from = 0;
    while let Some(off) = f.masked[from..].find("fn ") {
        let at = from + off;
        from = at + 3;
        if at > 0 && is_ident(m[at - 1]) {
            continue;
        }
        let name_start = at + 3;
        let name_len = m[name_start..].iter().take_while(|c| is_ident(**c)).count();
        if name_len == 0 {
            continue;
        }
        let name = &f.masked[name_start..name_start + name_len];
        // The body is the first `{` at bracket depth 0 after the name, unless
        // a `;` ends a bodiless declaration first.
        let mut depth = 0i32;
        let mut j = name_start + name_len;
        let mut open = None;
        while j < m.len() {
            match m[j] {
                b'(' | b'[' | b'<' => depth += 1,
                b')' | b']' => depth -= 1,
                b'>' if j > 0 && m[j - 1] != b'-' && m[j - 1] != b'=' => depth -= 1,
                b';' if depth <= 0 => break,
                b'{' if depth <= 0 => {
                    open = Some(j);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let Some(open) = open else { continue };
        let Some(close) = matching(m, open) else {
            continue;
        };
        out.push(FnDef {
            file: fi,
            name: name.to_string(),
            line: f.line_of(name_start),
            body: (open, close + 1),
        });
    }
    out
}

/// The `}` matching the `{` at `open`.
pub fn matching(m: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    for (k, c) in m.iter().enumerate().skip(open) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

const NOT_HELPERS: &[&str] = &[
    "if",
    "while",
    "for",
    "match",
    "loop",
    "return",
    "fn",
    "let",
    "Some",
    "Ok",
    "Err",
    "None",
    "new",
    "unwrap",
    "expect",
    "clone",
    "into",
    "to_string",
    "as_str",
    "iter",
    "map",
    "collect",
    "join",
    "push",
    "len",
    "format",
    "assert",
    "assert_eq",
    "assert_ne",
    "println",
    "eprintln",
    "vec",
    "default",
    "from",
    "to_owned",
    "is_empty",
    "contains",
    "get",
    "insert",
    "unwrap_or",
    "and_then",
    "filter",
    "any",
    "all",
    "write",
    "read",
    "main",
];

/// The names called in `text` (`name(`, `.name(`, `path::name(`).
fn calls(text: &str) -> Vec<(usize, String)> {
    let b = text.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if is_ident(b[i]) && (i == 0 || !is_ident(b[i - 1])) {
            let len = b[i..].iter().take_while(|c| is_ident(**c)).count();
            let mut j = i + len;
            // Turbofish: name::<T>(
            if text[j..].starts_with("::<")
                && let Some(gt) = text[j..].find(">(")
            {
                j += gt + 1;
            }
            if b.get(j) == Some(&b'(') && !b[i].is_ascii_digit() {
                let name = &text[i..i + len];
                if !NOT_HELPERS.contains(&name) {
                    out.push((i, name.to_string()));
                }
            }
            i += len;
        } else {
            i += 1;
        }
    }
    out
}

/// One wait written in the source.
#[derive(Debug, Clone, Serialize)]
pub struct Wait {
    /// sleep | shell-sleep | limit | timeout-wait
    pub kind: String,
    /// Seconds, when written as a literal.
    pub seconds: Option<f64>,
    /// Whether it sits in a `loop` or `while`: a poll, which waits until
    /// something holds, not a fixed time.
    pub in_loop: bool,
    /// Whether it sits in a `for` loop: a fixed wait, repeated.
    pub repeated: bool,
    pub at: String,
    pub code: String,
    /// The call path from the test to the function holding it.
    pub via: Vec<String>,
    /// How many times the path reaches it, counting call sites (not loop
    /// iterations).
    pub reached: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Scan {
    pub test_at: String,
    pub functions_read: usize,
    pub waits: Vec<Wait>,
    /// Seconds of fixed (non-loop) sleeps, times the call sites reaching them.
    pub fixed_wait_s: f64,
    /// Helpers called that poll: a loop holding a sleep.
    pub polls: Vec<String>,
}

/// Reads test `id`'s function and the helpers it reaches, `depth` calls deep.
pub fn scan(ws: &Workspace, id: &TestId, depth: usize) -> Option<Scan> {
    let sources = Sources::of(&ws.sources_of(&id.binary_id));
    let test = sources.test_fn(id)?.clone();
    let at = |d: &FnDef| {
        format!(
            "{}:{}",
            ws.relative(&sources.files[d.file].path).display(),
            d.line
        )
    };
    // Breadth-first through calls; each function read once, with the path
    // that first reached it and how many call sites reach it.
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut reach: BTreeMap<(usize, usize), (Vec<String>, usize)> = BTreeMap::new();
    let mut queue = VecDeque::new();
    queue.push_back((test.clone(), vec![test.name.clone()], 1usize, 0usize));
    let mut order = vec![];
    while let Some((def, path, times, d)) = queue.pop_front() {
        let key = (def.file, def.body.0);
        if let Some(r) = reach.get_mut(&key) {
            r.1 += times;
            continue;
        }
        if seen.len() > 400 {
            break;
        }
        seen.insert(key);
        reach.insert(key, (path.clone(), times));
        order.push(def.clone());
        if d >= depth {
            continue;
        }
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for (_, name) in calls(sources.body_text(&def)) {
            *counts.entry(name).or_default() += 1;
        }
        for (name, n) in counts {
            let Some(defs) = sources.fns.get(&name) else {
                continue;
            };
            // Same file first, then the first defined elsewhere.
            let target = defs
                .iter()
                .find(|x| x.file == def.file && x.body != def.body)
                .or_else(|| defs.first());
            if let Some(t) = target
                && t.body != def.body
            {
                let mut p = path.clone();
                p.push(name.clone());
                queue.push_back((t.clone(), p, times * n, d + 1));
            }
        }
    }
    let mut waits = vec![];
    let mut polls = vec![];
    for def in &order {
        let (via, times) = reach[&(def.file, def.body.0)].clone();
        let f = &sources.files[def.file];
        let found = waits_in(f, def.body);
        if found
            .iter()
            .any(|w| w.in_loop && (w.kind == "sleep" || w.kind == "shell-sleep"))
        {
            polls.push(format!("{} ({})", def.name, at(def)));
        }
        for mut w in found {
            w.at = format!("{}:{}", ws.relative(&f.path).display(), w.at);
            w.via = via.clone();
            w.reached = times;
            waits.push(w);
        }
    }
    let fixed_wait_s = waits
        .iter()
        .filter(|w| !w.in_loop && (w.kind == "sleep" || w.kind == "shell-sleep"))
        .filter_map(|w| w.seconds.map(|s| s * w.reached as f64))
        .sum();
    Some(Scan {
        test_at: at(&test),
        functions_read: order.len(),
        waits,
        fixed_wait_s,
        polls,
    })
}

/// Waits written in `f` between byte offsets `body`.
fn waits_in(f: &SourceFile, body: (usize, usize)) -> Vec<Wait> {
    let first = f.line_of(body.0);
    let last = f.line_of(body.1.saturating_sub(1));
    let mut out = vec![];
    for line in first..=last {
        let raw = f.line_text(line);
        let start = f.line_starts[line - 1];
        let masked = &f.masked[start..start + raw.len()];
        let trimmed = raw.trim();
        let pos = start + raw.len() - raw.trim_start().len();
        let (in_loop, repeated) = inside_loop(f, pos, body);
        let mut push = |kind: &str, seconds: Option<f64>| {
            out.push(Wait {
                kind: kind.into(),
                seconds,
                in_loop,
                repeated,
                at: line.to_string(),
                code: trimmed.chars().take(160).collect(),
                via: vec![],
                reached: 1,
            });
        };
        if masked.contains("sleep(") {
            push("sleep", duration_in(raw));
        } else if masked.contains("recv_timeout(")
            || masked.contains("park_timeout(")
            || masked.contains("wait_timeout(")
        {
            push("timeout-wait", duration_in(raw));
        } else if let Some(s) = shell_sleep(raw, masked) {
            push("shell-sleep", Some(s));
        } else if let Some(s) = limit_in(masked).or_else(|| limit_in(raw)) {
            // In code, or in a config the test writes as a string
            // (`"gate_seconds: 20"`).

            push("limit", Some(s));
        }
    }
    out
}

/// Whether `pos` is inside a `loop` or `while` block within `body` (a
/// poll), and whether inside a `for` (a repeat).
fn inside_loop(f: &SourceFile, pos: usize, body: (usize, usize)) -> (bool, bool) {
    let mut repeated = false;
    let m = f.masked.as_bytes();
    // Walk outwards: each unmatched `{` before pos encloses it.
    let mut depth = 0i32;
    let mut k = pos;
    while k > body.0 {
        k -= 1;
        match m[k] {
            b'}' => depth += 1,
            b'{' if depth > 0 => depth -= 1,
            b'{' => {
                let start = f.masked[..k]
                    .rfind([';', '{', '}'])
                    .map(|s| s + 1)
                    .unwrap_or(0);
                let header = f.masked[start..k].trim();
                // A label (`'outer: loop`) comes before the keyword.
                let header = match header.strip_prefix('\'').and_then(|h| h.split_once(':')) {
                    Some((_, rest)) => rest.trim(),
                    None => header,
                };
                let first = header.split_whitespace().next().unwrap_or("");
                if first == "loop" || first == "while" {
                    return (true, repeated);
                }
                if first == "for" || header.contains(".for_each(") {
                    repeated = true;
                }
            }
            _ => {}
        }
    }
    (false, repeated)
}

/// The literal duration on a line: `from_secs(21)`, `from_millis(500)`,
/// `from_secs_f64(0.5)`, `from_micros(..)`.
pub fn duration_in(line: &str) -> Option<f64> {
    for (pat, scale) in [
        ("from_secs_f64(", 1.0),
        ("from_secs_f32(", 1.0),
        ("from_secs(", 1.0),
        ("from_millis(", 1e-3),
        ("from_micros(", 1e-6),
        ("from_nanos(", 1e-9),
    ] {
        if let Some(i) = line.find(pat) {
            let rest = &line[i + pat.len()..];
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '_')
                .collect();
            if let Ok(v) = num.replace('_', "").parse::<f64>() {
                return Some(v * scale);
            }
        }
    }
    None
}

/// `sleep 0.2` inside a string literal (a shell script the test runs).
fn shell_sleep(raw: &str, masked: &str) -> Option<f64> {
    let rb = raw.as_bytes();
    let mut from = 0;
    while let Some(i) = raw[from..].find("sleep ") {
        let at = from + i;
        from = at + 6;
        // Only inside a literal: masked to spaces there.
        if masked.as_bytes().get(at) != Some(&b' ') || (at > 0 && is_ident(rb[at - 1])) {
            continue;
        }
        let num: String = raw[at + 6..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }
    None
}

/// A limit written as a field or argument: `gate_seconds: 20`,
/// `timeout_secs = 5`, `timeout: Duration::from_secs(30)`.
fn limit_in(masked: &str) -> Option<f64> {
    let lower = masked.to_ascii_lowercase();
    for key in ["_seconds", "_secs", "timeout", "_ms", "deadline", "_limit"] {
        let Some(i) = lower.find(key) else { continue };
        let rest = lower[i + key.len()..].trim_start();
        let rest = rest
            .strip_prefix(':')
            .or_else(|| rest.strip_prefix('='))
            .unwrap_or(rest)
            .trim_start();
        if let Some(d) = duration_in(rest) {
            return Some(d);
        }
        let num: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(if key == "_ms" { v / 1000.0 } else { v });
        }
    }
    None
}

/// Line ranges (1-based, inclusive) of test code in a file: `#[cfg(test)]`
/// modules and `#[test]` functions. Lines there cover tests, not the code
/// under test.
pub fn test_code_lines(f: &SourceFile) -> Vec<(usize, usize)> {
    let m = f.masked.as_bytes();
    let mut out = vec![];
    for attr in ["#[cfg(test)]", "#[test]"] {
        let mut from = 0;
        while let Some(i) = f.raw[from..].find(attr) {
            let at = from + i;
            from = at + attr.len();
            if m[at] != b'#' {
                continue; // inside a comment or string
            }
            let Some(open) = f.masked[at..].find('{').map(|o| at + o) else {
                continue;
            };
            // A `;` before the `{` is `#[cfg(test)] use ...;` or `mod x;`.
            if f.masked[at..open].contains(';') {
                continue;
            }
            if let Some(close) = matching(m, open) {
                out.push((f.line_of(at), f.line_of(close)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_comments_and_strings_keeping_offsets() {
        let src = "a(\"{x}\") // {\n/* } */ b('{', 'a) r#\"}\"#";
        let m = mask(src);
        assert_eq!(m.len(), src.len());
        assert!(!m.contains('{') && !m.contains('}'), "{m}");
        assert!(m.contains("a(") && m.contains("b("));
    }

    #[test]
    fn durations() {
        assert_eq!(
            duration_in("std::thread::sleep(Duration::from_secs(21));"),
            Some(21.0)
        );
        assert_eq!(
            duration_in("sleep(Duration::from_millis(1_500))"),
            Some(1.5)
        );
        assert_eq!(duration_in("sleep(t)"), None);
        assert_eq!(limit_in("    gate_seconds: 20,"), Some(20.0));
        assert_eq!(
            limit_in(r#"world.set(&format!("gate_command: {g}\ngate_seconds: 20"));"#),
            Some(20.0)
        );
        assert_eq!(
            limit_in("let timeout = Duration::from_secs(30);"),
            Some(30.0)
        );
    }

    #[test]
    fn finds_waits_in_loops_and_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.rs");
        std::fs::write(
            &p,
            "#[test]\nfn t() {\n    helper();\n    helper();\n    std::thread::sleep(Duration::from_secs(2));\n}\n\
             fn helper() {\n    loop {\n        if done() { break; }\n        sleep(Duration::from_millis(10));\n    }\n}\n\
             fn done() -> bool { true }\n",
        )
        .unwrap();
        let s = Sources::of(&[p]);
        let id = TestId {
            binary_id: "x".into(),
            name: "t".into(),
        };
        let t = s.test_fn(&id).unwrap();
        assert_eq!(t.line, 2);
        let f = &s.files[0];
        let w = waits_in(f, t.body);
        assert_eq!(w.len(), 1);
        assert!(!w[0].in_loop);
        let h = &s.fns["helper"][0];
        let w = waits_in(f, h.body);
        assert!(w[0].in_loop, "{w:?}");
        let calls: Vec<String> = calls(s.body_text(t)).into_iter().map(|c| c.1).collect();
        assert_eq!(calls.iter().filter(|c| *c == "helper").count(), 2);
    }
}
