//! Main index plus an overlay of re-indexed files.
//!
//! Rewriting a multi-gigabyte index for a one-file edit is the thing that makes
//! incremental pointless. Instead the changed files are written to a small
//! second index and their old `FileId`s are tombstoned; queries read both and
//! skip tombstoned results from the main shard.
//!
//! Each shard is internally self-consistent — its own names, symbols and
//! presence conditions — so nothing has to be renumbered when an overlay is
//! added. The only id that crosses a shard boundary is `SymbolId`, which is
//! namespaced by offsetting overlay ids past the end of the main shard.

use crate::store;
use crate::{FileId, Index, NameId, OccRec, SymbolId, SymbolRec};
use roaring::RoaringBitmap;
use std::io;
use std::path::Path;

/// Where a pre-generation index kept its overlay.
const OVERLAY_DIR: &str = "overlay";
pub const TOMBSTONES: &str = "tombstones.bin";
/// Overlay bitmaps; see [`Db`].
pub const REMOVED: &str = "removed";
pub const CLOSURE_OVERRIDES: &str = "closure_overrides";

/// A symbol together with its resolved name.
///
/// Names must travel with the record: `NameId`s are per-shard, so resolving an
/// overlay record against the main name table yields a *different symbol's*
/// name — wrong output that looks entirely plausible.
#[derive(Debug, Clone)]
pub struct Hit {
    pub rec: SymbolRec,
    pub name: String,
}

/// A symbol and the calls it makes to some name.
#[derive(Debug, Clone)]
pub struct Calls {
    pub target: Hit,
    pub sites: Vec<OccRec>,
}

/// An occurrence together with the name of the symbol enclosing it.
#[derive(Debug, Clone)]
pub struct OccHit {
    pub rec: OccRec,
    pub enclosing: Option<String>,
}

pub struct Db {
    main: Index,
    overlay: Option<Index>,
    /// FileIds whose records in `main` have been superseded by the overlay.
    tombstones: RoaringBitmap,
    /// Files deleted since `main` was built. Tombstoned with nothing in their
    /// place, and no longer found by path.
    removed: RoaringBitmap,
    /// Files whose include closure an update recomputed; the overlay holds
    /// the current one.
    closure_overrides: RoaringBitmap,
}

