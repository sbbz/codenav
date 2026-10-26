//! Where the live index lives, and how a new one replaces it.
//!
//! A running `codenav serve` keeps every index file memory-mapped. Rewriting
//! those files in place — which the writer used to do — changes the bytes under
//! a live mapping: the server keeps answering, but from old offsets into new
//! contents, so a symbol that still exists comes back as nothing at all, and a
//! file that shrinks faults outright. With the index rebuilt on every save that
//! would be the normal case, not a race.
//!
//! So nothing is ever rewritten. Each build writes a fresh generation directory
//! (`main-N` or `overlay-N`) and publishes it by renaming a small pointer file,
//! `CURRENT`, over the old one. A reader that mapped the previous generation
//! keeps a consistent view — on Unix an unlinked file stays valid while mapped —
//! and sees the new one the next time it opens.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub const CURRENT: &str = "CURRENT";
const LOCK: &str = "lock";
const MAIN: &str = "main-";
const OVERLAY: &str = "overlay-";

/// What an index written before generations existed left at the top level.
/// Frozen: this layout is never written again, only cleaned up.
const LEGACY: &[&str] = &[
    "names.fst",
    "names.idx",
    "names.bin",
    "paths.fst",
    "paths.idx",
    "paths.bin",
    "roots.txt",
    "symbols.bin",
    "occs.bin",
    "sym_by_name.idx",
    "sym_by_name.val",
    "occ_by_name.idx",
    "occ_by_name.val",
    "callers.idx",
    "callers.val",
    "calls.idx",
    "calls.val",
    "occ_by_file.idx",
    "occ_by_file.val",
    "sym_by_file.idx",
    "sym_by_file.val",
    "closures.idx",
    "closures.bin",
    "pcs.idx",
    "pcs.bin",
    "tombstones.bin",
    "overlay",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Main,
    Overlay,
}

/// The published generation: a main index, plus the overlay of files
/// re-indexed since it was built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    pub main: String,
    pub overlay: Option<String>,
}

impl Pointer {
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let main = lines.next().filter(|m| valid(m, MAIN))?.to_string();
        let overlay = match lines.next() {
            None | Some("") => None,
            Some(o) if valid(o, OVERLAY) => Some(o.to_string()),
            Some(_) => return None,
        };
        Some(Self { main, overlay })
    }

    fn render(&self) -> String {
        match &self.overlay {
            Some(o) => format!("{}\n{o}\n", self.main),
            None => format!("{}\n", self.main),
        }
    }
}

/// A generation name is a prefix and a number, never a path: the pointer is
/// joined onto the index directory, so anything else could escape it.
fn valid(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn number(name: &str) -> Option<u64> {
    name.strip_prefix(MAIN)
        .or_else(|| name.strip_prefix(OVERLAY))?
        .parse()
        .ok()
}

/// The published generation, or `None` when the directory holds no index or
/// one written before generations existed.
pub fn current(dir: &Path) -> io::Result<Option<Pointer>> {
    match fs::read_to_string(dir.join(CURRENT)) {
        Ok(t) => Pointer::parse(&t)
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "corrupt CURRENT")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Identity of the published pointer, for change detection by long-lived
/// readers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    inode: u64,
    mtime: Option<std::time::SystemTime>,
    len: u64,
}

/// Checked before every server request, so it stats rather than reads: 2 µs
/// against 15 µs, next to ~30 µs for a whole warm go-to-definition. Every
/// publish renames a new file into place, so the inode, the mtime and usually
/// the length all move with it.
pub fn stamp(dir: &Path) -> Option<Stamp> {
    let md = fs::metadata(dir.join(CURRENT)).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&md);
    #[cfg(not(unix))]
    let inode = 0;
    Some(Stamp {
        inode,
        mtime: md.modified().ok(),
        len: md.len(),
    })
}

/// Creates an empty directory for the next generation and returns its name.
///
/// Numbers only grow: the highest-numbered generation is always the published
/// one, and only lower ones are ever removed, so a name is never reused while
/// a reader might still be about to open it.
pub fn fresh(dir: &Path, kind: Kind) -> io::Result<String> {
    fs::create_dir_all(dir)?;
    let mut top = 0;
    for e in fs::read_dir(dir)? {
        if let Some(n) = e?.file_name().to_str().and_then(number) {
            top = top.max(n);
        }
    }
    let prefix = match kind {
        Kind::Main => MAIN,
        Kind::Overlay => OVERLAY,
    };
    let name = format!("{prefix}{}", top + 1);
    fs::create_dir(dir.join(&name))?;
    Ok(name)
}

