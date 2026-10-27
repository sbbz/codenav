//! Global macro table with closure-scoped lookup.
//!
//! Every `#define` in the tree is recorded once, tagged with the file it came
//! from. A use site sees a definition only if that file is in the use site's
//! include closure, which reproduces per-TU macro scoping without ever building
//! a per-TU table.

use crate::expand::{parse_define, Keywords, MacroDef, MAX_VARIANTS};
use crate::files::FileId;
use crate::intern::{SymId, Symbols};
use crate::lex::{Kind, Token, WellKnown};
use crate::pc::{CondKind, PcId, PcParser, PcStore, PC_TRUE};
use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;

#[derive(Debug, Clone)]
pub struct GlobalDef {
    pub file: FileId,
    pub def: MacroDef,
    /// Hash of the definition's meaning. The same macro is often written
    /// identically in many headers and under many guards; those are one
    /// definition for navigation purposes, not competing variants.
    pub hash: u64,
    /// `hash` plus the macro's name: what this definition contributes to its
    /// file's export signature.
    pub export: u64,
}

/// Hashes spellings, never `SymId`s. Ids depend on interning order, which
/// differs between a full build (one interner over the whole tree) and the
/// incremental check (a fresh one per changed file) — hashing ids made every
/// header with a `#define` look as if its macros had changed on every save,
/// forcing a full rebuild for a comment edit.
fn def_hash<S: Symbols>(d: &MacroDef, it: &S) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    d.is_fn.hash(&mut h);
    d.variadic.hash(&mut h);
    d.undef.hash(&mut h);
    for &p in &d.params {
        it.text(p).hash(&mut h);
    }
    for t in &d.body {
        it.text(t.sym).hash(&mut h);
    }
    h.finish()
}

fn text_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    s.hash(&mut h);
    h.finish()
}

#[derive(Default)]
pub struct GlobalMacros {
    defs: FxHashMap<SymId, Vec<GlobalDef>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LookupStats {
    pub hits: u64,
    pub out_of_scope: u64,
    pub ambiguous: u64,
}

impl GlobalMacros {
    pub fn insert<S: Symbols>(&mut self, name: SymId, file: FileId, def: MacroDef, it: &S) {
        let hash = def_hash(&def, it);
        let export = hash ^ text_hash(it.text(name)).wrapping_mul(0x9e3779b97f4a7c15);
        self.insert_hashed(name, file, def, hash, export);
    }

    fn insert_hashed(&mut self, name: SymId, file: FileId, def: MacroDef, hash: u64, export: u64) {
        let v = self.defs.entry(name).or_default();
        // Same file and same guard means a re-scan, not a new variant.
        if let Some(slot) = v.iter_mut().find(|d| d.file == file && d.def.pc == def.pc) {
            *slot = GlobalDef {
                file,
                def,
                hash,
                export,
            };
            return;
        }
        v.push(GlobalDef {
            file,
            def,
            hash,
            export,
        });
    }

