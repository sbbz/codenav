//! Rebuilds the index whenever the tree changes.
//!
//! Events say which source files changed, and the build re-stats only those
//! instead of walking the whole tree — about 0.45 s of a one-file update on the
//! kernel. Anything an event cannot pin to a file falls back to the walk: a
//! directory renamed or deleted (often with no per-file events), something that
//! vanished and may have been a directory, a backend asking for a rescan, or an
//! error. The walk re-stats everything against the manifest, so a dropped or
//! coalesced event there can delay an update but never lose one. A burst — a
//! branch switch touches thousands of files — becomes a single build.
//!
//! The build runs as a child process rather than on a thread. A full rebuild's
//! peak memory would otherwise stay with a long-lived server after the build
//! finished, and a crash in the indexer would take the server down with it.

use crate::files::is_source;
use crate::INDEX_DIR;
use notify::{EventKind, RecursiveMode, Watcher as _};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// A burst is over once it has been quiet this long. Editors save through a
/// temp file and a rename; a checkout writes for a while.
const QUIET: Duration = Duration::from_millis(200);
/// ...but a tree that never goes quiet still gets reindexed this often.
const MAX_DELAY: Duration = Duration::from_secs(2);

/// Where the child build's output goes. A file rather than a pipe: if the
/// server exits mid-build, a pipe would kill the build on its next write,
/// whereas this way it finishes and publishes for the next server.
const LOG: &str = "last-build.log";

pub enum Status {
    Indexing,
    Indexed {
        ok: bool,
        secs: f64,
        /// The build's last line, e.g. `incremental: 1 file(s) re-indexed`.
        summary: String,
    },
}

/// Watches until dropped.
pub struct Watcher {
    _inner: notify::RecommendedWatcher,
}

