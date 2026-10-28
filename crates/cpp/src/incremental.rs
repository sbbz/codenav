//! The incremental update path.
//!
//! A file's symbols depend on exactly two things: its own text, and the macros
//! visible in its include closure. So an update re-extracts:
//!
//! - every changed or added file;
//! - every file whose closure changed — an `#include` was added or removed, a
//!   header appeared and satisfies an include that used to resolve to nothing,
//!   or a header vanished;
//! - every file that can see a header whose macros changed.
//!
//! Closures are recomputed from the stored direct edges, with the same
//! algorithm the full build uses, whenever any edge changes. That costs about
//! half a second on the kernel and makes an update's closures match a full
//! build's by construction rather than by a separate, subtler algorithm.
//!
//! Results go into an overlay that supersedes main's records for those files.
//! Files a previous overlay already covered are carried into the new one by
//! copying their records, not re-extracting them, so an update costs what it
//! changed rather than everything changed since the last full build.

use crate::expand::{Expander, Keywords};
use crate::files::{FileId, FileRegistry, Resolver};
use crate::global::{collect_defines, GlobalMacros, ScopedMacros};
use crate::graph::{resolve_includes, scan_includes, IncludeGraph};
use crate::intern::{Interner, Symbols, ThreadInterner};
use crate::lex::{lex, Kind, Token, WellKnown};
use crate::pc::{PcStore, PC_TRUE};
use crate::py::{resolve_import, PyExtractor, PyKinds, RawImport};
use crate::render::render;
use crate::symbols::{Extracted, Extractor, Kinds, Role, SymKind, NO_SYMBOL};
use codenav_index::db::{CLOSURE_OVERRIDES, REMOVED};
use codenav_index::manifest::{self, Manifest};
use codenav_index::{
    store, Db, Index, IndexWriter, OccRec, SymbolRec, ROLE_CALL, ROLE_DECL, ROLE_DEF, ROLE_REF,
};
use rayon::prelude::*;
use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};

/// The most files an overlay may cover. Every update copies the overlay forward
/// and rewrites it, at ~0.64 ms per covered file on the kernel: a 22k-file
/// overlay made each later save cost 14 s. A count rather than a share of the
/// tree, because that per-file cost is what it bounds — 2,500 files keeps a
/// save under ~1.6 s. Anything larger is a full rebuild, which leaves no
/// overlay behind.
const MAX_OVERLAY_FILES: u64 = 2_500;

/// Overlay bitmaps the update keeps for the next one; see [`Db`] for the two
/// queries read.
const EDGE_OVERRIDES: &str = "edge_overrides";
/// Files whose text changed since main was built, so main's cached directives
/// for them are stale.
const CHANGED: &str = "changed";

