//! Deciding what an edit actually invalidates.
//!
//! The model makes this cheap. A file's extracted symbols depend on exactly two
//! things: its own text, and the macro definitions visible in its include
//! closure. Headers are indexed as their own files, never inlined into the
//! translation units that include them — so editing a header's *body* dirties
//! that header alone.
//!
//! Only two kinds of change reach other files:
//!
//! - the file's **macro exports** changed, so anything whose closure contains
//!   it may now expand differently;
//! - the file's **includes** changed, which reshapes closures downstream.

use crate::files::FileId;
use roaring::RoaringBitmap;
use rustc_hash::FxHashSet;

/// Outcome of planning an incremental update.
#[derive(Debug)]
pub enum Plan {
    /// Nothing changed.
    UpToDate,
    /// Re-extract exactly these files; everything else is reusable.
    Incremental { dirty: Vec<FileId> },
    /// Too much moved to update safely or cheaply.
    FullRebuild { reason: &'static str },
}

/// What changed about one file, already classified against the manifest.
#[derive(Debug, Clone, Copy)]
pub struct FileChange {
    pub file: FileId,
    /// Its `#define`s changed, so dependents may expand differently.
    pub exports_changed: bool,
    /// Its include/import edges changed, so closures downstream shift.
    pub edges_changed: bool,
}

/// Beyond this share of the tree, a full rebuild is simpler and no slower than
/// tracking the fallout.
const FULL_REBUILD_RATIO: f64 = 0.25;

/// Plans an update.
///
/// `reverse_closure(f)` must yield the files whose closure contains `f` — that
/// is the blast radius of an export change.
pub fn plan(
    changed: &[FileChange],
    added_or_removed: usize,
    total_files: usize,
    reverse_closure: impl Fn(FileId) -> Vec<FileId>,
) -> Plan {
    if changed.is_empty() && added_or_removed == 0 {
        return Plan::UpToDate;
    }

    // The file set itself moved, so ids shift and closures must be rebuilt.
    if added_or_removed > 0 {
        return Plan::FullRebuild {
            reason: "files added or removed",
        };
    }

    // An include change reshapes the graph; recomputing closures is a whole-tree
    // operation, so there is nothing to save by being clever here.
    if changed.iter().any(|c| c.edges_changed) {
        return Plan::FullRebuild {
            reason: "include or import edges changed",
        };
    }

    let mut dirty: FxHashSet<FileId> = changed.iter().map(|c| c.file).collect();

    // Only an export change escapes the file it happened in.
    for c in changed.iter().filter(|c| c.exports_changed) {
        for dependent in reverse_closure(c.file) {
            dirty.insert(dependent);
        }
        if dirty.len() as f64 > total_files as f64 * FULL_REBUILD_RATIO {
            return Plan::FullRebuild {
                reason: "macro change reaches too much of the tree",
            };
        }
    }

    let mut dirty: Vec<FileId> = dirty.into_iter().collect();
    dirty.sort_unstable();
    Plan::Incremental { dirty }
}

/// Files whose closure a re-extraction must see.
///
/// To expand one file we need the macros its closure publishes — and nothing
/// else. On the kernel that is ~623 files rather than all 63,779, which is what
/// turns the pre-pass from tens of seconds into a fraction of one.
pub fn scan_scope(dirty: &[FileId], closures: &[RoaringBitmap]) -> Vec<FileId> {
    let mut scope: FxHashSet<FileId> = dirty.iter().copied().collect();
    for &f in dirty {
        if let Some(c) = closures.get(f as usize) {
            scope.extend(c.iter());
        }
    }
    let mut scope: Vec<FileId> = scope.into_iter().collect();
    scope.sort_unstable();
    scope
}

/// Inverts the closure relation: for each file, who can see it.
///
/// Built once per update. Only files that actually changed their exports are
/// ever queried, so this is cheaper than it looks in the common case.
pub fn reverse_closures(closures: &[RoaringBitmap], of: &[FileId]) -> Vec<Vec<FileId>> {
    let wanted: FxHashSet<FileId> = of.iter().copied().collect();
    let mut out: Vec<Vec<FileId>> = vec![Vec::new(); of.len()];
    let index: rustc_hash::FxHashMap<FileId, usize> =
        of.iter().enumerate().map(|(i, &f)| (f, i)).collect();

    for (viewer, c) in closures.iter().enumerate() {
        for seen in c.iter() {
            if wanted.contains(&seen) {
                out[index[&seen]].push(viewer as FileId);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(ids: &[u32]) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &i in ids {
            b.insert(i);
        }
        b
    }

    fn changed(file: FileId, exports: bool, edges: bool) -> FileChange {
        FileChange {
            file,
            exports_changed: exports,
            edges_changed: edges,
        }
    }

    #[test]
    fn no_changes_is_up_to_date() {
        assert!(matches!(plan(&[], 0, 100, |_| vec![]), Plan::UpToDate));
    }

    #[test]
    fn a_body_edit_dirties_only_that_file() {
        // Even for a header: its own body is not inlined anywhere.
        let p = plan(&[changed(7, false, false)], 0, 100, |_| vec![1, 2, 3]);
        match p {
            Plan::Incremental { dirty } => assert_eq!(dirty, vec![7]),
            other => panic!("expected incremental, got {other:?}"),
        }
    }

    #[test]
    fn an_export_change_dirties_everything_that_can_see_it() {
        let p = plan(&[changed(7, true, false)], 0, 100, |f| {
            assert_eq!(f, 7);
            vec![10, 11]
        });
        match p {
            Plan::Incremental { dirty } => assert_eq!(dirty, vec![7, 10, 11]),
            other => panic!("expected incremental, got {other:?}"),
        }
    }

    #[test]
    fn an_edge_change_forces_a_full_rebuild() {
        assert!(matches!(
            plan(&[changed(7, false, true)], 0, 100, |_| vec![]),
            Plan::FullRebuild { .. }
        ));
    }

    #[test]
    fn adding_a_file_forces_a_full_rebuild() {
        // FileIds are dense indices; a new file shifts them.
        assert!(matches!(
            plan(&[], 1, 100, |_| vec![]),
            Plan::FullRebuild { .. }
        ));
    }

    #[test]
    fn a_widely_included_macro_change_falls_back_to_full() {
        let p = plan(&[changed(0, true, false)], 0, 100, |_| (0..90).collect());
        assert!(matches!(p, Plan::FullRebuild { .. }), "got {p:?}");
    }

    #[test]
    fn scan_scope_is_the_dirty_files_plus_their_closures() {
        let closures = vec![bm(&[1, 2]), bm(&[]), bm(&[3]), bm(&[])];
        assert_eq!(scan_scope(&[0], &closures), vec![0, 1, 2]);
    }

    #[test]
    fn scan_scope_deduplicates_shared_headers() {
        let closures = vec![
            bm(&[9]),
            bm(&[9]),
            bm(&[]),
            bm(&[]),
            bm(&[]),
            bm(&[]),
            bm(&[]),
            bm(&[]),
            bm(&[]),
            bm(&[]),
        ];
        assert_eq!(scan_scope(&[0, 1], &closures), vec![0, 1, 9]);
    }

    #[test]
    fn reverse_closure_finds_the_dependents() {
        // Files 0 and 2 both include file 5.
        let closures = vec![bm(&[5]), bm(&[]), bm(&[5, 6]), bm(&[])];
        let rev = reverse_closures(&closures, &[5]);
        assert_eq!(rev[0], vec![0, 2]);
    }

    #[test]
    fn reverse_closure_is_empty_for_something_nobody_includes() {
        let closures = vec![bm(&[5]), bm(&[])];
        let rev = reverse_closures(&closures, &[9]);
        assert!(rev[0].is_empty());
    }
}