impl Db {
    /// Opens whatever generation is published in `dir`.
    ///
    /// A build can publish and delete the generation named in `CURRENT` between
    /// reading the name and mapping its files. Mapping pins a generation, so
    /// when that happens, retrying lands on the newer one.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let mut tries = 0;
        loop {
            let before = store::stamp(dir);
            match Self::open_once(dir) {
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound
                        && tries < 8
                        && before.is_some()
                        && store::stamp(dir) != before =>
                {
                    tries += 1;
                }
                r => return r,
            }
        }
    }

    fn open_once(dir: &Path) -> io::Result<Self> {
        let Some(p) = store::current(dir)? else {
            return Self::open_legacy(dir);
        };
        let main = Index::open(&dir.join(&p.main))?;
        // The tombstones are part of the overlay generation. An overlay without
        // them would let superseded records answer, so either failing is an
        // error rather than a missing overlay.
        let (overlay, tombstones) = match &p.overlay {
            Some(o) => {
                let d = dir.join(o);
                (Some(Index::open(&d)?), Self::load_tombstones(&d)?)
            }
            None => (None, RoaringBitmap::new()),
        };
        let (removed, closure_overrides) = match &overlay {
            Some(o) => (o.bitmap(REMOVED)?, o.bitmap(CLOSURE_OVERRIDES)?),
            None => Default::default(),
        };
        Ok(Self {
            main,
            overlay,
            tombstones,
            removed,
            closure_overrides,
        })
    }

    /// An index written before generations existed: main at the top level,
    /// overlay beside it. Readable until the next build replaces it.
    fn open_legacy(dir: &Path) -> io::Result<Self> {
        Ok(Self {
            main: Index::open(dir)?,
            overlay: Index::open(&dir.join(OVERLAY_DIR)).ok(),
            tombstones: Self::load_tombstones(dir).unwrap_or_default(),
            removed: RoaringBitmap::new(),
            closure_overrides: RoaringBitmap::new(),
        })
    }

    pub fn load_tombstones(dir: &Path) -> io::Result<RoaringBitmap> {
        let b = std::fs::read(dir.join(TOMBSTONES))?;
        RoaringBitmap::deserialize_from(&b[..])
    }

    pub fn save_tombstones(dir: &Path, t: &RoaringBitmap) -> io::Result<()> {
        let mut buf = Vec::new();
        t.serialize_into(&mut buf)?;
        std::fs::write(dir.join(TOMBSTONES), buf)
    }

    pub fn overlay_files(&self) -> u64 {
        self.tombstones.len()
    }

    pub fn overlay(&self) -> Option<&Index> {
        self.overlay.as_ref()
    }

    pub fn tombstones(&self) -> &RoaringBitmap {
        &self.tombstones
    }

    pub fn removed(&self) -> &RoaringBitmap {
        &self.removed
    }

    pub fn closure_overrides(&self) -> &RoaringBitmap {
        &self.closure_overrides
    }

    pub fn main(&self) -> &Index {
        &self.main
    }

    /// Where overlay symbol ids begin.
    fn overlay_base(&self) -> SymbolId {
        self.main.symbol_count() as SymbolId
    }

    #[inline]
    fn live(&self, file: FileId) -> bool {
        !self.tombstones.contains(file)
    }

    pub fn symbol_count(&self) -> usize {
        self.main.symbol_count() + self.overlay.as_ref().map_or(0, |o| o.symbol_count())
    }

    /// Includes files added since `main` was built, which only the overlay's
    /// path table knows.
    pub fn file_count(&self) -> usize {
        self.overlay
            .as_ref()
            .map_or(0, |o| o.file_count())
            .max(self.main.file_count())
    }

    /// Paths are identical in both shards, so the main one answers.
    pub fn path(&self, id: FileId) -> Option<&str> {
        if id as usize >= self.main.file_count() {
            return self.overlay.as_ref()?.path(id);
        }
        self.main.path(id)
    }

    pub fn file_id(&self, path: &str) -> Option<FileId> {
        let id = self
            .main
            .file_id(path)
            .or_else(|| self.overlay.as_ref()?.file_id(path))?;
        (!self.removed.contains(id)).then_some(id)
    }

    pub fn root_rank(&self, path: &str) -> Option<usize> {
        self.main.root_rank(path)
    }

    /// The overlay's copy when an update recomputed it — after an include was
    /// added or removed, or a file appeared — and main's otherwise.
    pub fn closure(&self, file: FileId) -> Option<RoaringBitmap> {
        if self.closure_overrides.contains(file) {
            return self.overlay.as_ref()?.closure(file);
        }
        self.main.closure(file)
    }

    /// Presence conditions are per-shard. Comparing across shards would be
    /// meaningless, so overlay symbols are treated as unconditional.
    pub fn pc_contradicts(&self, a: u32, b: u32) -> bool {
        self.main.pc_contradicts(a, b)
    }

    pub fn symbol(&self, id: SymbolId) -> Option<SymbolRec> {
        let base = self.overlay_base();
        if id < base {
            self.main.symbol(id)
        } else {
            self.overlay.as_ref()?.symbol(id - base)
        }
    }

    pub fn name_text(&self, id: NameId) -> Option<&str> {
        self.main.name_text(id)
    }

    /// Name text for a symbol, taking the id's shard into account. Overlay
    /// NameIds index the overlay's own name table.
    pub fn symbol_name(&self, id: SymbolId) -> Option<&str> {
        let base = self.overlay_base();
        if id < base {
            self.main.name_text(self.main.symbol(id)?.name)
        } else {
            let o = self.overlay.as_ref()?;
            o.name_text(o.symbol(id - base)?.name)
        }
    }

    /// All symbols with this name. The name is echoed back on each hit so
    /// callers never have to resolve a `NameId` themselves.
    pub fn symbols_named(&self, name: &str) -> Vec<Hit> {
        let mut v: Vec<Hit> = self
            .main
            .symbols_named(name)
            .into_iter()
            .filter(|s| self.live(s.file))
            .map(|rec| Hit {
                rec,
                name: name.to_string(),
            })
            .collect();
        if let Some(o) = &self.overlay {
            v.extend(o.symbols_named(name).into_iter().map(|rec| Hit {
                rec,
                name: name.to_string(),
            }));
        }
        v
    }

    pub fn symbol_ids_named(&self, name: &str) -> Vec<SymbolId> {
        let base = self.overlay_base();
        let mut v: Vec<SymbolId> = self
            .main
            .symbol_ids_named(name)
            .into_iter()
            .filter(|&i| self.main.symbol(i).is_some_and(|s| self.live(s.file)))
            .collect();
        if let Some(o) = &self.overlay {
            v.extend(o.symbol_ids_named(name).into_iter().map(|i| i + base));
        }
        v
    }

    pub fn definitions(&self, name: &str) -> Vec<Hit> {
        let mut v = self.symbols_named(name);
        v.retain(|h| h.rec.is_def);
        v
    }

    /// Up to `limit` references, plus the true number of live matches.
    ///
    /// The count is reported so the editor can say "2,000 of 1,998,373", but
    /// only `limit` results are built — resolving each one's enclosing symbol
    /// name allocates, and doing that two million times to discard all but two
    /// thousand of them dominated the query.
    pub fn references_limited(&self, name: &str, limit: usize) -> (Vec<OccHit>, usize) {
        let mut out = Vec::with_capacity(limit.min(64));

        // The postings length is the total, minus whatever a tombstoned file
        // contributed. Counting the dead ones by walking those files' own
        // occurrence runs costs a few thousand records; recounting by scanning
        // every posting costs all 2M of them, and since an overlay exists after
        // *any* edit, that slow path would be the normal one.
        let ids = self.main.reference_ids(name);
        let mut total = ids.len();
        if !self.tombstones.is_empty() {
            if let Some(nid) = self.main.name_id(name) {
                for f in self.tombstones.iter() {
                    total -= self
                        .main
                        .occurrences_in_file(f)
                        .iter()
                        .filter(|o| o.name == nid)
                        .count();
                }
            }
        }

        for i in ids.iter() {
            if out.len() >= limit {
                break;
            }
            let Some(rec) = self.main.occurrence(i) else {
                continue;
            };
            if self.live(rec.file) {
                out.push(self.hit_in(&self.main, rec));
            }
        }

        if let Some(o) = &self.overlay {
            for i in o.reference_ids(name).iter() {
                let Some(rec) = o.occurrence(i) else { continue };
                total += 1;
                if out.len() < limit {
                    out.push(self.hit_in(o, rec));
                }
            }
        }
        (out, total)
    }

    /// Up to `limit` symbols that call `name`, each with the calls it makes,
    /// plus the true number of callers.
    ///
    /// The call positions are what an editor highlights when you pick a caller
    /// in a call hierarchy. Returning only the caller left it to highlight the
    /// caller's own name, which is not where anything is called.
    pub fn call_sites(&self, name: &str, limit: usize) -> (Vec<Calls>, usize) {
        // Keyed by (shard, enclosing id); first-seen order is occurrence
        // order, i.e. by file and then position, so the output is stable.
        let mut order: Vec<(bool, SymbolId)> = Vec::new();
        let mut sites: rustc_hash::FxHashMap<(bool, SymbolId), Vec<OccRec>> = Default::default();
        let mut collect = |overlay: bool, ix: &Index| {
            for i in ix.reference_ids(name).iter() {
                let Some(o) = ix.occurrence(i) else { continue };
                if o.role != crate::ROLE_CALL || o.enclosing == crate::NO_SYMBOL {
                    continue;
                }
                if !overlay && !self.live(o.file) {
                    continue;
                }
                sites
                    .entry((overlay, o.enclosing))
                    .or_insert_with(|| {
                        order.push((overlay, o.enclosing));
                        Vec::new()
                    })
                    .push(o);
            }
        };
        collect(false, &self.main);
        if let Some(o) = &self.overlay {
            collect(true, o);
        }

        let total = order.len();
        let out = order
            .into_iter()
            .take(limit)
            .filter_map(|key| {
                let ix = if key.0 {
                    self.overlay.as_ref()?
                } else {
                    &self.main
                };
                let rec = ix.symbol(key.1)?;
                Some(Calls {
                    target: Hit {
                        name: ix.name_text(rec.name).unwrap_or("").to_string(),
                        rec,
                    },
                    sites: sites.remove(&key).unwrap_or_default(),
                })
            })
            .collect();
        (out, total)
    }

    /// What `sym` calls: one entry per callee name, in order of first call,
    /// each with the positions of its calls inside `sym`.
    pub fn callee_sites(&self, sym: SymbolId) -> Vec<(String, Vec<OccRec>)> {
        let base = self.overlay_base();
        let (ix, local) = if sym < base {
            (&self.main, sym)
        } else {
            match &self.overlay {
                Some(o) => (o, sym - base),
                None => return Vec::new(),
            }
        };
        let Some(rec) = ix.symbol(local) else {
            return Vec::new();
        };
        // A symbol's calls all lie in its own file, whose occurrences are one
        // contiguous run — a few thousand records at most.
        let mut out: Vec<(String, Vec<OccRec>)> = Vec::new();
        let mut at: rustc_hash::FxHashMap<NameId, usize> = Default::default();
        for o in ix.occurrences_in_file(rec.file) {
            if o.role != crate::ROLE_CALL || o.enclosing != local {
                continue;
            }
            let i = *at.entry(o.name).or_insert_with(|| {
                out.push((ix.name_text(o.name).unwrap_or("").to_string(), Vec::new()));
                out.len() - 1
            });
            out[i].1.push(o);
        }
        out
    }

    /// Up to `limit` callers, plus the true number of live matches.
    pub fn callers_limited(&self, name: &str, limit: usize) -> (Vec<Hit>, usize) {
        let mut out = Vec::with_capacity(limit.min(64));
        let mut total = 0usize;

        for i in self.main.caller_ids(name) {
            let Some(rec) = self.main.symbol(i) else {
                continue;
            };
            if !self.live(rec.file) {
                continue;
            }
            total += 1;
            if out.len() < limit {
                out.push(Hit {
                    name: self.main.name_text(rec.name).unwrap_or("").to_string(),
                    rec,
                });
            }
        }
        if let Some(o) = &self.overlay {
            for i in o.caller_ids(name) {
                let Some(rec) = o.symbol(i) else { continue };
                total += 1;
                if out.len() < limit {
                    out.push(Hit {
                        name: o.name_text(rec.name).unwrap_or("").to_string(),
                        rec,
                    });
                }
            }
        }
        (out, total)
    }

    fn hit_in(&self, ix: &Index, rec: OccRec) -> OccHit {
        let enclosing = (rec.enclosing != crate::NO_SYMBOL)
            .then(|| ix.symbol(rec.enclosing))
            .flatten()
            .and_then(|s| ix.name_text(s.name))
            .map(|s| s.to_string());
        OccHit { enclosing, rec }
    }

    pub fn references(&self, name: &str) -> Vec<OccHit> {
        let enclosing_in = |ix: &Index, rec: &OccRec| -> Option<String> {
            (rec.enclosing != crate::NO_SYMBOL)
                .then(|| ix.symbol(rec.enclosing))
                .flatten()
                .and_then(|s| ix.name_text(s.name))
                .map(|s| s.to_string())
        };

        let mut v: Vec<OccHit> = self
            .main
            .references(name)
            .into_iter()
            .filter(|o| self.live(o.file))
            .map(|rec| OccHit {
                enclosing: enclosing_in(&self.main, &rec),
                rec,
            })
            .collect();
        if let Some(o) = &self.overlay {
            v.extend(o.references(name).into_iter().map(|rec| OccHit {
                enclosing: enclosing_in(o, &rec),
                rec,
            }));
        }
        v
    }

    /// Symbols that call `name`, each with its own resolved name.
    pub fn callers_of(&self, name: &str) -> Vec<Hit> {
        let named = |ix: &Index, rec: SymbolRec| Hit {
            name: ix.name_text(rec.name).unwrap_or("").to_string(),
            rec,
        };
        let mut v: Vec<Hit> = self
            .main
            .callers_of(name)
            .into_iter()
            .filter(|s| self.live(s.file))
            .map(|r| named(&self.main, r))
            .collect();
        if let Some(o) = &self.overlay {
            v.extend(o.callers_of(name).into_iter().map(|r| named(o, r)));
        }
        v
    }

    /// Callee names for a symbol. Resolved in whichever shard owns the id.
    pub fn callees_of(&self, sym: SymbolId) -> Vec<String> {
        let base = self.overlay_base();
        if sym < base {
            self.main
                .callees_of(sym)
                .into_iter()
                .filter_map(|n| self.main.name_text(n).map(|s| s.to_string()))
                .collect()
        } else {
            match &self.overlay {
                Some(o) => o
                    .callees_of(sym - base)
                    .into_iter()
                    .filter_map(|n| o.name_text(n).map(|s| s.to_string()))
                    .collect(),
                None => Vec::new(),
            }
        }
    }

    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<String> {
        let mut v: Vec<String> = self
            .main
            .names_with_prefix(prefix, limit)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        if let Some(o) = &self.overlay {
            for (n, _) in o.names_with_prefix(prefix, limit) {
                if !v.contains(&n) {
                    v.push(n);
                }
            }
        }
        v.sort();
        v.dedup();
        v.truncate(limit);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IndexWriter, NO_SYMBOL};
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "codenav-db-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// main: a.c defines `old_name`; b.c defines `keep`. Published as a fresh
    /// generation, which supersedes any overlay — as a full build does.
    fn build_main(dir: &Path) {
        let g = store::fresh(dir, store::Kind::Main).unwrap();
        write_main(&dir.join(&g));
        let p = store::Pointer {
            main: g,
            overlay: None,
        };
        store::publish(dir, &p).unwrap();
        store::remove_superseded(dir, &p);
    }

    fn write_main(dir: &Path) {
        let mut w = IndexWriter::new();
        let f0 = w.add_file("a.c");
        let f1 = w.add_file("b.c");
        let old = w.intern("old_name");
        let keep = w.intern("keep");
        w.push_symbol(SymbolRec {
            name: old,
            file: f0,
            off: 10,
            len: 8,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        w.push_symbol(SymbolRec {
            name: keep,
            file: f1,
            off: 20,
            len: 4,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        w.push_occurrence(OccRec {
            name: old,
            file: f0,
            off: 10,
            len: 8,
            enclosing: NO_SYMBOL,
            pc: 0,
            role: crate::ROLE_DEF,
            ..Default::default()
        });
        w.write(dir).unwrap();
    }

    /// a.c was edited: `old_name` is gone, `new_name` appeared.
    fn build_overlay(dir: &Path) {
        let g = store::fresh(dir, store::Kind::Overlay).unwrap();
        write_overlay(&dir.join(&g));
        let main = store::current(dir).unwrap().unwrap().main;
        let p = store::Pointer {
            main,
            overlay: Some(g),
        };
        store::publish(dir, &p).unwrap();
        store::remove_superseded(dir, &p);
    }

    fn write_overlay(dir: &Path) {
        let mut w = IndexWriter::new();
        let f0 = w.add_file("a.c");
        w.add_file("b.c");
        let new = w.intern("new_name");
        w.push_symbol(SymbolRec {
            name: new,
            file: f0,
            off: 30,
            len: 8,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        w.push_occurrence(OccRec {
            name: new,
            file: f0,
            off: 30,
            len: 8,
            enclosing: NO_SYMBOL,
            pc: 0,
            role: crate::ROLE_DEF,
            ..Default::default()
        });
        w.write(dir).unwrap();
        let mut t = RoaringBitmap::new();
        t.insert(f0);
        Db::save_tombstones(dir, &t).unwrap();
    }

    /// a.c: `outer` calls `inner` twice and `other` once; b.c: `elsewhere`
    /// calls `inner` once.
    fn build_calls(dir: &Path) {
        let g = store::fresh(dir, store::Kind::Main).unwrap();
        let mut w = IndexWriter::new();
        let a = w.add_file("a.c");
        let b = w.add_file("b.c");
        let (outer, inner, other, elsewhere) = (
            w.intern("outer"),
            w.intern("inner"),
            w.intern("other"),
            w.intern("elsewhere"),
        );
        let def = |name, file, off| SymbolRec {
            name,
            file,
            off,
            len: 5,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        };
        let s_outer = w.push_symbol(def(outer, a, 4));
        let s_else = w.push_symbol(def(elsewhere, b, 4));
        let call = |name, file, off, enclosing| OccRec {
            name,
            file,
            off,
            len: 5,
            enclosing,
            pc: 0,
            role: crate::ROLE_CALL,
            ..Default::default()
        };
        w.push_occurrence(call(inner, a, 20, s_outer));
        w.push_occurrence(call(other, a, 30, s_outer));
        w.push_occurrence(call(inner, a, 40, s_outer));
        w.push_occurrence(call(inner, b, 20, s_else));
        w.write(&dir.join(&g)).unwrap();
        store::publish(
            dir,
            &store::Pointer {
                main: g,
                overlay: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn callers_come_with_the_positions_of_their_calls() {
        let d = tmpdir("call-sites");
        build_calls(&d);
        let db = Db::open(&d).unwrap();
        let (calls, total) = db.call_sites("inner", 10);
        assert_eq!(total, 2);
        let got: Vec<(&str, Vec<u32>)> = calls
            .iter()
            .map(|c| {
                (
                    c.target.name.as_str(),
                    c.sites.iter().map(|o| o.off).collect(),
                )
            })
            .collect();
        assert_eq!(got, [("outer", vec![20, 40]), ("elsewhere", vec![20])]);
        // The cap limits callers, not the count.
        let (calls, total) = db.call_sites("inner", 1);
        assert_eq!((calls.len(), total), (1, 2));
    }

    #[test]
    fn callees_come_with_the_positions_of_their_calls() {
        let d = tmpdir("callee-sites");
        build_calls(&d);
        let db = Db::open(&d).unwrap();
        let outer = db.symbol_ids_named("outer")[0];
        let got: Vec<(String, Vec<u32>)> = db
            .callee_sites(outer)
            .into_iter()
            .map(|(n, s)| (n, s.iter().map(|o| o.off).collect()))
            .collect();
        assert_eq!(
            got,
            [
                ("inner".to_string(), vec![20, 40]),
                ("other".to_string(), vec![30])
            ]
        );
    }

    #[test]
    fn a_superseded_files_call_sites_disappear() {
        let d = tmpdir("call-sites-tomb");
        build_calls(&d);
        // b.c re-indexed with its call removed.
        let g = store::fresh(&d, store::Kind::Overlay).unwrap();
        let mut w = IndexWriter::new();
        w.add_file("a.c");
        let b = w.add_file("b.c");
        let n = w.intern("elsewhere");
        w.push_symbol(SymbolRec {
            name: n,
            file: b,
            off: 4,
            len: 9,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        w.write(&d.join(&g)).unwrap();
        let mut t = RoaringBitmap::new();
        t.insert(b);
        Db::save_tombstones(&d.join(&g), &t).unwrap();
        let main = store::current(&d).unwrap().unwrap().main;
        store::publish(
            &d,
            &store::Pointer {
                main,
                overlay: Some(g),
            },
        )
        .unwrap();

        let db = Db::open(&d).unwrap();
        let (calls, total) = db.call_sites("inner", 10);
        assert_eq!(total, 1);
        assert_eq!(calls[0].target.name, "outer");
    }

    #[test]
    fn an_open_db_keeps_its_snapshot_across_a_rebuild() {
        // The bug this guards: the writer rewrote files a running server had
        // mapped, and the server went on answering from the mix — `old_name`
        // came back empty although it was still defined.
        let d = tmpdir("snapshot");
        build_main(&d);
        let before = Db::open(&d).unwrap();
        build_overlay(&d);
        build_main(&d);
        build_overlay(&d);
        assert_eq!(before.definitions("old_name").len(), 1);
        assert!(before.definitions("new_name").is_empty());
        // A fresh open sees the new generation.
        let after = Db::open(&d).unwrap();
        assert!(after.definitions("old_name").is_empty());
        assert_eq!(after.definitions("new_name").len(), 1);
    }

    #[test]
    fn an_index_from_before_generations_still_opens() {
        let d = tmpdir("legacy");
        write_main(&d);
        write_overlay(&d.join(OVERLAY_DIR));
        let mut t = RoaringBitmap::new();
        t.insert(0);
        Db::save_tombstones(&d, &t).unwrap();
        let db = Db::open(&d).unwrap();
        assert!(db.definitions("old_name").is_empty());
        assert_eq!(db.definitions("new_name").len(), 1);
        assert_eq!(db.definitions("keep").len(), 1);
    }

    #[test]
    fn an_overlay_missing_its_tombstones_is_an_error_not_a_stale_answer() {
        let d = tmpdir("no-tomb");
        build_main(&d);
        build_overlay(&d);
        let o = store::current(&d).unwrap().unwrap().overlay.unwrap();
        std::fs::remove_file(d.join(o).join(TOMBSTONES)).unwrap();
        assert!(Db::open(&d).is_err());
    }

    #[test]
    fn without_an_overlay_the_main_index_answers() {
        let d = tmpdir("plain");
        build_main(&d);
        let db = Db::open(&d).unwrap();
        assert_eq!(db.definitions("old_name").len(), 1);
        assert_eq!(db.overlay_files(), 0);
    }

    #[test]
    fn a_tombstoned_files_old_symbols_disappear() {
        let d = tmpdir("tomb");
        build_main(&d);
        build_overlay(&d);
        let db = Db::open(&d).unwrap();
        assert!(
            db.definitions("old_name").is_empty(),
            "a superseded file must not answer from the main shard"
        );
    }

    #[test]
    fn limiting_changes_how_many_come_back_not_which() {
        let d = tmpdir("limited");
        build_main(&d);
        let db = Db::open(&d).unwrap();

        let full = db.references("old_name");
        let (some, total) = db.references_limited("old_name", 1);
        assert_eq!(total, full.len(), "the count must survive the cap");
        assert_eq!(some.len(), 1.min(full.len()));
        for (a, b) in some.iter().zip(full.iter()) {
            assert_eq!((a.rec.file, a.rec.off), (b.rec.file, b.rec.off));
            assert_eq!(a.enclosing, b.enclosing);
        }

        // A limit past the end must not invent or drop results.
        let (all, total) = db.references_limited("old_name", 999);
        assert_eq!(all.len(), full.len());
        assert_eq!(total, full.len());
    }

    #[test]
    fn limited_queries_respect_tombstones() {
        let d = tmpdir("limited-tomb");
        build_main(&d);
        build_overlay(&d);
        let db = Db::open(&d).unwrap();

        // The fast path is only valid without tombstones; with a.c superseded,
        // its occurrences must not be counted or returned.
        let (hits, total) = db.references_limited("old_name", 10);
        assert_eq!(total, 0);
        assert!(hits.is_empty());

        let (hits, total) = db.references_limited("new_name", 10);
        assert_eq!(total, 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn a_rebuild_must_not_inherit_the_previous_overlay() {
        let d = tmpdir("rebuild");
        build_main(&d);
        build_overlay(&d);
        assert!(Db::open(&d).unwrap().definitions("old_name").is_empty());

        // Rebuilding publishes a new main with no overlay. If the old overlay's
        // tombstone carried over, it would keep masking a symbol that is present
        // and correct in the new index — and nothing would report an error.
        build_main(&d);

        let db = Db::open(&d).unwrap();
        assert_eq!(db.overlay_files(), 0);
        assert_eq!(
            db.definitions("old_name").len(),
            1,
            "a full rebuild must supersede the overlay, not hide behind it"
        );
        assert!(
            db.definitions("new_name").is_empty(),
            "the discarded overlay's symbols must not survive"
        );
    }

    #[test]
    fn the_overlay_supplies_the_new_symbols() {
        let d = tmpdir("new");
        build_main(&d);
        build_overlay(&d);
        let db = Db::open(&d).unwrap();
        let defs = db.definitions("new_name");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].rec.off, 30);
        assert_eq!(defs[0].name, "new_name");
    }

    #[test]
    fn untouched_files_are_unaffected() {
        let d = tmpdir("keep");
        build_main(&d);
        build_overlay(&d);
        let db = Db::open(&d).unwrap();
        assert_eq!(db.definitions("keep").len(), 1, "b.c was not edited");
    }

    #[test]
    fn references_respect_tombstones_too() {
        let d = tmpdir("refs");
        build_main(&d);
        build_overlay(&d);
        let db = Db::open(&d).unwrap();
        assert!(db.references("old_name").is_empty());
        assert_eq!(db.references("new_name").len(), 1);
    }

    #[test]
    fn symbol_ids_are_namespaced_across_shards() {
        let d = tmpdir("ids");
        build_main(&d);
        build_overlay(&d);
        let db = Db::open(&d).unwrap();
        let ids = db.symbol_ids_named("new_name");
        assert_eq!(ids.len(), 1);
        // Overlay ids sit past the end of the main shard, and resolve back to
        // the right record and name.
        assert!(ids[0] >= db.main().symbol_count() as SymbolId);
        assert_eq!(db.symbol_name(ids[0]), Some("new_name"));
        assert_eq!(db.symbol(ids[0]).unwrap().off, 30);
    }

    #[test]
    fn overlay_results_carry_their_own_shards_names() {
        // The regression this guards: overlay NameIds index the overlay's name
        // table, so resolving them against main yields a different symbol's
        // name — plausible-looking but wrong.
        let d = tmpdir("names");
        build_main(&d);
        build_overlay(&d);
        let db = Db::open(&d).unwrap();
        for h in db.callers_of("anything") {
            assert!(!h.name.is_empty());
        }
        let defs = db.definitions("new_name");
        assert_eq!(defs[0].name, "new_name", "name must travel with the record");
    }

    #[test]
    fn prefix_search_sees_both_shards() {
        let d = tmpdir("prefix");
        build_main(&d);
        build_overlay(&d);
        let db = Db::open(&d).unwrap();
        assert!(db
            .names_with_prefix("new", 10)
            .contains(&"new_name".to_string()));
        assert!(db
            .names_with_prefix("keep", 10)
            .contains(&"keep".to_string()));
    }
}