/// Watches `root` and keeps its index current, reporting each build through
/// `on_status`. Builds once immediately, to catch up on anything that changed
/// while nothing was watching — or to create the index if there is none.
pub fn start(root: &Path, on_status: impl Fn(Status) + Send + 'static) -> notify::Result<Watcher> {
    let root = root.to_path_buf();
    // Backends report canonical paths (FSEvents resolves /var to /private/var),
    // so filtering against a non-canonical root would match nothing.
    let canon = root.canonicalize()?;

    let (tx, rx) = mpsc::channel::<Change>();
    let filter_root = canon.clone();
    let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(ev) => {
                for c in classify(&filter_root, &ev) {
                    let _ = tx.send(c);
                }
            }
            // An error can mean events were dropped; walking is the safe answer.
            Err(_) => {
                let _ = tx.send(Change::Walk);
            }
        }
    })?;
    inner.watch(&canon, RecursiveMode::Recursive)?;

    std::thread::spawn(move || {
        // The first build walks, to catch up on anything that changed while
        // nothing was watching.
        let mut walk = true;
        let mut paths: BTreeSet<String> = BTreeSet::new();
        loop {
            if !walk {
                match rx.recv() {
                    Ok(c) => take(c, &mut walk, &mut paths),
                    Err(_) => return,
                }
                let first = Instant::now();
                loop {
                    let left = MAX_DELAY.saturating_sub(first.elapsed());
                    if left.is_zero() {
                        break;
                    }
                    match rx.recv_timeout(QUIET.min(left)) {
                        Ok(c) => take(c, &mut walk, &mut paths),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            let hint = (!walk).then(|| std::mem::take(&mut paths));
            walk = false;
            paths.clear();
            // Anything arriving from here on queues up and triggers one more
            // build afterwards, so a change made mid-build is never missed.
            on_status(Status::Indexing);
            on_status(build(&root, hint));
        }
    });

    Ok(Watcher { _inner: inner })
}

/// What an event means for the next build.
#[derive(Debug, PartialEq, Eq)]
enum Change {
    /// This source file changed, named as the walk would name it (`./a/b.c`).
    Path(String),
    /// Something changed that only a walk of the tree can account for.
    Walk,
}

fn take(c: Change, walk: &mut bool, paths: &mut BTreeSet<String>) {
    match c {
        Change::Walk => *walk = true,
        Change::Path(p) => {
            paths.insert(p);
        }
    }
}

fn classify(root: &Path, ev: &notify::Event) -> Vec<Change> {
    // inotify reports opens and reads, and a build reads every source file —
    // so counting them made each build trigger the next, forever. A write is
    // always also reported as a modify or create, so nothing real is lost.
    if matches!(ev.kind, EventKind::Access(_)) {
        return Vec::new();
    }
    if ev.need_rescan() {
        return vec![Change::Walk];
    }
    ev.paths
        .iter()
        .filter_map(|p| classify_path(root, p))
        .collect()
}

fn classify_path(root: &Path, p: &Path) -> Option<Change> {
    let Ok(rel) = p.strip_prefix(root) else {
        return Some(Change::Walk);
    };
    if rel
        .components()
        .any(|c| c.as_os_str() == INDEX_DIR || c.as_os_str() == ".git")
    {
        return None;
    }
    if is_source(p) {
        return Some(Change::Path(
            Path::new(".").join(rel).to_string_lossy().into_owned(),
        ));
    }
    // Renaming or deleting a directory moves every file under it, often with
    // no per-file events. A path that is gone may have been one.
    (p.is_dir() || !p.exists()).then_some(Change::Walk)
}

/// Runs `codenav index .`, handing it the changed paths when events named
/// them all, so it can skip the walk.
fn build(root: &Path, hint: Option<BTreeSet<String>>) -> Status {
    let t = Instant::now();
    let log_path = root.join(INDEX_DIR).join(LOG);
    let ran = (|| {
        std::fs::create_dir_all(root.join(INDEX_DIR))?;
        let log = std::fs::File::create(&log_path)?;
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("index").arg(".").current_dir(root);
        cmd.stdout(Stdio::null()).stderr(log);
        if hint.is_some() {
            cmd.arg("--paths-from-stdin").stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd.spawn()?;
        if let (Some(paths), Some(mut stdin)) = (&hint, child.stdin.take()) {
            let mut list = String::new();
            for p in paths {
                list.push_str(p);
                list.push('\n');
            }
            stdin.write_all(list.as_bytes())?;
        }
        child.wait()
    })();
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    eprint!("{log}");
    let (ok, summary) = match ran {
        Ok(s) => (
            s.success(),
            log.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_string(),
        ),
        Err(e) => (false, format!("could not run the indexer: {e}")),
    };
    if !ok {
        eprintln!("codenav: index build failed: {summary}");
    }
    Status::Indexed {
        ok,
        secs: t.elapsed().as_secs_f64(),
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> Option<Change> {
        Some(Change::Path(p.to_string()))
    }

    #[test]
    fn source_edits_are_named_and_everything_else_walks_or_is_ignored() {
        let d = std::env::temp_dir().join(format!("codenav-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("Makefile"), b"").unwrap();

        // Named the way the walk names them, so the build can look them up.
        assert_eq!(classify_path(&d, &d.join("src/a.c")), path("./src/a.c"));
        assert_eq!(classify_path(&d, &d.join("lib/x.py")), path("./lib/x.py"));
        // Our own writes must not retrigger a build, or it never settles.
        assert_eq!(
            classify_path(&d, &d.join(".codenav/main-3/names.fst")),
            None
        );
        assert_eq!(classify_path(&d, &d.join(".git/index")), None);
        // An existing non-source file cannot change the index...
        assert_eq!(classify_path(&d, &d.join("Makefile")), None);
        // ...but a directory, or something that vanished, might hold sources,
        // and only a walk can say which.
        assert_eq!(classify_path(&d, &d.join("src")), Some(Change::Walk));
        assert_eq!(
            classify_path(&d, &d.join("renamed-away")),
            Some(Change::Walk)
        );
        // Outside the tree means the paths are not what we think: walk.
        assert_eq!(
            classify_path(&d, Path::new("/elsewhere/a.c")),
            Some(Change::Walk)
        );
    }

    #[test]
    fn reading_a_source_is_not_a_change() {
        use notify::event::{AccessKind, AccessMode, DataChange, ModifyKind};
        let d = Path::new("/tree");
        let at = |kind| notify::Event::new(kind).add_path(d.join("src/a.c"));
        // The build itself opens every file; on inotify that is an event.
        assert!(classify(d, &at(EventKind::Access(AccessKind::Open(AccessMode::Any)))).is_empty());
        assert!(classify(
            d,
            &at(EventKind::Access(AccessKind::Close(AccessMode::Read)))
        )
        .is_empty());
        assert_eq!(
            classify(d, &at(EventKind::Modify(ModifyKind::Data(DataChange::Any)))),
            vec![Change::Path("./src/a.c".into())]
        );
    }

    #[test]
    fn a_rescan_request_means_walk() {
        let ev = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert_eq!(classify(Path::new("/tree"), &ev), vec![Change::Walk]);
    }
}
