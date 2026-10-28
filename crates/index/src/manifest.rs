//! What the last build saw, so the next one can tell what actually changed.
//!
//! Three facts per file drive the whole incremental decision:
//!
//! - **content hash** — did this file change at all?
//! - **export signature** — did its `#define`s / module-level names change?
//!   Only that can make *other* files extract differently.
//! - **include edges** — did its includes/imports change, which would reshape
//!   closures downstream?
//!
//! A file's symbols depend only on its own text plus the macros visible in its
//! closure. Headers are indexed as their own files, never inlined into a TU. So
//! editing a header's function body dirties that header alone.

use rustc_hash::FxHashMap;
use std::io::{self, Write};
use std::path::Path;

/// Bumped whenever the on-disk layout changes, so a stale manifest is ignored
/// rather than misread.
pub const MANIFEST_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileFacts {
    pub content: u64,
    /// Hash of everything this file publishes to others: its macro definitions
    /// (name, body, guard). Unchanged exports mean dependents need no work.
    pub exports: u64,
    /// Hash of this file's resolved include/import edges.
    pub edges: u64,
    /// Modification time in nanoseconds since the epoch, and size in bytes.
    /// Together these let a rescan skip reading a file whose stat is unchanged,
    /// which is the difference between an O(bytes) and an O(files) update.
    pub mtime_ns: u64,
    pub size: u64,
}

/// What a rescan learned about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scan {
    pub content: u64,
    pub mtime_ns: u64,
    pub size: u64,
}

/// Cheap identity of a file on disk, without reading it.
pub fn stat_of(p: &Path) -> Option<(u64, u64)> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((mtime, md.len()))
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub files: FxHashMap<String, FileFacts>,
}

/// Content hash of a file's bytes.
pub fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write(b);
    // Length guards against FxHash's weak avalanche on short inputs.
    h.write_usize(b.len());
    h.finish()
}

/// splitmix64 finalizer — cheap, and gives each input good avalanche before it
/// is folded into a commutative accumulator.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

/// Order-independent combination, so a set of names hashes the same however it
/// was enumerated.
///
/// This has to be commutative: export sets are walked out of a hash map, so any
/// position-dependent mixing would produce a different digest on every run and
/// make every build look dirty. XOR and wrapping-add are both commutative;
/// using both together separates multisets that either alone would collide.
pub fn combine_unordered(parts: impl Iterator<Item = u64>) -> u64 {
    let mut xor = 0u64;
    let mut sum = 0u64;
    let mut n = 0u64;
    for p in parts {
        let m = mix(p);
        xor ^= m;
        sum = sum.wrapping_add(m);
        n += 1;
    }
    mix(xor ^ sum.rotate_left(17) ^ n)
}

impl Manifest {
    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join("manifest.tsv")).ok()?;
        let mut lines = text.lines();
        let version: u32 = lines.next()?.parse().ok()?;
        if version != MANIFEST_VERSION {
            return None;
        }
        let mut files = FxHashMap::default();
        for l in lines {
            let mut it = l.split('\t');
            let path = it.next()?;
            let content = u64::from_str_radix(it.next()?, 16).ok()?;
            let exports = u64::from_str_radix(it.next()?, 16).ok()?;
            let edges = u64::from_str_radix(it.next()?, 16).ok()?;
            let mtime_ns = u64::from_str_radix(it.next()?, 16).ok()?;
            let size = u64::from_str_radix(it.next()?, 16).ok()?;
            files.insert(
                path.to_string(),
                FileFacts {
                    content,
                    exports,
                    edges,
                    mtime_ns,
                    size,
                },
            );
        }
        Some(Self { files })
    }

    /// Written by rename, so a build killed mid-save leaves the previous
    /// manifest rather than a truncated one.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut buf = Vec::with_capacity(self.files.len() * 64);
        writeln!(buf, "{MANIFEST_VERSION}")?;
        for (path, fa) in &self.files {
            writeln!(
                buf,
                "{path}\t{:x}\t{:x}\t{:x}\t{:x}\t{:x}",
                fa.content, fa.exports, fa.edges, fa.mtime_ns, fa.size
            )?;
        }
        crate::store::write_atomic(&dir.join("manifest.tsv"), &buf)
    }

    pub fn get(&self, path: &str) -> Option<FileFacts> {
        self.files.get(path).copied()
    }

    pub fn insert(&mut self, path: String, facts: FileFacts) {
        self.files.insert(path, facts);
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// What changed between two builds.
#[derive(Debug, Default)]
pub struct Changes {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }

    pub fn touched(&self) -> usize {
        self.added.len() + self.modified.len() + self.removed.len()
    }
}

/// Rescans one file, reading its bytes only if its stat differs from what the
/// last build recorded.
///
/// An unchanged mtime and size is taken as proof the content is unchanged. That
/// is the same bet git and ninja make; it is wrong only if something rewrites a
/// file while preserving both, which in practice means a deliberate `touch -r`.
/// A zero mtime (a filesystem that does not report one) always forces a read.
pub fn rescan(path: &Path, previous: Option<FileFacts>) -> Option<Scan> {
    let (mtime_ns, size) = stat_of(path)?;
    if mtime_ns != 0 {
        if let Some(p) = previous {
            if p.mtime_ns == mtime_ns && p.size == size {
                return Some(Scan {
                    content: p.content,
                    mtime_ns,
                    size,
                });
            }
        }
    }
    let bytes = std::fs::read(path).ok()?;
    Some(Scan {
        content: hash_bytes(&bytes),
        mtime_ns,
        size,
    })
}