    /// Definitions of `name` visible from `from`, meaning defined in that file
    /// itself or anywhere in its include closure, and not contradicting `at`.
    ///
    /// A definition in the querying file wins over one from a header, matching
    /// the usual meaning of a local override.
    pub fn lookup(
        &self,
        name: SymId,
        from: FileId,
        closure: &RoaringBitmap,
        at: PcId,
        pcs: &PcStore,
        stats: &mut LookupStats,
    ) -> Option<&MacroDef> {
        let v = self.defs.get(&name)?;

        let mut own: Option<&MacroDef> = None;
        let mut unconditional: Option<&MacroDef> = None;
        let mut other: Option<&MacroDef> = None;
        let mut other_polarity = i32::MIN;
        let mut compatible = 0usize;
        let mut any_out_of_scope = false;
        // Distinct meanings among the visible definitions. Small and usually 1,
        // so a linear scan beats allocating a set.
        let mut distinct: [u64; MAX_VARIANTS + 1] = [0; MAX_VARIANTS + 1];
        let mut n_distinct = 0usize;

        for g in v.iter().rev() {
            let visible = g.file == from || closure.contains(g.file);
            if !visible {
                any_out_of_scope = true;
                continue;
            }
            if pcs.contradicts(g.def.pc, at) {
                continue;
            }
            compatible += 1;
            if !distinct[..n_distinct].contains(&g.hash) && n_distinct < distinct.len() {
                distinct[n_distinct] = g.hash;
                n_distinct += 1;
            }
            if g.file == from {
                if own.is_none() {
                    own = Some(&g.def);
                }
            } else if g.def.pc == PC_TRUE {
                if unconditional.is_none() {
                    unconditional = Some(&g.def);
                }
            } else {
                // Among guarded variants prefer the one that asserts most,
                // so `#ifdef CONFIG_X` beats its empty `#else` arm.
                let p = pcs.polarity(g.def.pc);
                if p > other_polarity {
                    other_polarity = p;
                    other = Some(&g.def);
                }
            }
        }

        if compatible == 0 {
            if any_out_of_scope {
                stats.out_of_scope += 1;
            }
            return None;
        }
        // Ambiguity is about competing *meanings*, not about how many places
        // spell the same definition.
        if n_distinct > MAX_VARIANTS {
            stats.ambiguous += 1;
            return None; // stays navigable as a macro reference
        }
        stats.hits += 1;
        // A local override beats a header; an unconditional definition beats a
        // guarded one, since guards we cannot decide are the weaker evidence.
        own.or(unconditional).or(other).filter(|d| !d.undef)
    }

    /// Folds a worker's table in, translating its symbol and presence-condition
    /// ids into this table's numbering.
    pub fn merge_from(&mut self, other: GlobalMacros, sym: &[SymId], pc: &[PcId]) {
        for (name, defs) in other.defs {
            let gname = sym[name as usize];
            for g in defs {
                let mut def = g.def;
                def.pc = pc[def.pc as usize];
                for p in &mut def.params {
                    *p = sym[*p as usize];
                }
                for t in &mut def.body {
                    t.sym = sym[t.sym as usize];
                    t.pc = pc[t.pc as usize];
                }
                // Spelling-based, so the hashes survive the id translation.
                self.insert_hashed(gname, g.file, def, g.hash, g.export);
            }
        }
    }

    /// Collects defines from a set of files, reading and lexing each one.
    ///
    /// Used by the incremental path: to re-expand a file you need the defines
    /// its closure publishes and nothing else, so the scan covers ~623 files on
    /// the kernel rather than all 63,779.
    pub fn scan_files<S: Symbols>(
        &mut self,
        files: &[FileId],
        reg: &crate::files::FileRegistry,
        it: &mut S,
        wk: &WellKnown,
        kw: &Keywords,
        pcs: &mut PcStore,
    ) {
        for &f in files {
            let Ok(src) = std::fs::read(reg.path(f)) else {
                continue;
            };
            let toks = crate::lex::lex(&src, it);
            collect_defines(f, &toks, wk, kw, it, pcs, self);
        }
    }

    /// Per-file digest of everything the file publishes to others: which macros
    /// it defines and what they expand to. Two builds agreeing here means no
    /// dependent can extract differently, whatever else changed in the file.
    pub fn export_signatures(&self) -> FxHashMap<FileId, u64> {
        let mut per_file: FxHashMap<FileId, Vec<u64>> = FxHashMap::default();
        for defs in self.defs.values() {
            for g in defs {
                // Name and meaning together; the offset deliberately is not
                // included, so moving a define down a file changes nothing.
                per_file.entry(g.file).or_default().push(g.export);
            }
        }
        per_file
            .into_iter()
            .map(|(f, parts)| {
                (
                    f,
                    codenav_index::manifest::combine_unordered(parts.into_iter()),
                )
            })
            .collect()
    }

