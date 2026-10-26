//! On-disk index: write-once, memory-mapped, never fully loaded.
//!
//! The dataset is fully known when the build finishes, so the index is sorted
//! and packed rather than stored in a B-tree — page overhead and MVCC would buy
//! nothing here. Names live in an `fst`, which additionally gives the ordered
//! prefix search that workspace-symbol needs.
//!
//! The central design point: **the on-disk graph is the answer**. Resolution
//! happens at index time, so a query is a couple of seeks rather than a
//! computation.

use fst::{IntoStreamer, Map, MapBuilder, Streamer};
use memmap2::Mmap;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

pub mod csr;
pub mod db;
pub mod manifest;
pub mod store;
pub use csr::Csr;
pub use db::Db;

pub type NameId = u32;
pub type FileId = u32;
pub type SymbolId = u32;

pub const NO_SYMBOL: SymbolId = u32::MAX;

/// Fixed-width so an id is an array index and lookup is O(1).
pub const SYMBOL_REC: usize = 32;
pub const OCC_REC: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SymbolRec {
    pub name: NameId,
    pub file: FileId,
    pub off: u32,
    pub len: u32,
    pub enclosing: SymbolId,
    pub pc: u32,
    pub kind: u8,
    pub is_def: bool,
    /// See [`OccRec::line`].
    pub line: u32,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OccRec {
    pub name: NameId,
    pub file: FileId,
    pub off: u32,
    pub len: u32,
    pub enclosing: SymbolId,
    pub pc: u32,
    pub role: u8,
    /// 1-based line and column, resolved at index time into bytes that were
    /// already padding. Deriving these at query time meant reading the source
    /// file, which was the whole cost of a large `refs` or `callers` result.
    pub line: u32,
    pub col: u16,
}

/// Columns are stored in 16 bits. Real code does not reach this; generated or
/// minified lines can, and clamping keeps them pointing at the right line.
const MAX_COL: u32 = u16::MAX as u32;

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl SymbolRec {
    fn encode(&self, buf: &mut Vec<u8>) {
        put_u32(buf, self.name);
        put_u32(buf, self.file);
        put_u32(buf, self.off);
        put_u32(buf, self.len);
        put_u32(buf, self.enclosing);
        put_u32(buf, self.pc);
        buf.push(self.kind);
        buf.push(self.is_def as u8);
        put_u32(buf, self.line);
        buf.extend_from_slice(&self.col.to_le_bytes());
    }

    fn decode(b: &[u8]) -> Self {
        Self {
            name: get_u32(b, 0),
            file: get_u32(b, 4),
            off: get_u32(b, 8),
            len: get_u32(b, 12),
            enclosing: get_u32(b, 16),
            pc: get_u32(b, 20),
            kind: b[24],
            is_def: b[25] != 0,
            line: get_u32(b, 26),
            col: u16::from_le_bytes([b[30], b[31]]),
        }
    }
}

impl OccRec {
    fn encode(&self, buf: &mut Vec<u8>) {
        put_u32(buf, self.name);
        put_u32(buf, self.file);
        put_u32(buf, self.off);
        put_u32(buf, self.len);
        put_u32(buf, self.enclosing);
        put_u32(buf, self.pc);
        buf.push(self.role);
        buf.push(0);
        put_u32(buf, self.line);
        buf.extend_from_slice(&self.col.to_le_bytes());
    }

    fn decode(b: &[u8]) -> Self {
        Self {
            name: get_u32(b, 0),
            file: get_u32(b, 4),
            off: get_u32(b, 8),
            len: get_u32(b, 12),
            enclosing: get_u32(b, 16),
            pc: get_u32(b, 20),
            role: b[24],
            line: get_u32(b, 26),
            col: u16::from_le_bytes([b[30], b[31]]),
        }
    }
}

/// Fills in line and column on every record, from the source files themselves.
///
/// Done here, once, rather than per query. Only files that actually carry a
/// record are read — which matters for the incremental path, where the writer
/// holds the whole path table but re-extracts a handful of files.
fn resolve_positions(paths: &[String], symbols: &mut [SymbolRec], occs: &mut [OccRec]) {
    let mut needed = vec![false; paths.len()];
    for s in symbols.iter() {
        if let Some(f) = needed.get_mut(s.file as usize) {
            *f = true;
        }
    }
    for o in occs.iter() {
        if let Some(f) = needed.get_mut(o.file as usize) {
            *f = true;
        }
    }

    // Line start offsets per file. Absent means "not referenced" or unreadable;
    // records in that file keep line 0, which the caller renders as unknown
    // rather than pretending to be line 1.
    let tables: Vec<Option<Vec<u32>>> = paths
        .par_iter()
        .zip(needed.par_iter())
        .map(|(p, &want)| {
            if !want {
                return None;
            }
            let bytes = std::fs::read(Path::new(p)).ok()?;
            let mut v = vec![0u32];
            v.extend(
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, &c)| c == b'\n')
                    .map(|(i, _)| i as u32 + 1),
            );
            Some(v)
        })
        .collect();

    let at = |file: u32, off: u32| -> (u32, u16) {
        let Some(Some(starts)) = tables.get(file as usize) else {
            return (0, 0);
        };
        let i = starts.partition_point(|&s| s <= off).saturating_sub(1);
        let col = (off - starts[i] + 1).min(MAX_COL) as u16;
        (i as u32 + 1, col)
    };

    symbols.par_iter_mut().for_each(|s| {
        let (l, c) = at(s.file, s.off);
        s.line = l;
        s.col = c;
    });
    occs.par_iter_mut().for_each(|o| {
        let (l, c) = at(o.file, o.off);
        o.line = l;
        o.col = c;
    });
}