/// Compares current content hashes against the manifest.
pub fn diff(old: &Manifest, current: &[(String, Scan)]) -> Changes {
    let mut ch = Changes::default();
    let mut seen: rustc_hash::FxHashSet<&str> = Default::default();

    for (path, scan) in current {
        seen.insert(path.as_str());
        match old.get(path) {
            None => ch.added.push(path.clone()),
            Some(f) if f.content != scan.content => ch.modified.push(path.clone()),
            Some(_) => {}
        }
    }
    for path in old.files.keys() {
        if !seen.contains(path.as_str()) {
            ch.removed.push(path.clone());
        }
    }
    ch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(c: u64) -> FileFacts {
        FileFacts {
            content: c,
            ..Default::default()
        }
    }

    fn scan(c: u64) -> Scan {
        Scan {
            content: c,
            mtime_ns: 0,
            size: 0,
        }
    }

    #[test]
    fn identical_content_produces_no_changes() {
        let mut m = Manifest::default();
        m.insert("a.c".into(), facts(1));
        m.insert("b.h".into(), facts(2));
        let cur = vec![("a.c".to_string(), scan(1)), ("b.h".to_string(), scan(2))];
        assert!(diff(&m, &cur).is_empty());
    }

    #[test]
    fn detects_added_modified_and_removed() {
        let mut m = Manifest::default();
        m.insert("a.c".into(), facts(1));
        m.insert("gone.c".into(), facts(9));
        let cur = vec![
            ("a.c".to_string(), scan(42)), // modified
            ("new.c".to_string(), scan(7)),
        ];
        let ch = diff(&m, &cur);
        assert_eq!(ch.modified, ["a.c"]);
        assert_eq!(ch.added, ["new.c"]);
        assert_eq!(ch.removed, ["gone.c"]);
        assert_eq!(ch.touched(), 3);
    }

    #[test]
    fn hash_distinguishes_content_and_length() {
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
        // Prefixes must not collide with their extensions.
        assert_ne!(hash_bytes(b"ab"), hash_bytes(b"ab\0"));
    }

    #[test]
    fn export_hash_ignores_enumeration_order() {
        let a = combine_unordered([1u64, 2, 3].into_iter());
        let b = combine_unordered([3u64, 1, 2].into_iter());
        assert_eq!(a, b, "a define set must hash the same however it is walked");
    }

    #[test]
    fn export_hash_notices_a_changed_member() {
        let a = combine_unordered([1u64, 2, 3].into_iter());
        let b = combine_unordered([1u64, 2, 4].into_iter());
        assert_ne!(a, b);
    }

    #[test]
    fn export_hash_separates_multisets_xor_alone_would_collide() {
        // Plain XOR maps {a, a, b} and {b} to the same value.
        let a = combine_unordered([5u64, 5, 9].into_iter());
        let b = combine_unordered([9u64].into_iter());
        assert_ne!(a, b);
    }

    #[test]
    fn export_hash_notices_a_removed_member() {
        let a = combine_unordered([1u64, 2, 3].into_iter());
        let b = combine_unordered([1u64, 2].into_iter());
        assert_ne!(a, b);
    }

    #[test]
    fn round_trips_through_disk() {
        let d = std::env::temp_dir().join(format!("codenav-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut m = Manifest::default();
        m.insert(
            "src/a.c".into(),
            FileFacts {
                content: 0xdead,
                exports: 0xbeef,
                edges: 0xfeed,
                mtime_ns: 0xcafe,
                size: 0x1234,
            },
        );
        m.save(&d).unwrap();
        let back = Manifest::load(&d).unwrap();
        let f = back.get("src/a.c").unwrap();
        assert_eq!(f.content, 0xdead);
        assert_eq!(f.exports, 0xbeef);
        assert_eq!(f.edges, 0xfeed);
        assert_eq!(f.mtime_ns, 0xcafe);
        assert_eq!(f.size, 0x1234);
    }

    #[test]
    fn rescan_skips_reading_when_stat_is_unchanged() {
        let d = std::env::temp_dir().join(format!("codenav-rescan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("a.c");
        std::fs::write(&p, b"int a;").unwrap();

        let first = rescan(&p, None).unwrap();
        assert_eq!(first.content, hash_bytes(b"int a;"));

        // A matching stat must short-circuit: seed a deliberately wrong content
        // hash and check it is handed back rather than recomputed.
        let stale = FileFacts {
            content: 0xdeadbeef,
            mtime_ns: first.mtime_ns,
            size: first.size,
            ..Default::default()
        };
        assert_eq!(rescan(&p, Some(stale)).unwrap().content, 0xdeadbeef);

        // A differing size must force a real read.
        let wrong_size = FileFacts {
            size: first.size + 1,
            ..stale
        };
        assert_eq!(rescan(&p, Some(wrong_size)).unwrap().content, first.content);
    }

    #[test]
    fn rescan_rereads_when_content_changes() {
        let d = std::env::temp_dir().join(format!("codenav-rescan2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("a.c");
        std::fs::write(&p, b"int a;").unwrap();
        let first = rescan(&p, None).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&p, b"int bb;").unwrap();

        let prev = FileFacts {
            content: first.content,
            mtime_ns: first.mtime_ns,
            size: first.size,
            ..Default::default()
        };
        let second = rescan(&p, Some(prev)).unwrap();
        assert_ne!(second.content, first.content);
        assert_eq!(second.content, hash_bytes(b"int bb;"));
    }

    #[test]
    fn a_stale_version_is_ignored_rather_than_misread() {
        let d = std::env::temp_dir().join(format!("codenav-manifest-v-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("manifest.tsv"), "999\nsrc/a.c\t1\t2\t3\n").unwrap();
        assert!(Manifest::load(&d).is_none());
    }
}
