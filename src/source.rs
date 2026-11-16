//! Reading a test's source with `syn`: the module tree of the target that
//! builds it (from its root file, through `mod x;` and inline modules), every
//! function in it with its module path, the calls in a body resolved by
//! Rust's module rules, and the waits written there.
//!
//! What it cannot see it says: a file that does not parse is listed, and a
//! call into another crate is not followed.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::nextest::TestId;
use crate::workspace::Workspace;

pub struct SourceFile {
    pub path: PathBuf,
    pub lines: Vec<String>,
    /// The module this file is.
    pub module: Vec<String>,
    /// Every name its `use` items mention: the modules and items it can
    /// reach besides its own and its ancestors'.
    pub uses: HashSet<String>,
}

/// A function: where it is, its module path, its body.
pub struct Func {
    pub file: usize,
    pub module: Vec<String>,
    pub name: String,
    pub line: usize,
    pub in_impl: bool,
    pub is_test: bool,
    pub block: syn::Block,
}

/// Every function of one target's module tree.
#[derive(Default)]
pub struct Index {
    pub files: Vec<SourceFile>,
    pub fns: Vec<Func>,
    by_name: HashMap<String, Vec<usize>>,
    pub unparsed: Vec<PathBuf>,
    /// Workspace crates loaded, by the name code calls them.
    crates: HashSet<String>,
}

fn line_of(span: proc_macro2::Span) -> usize {
    span.start().line
}

fn has_attr(attrs: &[syn::Attribute], what: &str) -> bool {
    attrs.iter().any(|a| {
        let p = a.path();
        let last = p
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        match what {
            "test" => last == "test",
            "cfg(test)" => {
                p.is_ident("cfg")
                    && a.meta
                        .require_list()
                        .is_ok_and(|l| l.tokens.to_string().split_whitespace().any(|t| t == "test"))
            }
            _ => false,
        }
    })
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.path().is_ident("path"))
        .and_then(|a| match &a.meta {
            syn::Meta::NameValue(nv) => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        })
}

impl Index {
    /// That target's module tree, and every library of the workspace under
    /// its crate name, so a call into another of its crates is followed.
    pub fn with_workspace_libs(ws: &Workspace, binary_id: &str) -> Index {
        let mut idx = Index::of_binary(ws, binary_id);
        for p in &ws.packages {
            if let Some(lib) = p.targets.iter().find(|t| t.kind == "lib") {
                let name = lib.name.replace('-', "_");
                let dir = lib
                    .src_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                idx.crates.insert(name.clone());
                idx.load_file(&lib.src_path, &dir, vec![name]);
            }
        }
        idx
    }

    /// The module tree of the target nextest's `binary_id` names.
    pub fn of_binary(ws: &Workspace, binary_id: &str) -> Index {
        let mut idx = Index::default();
        let Some(pkg) = ws.package_of(binary_id) else {
            return idx;
        };
        let target = match binary_id.split_once("::") {
            None => pkg
                .targets
                .iter()
                .find(|t| t.kind == "lib" || t.kind == "proc-macro"),
            Some((_, rest)) => {
                let name = rest.trim_start_matches("bin/");
                pkg.targets.iter().find(|t| t.name == name)
            }
        };
        if let Some(t) = target {
            let dir = t
                .src_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            idx.load_file(&t.src_path, &dir, vec![]);
        }
        idx
    }