/// Accumulates an index in memory, then writes it sorted and packed.
#[derive(Default)]
pub struct IndexWriter {
    names: Vec<String>,
    name_ids: HashMap<String, NameId>,
    paths: Vec<String>,
    symbols: Vec<SymbolRec>,
    occurrences: Vec<OccRec>,
    /// Per-file include closure — the strongest ranking signal, and the reason
    /// a definition in a file you actually include beats one that merely
    /// shares a name.
    closures: Vec<RoaringBitmap>,
    /// Raw literals per PcId, so contradiction can be tested at query time.
    pcs: Vec<Vec<u32>>,
    /// Inferred include roots, best-voted first. A definition under the
    /// canonical root is more authoritative than one in a driver or sample.
    roots: Vec<String>,
}

impl IndexWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.name_ids.get(name) {
            return id;
        }
        let id = self.names.len() as NameId;
        self.names.push(name.to_string());
        self.name_ids.insert(name.to_string(), id);
        id
    }

    pub fn add_file(&mut self, path: &str) -> FileId {
        let id = self.paths.len() as FileId;
        self.paths.push(path.to_string());
        id
    }

    pub fn push_symbol(&mut self, s: SymbolRec) -> SymbolId {
        let id = self.symbols.len() as SymbolId;
        self.symbols.push(s);
        id
    }

    pub fn push_occurrence(&mut self, o: OccRec) {
        self.occurrences.push(o);
    }

    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    pub fn occurrence_count(&self) -> usize {
        self.occurrences.len()
    }

    pub fn set_closures(&mut self, c: Vec<RoaringBitmap>) {
        self.closures = c;
    }

    pub fn set_pcs(&mut self, p: Vec<Vec<u32>>) {
        self.pcs = p;
    }

    pub fn set_roots(&mut self, r: Vec<String>) {
        self.roots = r;
    }

    pub fn write(self, dir: &Path) -> io::Result<IndexStats> {
        std::fs::create_dir_all(dir)?;
        let n_names = self.names.len();

        // fst requires keys in sorted order.
        let mut sorted: Vec<(&String, NameId)> = self
            .names
            .iter()
            .enumerate()
            .map(|(i, s)| (s, i as NameId))
            .collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));

        let f = BufWriter::new(File::create(dir.join("names.fst"))?);
        let mut b = MapBuilder::new(f).map_err(fst_err)?;
        for (s, id) in &sorted {
            b.insert(s.as_bytes(), *id as u64).map_err(fst_err)?;
        }
        b.finish().map_err(fst_err)?;

        // Reverse table: NameId -> text, so call hierarchy does not have to
        // stream the whole fst once per result.
        let mut nbuf = Vec::new();
        let mut noff: Vec<u32> = Vec::with_capacity(n_names + 1);
        for s in &self.names {
            noff.push(nbuf.len() as u32);
            nbuf.extend_from_slice(s.as_bytes());
        }
        noff.push(nbuf.len() as u32);
        write_u32s(dir.join("names.idx"), &noff)?;
        write_bytes(dir.join("names.bin"), &nbuf)?;

        // A second fst over paths, so mapping an editor-supplied path to a
        // FileId is O(path length). Scanning the table instead cost ~4 ms on
        // every context-carrying query, which is every go-to-definition.
        let mut sorted_paths: Vec<(&String, FileId)> = self
            .paths
            .iter()
            .enumerate()
            .map(|(i, s)| (s, i as FileId))
            .collect();
        sorted_paths.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let f = BufWriter::new(File::create(dir.join("paths.fst"))?);
        let mut pb = MapBuilder::new(f).map_err(fst_err)?;
        let mut prev: Option<&str> = None;
        for (s, id) in &sorted_paths {
            // fst rejects duplicate keys; a repeated path would be the same file.
            if prev == Some(s.as_str()) {
                continue;
            }
            pb.insert(s.as_bytes(), *id as u64).map_err(fst_err)?;
            prev = Some(s);
        }
        pb.finish().map_err(fst_err)?;

        // Paths are stored as a length-prefixed table so FileId indexes it.
        let mut pbuf = Vec::new();
        let mut poff: Vec<u32> = Vec::with_capacity(self.paths.len() + 1);
        for p in &self.paths {
            poff.push(pbuf.len() as u32);
            pbuf.extend_from_slice(p.as_bytes());
        }
        poff.push(pbuf.len() as u32);
        write_u32s(dir.join("paths.idx"), &poff)?;
        write_bytes(dir.join("paths.bin"), &pbuf)?;

        write_bytes(dir.join("roots.txt"), self.roots.join("\n").as_bytes())?;

        // Occurrences are sorted by (file, offset) so a file's occurrences are
        // one contiguous run — that is what makes "what is at this position"
        // a single seek.
        let mut occs = self.occurrences;
        occs.par_sort_unstable_by_key(|o| (o.file, o.off));

        let mut symbols = self.symbols;
        resolve_positions(&self.paths, &mut symbols, &mut occs);

        write_records(dir.join("symbols.bin"), &symbols, SymbolRec::encode)?;
        write_records(dir.join("occs.bin"), &occs, OccRec::encode)?;

        // name -> symbols
        // The five posting tables are independent, and building them dominates
        // the write phase, so they are built and written in parallel.
        let symbols = &symbols;
        let occs_ref = &occs;
        let results: Vec<io::Result<()>> = [0usize, 1, 2, 3, 4, 5]
            .par_iter()
            .map(|&which| -> io::Result<()> {
                match which {
                    0 => Csr::build(n_names, || {
                        symbols
                            .iter()
                            .enumerate()
                            .map(|(i, s)| (s.name as usize, i as u32))
                    })
                    .write(dir, "sym_by_name"),
                    1 => Csr::build(n_names, || {
                        occs_ref
                            .iter()
                            .enumerate()
                            .map(|(i, o)| (o.name as usize, i as u32))
                    })
                    .write(dir, "occ_by_name"),
                    // callee name -> calling symbols, for "who calls this"
                    2 => Csr::build(n_names, || {
                        occs_ref
                            .iter()
                            .filter(|o| o.role == ROLE_CALL && o.enclosing != NO_SYMBOL)
                            .map(|o| (o.name as usize, o.enclosing))
                    })
                    .write(dir, "callers"),
                    // calling symbol -> callee names, for "what does this call"
                    3 => Csr::build(symbols.len(), || {
                        occs_ref
                            .iter()
                            .filter(|o| o.role == ROLE_CALL && o.enclosing != NO_SYMBOL)
                            .map(|o| (o.enclosing as usize, o.name))
                    })
                    .write(dir, "calls"),
                    4 => Csr::build(self.paths.len(), || {
                        occs_ref
                            .iter()
                            .enumerate()
                            .map(|(i, o)| (o.file as usize, i as u32))
                    })
                    .write(dir, "occ_by_file"),
                    // file -> symbols, so an unchanged file's records can be copied
                    // forward during an incremental rebuild.
                    _ => Csr::build(self.paths.len(), || {
                        symbols
                            .iter()
                            .enumerate()
                            .map(|(i, s)| (s.file as usize, i as u32))
                    })
                    .write(dir, "sym_by_file"),
                }
            })
            .collect();
        for r in results {
            r?;
        }

        // Per-file include closures, each a serialized roaring bitmap.
        let mut cbuf: Vec<u8> = Vec::new();
        let mut coff: Vec<u32> = Vec::with_capacity(self.closures.len() + 1);
        for c in &self.closures {
            coff.push(cbuf.len() as u32);
            let _ = c.serialize_into(&mut cbuf);
        }
        coff.push(cbuf.len() as u32);
        write_u32s(dir.join("closures.idx"), &coff)?;
        write_bytes(dir.join("closures.bin"), &cbuf)?;

        // Presence-condition cubes: a flat u32 literal list per PcId.
        let mut qbuf: Vec<u32> = Vec::new();
        let mut qoff: Vec<u32> = Vec::with_capacity(self.pcs.len() + 1);
        for lits in &self.pcs {
            qoff.push(qbuf.len() as u32);
            qbuf.extend_from_slice(lits);
        }
        qoff.push(qbuf.len() as u32);
        write_u32s(dir.join("pcs.idx"), &qoff)?;
        write_u32s(dir.join("pcs.bin"), &qbuf)?;

        Ok(IndexStats {
            names: n_names,
            files: self.paths.len(),
            symbols: symbols.len(),
            occurrences: occs.len(),
            bytes_on_disk: dir_size(dir),
        })
    }
}

