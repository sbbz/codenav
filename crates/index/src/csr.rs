//! Compressed sparse row postings: `key -> [u32]`.
//!
//! An offsets array indexed directly by key, plus one flat values array. Lookup
//! is two loads and a slice — no hashing, no per-key allocation.

use memmap2::Mmap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

enum Backing {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl Backing {
    fn bytes(&self) -> &[u8] {
        match self {
            Backing::Mapped(m) => m,
            Backing::Owned(v) => v,
        }
    }
}

pub struct Csr {
    idx: Backing,
    val: Backing,
}

fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl Csr {
    /// Builds from `(key, value)` pairs in any order. Values keep their
    /// relative order within a key, so callers can rely on insertion order.
    ///
    /// Takes a factory rather than an iterator so the pairs can be walked twice
    /// — once to count, once to place. Collecting them instead costs 12 bytes
    /// per pair, which at the kernel's 99M occurrences is 1.2 GB per table.
    pub fn build<I>(n_keys: usize, pairs: impl Fn() -> I) -> Self
    where
        I: Iterator<Item = (usize, u32)>,
    {
        let mut counts = vec![0u32; n_keys + 1];
        for (k, _) in pairs() {
            if k < n_keys {
                counts[k + 1] += 1;
            }
        }
        for i in 1..counts.len() {
            counts[i] += counts[i - 1];
        }

        let total = *counts.last().unwrap_or(&0) as usize;
        let mut values = vec![0u32; total];
        let mut cursor = counts.clone();
        for (k, v) in pairs() {
            if k < n_keys {
                values[cursor[k] as usize] = v;
                cursor[k] += 1;
            }
        }
        drop(cursor);

        let mut idx = Vec::with_capacity(counts.len() * 4);
        for c in &counts {
            idx.extend_from_slice(&c.to_le_bytes());
        }
        let mut val = Vec::with_capacity(values.len() * 4);
        for v in &values {
            val.extend_from_slice(&v.to_le_bytes());
        }

        Self {
            idx: Backing::Owned(idx),
            val: Backing::Owned(val),
        }
    }

    pub fn write(&self, dir: &Path, stem: &str) -> io::Result<()> {
        let mut f = BufWriter::new(File::create(dir.join(format!("{stem}.idx")))?);
        f.write_all(self.idx.bytes())?;
        f.flush()?;
        let mut f = BufWriter::new(File::create(dir.join(format!("{stem}.val")))?);
        f.write_all(self.val.bytes())?;
        f.flush()
    }

    pub fn open(dir: &Path, stem: &str) -> io::Result<Self> {
        Ok(Self {
            idx: Backing::Mapped(map(dir.join(format!("{stem}.idx")))?),
            val: Backing::Mapped(map(dir.join(format!("{stem}.val")))?),
        })
    }

    pub fn keys(&self) -> usize {
        (self.idx.bytes().len() / 4).saturating_sub(1)
    }

    /// Values for a key. Empty for out-of-range keys rather than panicking, so
    /// a stale id from another table degrades to "no results".
    pub fn get(&self, key: usize) -> Vec<u32> {
        self.postings(key).to_vec()
    }

    /// A borrowed view of a key's values.
    ///
    /// Values stay in the mapping and are decoded on access, so asking how many
    /// there are — or reading only the first few — costs nothing. Hot kernel
    /// names have millions of postings, where copying them is the query.
    pub fn postings(&self, key: usize) -> Postings<'_> {
        let idx = self.idx.bytes();
        let empty = Postings {
            val: &[],
            start: 0,
            end: 0,
        };
        if (key + 1) * 4 + 4 > idx.len() {
            return empty;
        }
        let s = get_u32(idx, key * 4) as usize;
        let e = get_u32(idx, (key + 1) * 4) as usize;
        let val = self.val.bytes();
        if e * 4 > val.len() || s > e {
            return empty;
        }
        Postings {
            val,
            start: s,
            end: e,
        }
    }
}

/// A borrowed run of postings, decoded on access.
#[derive(Clone, Copy, Default)]
pub struct Postings<'a> {
    val: &'a [u8],
    start: usize,
    end: usize,
}

impl Postings<'_> {
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> Option<u32> {
        (i < self.len()).then(|| get_u32(self.val, (self.start + i) * 4))
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (self.start..self.end).map(move |i| get_u32(self.val, i * 4))
    }

    pub fn to_vec(&self) -> Vec<u32> {
        self.iter().collect()
    }
}

fn map(p: std::path::PathBuf) -> io::Result<Mmap> {
    let f = File::open(p)?;
    // Safety: index files are written once and not mutated while open.
    unsafe { Mmap::map(&f) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_values_by_key() {
        let c = Csr::build(3, || [(0, 10), (2, 30), (0, 11), (1, 20)].into_iter());
        assert_eq!(c.get(0), vec![10, 11]);
        assert_eq!(c.get(1), vec![20]);
        assert_eq!(c.get(2), vec![30]);
    }

    #[test]
    fn empty_keys_yield_empty_slices() {
        let c = Csr::build(4, || [(1, 5)].into_iter());
        assert!(c.get(0).is_empty());
        assert!(c.get(3).is_empty());
    }

    #[test]
    fn out_of_range_key_is_empty_not_panic() {
        let c = Csr::build(2, || [(0, 1)].into_iter());
        assert!(c.get(99).is_empty());
    }

    #[test]
    fn preserves_insertion_order_within_a_key() {
        let c = Csr::build(1, || [(0, 3), (0, 1), (0, 2)].into_iter());
        assert_eq!(c.get(0), vec![3, 1, 2]);
    }

    #[test]
    fn round_trips_through_disk() {
        let d = std::env::temp_dir().join(format!("codenav-csr-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let c = Csr::build(3, || [(0, 7), (2, 9), (0, 8)].into_iter());
        c.write(&d, "t").unwrap();
        let r = Csr::open(&d, "t").unwrap();
        assert_eq!(r.get(0), vec![7, 8]);
        assert_eq!(r.get(2), vec![9]);
        assert_eq!(r.keys(), 3);
    }

    #[test]
    fn handles_no_pairs() {
        let c = Csr::build(2, std::iter::empty::<(usize, u32)>);
        assert!(c.get(0).is_empty());
        assert_eq!(c.keys(), 2);
    }
}