    /// Loads `path` as module `module`; its child modules' files are under
    /// `child_dir`.
    fn load_file(&mut self, path: &Path, child_dir: &Path, module: Vec<String>) {
        if self.files.iter().any(|f| f.path == path) {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(ast) = syn::parse_file(&text) else {
            self.unparsed.push(path.to_path_buf());
            return;
        };
        let file = self.files.len();
        self.files.push(SourceFile {
            path: path.to_path_buf(),
            lines: text.lines().map(str::to_string).collect(),
            module: module.clone(),
            uses: use_names(&ast.items),
        });
        self.items(&ast.items, file, child_dir, &module);
    }

    fn items(&mut self, items: &[syn::Item], file: usize, child_dir: &Path, module: &[String]) {
        for item in items {
            match item {
                syn::Item::Fn(f) => self.add(file, module, &f.sig, &f.attrs, &f.block, false),
                syn::Item::Impl(i) => {
                    for it in &i.items {
                        if let syn::ImplItem::Fn(f) = it {
                            self.add(file, module, &f.sig, &f.attrs, &f.block, true);
                        }
                    }
                }
                syn::Item::Mod(m) => {
                    let name = m.ident.to_string();
                    let mut sub = module.to_vec();
                    sub.push(name.clone());
                    match &m.content {
                        Some((_, inner)) => self.items(inner, file, &child_dir.join(&name), &sub),
                        None => {
                            let candidates = match path_attr(&m.attrs) {
                                Some(p) => vec![child_dir.join(p)],
                                None => vec![
                                    child_dir.join(format!("{name}.rs")),
                                    child_dir.join(&name).join("mod.rs"),
                                ],
                            };
                            if let Some(p) = candidates.into_iter().find(|p| p.exists()) {
                                let next = if p.file_name().is_some_and(|n| n == "mod.rs") {
                                    p.parent().map(Path::to_path_buf).unwrap_or_default()
                                } else {
                                    child_dir.join(&name)
                                };
                                self.load_file(&p, &next, sub);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn add(
        &mut self,
        file: usize,
        module: &[String],
        sig: &syn::Signature,
        attrs: &[syn::Attribute],
        block: &syn::Block,
        in_impl: bool,
    ) {
        let i = self.fns.len();
        let name = sig.ident.to_string();
        self.by_name.entry(name.clone()).or_default().push(i);
        self.fns.push(Func {
            file,
            module: module.to_vec(),
            name,
            line: line_of(sig.ident.span()),
            in_impl,
            is_test: has_attr(attrs, "test"),
            block: block.clone(),
        });
    }

    /// The function of test `id`: its module path and name, exactly.
    pub fn test_fn(&self, id: &TestId) -> Option<&Func> {
        let mut segs: Vec<&str> = id.name.split("::").collect();
        let leaf = segs.pop()?;
        let found = self
            .by_name
            .get(leaf)?
            .iter()
            .map(|&i| &self.fns[i])
            .filter(|f| f.module == segs);
        let all: Vec<&Func> = found.collect();
        all.iter().find(|f| f.is_test).or(all.first()).copied()
    }

    /// Where test `id` is written: its function, or, for a test a macro
    /// generates, the first line naming it in its module's file. The bool
    /// says which (true: generated).
    pub fn locate(&self, id: &TestId) -> Option<(&Path, usize, bool)> {
        if let Some(f) = self.test_fn(id) {
            return Some((self.path(f), f.line, false));
        }
        let mut segs: Vec<&str> = id.name.split("::").collect();
        let leaf = segs.pop()?;
        let word = |l: &str| {
            l.match_indices(leaf).any(|(i, _)| {
                let before = l[..i].chars().next_back();
                let after = l[i + leaf.len()..].chars().next();
                !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                    && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
        };
        // The deepest file whose module encloses the test's.
        let mut files: Vec<&SourceFile> = self
            .files
            .iter()
            .filter(|f| segs.starts_with(&f.module.iter().map(String::as_str).collect::<Vec<_>>()))
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.module.len()));
        files.iter().find_map(|f| {
            f.lines
                .iter()
                .position(|l| word(l))
                .map(|i| (f.path.as_path(), i + 1, true))
        })
    }

    pub fn path(&self, f: &Func) -> &Path {
        &self.files[f.file].path
    }

    /// The body's tokens, so a change of layout alone is no change.
    pub fn body_tokens(f: &Func) -> String {
        quote::ToTokens::to_token_stream(&f.block).to_string()
    }

    /// Whether code in `from` can see function `i`: the same file, a module
    /// enclosing `from`'s, or a module its file names in a `use`.
    fn visible(&self, from: &Func, i: usize) -> bool {
        let f = &self.fns[i];
        f.file == from.file
            || from.module.starts_with(&f.module)
            || f.module
                .iter()
                .any(|m| self.files[from.file].uses.contains(m))
    }

    /// The function a call from `from` names: `name` qualified by `qual`
    /// (`super`, `crate`, module names), or a method.
    fn resolve(&self, from: &Func, call: &Call) -> Option<usize> {
        let cands = self.by_name.get(&call.name)?;
        if call.method {
            // A method: one in an impl the caller can see (its receiver's
            // type is not known here), in the same file first.
            return cands
                .iter()
                .copied()
                .filter(|&i| self.fns[i].in_impl && self.visible(from, i))
                .min_by_key(|&i| (self.fns[i].file != from.file) as u8);
        }
        // The module the path names, from `from`'s module.
        let mut module = from.module.clone();
        let mut qual: &[String] = &call.qual;
        let mut absolute = false;
        while let Some(first) = qual.first() {
            match first.as_str() {
                c if self.crates.contains(c) => {
                    module = vec![c.to_string()];
                    absolute = true;
                    qual = &qual[1..];
                    break;
                }
                "crate" => {
                    module.clear();
                    absolute = true;
                }
                "self" => absolute = true,
                "super" => {
                    module.pop();
                    absolute = true;
                }
                _ => break,
            }
            qual = &qual[1..];
        }
        let free = |i: &usize| !self.fns[*i].in_impl;
        if !qual.is_empty() {
            // `a::b::f`: a module path, from here or any ancestor.
            let mut base = module.clone();
            loop {
                let want: Vec<String> = base.iter().chain(qual.iter()).cloned().collect();
                if let Some(&i) = cands
                    .iter()
                    .filter(|i| free(i))
                    .find(|&&i| self.fns[i].module == want)
                {
                    return Some(i);
                }
                if absolute || base.pop().is_none() {
                    break;
                }
            }
            // `Type::f` (an associated function) or a path into a module we
            // did not load: one of that name whose module ends the same, or
            // an impl's.
            return cands.iter().copied().find(|&i| {
                (self.fns[i].module.ends_with(qual) || self.fns[i].in_impl) && self.visible(from, i)
            });
        }
        // `f`: this module, then each enclosing one (where a `use` would
        // most often bring it from), then the only one of that name.
        let mut base = module;
        loop {
            if let Some(&i) = cands
                .iter()
                .filter(|i| free(i))
                .find(|&&i| self.fns[i].module == base)
            {
                return Some(i);
            }
            if base.pop().is_none() {
                break;
            }
        }
        let frees: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|i| free(i) && self.visible(from, *i))
            .collect();
        (frees.len() == 1).then(|| frees[0])
    }
}

#[derive(Debug, Clone)]
struct Call {
    name: String,
    qual: Vec<String>,
    method: bool,
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
    /// Whether it sits in a `for` loop (or `for_each`): a fixed wait, repeated.
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
    /// Helpers called that poll: a loop holding a sleep.
    pub polls: Vec<String>,
    /// Files of the target that did not parse, so were not read.
    pub unparsed: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Loop {
    Poll,
    Repeat,
}

/// A wait in one body: kind, seconds, line, in a poll loop, in a repeat.
#[derive(Debug, Clone, PartialEq)]
struct Found {
    kind: String,
    seconds: Option<f64>,
    line: usize,
    poll: bool,
    repeat: bool,
}

/// What one body holds: its calls and its waits, with their lines.
#[derive(Default)]
struct Body {
    calls: Vec<Call>,
    waits: Vec<Found>,
    loops: Vec<Loop>,
}

impl Body {
    fn wait(&mut self, kind: &str, seconds: Option<f64>, line: usize) {
        let poll = self.loops.contains(&Loop::Poll);
        let repeat = self.loops.contains(&Loop::Repeat);
        self.waits.push(Found {
            kind: kind.into(),
            seconds,
            line,
            poll,
            repeat,
        });
    }

    fn string(&mut self, s: &str, line: usize) {
        if let Some(x) = shell_sleep(s) {
            self.wait("shell-sleep", Some(x), line);
        } else if let Some(x) = limit_in(s) {
            self.wait("limit", Some(x), line);
        }
    }

    /// A macro's arguments, read as expressions where they are (format!,
    /// assert!, vec!), and their string literals otherwise.
    fn tokens(&mut self, tokens: &proc_macro2::TokenStream) {
        let parser = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
        if let Ok(exprs) = syn::parse::Parser::parse2(parser, tokens.clone()) {
            for e in &exprs {
                self.visit_expr(e);
            }
            return;
        }
        for t in tokens.clone() {
            match t {
                proc_macro2::TokenTree::Literal(l) => {
                    if let Ok(s) = syn::parse_str::<syn::LitStr>(&l.to_string()) {
                        self.string(&s.value(), l.span().start().line);
                    }
                }
                proc_macro2::TokenTree::Group(g) => self.tokens(&g.stream()),
                _ => {}
            }
        }
    }
}

const TIMEOUT_WAITS: &[&str] = &[
    "recv_timeout",
    "park_timeout",
    "wait_timeout",
    "wait_timeout_ms",
];

impl<'ast> Visit<'ast> for Body {
    fn visit_expr_loop(&mut self, e: &'ast syn::ExprLoop) {
        self.loops.push(Loop::Poll);
        syn::visit::visit_expr_loop(self, e);
        self.loops.pop();
    }
    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        self.loops.push(Loop::Poll);
        syn::visit::visit_expr_while(self, e);
        self.loops.pop();
    }
    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        self.visit_expr(&e.expr);
        self.loops.push(Loop::Repeat);
        self.visit_block(&e.body);
        self.loops.pop();
    }
    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let segs: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if let Some((name, qual)) = segs.split_last() {
                let line = line_of(e.span());
                if name == "sleep" {
                    let secs = e.args.first().and_then(duration_of);
                    self.wait("sleep", secs, line);
                } else if TIMEOUT_WAITS.contains(&name.as_str()) {
                    let secs = e.args.iter().find_map(duration_of);
                    self.wait("timeout-wait", secs, line);
                }
                self.calls.push(Call {
                    name: name.clone(),
                    qual: qual.to_vec(),
                    method: false,
                });
            }
        }
        syn::visit::visit_expr_call(self, e);
    }
    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let name = e.method.to_string();
        if TIMEOUT_WAITS.contains(&name.as_str()) {
            let secs = e.args.iter().find_map(duration_of);
            self.wait("timeout-wait", secs, line_of(e.method.span()));
        }
        self.calls.push(Call {
            name: name.clone(),
            qual: vec![],
            method: true,
        });
        self.visit_expr(&e.receiver);
        let repeat = name == "for_each" || name == "try_for_each";
        if repeat {
            self.loops.push(Loop::Repeat);
        }
        for a in &e.args {
            self.visit_expr(a);
        }
        if repeat {
            self.loops.pop();
        }
    }
    fn visit_lit_str(&mut self, s: &'ast syn::LitStr) {
        self.string(&s.value(), line_of(s.span()));
    }
    fn visit_field_value(&mut self, f: &'ast syn::FieldValue) {
        if let syn::Member::Named(n) = &f.member
            && let Some(secs) = limit_value(&n.to_string(), &f.expr)
        {
            self.wait("limit", Some(secs), line_of(f.span()));
        }
        syn::visit::visit_field_value(self, f);
    }
    fn visit_local(&mut self, l: &'ast syn::Local) {
        if let syn::Pat::Ident(p) = &l.pat
            && let Some(init) = &l.init
            && let Some(secs) = limit_value(&p.ident.to_string(), &init.expr)
        {
            self.wait("limit", Some(secs), line_of(l.span()));
        }
        syn::visit::visit_local(self, l);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.tokens(&m.tokens);
    }
    // Closures and nested fns are part of the body; items inside are not
    // followed as calls but still read.
}

/// `Duration::from_secs(21)`, `from_millis(500)`, `from_secs_f64(0.5)`.
fn duration_of(e: &syn::Expr) -> Option<f64> {
    let syn::Expr::Call(c) = e else {
        if let syn::Expr::Reference(r) = e {
            return duration_of(&r.expr);
        }
        return None;
    };
    let syn::Expr::Path(p) = &*c.func else {
        return None;
    };
    let name = p.path.segments.last()?.ident.to_string();
    let scale = match name.as_str() {
        "from_secs" | "from_secs_f64" | "from_secs_f32" => 1.0,
        "from_millis" => 1e-3,
        "from_micros" => 1e-6,
        "from_nanos" => 1e-9,
        _ => return None,
    };
    number(c.args.first()?).map(|v| v * scale)
}

fn number(e: &syn::Expr) -> Option<f64> {
    match e {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(i),
            ..
        }) => i.base10_parse::<f64>().ok(),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Float(f),
            ..
        }) => f.base10_parse::<f64>().ok(),
        _ => None,
    }
}