pub const ROLE_DECL: u8 = 0;
pub const ROLE_DEF: u8 = 1;
pub const ROLE_REF: u8 = 2;
pub const ROLE_CALL: u8 = 3;

#[derive(Debug, Clone, Copy)]
pub struct IndexStats {
    pub names: usize,
    pub files: usize,
    pub symbols: usize,
    pub occurrences: usize,
    pub bytes_on_disk: u64,
}

fn fst_err(e: fst::Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// Encodes records straight to disk in small chunks. Building the whole file in
/// memory first doubled peak usage — 3.2 GB for the kernel's occurrences alone,
/// on top of the records themselves.
fn write_records<T>(p: PathBuf, items: &[T], encode: fn(&T, &mut Vec<u8>)) -> io::Result<()> {
    let mut f = BufWriter::with_capacity(1 << 20, File::create(p)?);
    let mut buf = Vec::with_capacity(64 * 1024);
    for chunk in items.chunks(2048) {
        buf.clear();
        for it in chunk {
            encode(it, &mut buf);
        }
        f.write_all(&buf)?;
    }
    f.flush()
}

fn write_bytes(p: PathBuf, b: &[u8]) -> io::Result<()> {
    let mut f = BufWriter::new(File::create(p)?);
    f.write_all(b)?;
    f.flush()
}

fn write_u32s(p: PathBuf, v: &[u32]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(v.len() * 4);
    for &x in v {
        buf.extend_from_slice(&x.to_le_bytes());
    }
    write_bytes(p, &buf)
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// A memory-mapped index. Opening it maps files but reads nothing; a query
/// touches only the pages it needs.
pub struct Index {
    names: Map<Mmap>,
    /// NameId -> text, the reverse of the fst.
    names_bin: Mmap,
    names_idx: Mmap,
    paths_fst: Map<Mmap>,
    paths: Mmap,
    paths_idx: Mmap,
    symbols: Mmap,
    occs: Mmap,
    sym_by_name: Csr,
    occ_by_name: Csr,
    callers: Csr,
    calls: Csr,
    occ_by_file: Csr,
    sym_by_file: Csr,
    closures: Mmap,
    closures_idx: Mmap,
    pcs: Mmap,
    pcs_idx: Mmap,
    roots: Vec<String>,
}

impl Index {
    pub fn open(dir: &Path) -> io::Result<Self> {
        Ok(Self {
            names: Map::new(map(dir.join("names.fst"))?).map_err(fst_err)?,
            names_bin: map(dir.join("names.bin"))?,
            names_idx: map(dir.join("names.idx"))?,
            paths_fst: Map::new(map(dir.join("paths.fst"))?).map_err(fst_err)?,
            paths: map(dir.join("paths.bin"))?,
            paths_idx: map(dir.join("paths.idx"))?,
            symbols: map(dir.join("symbols.bin"))?,
            occs: map(dir.join("occs.bin"))?,
            sym_by_name: Csr::open(dir, "sym_by_name")?,
            occ_by_name: Csr::open(dir, "occ_by_name")?,
            callers: Csr::open(dir, "callers")?,
            calls: Csr::open(dir, "calls")?,
            occ_by_file: Csr::open(dir, "occ_by_file")?,
            sym_by_file: Csr::open(dir, "sym_by_file")?,
            closures: map(dir.join("closures.bin"))?,
            closures_idx: map(dir.join("closures.idx"))?,
            pcs: map(dir.join("pcs.bin"))?,
            pcs_idx: map(dir.join("pcs.idx"))?,
            roots: std::fs::read_to_string(dir.join("roots.txt"))
                .unwrap_or_default()
                .lines()
                .map(|l| l.to_string())
                .collect(),
        })
    }

    pub fn name_id(&self, name: &str) -> Option<NameId> {
        self.names.get(name).map(|v| v as NameId)
    }

    pub fn symbol_count(&self) -> usize {
        self.symbols.len() / SYMBOL_REC
    }

    pub fn occurrence_count(&self) -> usize {
        self.occs.len() / OCC_REC
    }

    pub fn file_count(&self) -> usize {
        (self.paths_idx.len() / 4).saturating_sub(1)
    }

    pub fn symbol(&self, id: SymbolId) -> Option<SymbolRec> {
        let at = id as usize * SYMBOL_REC;
        (at + SYMBOL_REC <= self.symbols.len())
            .then(|| SymbolRec::decode(&self.symbols[at..at + SYMBOL_REC]))
    }

    pub fn occurrence(&self, i: u32) -> Option<OccRec> {
        let at = i as usize * OCC_REC;
        (at + OCC_REC <= self.occs.len()).then(|| OccRec::decode(&self.occs[at..at + OCC_REC]))
    }

    pub fn path(&self, id: FileId) -> Option<&str> {
        let i = id as usize;
        if (i + 1) * 4 + 4 > self.paths_idx.len() {
            return None;
        }
        let s = get_u32(&self.paths_idx, i * 4) as usize;
        let e = get_u32(&self.paths_idx, (i + 1) * 4) as usize;
        std::str::from_utf8(&self.paths[s..e]).ok()
    }

    /// Symbol *ids* for a name. Needed because callers otherwise scan every
    /// record to recover an id, which is O(symbols) per query.
    pub fn symbol_ids_named(&self, name: &str) -> Vec<SymbolId> {
        match self.name_id(name) {
            Some(nid) => self.sym_by_name.get(nid as usize),
            None => Vec::new(),
        }
    }

    /// All symbols carrying a name, definitions and declarations alike.
    pub fn symbols_named(&self, name: &str) -> Vec<SymbolRec> {
        let Some(nid) = self.name_id(name) else {
            return Vec::new();
        };
        self.sym_by_name
            .get(nid as usize)
            .iter()
            .filter_map(|&i| self.symbol(i))
            .collect()
    }

    /// Definitions only — what go-to-definition ranks.
    pub fn definitions(&self, name: &str) -> Vec<SymbolRec> {
        let mut v = self.symbols_named(name);
        v.retain(|s| s.is_def);
        v
    }

    pub fn references(&self, name: &str) -> Vec<OccRec> {
        self.reference_ids(name)
            .iter()
            .filter_map(|i| self.occurrence(i))
            .collect()
    }

    /// Occurrence ids for a name, straight out of the postings.
    ///
    /// Zero-copy, so a caller that only wants the first N never pays for the
    /// rest. `file` has ~2M occurrences in the kernel, and materializing all of
    /// them to return 2,000 is most of what a `refs` query used to cost.
    pub fn reference_ids(&self, name: &str) -> crate::csr::Postings<'_> {
        match self.name_id(name) {
            Some(nid) => self.occ_by_name.postings(nid as usize),
            None => Default::default(),
        }
    }

    /// Symbols that call `name`. One seek, because every occurrence stored its
    /// enclosing symbol at index time.
    pub fn callers_of(&self, name: &str) -> Vec<SymbolRec> {
        self.caller_ids(name)
            .iter()
            .filter_map(|&i| self.symbol(i))
            .collect()
    }

    /// Deduplicated ids of the symbols that call `name`.
    pub fn caller_ids(&self, name: &str) -> Vec<u32> {
        let Some(nid) = self.name_id(name) else {
            return Vec::new();
        };
        let mut ids: Vec<u32> = self.callers.get(nid as usize).to_vec();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Names called by a symbol.
    pub fn callees_of(&self, sym: SymbolId) -> Vec<NameId> {
        let mut v: Vec<NameId> = self.calls.get(sym as usize).to_vec();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Include closure of a file, deserialized on demand. Only the queried
    /// file's bitmap is touched, so this costs a few pages rather than reading
    /// the whole table.
    pub fn closure(&self, file: FileId) -> Option<RoaringBitmap> {
        let i = file as usize;
        if (i + 1) * 4 + 4 > self.closures_idx.len() {
            return None;
        }
        let s = get_u32(&self.closures_idx, i * 4) as usize;
        let e = get_u32(&self.closures_idx, (i + 1) * 4) as usize;
        RoaringBitmap::deserialize_from(&self.closures[s..e]).ok()
    }

    fn pc_lits(&self, pc: u32) -> &[u8] {
        let i = pc as usize;
        if (i + 1) * 4 + 4 > self.pcs_idx.len() {
            return &[];
        }
        let s = get_u32(&self.pcs_idx, i * 4) as usize;
        let e = get_u32(&self.pcs_idx, (i + 1) * 4) as usize;
        if e * 4 > self.pcs.len() || s > e {
            return &[];
        }
        &self.pcs[s * 4..e * 4]
    }

    /// True when no configuration satisfies both conditions — the same
    /// merge-scan as `PcStore::contradicts`, over the mapped literal lists.
    pub fn pc_contradicts(&self, a: u32, b: u32) -> bool {
        let (x, y) = (self.pc_lits(a), self.pc_lits(b));
        let (nx, ny) = (x.len() / 4, y.len() / 4);
        let (mut i, mut j) = (0usize, 0usize);
        while i < nx && j < ny {
            let la = get_u32(x, i * 4);
            let lb = get_u32(y, j * 4);
            match (la >> 1).cmp(&(lb >> 1)) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    if la & 1 != lb & 1 {
                        return true;
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        false
    }

    /// The inferred include roots this index was built with.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Index of the best inferred include root containing `path`, if any.
    /// Lower is more authoritative.
    ///
    /// Both sides are trimmed of a leading `./`: stored paths keep the prefix
    /// the tree was indexed with, while roots come from normalised paths that
    /// do not. Comparing them raw silently never matches.
    pub fn root_rank(&self, path: &str) -> Option<usize> {
        let p = path.trim_start_matches("./");
        self.roots.iter().position(|r| {
            let r = r.trim_start_matches("./");
            // Require a component boundary so `include` does not match
            // `includeX/...`.
            !r.is_empty() && p.len() > r.len() && p.starts_with(r) && p.as_bytes()[r.len()] == b'/'
        })
    }

    /// FileId for a path, via the path fst.
    pub fn file_id(&self, path: &str) -> Option<FileId> {
        self.paths_fst.get(path).map(|v| v as FileId)
    }

    /// Symbols declared in a file, in index order. Their `enclosing` fields
    /// point within this same run, so the run can be relocated by shifting
    /// them uniformly.
    pub fn symbols_in_file(&self, file: FileId) -> Vec<SymbolRec> {
        self.sym_by_file
            .get(file as usize)
            .iter()
            .filter_map(|&i| self.symbol(i))
            .collect()
    }

    /// Index of a file's first symbol, which is the base its `enclosing`
    /// references are relative to.
    pub fn symbol_base(&self, file: FileId) -> Option<SymbolId> {
        self.sym_by_file.get(file as usize).first().copied()
    }

    pub fn occurrences_in_file(&self, file: FileId) -> Vec<OccRec> {
        self.occ_by_file
            .get(file as usize)
            .iter()
            .filter_map(|&i| self.occurrence(i))
            .collect()
    }

    /// Names beginning with `prefix`, for workspace-symbol. Ordered by the fst,
    /// so this streams rather than scanning every name.
    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, NameId)> {
        let mut out = Vec::new();
        let mut s = self.names.range().ge(prefix).into_stream();
        while let Some((k, v)) = s.next() {
            if !k.starts_with(prefix.as_bytes()) {
                break;
            }
            if let Ok(name) = std::str::from_utf8(k) {
                out.push((name.to_string(), v as NameId));
            }
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// Text of a NameId, via a reverse table rather than the fst.
    ///
    /// The fst maps text to id, so reversing it means streaming every key.
    /// Call hierarchy resolves one name per caller, and `kfree` has thousands,
    /// so a scan here turned a millisecond query into minutes.
    pub fn name_text(&self, id: NameId) -> Option<&str> {
        let i = id as usize;
        if (i + 1) * 4 + 4 > self.names_idx.len() {
            return None;
        }
        let s = get_u32(&self.names_idx, i * 4) as usize;
        let e = get_u32(&self.names_idx, (i + 1) * 4) as usize;
        std::str::from_utf8(&self.names_bin[s..e]).ok()
    }
}

fn map(p: PathBuf) -> io::Result<Mmap> {
    let f = File::open(p)?;
    // Safety: the index is written once and not mutated while open.
    unsafe { Mmap::map(&f) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "codenav-idx-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Builds: file0 defines `main` calling `helper`; file1 defines `helper`.
    fn sample(dir: &Path) {
        let mut w = IndexWriter::new();
        let f0 = w.add_file("src/main.c");
        let f1 = w.add_file("src/util.c");
        let n_main = w.intern("main");
        let n_helper = w.intern("helper");
        let n_zzz = w.intern("zzz_last");

        let main_id = w.push_symbol(SymbolRec {
            name: n_main,
            file: f0,
            off: 10,
            len: 4,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        w.push_symbol(SymbolRec {
            name: n_helper,
            file: f1,
            off: 20,
            len: 6,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        // A declaration of helper, which must not win over the definition.
        w.push_symbol(SymbolRec {
            name: n_helper,
            file: f0,
            off: 2,
            len: 6,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: false,
            ..Default::default()
        });
        w.push_symbol(SymbolRec {
            name: n_zzz,
            file: f1,
            off: 90,
            len: 8,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });

        w.push_occurrence(OccRec {
            name: n_helper,
            file: f0,
            off: 30,
            len: 6,
            enclosing: main_id,
            pc: 0,
            role: ROLE_CALL,
            ..Default::default()
        });
        w.push_occurrence(OccRec {
            name: n_helper,
            file: f0,
            off: 50,
            len: 6,
            enclosing: main_id,
            pc: 0,
            role: ROLE_REF,
            ..Default::default()
        });
        w.write(dir).unwrap();
    }

    #[test]
    fn root_rank_matches_regardless_of_leading_dot_slash() {
        let d = tmpdir("roots");
        let mut w = IndexWriter::new();
        w.add_file("./include/linux/minmax.h");
        w.add_file("./drivers/gpu/thing.c");
        w.add_file("./includeX/other.h");
        w.set_roots(vec!["include".into(), "tools/include".into()]);
        w.write(&d).unwrap();
        let ix = Index::open(&d).unwrap();

        assert_eq!(ix.root_rank("./include/linux/minmax.h"), Some(0));
        assert_eq!(ix.root_rank("./drivers/gpu/thing.c"), None);
        // Must not match on a bare string prefix.
        assert_eq!(ix.root_rank("./includeX/other.h"), None);
    }

    #[test]
    fn a_files_symbols_are_contiguous_and_relocatable() {
        // Incremental rebuild relies on this: a file's symbols form one run,
        // and their `enclosing` ids point inside it, so the run can be moved by
        // shifting every reference by the same delta.
        let d = tmpdir("byfile-sym");
        let mut w = IndexWriter::new();
        let f0 = w.add_file("a.c");
        let f1 = w.add_file("b.c");
        let n = w.intern("x");
        // b.c gets two symbols, the second enclosed by the first.
        w.push_symbol(SymbolRec {
            name: n,
            file: f0,
            off: 0,
            len: 1,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        let outer = w.push_symbol(SymbolRec {
            name: n,
            file: f1,
            off: 10,
            len: 1,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        w.push_symbol(SymbolRec {
            name: n,
            file: f1,
            off: 20,
            len: 1,
            enclosing: outer,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        w.write(&d).unwrap();

        let ix = Index::open(&d).unwrap();
        let run = ix.symbols_in_file(f1);
        assert_eq!(run.len(), 2);
        let base = ix.symbol_base(f1).unwrap();
        assert_eq!(base, outer, "the run starts at the file's first symbol");
        // The inner symbol's enclosing is expressible as base + offset.
        assert_eq!(run[1].enclosing - base, 0);
        assert!(ix.symbols_in_file(f0).len() == 1);
    }

    #[test]
    fn round_trips_definitions() {
        let d = tmpdir("def");
        sample(&d);
        let ix = Index::open(&d).unwrap();
        let defs = ix.definitions("helper");
        assert_eq!(
            defs.len(),
            1,
            "the declaration must not count as a definition"
        );
        assert_eq!(ix.path(defs[0].file), Some("src/util.c"));
        assert_eq!(defs[0].off, 20);
    }

    #[test]
    fn declarations_and_definitions_are_both_retrievable() {
        let d = tmpdir("both");
        sample(&d);
        let ix = Index::open(&d).unwrap();
        assert_eq!(ix.symbols_named("helper").len(), 2);
    }

    #[test]
    fn references_include_calls_and_reads() {
        let d = tmpdir("refs");
        sample(&d);
        let ix = Index::open(&d).unwrap();
        assert_eq!(ix.references("helper").len(), 2);
    }

    #[test]
    fn call_hierarchy_resolves_both_directions() {
        let d = tmpdir("calls");
        sample(&d);
        let ix = Index::open(&d).unwrap();

        let callers = ix.callers_of("helper");
        assert_eq!(callers.len(), 1);
        assert_eq!(ix.name_text(callers[0].name), Some("main"));

        let main_sym = ix
            .symbols_named("main")
            .first()
            .copied()
            .expect("main exists");
        let main_id = (0..ix.symbol_count() as u32)
            .find(|&i| ix.symbol(i).unwrap() == main_sym)
            .unwrap();
        let callees = ix.callees_of(main_id);
        assert_eq!(callees.len(), 1);
        assert_eq!(ix.name_text(callees[0]), Some("helper"));
    }

    #[test]
    fn unknown_names_return_empty_not_error() {
        let d = tmpdir("miss");
        sample(&d);
        let ix = Index::open(&d).unwrap();
        assert!(ix.definitions("nonexistent").is_empty());
        assert!(ix.references("nonexistent").is_empty());
        assert!(ix.callers_of("nonexistent").is_empty());
    }

    #[test]
    fn prefix_search_is_bounded_and_ordered() {
        let d = tmpdir("prefix");
        sample(&d);
        let ix = Index::open(&d).unwrap();
        let hits = ix.names_with_prefix("he", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "helper");
        // A prefix matching nothing must not run off the end of the fst.
        assert!(ix.names_with_prefix("qqq", 10).is_empty());
    }

    #[test]
    fn line_and_column_are_resolved_from_the_real_file() {
        let d = tmpdir("positions");
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("a.c");
        // Offsets:      0         10        20
        //               |int a;\n | int bb;\n|
        std::fs::write(&src, b"int a;\nint bb;\nint c;\n").unwrap();

        let mut w = IndexWriter::new();
        let f = w.add_file(src.to_str().unwrap());
        let n = w.intern("x");
        // Byte 0 is line 1 col 1; byte 7 opens line 2; byte 11 is mid-line 2.
        for off in [0u32, 7, 11, 15] {
            w.push_symbol(SymbolRec {
                name: n,
                file: f,
                off,
                len: 1,
                enclosing: NO_SYMBOL,
                pc: 0,
                kind: 0,
                is_def: true,
                ..Default::default()
            });
            w.push_occurrence(OccRec {
                name: n,
                file: f,
                off,
                len: 1,
                enclosing: NO_SYMBOL,
                pc: 0,
                role: ROLE_DEF,
                ..Default::default()
            });
        }
        w.write(&d).unwrap();

        let ix = Index::open(&d).unwrap();
        let got: Vec<(u32, u16)> = ix
            .symbols_named("x")
            .iter()
            .map(|s| (s.line, s.col))
            .collect();
        assert_eq!(got, vec![(1, 1), (2, 1), (2, 5), (3, 1)]);

        let occs: Vec<(u32, u16)> = ix.references("x").iter().map(|o| (o.line, o.col)).collect();
        assert_eq!(occs, vec![(1, 1), (2, 1), (2, 5), (3, 1)]);
    }

    #[test]
    fn a_record_in_an_unreadable_file_reports_no_line() {
        let d = tmpdir("positions-missing");
        let mut w = IndexWriter::new();
        let f = w.add_file("/definitely/not/here.c");
        let n = w.intern("x");
        w.push_symbol(SymbolRec {
            name: n,
            file: f,
            off: 42,
            len: 1,
            enclosing: NO_SYMBOL,
            pc: 0,
            kind: 0,
            is_def: true,
            ..Default::default()
        });
        w.write(&d).unwrap();

        // Line 0 is not a real line, so a caller can tell it is unknown rather
        // than being pointed confidently at the top of a file.
        let ix = Index::open(&d).unwrap();
        assert_eq!(ix.symbols_named("x")[0].line, 0);
    }

    #[test]
    fn occurrences_in_a_file_are_sorted_by_offset() {
        let d = tmpdir("byfile");
        sample(&d);
        let ix = Index::open(&d).unwrap();
        let occs = ix.occurrences_in_file(0);
        assert_eq!(occs.len(), 2);
        assert!(occs.windows(2).all(|w| w[0].off <= w[1].off));
    }
}
