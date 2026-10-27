//! The incremental update path.
//!
//! Only attempted when nothing structural moved. If a changed file's macro
//! exports or its includes changed, we fall back to a full rebuild — those
//! reshape what *other* files see, and chasing that is neither cheaper nor
//! safer than starting over.
//!
//! The win comes from two properties of the model:
//!
//! - a file's symbols depend only on its own text plus the macros visible in
//!   its closure, so a body edit is local;
//! - to re-expand one file you only need the defines from *its closure* — about
//!   623 files on the kernel rather than all 63,779.
//!
//! Results go into a small overlay index and the superseded `FileId`s are
//! tombstoned, so a one-file edit never rewrites the multi-gigabyte main index.

use crate::expand::{Expander, Keywords};
use crate::files::{FileId, FileRegistry, Resolver};
use crate::global::{collect_defines, GlobalMacros, ScopedMacros};
use crate::graph::scan_includes;
use crate::incr::{self, Plan};
use crate::intern::Interner;
use crate::lex::{lex, Kind, Token, WellKnown};
use crate::pc::PcStore;
use crate::py::{PyExtractor, PyKinds};
use crate::render::render;
use crate::symbols::{Extractor, Kinds, Role, NO_SYMBOL};
use codenav_index::manifest::{self, Manifest};
use codenav_index::{
    store, Db, Index, IndexWriter, OccRec, SymbolRec, ROLE_CALL, ROLE_DECL, ROLE_DEF, ROLE_REF,
};
use roaring::RoaringBitmap;
use std::path::{Path, PathBuf};

/// Past this share of files, a clean rebuild beats an ever-growing overlay.
const MAX_OVERLAY_SHARE: f64 = 0.15;

