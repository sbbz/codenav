//! Translation-unit preprocessing: follows `#include` across files.
//!
//! Files are lexed once and cached, since a header is pulled in by many
//! translation units. Include guards are honoured structurally rather than by
//! evaluating the guard conditional — with every branch taken, a guard's `#if`
//! would not suppress anything, so re-inclusion is stopped by file identity.

use crate::expand::{parse_define, ExpStats, Expander, Keywords, MacroTable};
use crate::files::{FileId, FileRegistry, Resolver};
use crate::intern::{Interner, SymId};
use crate::lex::{lex, Kind, Token, WellKnown};
use crate::pc::{CondKind, PcId, PcParser, PcStore, PC_TRUE};
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::PathBuf;
use std::sync::Arc;

pub struct Session {
    pub it: Interner,
    pub wk: WellKnown,
    pub kw: Keywords,
    pub pcs: PcStore,
    pub reg: FileRegistry,
    pub resolver: Resolver,
    toks: FxHashMap<FileId, Arc<Vec<Token>>>,
    /// Files that failed to load, so the error is reported once.
    bad: FxHashSet<FileId>,
}

impl Session {
    pub fn new(reg: FileRegistry, resolver: Resolver) -> Self {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = Keywords::new(&mut it);
        Self {
            it,
            wk,
            kw,
            pcs: PcStore::new(),
            reg,
            resolver,
            toks: FxHashMap::default(),
            bad: FxHashSet::default(),
        }
    }

    /// Lexes a file once and caches the result. This is the cheap precursor to
    /// the header fragment cache: it removes re-lexing, not re-expansion.
    pub fn tokens(&mut self, id: FileId) -> Option<Arc<Vec<Token>>> {
        if let Some(t) = self.toks.get(&id) {
            return Some(t.clone());
        }
        if self.bad.contains(&id) {
            return None;
        }
        let Ok(src) = std::fs::read(self.reg.path(id)) else {
            self.bad.insert(id);
            return None;
        };
        let toks = Arc::new(lex(&src, &mut self.it));
        let guard = detect_guard(&toks, &self.wk, &self.kw, &mut self.it);
        match guard {
            Guard::Ifndef(g) => self.reg.set_guard(id, Some(g)),
            Guard::PragmaOnce => self.reg.set_pragma_once(id),
            Guard::None => {}
        }
        self.toks.insert(id, toks.clone());
        Some(toks)
    }

    pub fn cached_files(&self) -> usize {
        self.toks.len()
    }
}

enum Guard {
    Ifndef(SymId),
    PragmaOnce,
    None,
}