/// Makes `p` the live index. Readers see either the old pointer or the new
/// one, never a mix.
pub fn publish(dir: &Path, p: &Pointer) -> io::Result<()> {
    write_atomic(&dir.join(CURRENT), p.render().as_bytes())
}

/// Replaces `path` by rename, so a reader never sees it half-written.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    drop(f);
    fs::rename(&tmp, path)
}

/// Deletes every generation `keep` does not name — the ones it replaced, and
/// any a killed build left half-written — plus a pre-generation index.
///
/// Safe while a server still maps an old generation: the files stay readable
/// until it lets go, and only then is the space freed. On Windows a mapped file
/// cannot be deleted at all; that error is ignored and the next build retries.
pub fn remove_superseded(dir: &Path, keep: &Pointer) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let superseded =
            number(name).is_some() && name != keep.main && Some(name) != keep.overlay.as_deref();
        if superseded || LEGACY.contains(&name) {
            let p = e.path();
            let _ = if p.is_dir() {
                fs::remove_dir_all(&p)
            } else {
                fs::remove_file(&p)
            };
        }
    }
}

/// Held for the whole of a build, so two builds never interleave — a terminal
/// `codenav index` and the server's own, say. Readers never take it: they only
/// ever open published generations. Released when dropped, or when the
/// process dies.
pub struct BuildLock(#[allow(dead_code)] File);

impl BuildLock {
    /// Blocks until no other build holds the lock, calling `on_wait` first if
    /// it has to wait.
    pub fn acquire(dir: &Path, on_wait: impl FnOnce()) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK))?;
        match f.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                on_wait();
                f.lock()?;
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
        Ok(Self(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("codenav-store-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pointer_round_trips() {
        let d = tmpdir("ptr");
        assert_eq!(current(&d).unwrap(), None);
        let p = Pointer {
            main: "main-3".into(),
            overlay: Some("overlay-7".into()),
        };
        publish(&d, &p).unwrap();
        assert_eq!(current(&d).unwrap(), Some(p));
        let p = Pointer {
            main: "main-8".into(),
            overlay: None,
        };
        publish(&d, &p).unwrap();
        assert_eq!(current(&d).unwrap(), Some(p));
    }

    #[test]
    fn a_pointer_that_names_a_path_is_rejected() {
        let d = tmpdir("escape");
        fs::write(d.join(CURRENT), "../../etc\n").unwrap();
        assert!(current(&d).is_err());
        fs::write(d.join(CURRENT), "main-1\noverlay-../x\n").unwrap();
        assert!(current(&d).is_err());
    }

    #[test]
    fn generation_numbers_only_grow() {
        let d = tmpdir("fresh");
        assert_eq!(fresh(&d, Kind::Main).unwrap(), "main-1");
        assert_eq!(fresh(&d, Kind::Overlay).unwrap(), "overlay-2");
        let keep = Pointer {
            main: "main-1".into(),
            overlay: Some("overlay-2".into()),
        };
        remove_superseded(&d, &keep);
        assert_eq!(fresh(&d, Kind::Main).unwrap(), "main-3");
    }

    #[test]
    fn only_what_the_pointer_names_survives() {
        let d = tmpdir("remove");
        for g in ["main-1", "overlay-2", "main-3", "overlay-4", "main-5"] {
            fs::create_dir(d.join(g)).unwrap();
        }
        // A pre-generation index, and things that are not ours to delete.
        fs::write(d.join("names.fst"), b"").unwrap();
        fs::create_dir(d.join("overlay")).unwrap();
        fs::write(d.join("manifest.tsv"), b"").unwrap();
        fs::write(d.join("notes.txt"), b"").unwrap();

        let keep = Pointer {
            main: "main-3".into(),
            overlay: Some("overlay-4".into()),
        };
        publish(&d, &keep).unwrap();
        remove_superseded(&d, &keep);

        let mut left: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "CURRENT",
                "main-3",
                "manifest.tsv",
                "notes.txt",
                "overlay-4"
            ]
        );
    }

    #[test]
    fn a_second_build_waits_for_the_first() {
        let d = tmpdir("lock");
        let first = BuildLock::acquire(&d, || panic!("nothing else holds it")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let d2 = d.clone();
        let t = std::thread::spawn(move || {
            let waited = std::cell::Cell::new(false);
            let _second = BuildLock::acquire(&d2, || waited.set(true)).unwrap();
            tx.send(waited.get()).unwrap();
        });
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err());
        drop(first);
        assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        t.join().unwrap();
    }
}