/// A named limit: `gate_seconds: 20`, `timeout = Duration::from_secs(30)`.
fn limit_value(name: &str, e: &syn::Expr) -> Option<f64> {
    let n = name.to_ascii_lowercase();
    let keyed = ["_seconds", "_secs", "timeout", "deadline", "_limit"]
        .iter()
        .any(|k| n.contains(k))
        || n.ends_with("_ms");
    if !keyed {
        return None;
    }
    duration_of(e).or_else(|| number(e).map(|v| if n.ends_with("_ms") { v / 1000.0 } else { v }))
}

/// `sleep 0.2` in a string (a shell script the test runs).
pub fn shell_sleep(s: &str) -> Option<f64> {
    let mut from = 0;
    while let Some(i) = s[from..].find("sleep ") {
        let at = from + i;
        from = at + 6;
        if at > 0 && s.as_bytes()[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        let num: String = s[at + 6..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }
    None
}

/// A limit in a config the test writes as a string: `gate_seconds: 20`.
pub fn limit_in(s: &str) -> Option<f64> {
    let lower = s.to_ascii_lowercase();
    for key in ["_seconds", "_secs", "timeout"] {
        let Some(i) = lower.find(key) else { continue };
        let rest = lower[i + key.len()..].trim_start();
        let rest = rest
            .strip_prefix(':')
            .or_else(|| rest.strip_prefix('='))
            .unwrap_or(rest)
            .trim_start();
        let num: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }
    None
}

/// Reads test `id`'s function and the helpers it reaches, `depth` calls
/// deep, resolving each call by the module rules.
pub fn scan(ws: &Workspace, id: &TestId, depth: usize) -> Option<Scan> {
    let idx = Index::with_workspace_libs(ws, &id.binary_id);
    let test = idx.test_fn(id)?;
    let test_i = idx.fns.iter().position(|f| std::ptr::eq(f, test))?;
    let at = |f: &Func| format!("{}:{}", ws.relative(idx.path(f)).display(), f.line);
    // Breadth-first: each function read once, with the path that first
    // reached it and how many call sites reach it.
    let mut reach: BTreeMap<usize, (Vec<String>, usize)> = BTreeMap::new();
    let mut bodies: HashMap<usize, Body> = HashMap::new();
    let mut queue = VecDeque::from([(test_i, vec![test.name.clone()], 1usize, 0usize)]);
    let mut order = vec![];
    while let Some((i, path, times, d)) = queue.pop_front() {
        if let Some(r) = reach.get_mut(&i) {
            r.1 += times;
            continue;
        }
        if reach.len() > 400 {
            break;
        }
        reach.insert(i, (path.clone(), times));
        order.push(i);
        let f = &idx.fns[i];
        let mut body = Body::default();
        body.visit_block(&f.block);
        if d < depth {
            let mut counts: BTreeMap<usize, (String, usize)> = BTreeMap::new();
            for c in &body.calls {
                if let Some(t) = idx.resolve(f, c)
                    && t != i
                {
                    counts.entry(t).or_insert((c.name.clone(), 0)).1 += 1;
                }
            }
            for (t, (name, n)) in counts {
                let mut p = path.clone();
                p.push(name);
                queue.push_back((t, p, times * n, d + 1));
            }
        }
        bodies.insert(i, body);
    }
    let mut waits = vec![];
    let mut polls = vec![];
    let mut seen_lines: HashSet<(usize, usize, String)> = HashSet::new();
    for i in &order {
        let f = &idx.fns[*i];
        let (via, times) = reach[i].clone();
        let body = &bodies[i];
        if body
            .waits
            .iter()
            .any(|w| w.poll && (w.kind == "sleep" || w.kind == "shell-sleep"))
        {
            polls.push(format!("{} ({})", f.name, at(f)));
        }
        for Found {
            kind,
            seconds,
            line,
            poll,
            repeat,
        } in &body.waits
        {
            // A nested fn or closure is read with its parent: one wait once.
            if !seen_lines.insert((f.file, *line, kind.clone())) {
                continue;
            }
            let code = idx.files[f.file]
                .lines
                .get(line - 1)
                .map(|l| l.trim().chars().take(160).collect())
                .unwrap_or_default();
            waits.push(Wait {
                kind: kind.clone(),
                seconds: *seconds,
                in_loop: *poll,
                repeated: *repeat,
                at: format!("{}:{line}", ws.relative(&idx.files[f.file].path).display()),
                code,
                via: via.clone(),
                reached: times,
            });
        }
    }
    Some(Scan {
        test_at: at(test),
        functions_read: order.len(),
        waits,
        polls,
        unparsed: idx
            .unparsed
            .iter()
            .map(|p| ws.relative(p).display().to_string())
            .collect(),
    })
}

/// Every identifier in a file's `use` items, at any depth of modules.
fn use_names(items: &[syn::Item]) -> HashSet<String> {
    fn tree(t: &syn::UseTree, out: &mut HashSet<String>) {
        match t {
            syn::UseTree::Path(p) => {
                out.insert(p.ident.to_string());
                tree(&p.tree, out);
            }
            syn::UseTree::Name(n) => {
                out.insert(n.ident.to_string());
            }
            syn::UseTree::Rename(r) => {
                out.insert(r.ident.to_string());
            }
            syn::UseTree::Group(g) => g.items.iter().for_each(|t| tree(t, out)),
            syn::UseTree::Glob(_) => {}
        }
    }
    let mut out = HashSet::new();
    for item in items {
        match item {
            syn::Item::Use(u) => tree(&u.tree, &mut out),
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    out.extend(use_names(inner));
                }
            }
            _ => {}
        }
    }
    out
}