/// Recognizes the canonical `#ifndef G / #define G` opening, or `#pragma once`.
fn detect_guard(toks: &[Token], wk: &WellKnown, kw: &Keywords, it: &mut Interner) -> Guard {
    let sym_pragma = it.intern("pragma");
    let sym_once = it.intern("once");
    let mut seen = 0;
    let mut i = 0usize;
    let mut pending: Option<SymId> = None;

    while i < toks.len() && seen < 3 {
        let t = toks[i];
        if !(t.bol() && t.kind == Kind::Punct && t.sym == wk.hash) {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < toks.len() && !toks[j].bol() {
            j += 1;
        }
        let line = &toks[start..j];
        seen += 1;

        if let Some(k) = line.first().filter(|k| k.kind == Kind::Ident) {
            if k.sym == sym_pragma && line.get(1).is_some_and(|x| x.sym == sym_once) {
                return Guard::PragmaOnce;
            }
            match pending {
                None if k.sym == kw.ifndef => {
                    pending = line.get(1).filter(|n| n.kind == Kind::Ident).map(|n| n.sym);
                    if pending.is_none() {
                        return Guard::None;
                    }
                }
                Some(g) if k.sym == kw.define => {
                    return if line.get(1).is_some_and(|n| n.sym == g) {
                        Guard::Ifndef(g)
                    } else {
                        Guard::None
                    };
                }
                _ => return Guard::None,
            }
        }
        i = j;
    }
    Guard::None
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PpStats {
    pub files_entered: u64,
    pub includes_resolved: u64,
    pub includes_unresolved: u64,
    pub includes_skipped_guard: u64,
    pub includes_too_deep: u64,
    pub exp: ExpStats,
}

/// Hard backstop against pathological include cycles that no guard catches.
const MAX_STACK: usize = 200;
const MAX_SAME_FILE: usize = 2;

struct CondFrame {
    outer: PcId,
    prior_negated: PcId,
}

pub struct Preprocessor<'a> {
    pub sess: &'a mut Session,
    pub mt: MacroTable,
    pub stats: PpStats,
    out: Vec<Token>,
    stack: Vec<FileId>,
    once: FxHashSet<FileId>,
    /// Include edges discovered, for the include graph.
    pub edges: Vec<(FileId, FileId, PcId)>,
}

impl<'a> Preprocessor<'a> {
    pub fn new(sess: &'a mut Session) -> Self {
        Self {
            sess,
            mt: MacroTable::default(),
            stats: PpStats::default(),
            out: Vec::new(),
            stack: Vec::new(),
            once: FxHashSet::default(),
            edges: Vec::new(),
        }
    }

    pub fn run(mut self, entry: FileId) -> (Vec<Token>, PpStats, Vec<(FileId, FileId, PcId)>) {
        self.run_file(entry, PC_TRUE);
        self.stats.exp.tokens_out = self.out.len() as u64;
        (self.out, self.stats, self.edges)
    }

    fn run_file(&mut self, id: FileId, entry_pc: PcId) {
        let Some(toks) = self.sess.tokens(id) else {
            return;
        };
        self.stats.files_entered += 1;
        self.stack.push(id);

        let dir: PathBuf = self
            .sess
            .reg
            .path(id)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        let mut pc = entry_pc;
        let mut frames: Vec<CondFrame> = Vec::new();
        let mut run: Vec<Token> = Vec::new();
        let mut i = 0usize;

        while i < toks.len() {
            let t = toks[i];
            let is_directive = t.bol() && t.kind == Kind::Punct && t.sym == self.sess.wk.hash;

            if !is_directive {
                let start = i;
                while i < toks.len() {
                    let t = toks[i];
                    if t.bol() && t.kind == Kind::Punct && t.sym == self.sess.wk.hash {
                        break;
                    }
                    i += 1;
                }
                run.clear();
                run.extend_from_slice(&toks[start..i]);
                for t in &mut run {
                    t.pc = pc;
                }
                let mut ex =
                    Expander::new(&self.mt, &mut self.sess.it, &self.sess.wk, &self.sess.pcs);
                ex.expand_seq(&run, &mut self.out);
                let s = ex.stats;
                self.stats.exp.expansions += s.expansions;
                self.stats.exp.pastes += s.pastes;
                self.stats.exp.stringizes += s.stringizes;
                self.stats.exp.budget_hits += s.budget_hits;
                self.stats.exp.unexpanded_known += s.unexpanded_known;
                continue;
            }

            let start = i + 1;
            let mut j = start;
            while j < toks.len() && !toks[j].bol() {
                j += 1;
            }
            let line = &toks[start..j];
            i = j;

            let Some(k) = line.first().filter(|k| k.kind == Kind::Ident) else {
                continue;
            };
            let expr = &line[1..];
            let s = k.sym;
            let kw = &self.sess.kw;

            if s == kw.define {
                if let Some((name, mut def)) = parse_define(expr, &self.sess.wk) {
                    def.pc = pc;
                    self.mt.insert(name, def);
                }
            } else if s == kw.undef {
                if let Some(n) = expr.first() {
                    self.mt.undef(n.sym, pc, n.off);
                }
            } else if s == kw.include {
                self.do_include(id, &dir, expr, pc);
            } else if s == kw.if_ || s == kw.ifdef || s == kw.ifndef {
                let kind = if s == kw.ifdef {
                    CondKind::Ifdef
                } else if s == kw.ifndef {
                    CondKind::Ifndef
                } else {
                    CondKind::If
                };
                let parser = PcParser::new(&self.sess.wk);
                let cond = parser.build(kind, expr, &mut self.sess.pcs, &mut self.sess.it);
                let neg = self.sess.pcs.not(cond);
                frames.push(CondFrame {
                    outer: pc,
                    prior_negated: neg,
                });
                pc = self.sess.pcs.and(pc, cond);
            } else if s == kw.elif || s == kw.elifdef || s == kw.elifndef || s == kw.else_ {
                if let Some(f) = frames.last_mut() {
                    let branch = if s == kw.else_ {
                        f.prior_negated
                    } else {
                        let kind = if s == kw.elifdef {
                            CondKind::Ifdef
                        } else if s == kw.elifndef {
                            CondKind::Ifndef
                        } else {
                            CondKind::If
                        };
                        let parser = PcParser::new(&self.sess.wk);
                        let cond = parser.build(kind, expr, &mut self.sess.pcs, &mut self.sess.it);
                        let branch = self.sess.pcs.and(f.prior_negated, cond);
                        let neg = self.sess.pcs.not(cond);
                        f.prior_negated = self.sess.pcs.and(f.prior_negated, neg);
                        branch
                    };
                    pc = self.sess.pcs.and(f.outer, branch);
                }
            } else if s == kw.endif {
                pc = frames.pop().map(|f| f.outer).unwrap_or(entry_pc);
            }
        }

        self.stack.pop();
    }

    fn do_include(&mut self, from: FileId, dir: &std::path::Path, expr: &[Token], pc: PcId) {
        let Some((target, angled)) = self.include_target(expr) else {
            self.stats.includes_unresolved += 1;
            return;
        };

        let resolved = self
            .sess
            .resolver
            .resolve(&self.sess.reg, dir, &target, angled)
            .or_else(|| {
                self.sess
                    .resolver
                    .resolve_by_suffix(&self.sess.reg, &target)
            });

        let Some(id) = resolved else {
            self.stats.includes_unresolved += 1;
            return;
        };
        self.stats.includes_resolved += 1;
        self.edges.push((from, id, pc));

        // Lex before asking about the guard: detection happens at load time, so
        // otherwise the first inclusion would not yet know the file is guarded.
        if self.sess.tokens(id).is_none() {
            return;
        }
        // A guarded or pragma-once header contributes only the first time.
        let guarded = self.sess.reg.guard(id).is_some() || self.sess.reg.pragma_once(id);
        if guarded && !self.once.insert(id) {
            self.stats.includes_skipped_guard += 1;
            return;
        }

        if self.stack.len() >= MAX_STACK
            || self.stack.iter().filter(|&&f| f == id).count() >= MAX_SAME_FILE
        {
            self.stats.includes_too_deep += 1;
            return;
        }

        self.run_file(id, pc);
    }

    /// Extracts the include target. Handles `"x.h"`, `<a/b.h>`, and the
    /// computed form where a macro expands to either.
    fn include_target(&mut self, expr: &[Token]) -> Option<(String, bool)> {
        if let Some(t) = expr.first() {
            if t.kind == Kind::Str {
                let s = self.sess.it.text(t.sym);
                return Some((s.trim_matches('"').to_string(), false));
            }
            if t.kind == Kind::Punct && self.sess.it.text(t.sym) == "<" {
                let mut s = String::new();
                for t in &expr[1..] {
                    let txt = self.sess.it.text(t.sym);
                    if txt == ">" {
                        return Some((s, true));
                    }
                    s.push_str(txt);
                }
                return None;
            }
            if t.kind == Kind::Ident {
                // Computed include: expand, then re-read the result.
                let mut expanded = Vec::new();
                let mut ex =
                    Expander::new(&self.mt, &mut self.sess.it, &self.sess.wk, &self.sess.pcs);
                ex.expand_seq(expr, &mut expanded);
                if expanded.first().is_some_and(|e| e.sym != t.sym) {
                    return self.include_target_flat(&expanded);
                }
            }
        }
        None
    }

    fn include_target_flat(&self, expr: &[Token]) -> Option<(String, bool)> {
        let t = expr.first()?;
        if t.kind == Kind::Str {
            return Some((
                self.sess.it.text(t.sym).trim_matches('"').to_string(),
                false,
            ));
        }
        if t.kind == Kind::Punct && self.sess.it.text(t.sym) == "<" {
            let mut s = String::new();
            for t in &expr[1..] {
                let txt = self.sess.it.text(t.sym);
                if txt == ">" {
                    return Some((s, true));
                }
                s.push_str(txt);
            }
        }
        None
    }
}

/// Scans a file's directives for angled include targets, for root inference.
/// Deliberately does no expansion: this runs before any macro table exists.
pub fn scan_angled_targets(
    toks: &[Token],
    wk: &WellKnown,
    kw: &Keywords,
    it: &Interner,
    out: &mut FxHashMap<String, u32>,
) {
    let mut i = 0usize;
    while i < toks.len() {
        let t = toks[i];
        if !(t.bol() && t.kind == Kind::Punct && t.sym == wk.hash) {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < toks.len() && !toks[j].bol() {
            j += 1;
        }
        let line = &toks[start..j];
        i = j;

        if line.first().is_none_or(|k| k.sym != kw.include) {
            continue;
        }
        let rest = &line[1..];
        let Some(f) = rest.first() else { continue };
        if f.kind == Kind::Punct && it.text(f.sym) == "<" {
            let mut s = String::new();
            for t in &rest[1..] {
                let txt = it.text(t.sym);
                if txt == ">" {
                    if !s.is_empty() {
                        *out.entry(s).or_default() += 1;
                    }
                    break;
                }
                s.push_str(txt);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileRegistry;
    use std::io::Write;

    fn write_tree(files: &[(&str, &str)]) -> (PathBuf, FileRegistry) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        // Tests run in parallel, so each needs its own tree.
        let dir = std::env::temp_dir().join(format!(
            "codenav-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut reg = FileRegistry::default();
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(body.as_bytes()).unwrap();
            reg.add(p);
        }
        (dir, reg)
    }

    fn run(files: &[(&str, &str)], entry: &str) -> (String, PpStats) {
        let (dir, reg) = write_tree(files);
        let mut targets = FxHashMap::default();
        {
            let mut it = Interner::new();
            let wk = WellKnown::new(&mut it);
            let kw = Keywords::new(&mut it);
            for (rel, _) in files {
                let src = std::fs::read(dir.join(rel)).unwrap();
                let toks = lex(&src, &mut it);
                scan_angled_targets(&toks, &wk, &kw, &it, &mut targets);
            }
        }
        let resolver = Resolver::infer(&reg, &targets);
        let entry_id = reg.get(&dir.join(entry)).unwrap();
        let mut sess = Session::new(reg, resolver);
        let (out, stats, _) = Preprocessor::new(&mut sess).run(entry_id);
        let text = out
            .iter()
            .map(|t| sess.it.text(t.sym))
            .collect::<Vec<_>>()
            .join(" ");
        (text, stats)
    }

    #[test]
    fn quoted_include_is_followed() {
        let (text, st) = run(
            &[
                ("a.h", "int from_header;\n"),
                ("m.c", "#include \"a.h\"\nint main_body;\n"),
            ],
            "m.c",
        );
        assert_eq!(st.includes_resolved, 1);
        assert!(text.contains("from_header"), "got: {text}");
        assert!(text.contains("main_body"), "got: {text}");
    }

    #[test]
    fn macros_cross_the_include_boundary() {
        let (text, _) = run(
            &[
                ("d.h", "#define ANSWER 42\n"),
                ("m.c", "#include \"d.h\"\nint x = ANSWER;\n"),
            ],
            "m.c",
        );
        assert!(text.contains("42"), "got: {text}");
    }

    #[test]
    fn angled_include_resolves_through_inferred_root() {
        let (text, st) = run(
            &[
                ("include/sys/thing.h", "int thing;\n"),
                ("src/m.c", "#include <sys/thing.h>\nint m;\n"),
            ],
            "src/m.c",
        );
        assert_eq!(st.includes_resolved, 1, "root inference failed");
        assert!(text.contains("thing"), "got: {text}");
    }

    #[test]
    fn include_guard_prevents_double_inclusion() {
        let (text, st) = run(
            &[
                ("g.h", "#ifndef G_H\n#define G_H\nint once;\n#endif\n"),
                ("m.c", "#include \"g.h\"\n#include \"g.h\"\n"),
            ],
            "m.c",
        );
        assert_eq!(st.includes_resolved, 2);
        assert_eq!(st.includes_skipped_guard, 1);
        assert_eq!(text.matches("once").count(), 1, "got: {text}");
    }

    #[test]
    fn pragma_once_prevents_double_inclusion() {
        let (text, st) = run(
            &[
                ("p.h", "#pragma once\nint solo;\n"),
                ("m.c", "#include \"p.h\"\n#include \"p.h\"\n"),
            ],
            "m.c",
        );
        assert_eq!(st.includes_skipped_guard, 1);
        assert_eq!(text.matches("solo").count(), 1, "got: {text}");
    }

    #[test]
    fn mutually_recursive_includes_terminate() {
        let (_, st) = run(
            &[
                ("x.h", "#include \"y.h\"\nint x;\n"),
                ("y.h", "#include \"x.h\"\nint y;\n"),
                ("m.c", "#include \"x.h\"\n"),
            ],
            "m.c",
        );
        assert!(st.includes_too_deep > 0, "cycle backstop did not fire");
    }

    #[test]
    fn unresolved_include_is_counted_not_fatal() {
        let (text, st) = run(&[("m.c", "#include <no/such.h>\nint still_here;\n")], "m.c");
        assert_eq!(st.includes_unresolved, 1);
        assert!(text.contains("still_here"), "got: {text}");
    }

    #[test]
    fn conditional_define_from_header_applies_at_use_site() {
        let (text, _) = run(
            &[
                (
                    "c.h",
                    "#ifdef CONFIG_A\n#define SZ 8\n#else\n#define SZ 1\n#endif\n",
                ),
                (
                    "m.c",
                    "#include \"c.h\"\n#ifdef CONFIG_A\nint a[SZ];\n#endif\n",
                ),
            ],
            "m.c",
        );
        assert!(text.contains("8"), "got: {text}");
        assert!(!text.contains("1"), "wrong arm selected: {text}");
    }
}
