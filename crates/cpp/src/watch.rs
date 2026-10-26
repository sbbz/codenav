//! Rebuilds the index whenever the tree changes.
//!
//! Filesystem events are only a trigger. Every rebuild is an ordinary
//! `codenav index`, which re-stats the whole tree against the manifest, so a
//! dropped, coalesced or misattributed event can delay an update but never lose
//! one. That is what makes it safe to filter events loosely and to fold a burst
//! — a branch switch touches thousands of files — into a single build.
//!
//! The build runs as a child process rather than on a thread. A full rebuild's
//! peak memory would otherwise stay with a long-lived server after the build
//! finished, and a crash in the indexer would take the server down with it.

use crate::files::is_source;
use crate::INDEX_DIR;
use notify::{EventKind, RecursiveMode, Watcher as _};
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

    let (tx, rx) = mpsc::channel::<()>();
    let filter_root = canon.clone();
    let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let trigger = match res {
            Ok(ev) => wanted(&filter_root, &ev),
            // An error can mean events were dropped; a build is the safe answer.
            Err(_) => true,
        };
        if trigger {
            let _ = tx.send(());
        }
    })?;
    inner.watch(&canon, RecursiveMode::Recursive)?;

    std::thread::spawn(move || {
        let mut startup = true;
        loop {
            if !startup {
                if rx.recv().is_err() {
                    return;
                }
                let first = Instant::now();
                loop {
                    let left = MAX_DELAY.saturating_sub(first.elapsed());
                    if left.is_zero() {
                        break;
                    }
                    match rx.recv_timeout(QUIET.min(left)) {
                        Ok(()) => {}
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            startup = false;
            // Anything arriving from here on queues up and triggers one more
            // build afterwards, so a change made mid-build is never missed.
            on_status(Status::Indexing);
            on_status(build(&root));
        }
    });

    Ok(Watcher { _inner: inner })
}

/// Whether an event should trigger a build.
fn wanted(root: &Path, ev: &notify::Event) -> bool {
    // inotify reports opens and reads, and a build reads every source file —
    // so counting them made each build trigger the next, forever. A write is
    // always also reported as a modify or create, so nothing real is lost.
    if matches!(ev.kind, EventKind::Access(_)) {
        return false;
    }
    ev.need_rescan() || ev.paths.iter().any(|p| relevant(root, p))
}

/// Whether an event at `p` could change the index.
fn relevant(root: &Path, p: &Path) -> bool {
    let rel = p.strip_prefix(root).unwrap_or(p);
    if rel
        .components()
        .any(|c| c.as_os_str() == INDEX_DIR || c.as_os_str() == ".git")
    {
        return false;
    }
    if is_source(p) {
        return true;
    }
    // Renaming or deleting a directory moves every file under it, often with
    // no per-file events. A path that is gone may have been one.
    p.is_dir() || !p.exists()
}

fn build(root: &Path) -> Status {
    let t = Instant::now();
    let log_path = root.join(INDEX_DIR).join(LOG);
    let ran = (|| {
        std::fs::create_dir_all(root.join(INDEX_DIR))?;
        let log = std::fs::File::create(&log_path)?;
        Command::new(std::env::current_exe()?)
            .arg("index")
            .arg(".")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .status()
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

    #[test]
    fn only_changes_that_can_reach_the_index_count() {
        let d = std::env::temp_dir().join(format!("codenav-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("Makefile"), b"").unwrap();

        assert!(relevant(&d, &d.join("src/a.c")));
        assert!(relevant(&d, &d.join("lib/x.py")));
        // Our own writes must not retrigger a build, or it never settles.
        assert!(!relevant(&d, &d.join(".codenav/main-3/names.fst")));
        assert!(!relevant(&d, &d.join(".git/index")));
        // An existing non-source file cannot change the index...
        assert!(!relevant(&d, &d.join("Makefile")));
        // ...but a directory, or something that vanished, might hold sources.
        assert!(relevant(&d, &d.join("src")));
        assert!(relevant(&d, &d.join("renamed-away")));
    }

    #[test]
    fn reading_a_source_is_not_a_change() {
        use notify::event::{AccessKind, AccessMode, DataChange, ModifyKind};
        let d = Path::new("/tree");
        let at = |kind| notify::Event::new(kind).add_path(d.join("src/a.c"));
        // The build itself opens every file; on inotify that is an event.
        assert!(!wanted(
            d,
            &at(EventKind::Access(AccessKind::Open(AccessMode::Any)))
        ));
        assert!(!wanted(
            d,
            &at(EventKind::Access(AccessKind::Close(AccessMode::Read)))
        ));
        assert!(wanted(
            d,
            &at(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
        ));
        assert!(wanted(d, &at(EventKind::Any)));
    }
}