    /// Every `#define` site: `(name, file, offset, presence condition)`.
    ///
    /// The build strips directive lines before parsing, so tree-sitter never
    /// sees a `#define` and macros cannot be extracted from the tree. They are
    /// emitted from here instead — the table already knows where each one is.
    pub fn definition_sites(&self) -> impl Iterator<Item = (SymId, FileId, u32, PcId)> + '_ {
        self.defs.iter().flat_map(|(&name, defs)| {
            defs.iter()
                .filter(|g| !g.def.undef)
                .map(move |g| (name, g.file, g.def.off, g.def.pc))
        })
    }

    /// Every recorded definition of a name, regardless of scope. Used to tell
    /// "not a macro here" apart from "not a macro anywhere".
    pub fn all(&self, name: SymId) -> &[GlobalDef] {
        self.defs.get(&name).map(|v| &v[..]).unwrap_or(&[])
    }

    pub fn names(&self) -> usize {
        self.defs.len()
    }

    pub fn total_defs(&self) -> usize {
        self.defs.values().map(|v| v.len()).sum()
    }

    pub fn multi_defined(&self) -> usize {
        self.defs.values().filter(|v| v.len() > 1).count()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Names with the most distinct meanings, worst first. Diagnostic for why
    /// expansion gives up: `(name, total definitions, distinct meanings)`.
    pub fn top_ambiguous(&self, n: usize) -> Vec<(SymId, usize, usize)> {
        let mut v: Vec<(SymId, usize, usize)> = self
            .defs
            .iter()
            .filter(|(_, d)| d.len() > 1)
            .map(|(&name, d)| {
                let mut hashes: Vec<u64> = d.iter().map(|g| g.hash).collect();
                hashes.sort_unstable();
                hashes.dedup();
                (name, d.len(), hashes.len())
            })
            .collect();
        v.sort_unstable_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        v.truncate(n);
        v
    }

    /// Distribution of distinct meanings per name.
    pub fn distinct_histogram(&self) -> Vec<(usize, usize)> {
        let mut h: FxHashMap<usize, usize> = FxHashMap::default();
        for d in self.defs.values() {
            let mut hashes: Vec<u64> = d.iter().map(|g| g.hash).collect();
            hashes.sort_unstable();
            hashes.dedup();
            *h.entry(hashes.len().min(32)).or_default() += 1;
        }
        let mut out: Vec<_> = h.into_iter().collect();
        out.sort_unstable();
        out
    }

    /// Distribution of definition counts per name, for sizing decisions.
    pub fn variant_histogram(&self) -> Vec<(usize, usize)> {
        let mut h: FxHashMap<usize, usize> = FxHashMap::default();
        for v in self.defs.values() {
            *h.entry(v.len().min(64)).or_default() += 1;
        }
        let mut out: Vec<_> = h.into_iter().collect();
        out.sort_unstable();
        out
    }
}

/// A view of the global table as seen from one file, which is what the expander
/// consumes. Cheap to construct — it borrows the closure bitmap rather than
/// building any per-file macro table.
pub struct ScopedMacros<'a> {
    pub gm: &'a GlobalMacros,
    pub from: FileId,
    pub closure: &'a RoaringBitmap,
    stats: std::cell::Cell<LookupStats>,
}

impl<'a> ScopedMacros<'a> {
    pub fn new(gm: &'a GlobalMacros, from: FileId, closure: &'a RoaringBitmap) -> Self {
        Self {
            gm,
            from,
            closure,
            stats: std::cell::Cell::new(LookupStats::default()),
        }
    }

    pub fn stats(&self) -> LookupStats {
        self.stats.get()
    }
}

impl crate::expand::MacroSource for ScopedMacros<'_> {
    fn lookup_def(&self, name: SymId, at: PcId, pcs: &PcStore) -> Option<&MacroDef> {
        let mut st = self.stats.get();
        let r = self
            .gm
            .lookup(name, self.from, self.closure, at, pcs, &mut st);
        self.stats.set(st);
        r
    }

    fn is_known(&self, name: SymId) -> bool {
        !self.gm.all(name).is_empty()
    }
}