pub enum Outcome {
    /// Overlay written; `files` were re-extracted.
    Applied { files: usize },
    /// Caller should do a full rebuild.
    Fallback(&'static str),
}

/// Attempts an incremental update against an existing index.
///
/// `hashes` is every file now on disk with its scan; `changes` is how that
/// differs from `old_manifest`.
pub fn try_update(
    index_dir: &Path,
    changes: &manifest::Changes,
    hashes: &[(String, manifest::Scan)],
    old_manifest: &Manifest,
) -> std::io::Result<Outcome> {
    let mut prof = Phases::new();
    let Some(cur) = store::current(index_dir).ok().flatten() else {
        return Ok(Outcome::Fallback("no usable index"));
    };
    let Ok(db) = Db::open(index_dir) else {
        return Ok(Outcome::Fallback("no usable index"));
    };
    let main = db.main();
    if !main.has_edges() {
        return Ok(Outcome::Fallback("index predates include tracking"));
    }
    let prev = db.overlay();
    let bitmap = |name: &str| {
        prev.map(|o| o.bitmap(name))
            .unwrap_or(Ok(RoaringBitmap::new()))
    };
    let (Ok(mut edge_overrides), Ok(mut changed_since_main)) =
        (bitmap(EDGE_OVERRIDES), bitmap(CHANGED))
    else {
        return Ok(Outcome::Fallback("unreadable overlay"));
    };
    let mut removed = db.removed().clone();

    prof.mark("open index");
    // ---- The file set: ids never move; added files go on the end. ----
    let mut paths: Vec<String> = (0..db.file_count() as FileId)
        .map(|i| db.path(i).unwrap_or("").to_string())
        .collect();
    let mut id_of: FxHashMap<String, FileId> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), i as FileId))
        .collect();
    let mut edges: Vec<Vec<FileId>> = (0..paths.len() as FileId)
        .map(|f| match prev {
            Some(o) if edge_overrides.contains(f) => o.edges(f),
            _ => main.edges(f),
        })
        .collect();

    let mut touched: Vec<FileId> = Vec::new(); // modified or added, on disk
    for p in &changes.modified {
        match id_of.get(p) {
            Some(&id) => touched.push(id),
            None => return Ok(Outcome::Fallback("changed file is not in the index")),
        }
    }
    for p in &changes.added {
        let id = match id_of.get(p) {
            Some(&id) => id, // re-added at a path seen before
            None => {
                let id = paths.len() as FileId;
                paths.push(p.clone());
                id_of.insert(p.clone(), id);
                edges.push(Vec::new());
                id
            }
        };
        removed.remove(id);
        touched.push(id);
    }
    let mut gone: Vec<FileId> = Vec::new();
    for p in &changes.removed {
        let Some(&id) = id_of.get(p) else {
            return Ok(Outcome::Fallback("removed file is not in the index"));
        };
        removed.insert(id);
        if !edges[id as usize].is_empty() {
            edges[id as usize].clear();
            edge_overrides.insert(id);
        }
        gone.push(id);
    }
    let n = paths.len();

    // A removed file keeps its id but must never be resolved to again, so the
    // registry gets a placeholder no include can match.
    let mut reg = FileRegistry::default();
    for (i, p) in paths.iter().enumerate() {
        if removed.contains(i as FileId) {
            reg.add(PathBuf::from(format!("\0removed/{i}")));
        } else {
            reg.add(PathBuf::from(p));
        }
    }
    if reg.len() != n {
        return Ok(Outcome::Fallback("path table has duplicates"));
    }

    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let mut pcs = PcStore::for_c(&mut it);
    let resolver = Resolver::new(main.roots().iter().map(PathBuf::from).collect());
    let is_py = |f: FileId| paths[f as usize].ends_with(".py");
    let py_lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    let py_kinds = PyKinds::new(&py_lang);
    let py_resolver = Resolver::new(main.py_roots().iter().map(PathBuf::from).collect());
    // A Python file's resolved imports, exactly as the full build resolves them.
    let py_edges = |f: FileId, it: &mut Interner| -> Option<Vec<FileId>> {
        let src = std::fs::read(reg.path(f)).ok()?;
        let mut p = tree_sitter::Parser::new();
        p.set_language(&py_lang).ok()?;
        let tree = p.parse(&src, None)?;
        let (_, imports, _) = PyExtractor::new(&py_kinds, &src, f, it).run(&tree);
        let dir = reg
            .path(f)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut targets: Vec<FileId> = imports
            .iter()
            .filter_map(|imp| resolve_import(&reg, &py_resolver, &dir, imp))
            .filter(|&t| t != f)
            .collect();
        targets.sort_unstable();
        targets.dedup();
        Some(targets)
    };

    // Directive tokens of a file: main's cache when the text has not changed
    // since main was built, otherwise the file itself.
    let directive_tokens =
        |f: FileId, it: &mut Interner, changed_since_main: &RoaringBitmap| -> Option<Vec<Token>> {
            if !changed_since_main.contains(f) {
                if let Some(t) = main
                    .directives(f)
                    .and_then(|b| crate::directives::decode(b, it))
                {
                    return Some(t);
                }
            }
            let src = std::fs::read(reg.path(f)).ok()?;
            Some(lex(&src, it))
        };

    prof.mark("path table, edges, registry");
    // ---- Rescan what changed: its macros and its includes. ----
    let mut new_exports: FxHashMap<FileId, u64> = FxHashMap::default();
    let mut exporters: Vec<FileId> = Vec::new();
    let mut edges_moved = !gone.is_empty();
    for &f in &touched {
        changed_since_main.insert(f);
        if is_py(f) {
            // Python publishes nothing another file's extraction depends on;
            // its imports only shape the closure, which ranking reads.
            if let Some(targets) = py_edges(f, &mut it) {
                edges_moved |= update_edges(&mut edges, &mut edge_overrides, f, targets);
            }
            continue;
        }
        let Ok(src) = std::fs::read(reg.path(f)) else {
            return Ok(Outcome::Fallback("changed file could not be read"));
        };
        let toks = lex(&src, &mut it);
        let mut one = GlobalMacros::default();
        collect_defines(f, &toks, &wk, &kw, &mut it, &mut pcs, &mut one);
        let exports = one.export_signatures().get(&f).copied().unwrap_or(0);
        let was = old_manifest
            .get(&paths[f as usize])
            .map(|m| m.exports)
            .unwrap_or(0);
        if exports != was {
            exporters.push(f);
        }
        new_exports.insert(f, exports);

        let (targets, _) =
            resolve_includes(f, &scan_includes(&toks, &wk, &kw, &it), &reg, &resolver);
        edges_moved |= update_edges(&mut edges, &mut edge_overrides, f, targets);
    }
    // A removed header's macros vanish from everything that saw it.
    for &f in &gone {
        if old_manifest
            .get(&paths[f as usize])
            .is_some_and(|m| m.exports != 0)
        {
            exporters.push(f);
        }
    }

    prof.mark("rescan changed files");
    // ---- Includes that may now resolve differently. ----
    // Files that included a removed file: their include may now reach another
    // file of the same name, or nothing.
    let mut reresolve: Vec<FileId> = Vec::new();
    if !gone.is_empty() {
        for (f, es) in edges.iter().enumerate() {
            if !removed.contains(f as u32) && es.iter().any(|t| gone.contains(t)) {
                reresolve.push(f as FileId);
            }
        }
    }
    // A new header can satisfy an include that resolved to nothing in a file
    // nobody touched. Main records those; files changed since main was built
    // are re-resolved outright, since main's record of them is stale.
    let added_any = !changes.added.is_empty();
    if added_any {
        for (f, angled, target) in main.unresolved() {
            if removed.contains(f) || changed_since_main.contains(f) {
                continue;
            }
            let dir = reg
                .path(f)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            if resolver
                .resolve(&reg, &dir, &target, angled)
                .or_else(|| resolver.resolve_by_suffix(&reg, &target))
                .is_some()
            {
                reresolve.push(f);
            }
        }
        for (f, level, path) in main.unresolved_py() {
            if removed.contains(f) || changed_since_main.contains(f) {
                continue;
            }
            let dir = reg
                .path(f)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            if resolve_import(&reg, &py_resolver, &dir, &RawImport { path, level }).is_some() {
                reresolve.push(f);
            }
        }
        reresolve.extend(changed_since_main.iter().filter(|&f| !removed.contains(f)));
    }
    reresolve.sort_unstable();
    reresolve.dedup();
    for f in reresolve {
        if touched.contains(&f) {
            continue;
        }
        let targets = if is_py(f) {
            py_edges(f, &mut it)
        } else {
            directive_tokens(f, &mut it, &changed_since_main).map(|toks| {
                resolve_includes(f, &scan_includes(&toks, &wk, &kw, &it), &reg, &resolver).0
            })
        };
        if let Some(targets) = targets {
            edges_moved |= update_edges(&mut edges, &mut edge_overrides, f, targets);
        }
    }

    prof.mark("re-resolve includes");
    // ---- Closures. ----
    let closure_now = |f: FileId| db.closure(f).unwrap_or_default();
    let mut closure_overrides = db.closure_overrides().clone();
    let mut new_closures: Option<Vec<RoaringBitmap>> = None;
    let mut reclosured: Vec<FileId> = Vec::new();
    if edges_moved {
        let g = IncludeGraph {
            edges: edges.clone(),
            ..Default::default()
        };
        let (fresh, _) = g.closures();
        closure_overrides = RoaringBitmap::new();
        for (f, c) in fresh.iter().enumerate() {
            let f = f as FileId;
            if removed.contains(f) {
                continue;
            }
            if *c != main.closure(f).unwrap_or_default() {
                closure_overrides.insert(f);
            }
            if *c != closure_now(f) {
                reclosured.push(f);
            }
        }
        new_closures = Some(fresh);
    }
    let closure_of = |f: FileId| match &new_closures {
        Some(c) => c[f as usize].clone(),
        None => closure_now(f),
    };

    prof.mark("closures");
    // ---- What has to be re-extracted. ----
    let mut dirty: RoaringBitmap = touched.iter().copied().collect();
    dirty.extend(reclosured.iter().copied());
    if !exporters.is_empty() {
        let seen: RoaringBitmap = exporters.iter().copied().collect();
        for f in 0..n as FileId {
            if !removed.contains(f) && !closure_of(f).is_disjoint(&seen) {
                dirty.insert(f);
            }
            if dirty.len() > MAX_OVERLAY_FILES {
                return Ok(Outcome::Fallback(
                    "macro change reaches too much of the tree",
                ));
            }
        }
    }
    dirty -= &removed;
    if dirty.len() > MAX_OVERLAY_FILES {
        return Ok(Outcome::Fallback("change reaches too much of the tree"));
    }

    // Files an earlier overlay covers keep their records, copied forward.
    let Ok(prev_tombstones) = (match &cur.overlay {
        Some(o) => Db::load_tombstones(&index_dir.join(o)),
        None => Ok(RoaringBitmap::new()),
    }) else {
        return Ok(Outcome::Fallback("unreadable tombstones"));
    };
    let mut carried = prev_tombstones - &db.removed().clone();
    carried -= &dirty;
    carried -= &removed;
    if carried.len() + dirty.len() > MAX_OVERLAY_FILES {
        return Ok(Outcome::Fallback(
            "overlay would cover too much of the tree",
        ));
    }

    prof.mark("plan");
    // ---- Macros visible to the dirty files: their closures, from cache. ----
    let dirty_list: Vec<FileId> = dirty.iter().collect();
    let closures: FxHashMap<FileId, RoaringBitmap> =
        dirty_list.iter().map(|&f| (f, closure_of(f))).collect();
    let mut scope = dirty.clone();
    for c in closures.values() {
        scope |= c;
    }
    scope -= &removed;
    let mut gm = GlobalMacros::default();
    for f in scope.iter() {
        if is_py(f) {
            continue;
        }
        if let Some(toks) = directive_tokens(f, &mut it, &changed_since_main) {
            collect_defines(f, &toks, &wk, &kw, &mut it, &mut pcs, &mut gm);
        }
    }

    prof.mark("closure macros");
    // ---- Re-extract, in parallel. ----
    let c_lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
    let c_kinds = Kinds::new(&c_lang);
    // The extractor's tree walk is recursive; see the full build.
    let pool = rayon::ThreadPoolBuilder::new()
        .stack_size(64 << 20)
        .build()
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    struct Out {
        file: FileId,
        ex: Extracted,
        names: FxHashMap<u32, String>,
    }
    let mut w = IndexWriter::new();
    for p in &paths {
        w.add_file(p);
    }
    w.set_roots(main.roots().to_vec());

    // In batches, as the full build does, so a large update never holds every
    // re-extracted file at once: unbatched, a 22k-file update peaked at 7.2 GB.
    for chunk in dirty_list.chunks(4096) {
        let outs: Vec<Out> = pool.install(|| {
            chunk
                .par_iter()
                .filter_map(|&file| {
                    let src = std::fs::read(reg.path(file)).ok()?;
                    let mut ti = ThreadInterner::new(&it);
                    let ex = if is_py(file) {
                        let mut p = tree_sitter::Parser::new();
                        p.set_language(&py_lang).ok()?;
                        let tree = p.parse(&src, None)?;
                        PyExtractor::new(&py_kinds, &src, file, &mut ti)
                            .run(&tree)
                            .0
                    } else {
                        let toks = lex(&src, &mut ti);
                        let mut expanded = Vec::with_capacity(toks.len());
                        {
                            let scoped = ScopedMacros::new(&gm, file, &closures[&file]);
                            let mut ex = Expander::new(&scoped, &mut ti, &wk, &pcs);
                            ex.expand_seq(&strip_directives(&toks, &wk), &mut expanded);
                        }
                        let r = render(&expanded, &ti);
                        let mut p = tree_sitter::Parser::new();
                        p.set_language(&c_lang).ok()?;
                        let tree = p.parse(r.text.as_bytes(), None)?;
                        Extractor::new(&c_kinds, &r, file).run(&tree).0
                    };
                    let mut names = FxHashMap::default();
                    for s in &ex.symbols {
                        names
                            .entry(s.name)
                            .or_insert_with(|| ti.text(s.name).to_string());
                    }
                    for o in &ex.occurrences {
                        names
                            .entry(o.name)
                            .or_insert_with(|| ti.text(o.name).to_string());
                    }
                    Some(Out { file, ex, names })
                })
                .collect()
        });

        // Overlay records carry no presence condition: conditions are numbered
        // per shard, and an overlay has no table of its own to resolve them in.
        for out in &outs {
            let base = w.symbol_count() as u32;
            let rebase = |e: u32| if e == NO_SYMBOL { NO_SYMBOL } else { base + e };
            for s in &out.ex.symbols {
                let name = w.intern(&out.names[&s.name]);
                w.push_symbol(SymbolRec {
                    name,
                    file: out.file,
                    off: s.off,
                    len: s.len,
                    enclosing: rebase(s.enclosing),
                    pc: PC_TRUE,
                    kind: s.kind as u8,
                    is_def: s.is_def,
                    ..Default::default()
                });
            }
            for o in &out.ex.occurrences {
                let name = w.intern(&out.names[&o.name]);
                w.push_occurrence(OccRec {
                    name,
                    file: out.file,
                    off: o.off,
                    len: o.len,
                    enclosing: rebase(o.enclosing),
                    pc: PC_TRUE,
                    role: match o.role {
                        Role::Decl => ROLE_DECL,
                        Role::Def => ROLE_DEF,
                        Role::Ref => ROLE_REF,
                        Role::Call => ROLE_CALL,
                    },
                    ..Default::default()
                });
            }
        }
    }
    prof.mark("extract");
    // Macros come from the macro table, as in the full build: their directive
    // lines never reach the parser. Without this, re-indexing a header made
    // its macros vanish from go-to-definition.
    for (name, file, off, _) in gm.definition_sites() {
        if !dirty.contains(file) || is_py(file) {
            continue;
        }
        let text = it.text(name);
        let len = text.len() as u32;
        let name = w.intern(text);
        w.push_symbol(SymbolRec {
            name,
            file,
            off,
            len,
            enclosing: NO_SYMBOL,
            pc: PC_TRUE,
            kind: SymKind::Macro as u8,
            is_def: true,
            ..Default::default()
        });
    }
    if let Some(o) = prev {
        for f in carried.iter() {
            copy_file(o, f, &mut w);
        }
    }

    let mut tombstones = carried.clone();
    tombstones |= &dirty;
    tombstones |= &removed;

    let mut closure_vec = vec![RoaringBitmap::new(); n];
    for f in closure_overrides.iter() {
        closure_vec[f as usize] = closure_of(f);
    }
    w.set_closures(closure_vec);
    let mut edge_vec = vec![Vec::new(); n];
    for f in edge_overrides.iter() {
        edge_vec[f as usize] = edges[f as usize].clone();
    }
    w.set_edges(edge_vec);
    w.add_bitmap(REMOVED, removed.clone());
    w.add_bitmap(CLOSURE_OVERRIDES, closure_overrides);
    w.add_bitmap(EDGE_OVERRIDES, edge_overrides);
    w.add_bitmap(CHANGED, changed_since_main);

    prof.mark("assemble");
    let gen = store::fresh(index_dir, store::Kind::Overlay)?;
    let gen_dir = index_dir.join(&gen);
    w.write(&gen_dir)?;
    Db::save_tombstones(&gen_dir, &tombstones)?;
    let live = store::Pointer {
        main: cur.main,
        overlay: Some(gen),
    };
    store::publish(index_dir, &live)?;
    prof.mark("write overlay");

    // The manifest records each file's macro and edge signatures as they now
    // are, so the next update compares against the truth.
    let mut man = Manifest::default();
    for (path, scan) in hashes {
        let prev = old_manifest.get(path);
        let id = id_of.get(path).copied();
        let exports = id
            .and_then(|f| new_exports.get(&f).copied())
            .unwrap_or_else(|| prev.map(|f| f.exports).unwrap_or(0));
        // Python records no edge signature, matching the full build.
        let edges = match id {
            Some(f) if !path.ends_with(".py") => {
                manifest::combine_unordered(edges[f as usize].iter().map(|&t| t as u64))
            }
            _ => prev.map(|f| f.edges).unwrap_or(0),
        };
        man.insert(
            path.clone(),
            manifest::FileFacts {
                content: scan.content,
                exports,
                edges,
                mtime_ns: scan.mtime_ns,
                size: scan.size,
            },
        );
    }
    // After publishing: a crash in between leaves an older manifest, which only
    // makes the next build redo work, never skip it.
    man.save(index_dir)?;
    prof.mark("manifest");
    store::remove_superseded(index_dir, &live);

    Ok(Outcome::Applied {
        files: dirty_list.len(),
    })
}