pub enum Outcome {
    /// Overlay written; `files` were re-extracted.
    Applied { files: usize },
    /// Caller should do a full rebuild.
    Fallback(&'static str),
}

/// Attempts an incremental update against an existing index.
pub fn try_update(
    index_dir: &Path,
    changed_paths: &[String],
    hashes: &[(String, manifest::Scan)],
    old_manifest: &Manifest,
) -> std::io::Result<Outcome> {
    let Some(cur) = store::current(index_dir).ok().flatten() else {
        return Ok(Outcome::Fallback("no usable index"));
    };
    let Ok(ix) = Index::open(&index_dir.join(&cur.main)) else {
        return Ok(Outcome::Fallback("no usable index"));
    };

    // Reuse the old path table verbatim so FileIds cannot shift.
    let mut reg = FileRegistry::default();
    for i in 0..ix.file_count() as FileId {
        let Some(p) = ix.path(i) else {
            return Ok(Outcome::Fallback("unreadable path table"));
        };
        reg.add(PathBuf::from(p));
    }

    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let mut pcs = PcStore::for_c(&mut it);
    let resolver = Resolver::new(ix.roots().iter().map(PathBuf::from).collect());

    // Rescan only the changed files, to see whether the change escapes them.
    let mut changes = Vec::with_capacity(changed_paths.len());
    let mut new_exports: rustc_hash::FxHashMap<&str, u64> = Default::default();
    for path in changed_paths {
        let Some(id) = ix.file_id(path) else {
            return Ok(Outcome::Fallback("changed file is not in the index"));
        };
        let Ok(src) = std::fs::read(reg.path(id)) else {
            return Ok(Outcome::Fallback("changed file could not be read"));
        };

        // Python publishes nothing another file's extraction depends on.
        let (exports, edges) = if path.ends_with(".py") {
            (0, 0)
        } else {
            let toks = lex(&src, &mut it);
            let mut one = GlobalMacros::default();
            collect_defines(id, &toks, &wk, &kw, &mut it, &mut pcs, &mut one);
            let exports = one.export_signatures().get(&id).copied().unwrap_or(0);

            let dir = reg
                .path(id)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let mut targets: Vec<u64> = scan_includes(&toks, &wk, &kw, &it)
                .iter()
                .filter_map(|i| {
                    resolver
                        .resolve(&reg, &dir, &i.target, i.angled)
                        .or_else(|| resolver.resolve_by_suffix(&reg, &i.target))
                })
                .map(|t| t as u64)
                .collect();
            targets.sort_unstable();
            targets.dedup();
            (exports, manifest::combine_unordered(targets.into_iter()))
        };

        let was = old_manifest.get(path);
        new_exports.insert(path.as_str(), exports);
        changes.push(incr::FileChange {
            file: id,
            exports_changed: was.is_some_and(|f| f.exports != exports),
            edges_changed: was.is_some_and(|f| f.edges != edges),
        });
    }

    // The reverse closure is only needed when exports actually changed.
    let exporters: Vec<FileId> = changes
        .iter()
        .filter(|c| c.exports_changed)
        .map(|c| c.file)
        .collect();
    let reverse = if exporters.is_empty() {
        Vec::new()
    } else {
        let all: Vec<RoaringBitmap> = (0..ix.file_count() as FileId)
            .map(|i| ix.closure(i).unwrap_or_default())
            .collect();
        incr::reverse_closures(&all, &exporters)
    };
    let lookup = |f: FileId| {
        exporters
            .iter()
            .position(|&e| e == f)
            .map(|i| reverse[i].clone())
            .unwrap_or_default()
    };

    let dirty = match incr::plan(&changes, 0, ix.file_count(), lookup) {
        Plan::UpToDate => return Ok(Outcome::Fallback("nothing to do")),
        Plan::FullRebuild { reason } => return Ok(Outcome::Fallback(reason)),
        Plan::Incremental { dirty } => dirty,
    };

    // An overlay replaces rather than merges, so anything a previous overlay
    // already superseded must be re-extracted into this one too.
    // Unreadable tombstones must not read as "none": the files they name would
    // drop out of the new overlay and answer from their stale main records.
    let mut tombstones = match &cur.overlay {
        Some(o) => match Db::load_tombstones(&index_dir.join(o)) {
            Ok(t) => t,
            Err(_) => return Ok(Outcome::Fallback("unreadable tombstones")),
        },
        None => RoaringBitmap::new(),
    };
    let mut all_dirty: Vec<FileId> = dirty.clone();
    all_dirty.extend(tombstones.iter());
    all_dirty.sort_unstable();
    all_dirty.dedup();

    if all_dirty.len() as f64 > ix.file_count() as f64 * MAX_OVERLAY_SHARE {
        return Ok(Outcome::Fallback(
            "overlay would cover too much of the tree",
        ));
    }

    // Defines from the dirty files' closures — everything their expansion can
    // see, and nothing more.
    let mut closures = vec![RoaringBitmap::new(); ix.file_count()];
    for &f in &all_dirty {
        closures[f as usize] = ix.closure(f).unwrap_or_default();
    }
    let scope = incr::scan_scope(&all_dirty, &closures);

    let mut gm = GlobalMacros::default();
    gm.scan_files(&scope, &reg, &mut it, &wk, &kw, &mut pcs);

    // Re-extract into a fresh overlay.
    let mut w = IndexWriter::new();
    for i in 0..ix.file_count() as FileId {
        w.add_file(ix.path(i).unwrap_or(""));
    }
    w.set_roots(ix.roots().to_vec());

    let c_lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
    let c_kinds = Kinds::new(&c_lang);
    let py_lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    let py_kinds = PyKinds::new(&py_lang);

    for &file in &all_dirty {
        let path = reg.path(file).to_path_buf();
        let Ok(src) = std::fs::read(&path) else {
            continue;
        };
        let is_py = path.extension().and_then(|s| s.to_str()) == Some("py");

        let extracted = if is_py {
            let mut p = tree_sitter::Parser::new();
            if p.set_language(&py_lang).is_err() {
                continue;
            }
            let Some(tree) = p.parse(&src, None) else {
                continue;
            };
            let (ex, _, _) = PyExtractor::new(&py_kinds, &src, file, &mut it).run(&tree);
            ex
        } else {
            let toks = lex(&src, &mut it);
            let closure = closures[file as usize].clone();
            let mut expanded = Vec::with_capacity(toks.len());
            {
                let scoped = ScopedMacros::new(&gm, file, &closure);
                let mut ex = Expander::new(&scoped, &mut it, &wk, &pcs);
                let body = strip_directives(&toks, &wk);
                ex.expand_seq(&body, &mut expanded);
            }
            let r = render(&expanded, &it);
            let mut p = tree_sitter::Parser::new();
            if p.set_language(&c_lang).is_err() {
                continue;
            }
            let Some(tree) = p.parse(r.text.as_bytes(), None) else {
                continue;
            };
            Extractor::new(&c_kinds, &r, file).run(&tree).0
        };

        let base = w.symbol_count() as u32;
        let rebase = |e: u32| if e == NO_SYMBOL { NO_SYMBOL } else { base + e };
        for s in &extracted.symbols {
            let name = w.intern(it.text(s.name));
            w.push_symbol(SymbolRec {
                name,
                file,
                off: s.off,
                len: s.len,
                enclosing: rebase(s.enclosing),
                pc: s.pc,
                kind: s.kind as u8,
                is_def: s.is_def,
                ..Default::default()
            });
        }
        for o in &extracted.occurrences {
            let name = w.intern(it.text(o.name));
            w.push_occurrence(OccRec {
                name,
                file,
                off: o.off,
                len: o.len,
                enclosing: rebase(o.enclosing),
                pc: o.pc,
                role: match o.role {
                    Role::Decl => ROLE_DECL,
                    Role::Def => ROLE_DEF,
                    Role::Ref => ROLE_REF,
                    Role::Call => ROLE_CALL,
                },
                ..Default::default()
            });
        }
        tombstones.insert(file);
    }

    let gen = store::fresh(index_dir, store::Kind::Overlay)?;
    let gen_dir = index_dir.join(&gen);
    w.write(&gen_dir)?;
    Db::save_tombstones(&gen_dir, &tombstones)?;
    let live = store::Pointer {
        main: cur.main,
        overlay: Some(gen),
    };
    store::publish(index_dir, &live)?;

    // Refresh content hashes. Edges are unchanged by construction — any change
    // forced a full rebuild above — but exports may have changed: that only
    // dirtied the dependents. Record the new value, or the next edit would see
    // the same change again and re-dirty them for nothing.
    let mut man = Manifest::default();
    for (path, scan) in hashes {
        let prev = old_manifest.get(path);
        man.insert(
            path.clone(),
            manifest::FileFacts {
                content: scan.content,
                exports: new_exports
                    .get(path.as_str())
                    .copied()
                    .unwrap_or_else(|| prev.map(|f| f.exports).unwrap_or(0)),
                edges: prev.map(|f| f.edges).unwrap_or(0),
                mtime_ns: scan.mtime_ns,
                size: scan.size,
            },
        );
    }
    // After publishing: a crash in between leaves an older manifest, which only
    // makes the next build redo work, never skip it.
    man.save(index_dir)?;
    store::remove_superseded(index_dir, &live);

    Ok(Outcome::Applied {
        files: all_dirty.len(),
    })
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
        t.pc = crate::pc::PC_TRUE;
        out.push(t);
        i += 1;
    }
    out
}