/// Line ranges (1-based, inclusive) of test code in a file: `#[cfg(test)]`
/// modules and `#[test]` functions. Lines there cover tests, not the code
/// under test.
pub fn test_code_lines(path: &Path) -> Vec<(usize, usize)> {
    let Some(ast) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| syn::parse_file(&t).ok())
    else {
        return vec![];
    };
    let mut out = vec![];
    fn walk(items: &[syn::Item], out: &mut Vec<(usize, usize)>) {
        for item in items {
            match item {
                syn::Item::Mod(m) if has_attr(&m.attrs, "cfg(test)") => {
                    out.push((m.span().start().line, m.span().end().line));
                }
                syn::Item::Mod(m) => {
                    if let Some((_, inner)) = &m.content {
                        walk(inner, out);
                    }
                }
                syn::Item::Fn(f)
                    if has_attr(&f.attrs, "test") || has_attr(&f.attrs, "cfg(test)") =>
                {
                    out.push((f.span().start().line, f.span().end().line));
                }
                _ => {}
            }
        }
    }
    walk(&ast.items, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let mut idx = Index::default();
        idx.load_file(&dir.path().join(files[0].0), dir.path(), vec![]);
        (dir, idx)
    }

    fn scan_of(idx: &Index, name: &str) -> (Vec<Found>, Vec<String>) {
        let f = idx
            .test_fn(&TestId {
                binary_id: "x".into(),
                name: name.into(),
            })
            .unwrap();
        let mut b = Body::default();
        b.visit_block(&f.block);
        let resolved = b
            .calls
            .iter()
            .filter_map(|c| idx.resolve(f, c))
            .map(|i| format!("{}::{}", idx.fns[i].module.join("::"), idx.fns[i].name))
            .collect();
        (b.waits, resolved)
    }

    #[test]
    fn module_paths_name_tests_exactly() {
        let (_d, idx) = index(&[
            ("main.rs", "mod common;\nmod exclusive;\nmod other;\n"),
            ("other.rs", "struct F;\nimpl F { fn open(&self) {} }\n"),
            (
                "exclusive.rs",
                "use crate::common::World;\n#[test]\nfn a_cut() {\n    common::until();\n    helper();\n    w.afk();\n    f.open();\n}\nfn helper() {}\nmod inner { #[test] fn a_cut() {} }\n",
            ),
            (
                "common/mod.rs",
                "pub fn until() { loop { std::thread::sleep(std::time::Duration::from_millis(20)); } }\npub fn helper() {}\npub struct World;\nimpl World { pub fn afk(&self) {} }\n",
            ),
        ]);
        let f = idx
            .test_fn(&TestId {
                binary_id: "x".into(),
                name: "exclusive::a_cut".into(),
            })
            .unwrap();
        assert_eq!(
            (f.module.clone(), f.line),
            (vec!["exclusive".to_string()], 3)
        );
        let g = idx
            .test_fn(&TestId {
                binary_id: "x".into(),
                name: "exclusive::inner::a_cut".into(),
            })
            .unwrap();
        assert_eq!(g.module, vec!["exclusive", "inner"]);
        // `common::until` resolves into common/mod.rs; `helper` to this
        // module's own, not common's; `w.afk()` to the impl of a module
        // this file uses; `f.open()` to nothing: other.rs is not used here.
        let (_, calls) = scan_of(&idx, "exclusive::a_cut");
        assert_eq!(
            calls,
            vec!["common::until", "exclusive::helper", "common::afk"]
        );
        let (waits, _) = scan_of(&idx, "common::until");
        assert_eq!(waits[0].seconds, Some(0.02));
        assert!(waits[0].poll, "a sleep in a loop is a poll");
    }

    #[test]
    fn a_call_into_another_workspace_crate_is_followed() {
        let (dir, mut idx) = index(&[
            ("t.rs", "#[test]\nfn t() {\n    dep::wait();\n}\n"),
            (
                "dep/lib.rs",
                "pub fn wait() { loop { std::thread::sleep(std::time::Duration::from_millis(5)); } }\n",
            ),
        ]);
        idx.crates.insert("dep".into());
        idx.load_file(
            &dir.path().join("dep/lib.rs"),
            &dir.path().join("dep"),
            vec!["dep".into()],
        );
        let (_, calls) = scan_of(&idx, "t");
        assert_eq!(calls, vec!["dep::wait"]);
    }

    #[test]
    fn waits_in_code_strings_macros_and_loops() {
        let (_d, idx) = index(&[(
            "t.rs",
            r#"
#[test]
fn t() {
    std::thread::sleep(Duration::from_secs(21));
    world.set(&format!("gate_command: {g}\ngate_seconds: 20"));
    let c = Config { gate_seconds: 5, ..x };
    for _ in 0..3 { run("sh", "sleep 0.2"); }
    rx.recv_timeout(Duration::from_millis(500));
}
"#,
        )]);
        let (w, _) = scan_of(&idx, "t");
        let got: Vec<(&str, Option<f64>, usize, bool)> = w
            .iter()
            .map(|w| (w.kind.as_str(), w.seconds, w.line, w.repeat))
            .collect();
        assert_eq!(
            got,
            vec![
                ("sleep", Some(21.0), 4, false),
                ("limit", Some(20.0), 5, false),
                ("limit", Some(5.0), 6, false),
                ("shell-sleep", Some(0.2), 7, true),
                ("timeout-wait", Some(0.5), 8, false),
            ]
        );
    }

    #[test]
    fn test_code_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lib.rs");
        std::fs::write(
            &p,
            "pub fn a() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
        )
        .unwrap();
        assert_eq!(test_code_lines(&p), vec![(2, 6)]);
    }
}