/// Per-phase timings on stderr when `CODENAV_PROFILE` is set, as the full
/// build prints. Measuring where an update spends its time is how every
/// speedup here was chosen.
struct Phases {
    on: bool,
    last: std::time::Instant,
}

impl Phases {
    fn new() -> Self {
        Self {
            on: std::env::var_os("CODENAV_PROFILE").is_some(),
            last: std::time::Instant::now(),
        }
    }

    fn mark(&mut self, phase: &str) {
        if self.on {
            eprintln!(
                "  {phase:<28} {:>7.1} ms",
                self.last.elapsed().as_secs_f64() * 1e3
            );
        }
        self.last = std::time::Instant::now();
    }
}

/// Records a file's new direct edges; true if they differ from before.
fn update_edges(
    edges: &mut [Vec<FileId>],
    overrides: &mut RoaringBitmap,
    f: FileId,
    targets: Vec<FileId>,
) -> bool {
    if targets == edges[f as usize] {
        return false;
    }
    edges[f as usize] = targets;
    overrides.insert(f);
    true
}

/// Copies one file's records from a previous overlay into the new one.
/// Symbol ids are renumbered; a file's `enclosing` references only ever point
/// at its own symbols, so a per-file map is enough.
fn copy_file(from: &Index, file: FileId, w: &mut IndexWriter) {
    let ids = from.symbol_ids_in_file(file);
    let mut renum: FxHashMap<u32, u32> = FxHashMap::default();
    let base = w.symbol_count() as u32;
    for (k, &id) in ids.iter().enumerate() {
        renum.insert(id, base + k as u32);
    }
    let map = |e: u32| {
        if e == NO_SYMBOL {
            NO_SYMBOL
        } else {
            renum.get(&e).copied().unwrap_or(NO_SYMBOL)
        }
    };
    for &id in &ids {
        let Some(mut s) = from.symbol(id) else {
            continue;
        };
        s.name = w.intern(from.name_text(s.name).unwrap_or(""));
        s.enclosing = map(s.enclosing);
        w.push_symbol(s);
    }
    for mut o in from.occurrences_in_file(file) {
        o.name = w.intern(from.name_text(o.name).unwrap_or(""));
        o.enclosing = map(o.enclosing);
        w.push_occurrence(o);
    }
}

pub fn strip_directives(toks: &[Token], wk: &WellKnown) -> Vec<Token> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0usize;
    while i < toks.len() {
        let t = toks[i];
        if t.bol() && t.kind == Kind::Punct && t.sym == wk.hash {
            i += 1;
            while i < toks.len() && !toks[i].bol() {
                i += 1;
            }
            continue;
        }
        let mut t = t;
        t.pc = PC_TRUE;
        out.push(t);
        i += 1;
    }
    out
}