/// Walks one file's directives and records its `#define`s with the presence
/// condition each sits under. No expansion, no include following.
pub fn collect_defines<S: Symbols>(
    file: FileId,
    toks: &[Token],
    wk: &WellKnown,
    kw: &Keywords,
    it: &mut S,
    pcs: &mut PcStore,
    out: &mut GlobalMacros,
) {
    struct Frame {
        outer: PcId,
        prior_negated: PcId,
    }

    let parser = PcParser::new(wk);
    let mut frames: Vec<Frame> = Vec::new();
    let mut pc = PC_TRUE;
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

        let Some(k) = line.first().filter(|k| k.kind == Kind::Ident) else {
            continue;
        };
        let expr = &line[1..];
        let s = k.sym;

        if s == kw.define {
            if let Some((name, mut def)) = parse_define(expr, wk) {
                def.pc = pc;
                out.insert(name, file, def, it);
            }
        } else if s == kw.undef {
            if let Some(n) = expr.first() {
                out.insert(
                    n.sym,
                    file,
                    MacroDef {
                        is_fn: false,
                        variadic: false,
                        params: Vec::new(),
                        body: Vec::new(),
                        off: n.off,
                        pc,
                        undef: true,
                    },
                    it,
                );
            }
        } else if s == kw.if_ || s == kw.ifdef || s == kw.ifndef {
            let kind = if s == kw.ifdef {
                CondKind::Ifdef
            } else if s == kw.ifndef {
                CondKind::Ifndef
            } else {
                CondKind::If
            };
            let cond = parser.build(kind, expr, pcs, it);
            let neg = pcs.not(cond);
            frames.push(Frame {
                outer: pc,
                prior_negated: neg,
            });
            pc = pcs.and(pc, cond);
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
                    let cond = parser.build(kind, expr, pcs, it);
                    let branch = pcs.and(f.prior_negated, cond);
                    let neg = pcs.not(cond);
                    f.prior_negated = pcs.and(f.prior_negated, neg);
                    branch
                };
                pc = pcs.and(f.outer, branch);
            }
        } else if s == kw.endif {
            pc = frames.pop().map(|f| f.outer).unwrap_or(PC_TRUE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intern::Interner;
    use crate::lex::lex;

    struct Fix {
        it: Interner,
        wk: WellKnown,
        kw: Keywords,
        pcs: PcStore,
        gm: GlobalMacros,
    }

    fn fixture(files: &[&str]) -> Fix {
        fixture_from(0, files)
    }

    /// `base` keeps FileIds globally unique across pieces, which is what the
    /// real pipeline does — reusing ids would make `insert`'s (file, pc) dedup
    /// collapse unrelated definitions.
    fn fixture_from(base: FileId, files: &[&str]) -> Fix {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = Keywords::new(&mut it);
        let mut pcs = PcStore::new();
        let mut gm = GlobalMacros::default();
        for (i, src) in files.iter().enumerate() {
            let toks = lex(src.as_bytes(), &mut it);
            collect_defines(
                base + i as FileId,
                &toks,
                &wk,
                &kw,
                &mut it,
                &mut pcs,
                &mut gm,
            );
        }
        Fix {
            it,
            wk,
            kw,
            pcs,
            gm,
        }
    }

    fn closure(ids: &[FileId]) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &i in ids {
            b.insert(i);
        }
        b
    }

    #[test]
    fn definition_in_closure_is_visible() {
        let mut f = fixture(&["#define A 1\n", ""]);
        let a = f.it.intern("A");
        let mut st = LookupStats::default();
        let got = f.gm.lookup(a, 1, &closure(&[0]), PC_TRUE, &f.pcs, &mut st);
        assert!(got.is_some());
        assert_eq!(st.hits, 1);
    }

    /// Building a table in one pass must equal building it in pieces and
    /// merging. Any divergence changes which macro definition wins, and so
    /// silently changes every symbol the expander produces downstream.
    #[test]
    fn merged_table_matches_single_pass() {
        let files: Vec<String> = (0..12)
            .map(|i| {
                format!(
                    "#define COMMON c{i}\n\
                     #ifdef CONFIG_{i}\n#define G g{i}\n#else\n#define G\n#endif\n\
                     #define UNIQ_{i} u{i}\n\
                     #if defined(A) && defined(B_{i})\n#define BOTH b{i}\n#endif\n\
                     #undef GONE_{i}\n"
                )
            })
            .collect();
        let refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();

        let single = fixture(&refs);

        // Same inputs, processed in three pieces and merged.
        let mut m = fixture(&[]);
        for (ci, part) in refs.chunks(4).enumerate() {
            let f = fixture_from((ci * 4) as FileId, part);
            let sym_map = m.it.merge_from(&f.it);
            let pc_map = m.pcs.merge_from(&f.pcs, &sym_map);
            m.gm.merge_from(f.gm, &sym_map, &pc_map);
        }

        assert_eq!(
            single.gm.names(),
            m.gm.names(),
            "distinct macro names must match"
        );
        assert_eq!(
            single.gm.total_defs(),
            m.gm.total_defs(),
            "total definitions must match"
        );
    }

    #[test]
    fn merging_worker_tables_preserves_guarded_lookup() {
        // Two workers see different files and intern independently, so their
        // symbol and PcIds collide numerically while meaning different things.
        // Merging must reconcile both, or every guard silently becomes wrong.
        let mut a = fixture(&["#ifdef CONFIG_A
#define M from_a
#endif
"]);
        let b = fixture(&["#ifdef CONFIG_B
#define OTHER x
#endif
#define N from_b
"]);

        let sym_map = a.it.merge_from(&b.it);
        let pc_map = a.pcs.merge_from(&b.pcs, &sym_map);
        a.gm.merge_from(b.gm, &sym_map, &pc_map);

        let m = a.it.intern("M");
        let n = a.it.intern("N");
        let from_b = a.it.intern("from_b");
        let cfg_a = a.it.intern("CONFIG_A");
        let mut st = LookupStats::default();

        // The merged-in unconditional definition survives with its body intact.
        let got =
            a.gm.lookup(n, 9, &closure(&[0]), PC_TRUE, &a.pcs, &mut st)
                .expect("merged definition must be found");
        assert_eq!(got.body[0].sym, from_b);

        // And the original guard still discriminates after renumbering.
        let not_a = a.pcs.and_lit(PC_TRUE, crate::pc::Lit::new(cfg_a, false));
        assert!(
            a.gm.lookup(m, 9, &closure(&[0]), not_a, &a.pcs, &mut st)
                .is_none(),
            "guard must still contradict after merge"
        );
    }

    #[test]
    fn definition_outside_closure_is_invisible() {
        // Measured: expanding out-of-scope macros anyway (even when the name has
        // one meaning tree-wide) raised tree-sitter error nodes by 50%, from 696
        // to 1041 per MiB. An unexpanded macro usually parses harmlessly as an
        // identifier; a wrongly expanded one injects garbage. So scope is final.
        let mut f = fixture(&["#define A 1\n", ""]);
        let a = f.it.intern("A");
        let mut st = LookupStats::default();
        let got = f.gm.lookup(a, 1, &closure(&[]), PC_TRUE, &f.pcs, &mut st);
        assert!(got.is_none(), "must not see a header it does not include");
        assert_eq!(st.out_of_scope, 1);
    }

    #[test]
    fn contested_definition_outside_closure_is_not_rescued() {
        // Two meanings, so scope genuinely matters and guessing would be wrong.
        let mut f = fixture(&["#define A one\n", "#define A two\n", ""]);
        let a = f.it.intern("A");
        let mut st = LookupStats::default();
        let got = f.gm.lookup(a, 2, &closure(&[]), PC_TRUE, &f.pcs, &mut st);
        assert!(got.is_none(), "must not guess between competing meanings");
    }

    #[test]
    fn own_file_definition_beats_a_header() {
        let mut f = fixture(&["#define A header\n", "#define A local\n"]);
        let a = f.it.intern("A");
        let local = f.it.intern("local");
        let mut st = LookupStats::default();
        let got =
            f.gm.lookup(a, 1, &closure(&[0]), PC_TRUE, &f.pcs, &mut st)
                .unwrap();
        assert_eq!(got.body[0].sym, local);
    }

    #[test]
    fn contradicting_guard_is_filtered_out() {
        let mut f = fixture(&["#ifdef X\n#define A 1\n#endif\n"]);
        let a = f.it.intern("A");
        let x = f.it.intern("X");
        // Query under !X: the only definition contradicts it.
        let not_x = f.pcs.and_lit(PC_TRUE, crate::pc::Lit::new(x, false));
        let mut st = LookupStats::default();
        let got = f.gm.lookup(a, 1, &closure(&[0]), not_x, &f.pcs, &mut st);
        assert!(got.is_none());
    }

    #[test]
    fn compatible_guard_is_selected() {
        let mut f = fixture(&["#ifdef X\n#define A 1\n#endif\n"]);
        let a = f.it.intern("A");
        let x = f.it.intern("X");
        let with_x = f.pcs.and_lit(PC_TRUE, crate::pc::Lit::new(x, true));
        let mut st = LookupStats::default();
        assert!(f
            .gm
            .lookup(a, 1, &closure(&[0]), with_x, &f.pcs, &mut st)
            .is_some());
    }

    #[test]
    fn too_many_visible_variants_is_ambiguous() {
        let mut srcs = Vec::new();
        for i in 0..MAX_VARIANTS + 2 {
            srcs.push(format!("#ifdef C{i}\n#define A v{i}\n#endif\n"));
        }
        let refs: Vec<&str> = srcs.iter().map(|s| s.as_str()).collect();
        let mut f = fixture(&refs);
        let a = f.it.intern("A");
        let all: Vec<FileId> = (0..refs.len() as FileId).collect();
        let mut st = LookupStats::default();
        assert!(f
            .gm
            .lookup(a, 99, &closure(&all), PC_TRUE, &f.pcs, &mut st)
            .is_none());
        assert_eq!(st.ambiguous, 1);
    }

    #[test]
    fn narrowing_the_closure_disambiguates() {
        let mut srcs = Vec::new();
        for i in 0..MAX_VARIANTS + 2 {
            srcs.push(format!("#ifdef C{i}\n#define A v{i}\n#endif\n"));
        }
        let refs: Vec<&str> = srcs.iter().map(|s| s.as_str()).collect();
        let mut f = fixture(&refs);
        let a = f.it.intern("A");
        let v0 = f.it.intern("v0");
        let mut st = LookupStats::default();
        // Seeing only one of the defining headers resolves it cleanly.
        let got =
            f.gm.lookup(a, 99, &closure(&[0]), PC_TRUE, &f.pcs, &mut st)
                .unwrap();
        assert_eq!(got.body[0].sym, v0);
    }

    #[test]
    fn identical_definitions_in_many_headers_are_not_ambiguous() {
        // The same macro spelled the same way in many headers is one meaning.
        let mut srcs = Vec::new();
        for i in 0..MAX_VARIANTS + 4 {
            srcs.push(format!("#ifdef C{i}\n#define A same_body\n#endif\n"));
        }
        let refs: Vec<&str> = srcs.iter().map(|s| s.as_str()).collect();
        let mut f = fixture(&refs);
        let a = f.it.intern("A");
        let all: Vec<FileId> = (0..refs.len() as FileId).collect();
        let mut st = LookupStats::default();
        assert!(
            f.gm.lookup(a, 99, &closure(&all), PC_TRUE, &f.pcs, &mut st)
                .is_some(),
            "identical bodies must not count as competing variants"
        );
        assert_eq!(st.ambiguous, 0);
    }

    #[test]
    fn asserting_guard_beats_its_empty_else_arm() {
        // The kernel's `#ifdef CONFIG_X / real / #else / empty / #endif` shape.
        // Picking by source order would always take the empty arm, which is how
        // the __x64_sys_* symbols went missing from the index.
        let mut f =
            fixture(&["#ifdef CONFIG_X64\n#define M real_body\n#else\n#define M\n#endif\n"]);
        let a = f.it.intern("M");
        let real = f.it.intern("real_body");
        let mut st = LookupStats::default();
        let got =
            f.gm.lookup(a, 9, &closure(&[0]), PC_TRUE, &f.pcs, &mut st)
                .expect("a definition must be chosen");
        assert_eq!(
            got.body.first().map(|t| t.sym),
            Some(real),
            "must prefer the asserted branch, not the #else"
        );
    }

    #[test]
    fn unconditional_definition_beats_a_guarded_one() {
        let mut f = fixture(&["#ifdef X\n#define A guarded\n#endif\n", "#define A plain\n"]);
        let a = f.it.intern("A");
        let plain = f.it.intern("plain");
        let mut st = LookupStats::default();
        let got =
            f.gm.lookup(a, 99, &closure(&[0, 1]), PC_TRUE, &f.pcs, &mut st)
                .unwrap();
        assert_eq!(got.body[0].sym, plain);
    }

    #[test]
    fn undef_in_scope_suppresses_the_macro() {
        let mut f = fixture(&["#define A 1\n", "#undef A\n"]);
        let a = f.it.intern("A");
        let mut st = LookupStats::default();
        let got = f.gm.lookup(a, 1, &closure(&[0]), PC_TRUE, &f.pcs, &mut st);
        assert!(got.is_none(), "the local #undef must win");
    }

    #[test]
    fn rescanning_a_file_does_not_create_variants() {
        let src = "#define A 1\n";
        let mut f = fixture(&[src]);
        let toks = lex(src.as_bytes(), &mut f.it);
        collect_defines(0, &toks, &f.wk, &f.kw, &mut f.it, &mut f.pcs, &mut f.gm);
        let a = f.it.intern("A");
        assert_eq!(f.gm.all(a).len(), 1);
    }
}

#[cfg(test)]
mod signature_tests {
    use super::*;
    use crate::intern::Interner;
    use crate::lex::lex;

    /// Export signature of the last file in `files`, scanned in order with one
    /// interner — so earlier files decide what ids its names get.
    fn sig(files: &[&str]) -> u64 {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = Keywords::new(&mut it);
        let mut pcs = PcStore::new();
        let mut gm = GlobalMacros::default();
        for (i, src) in files.iter().enumerate() {
            let toks = lex(src.as_bytes(), &mut it);
            collect_defines(i as FileId, &toks, &wk, &kw, &mut it, &mut pcs, &mut gm);
        }
        gm.export_signatures()[&(files.len() as FileId - 1)]
    }

    const HEADER: &str = "/* a */\n#define LIMIT(x) ((x) + OFFSET)\n#define OFFSET 4\n";

    #[test]
    fn the_signature_does_not_depend_on_interning_order() {
        // The regression: a full build interns the whole tree in one pass, the
        // incremental check interns one file afresh. Hashing ids made the two
        // disagree, so every save of a header with a #define looked like a
        // macro change and forced a full rebuild.
        let alone = sig(&[HEADER]);
        let after_others = sig(&["#define OFFSET 9\nint x, LIMIT, zz;\n", HEADER]);
        assert_eq!(alone, after_others);
    }

    #[test]
    fn a_comment_or_a_move_is_not_a_macro_change() {
        let base = sig(&[HEADER]);
        assert_eq!(
            base,
            sig(&["/* edited */\n#define LIMIT(x) ((x) + OFFSET)\n#define OFFSET 4\n"])
        );
        assert_eq!(
            base,
            sig(&["\n\n#define OFFSET 4\n#define LIMIT(x) ((x) + OFFSET)\n"])
        );
    }

    #[test]
    fn a_changed_body_parameter_or_name_is() {
        let base = sig(&[HEADER]);
        assert_ne!(
            base,
            sig(&["#define LIMIT(x) ((x) + OFFSET)\n#define OFFSET 5\n"])
        );
        assert_ne!(
            base,
            sig(&["#define LIMIT(y) ((y) + OFFSET)\n#define OFFSET 4\n"])
        );
        assert_ne!(
            base,
            sig(&["#define LIMITS(x) ((x) + OFFSET)\n#define OFFSET 4\n"])
        );
    }
}
