//! File registry and config-less include resolution.
//!
//! There is no `compile_commands.json`, so include roots are inferred: every
//! `#include <...>` target is matched against the file tree by path suffix, and
//! the directory prefixes that would make it resolve are voted on. On the kernel
//! this recovers `include/`, `arch/*/include/` and friends without being told.

use crate::intern::SymId;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Component, Path, PathBuf};

pub type FileId = u32;

#[derive(Default)]
pub struct FileRegistry {
    paths: Vec<PathBuf>,
    map: FxHashMap<PathBuf, FileId>,
    by_basename: FxHashMap<String, Vec<FileId>>,
    /// Include-guard macro name, once known.
    guard: Vec<Option<SymId>>,
    pragma_once: Vec<bool>,
}

impl FileRegistry {
    pub fn add(&mut self, path: PathBuf) -> FileId {
        // Registry keys must match what `Resolver::resolve` builds, and that
        // goes through `normalize`. Storing a raw `./x/y.h` here would make
        // every lookup miss and fall through to the slow suffix scan.
        let path = normalize(&path);
        if let Some(&id) = self.map.get(&path) {
            return id;
        }
        let id = self.paths.len() as FileId;
        if let Some(base) = path.file_name().and_then(|s| s.to_str()) {
            self.by_basename
                .entry(base.to_string())
                .or_default()
                .push(id);
        }
        self.map.insert(path.clone(), id);
        self.paths.push(path);
        self.guard.push(None);
        self.pragma_once.push(false);
        id
    }

    pub fn get(&self, path: &Path) -> Option<FileId> {
        self.map.get(path).copied()
    }

    pub fn path(&self, id: FileId) -> &Path {
        &self.paths[id as usize]
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn ids(&self) -> impl Iterator<Item = FileId> {
        0..self.paths.len() as FileId
    }

    pub fn set_guard(&mut self, id: FileId, g: Option<SymId>) {
        self.guard[id as usize] = g;
    }

    pub fn guard(&self, id: FileId) -> Option<SymId> {
        self.guard[id as usize]
    }

    pub fn set_pragma_once(&mut self, id: FileId) {
        self.pragma_once[id as usize] = true;
    }

    pub fn pragma_once(&self, id: FileId) -> bool {
        self.pragma_once[id as usize]
    }

    fn with_basename(&self, base: &str) -> &[FileId] {
        self.by_basename.get(base).map(|v| &v[..]).unwrap_or(&[])
    }
}

/// Number of leading path components two paths share.
fn shared_prefix_len(a: &Path, b: &Path) -> usize {
    a.components()
        .zip(b.components())
        .take_while(|(x, y)| x == y)
        .count()
}

/// Collapses `.` and `..` without touching the filesystem, so resolution stays
/// a pure lookup against the registry.
/// Whether codenav indexes this file at all. Shared by the build's tree walk
/// and the watcher, which must agree on what counts as a source change.
pub fn is_source(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|s| s.to_str()),
        Some("c") | Some("h") | Some("py")
    )
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub struct Resolver {
    /// Ordered by how many includes they resolve, best first.
    roots: Vec<PathBuf>,
}

/// How many distinct roots to keep. The kernel needs one per arch plus the
/// shared ones; beyond this the tail is noise and costs a lookup each.
const MAX_ROOTS: usize = 64;

impl Resolver {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Votes for the directory prefixes that would make the observed angled
    /// includes resolve against files actually present in the tree.
    pub fn infer(reg: &FileRegistry, angled_targets: &FxHashMap<String, u32>) -> Self {
        let mut votes: FxHashMap<PathBuf, u64> = FxHashMap::default();

        for (target, &count) in angled_targets {
            let tpath = normalize(Path::new(target));
            let Some(base) = tpath.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let tcomps: Vec<_> = tpath.components().collect();

            for &fid in reg.with_basename(base) {
                let fpath = reg.path(fid);
                let fcomps: Vec<_> = fpath.components().collect();
                if fcomps.len() < tcomps.len() {
                    continue;
                }
                let split = fcomps.len() - tcomps.len();
                if fcomps[split..] != tcomps[..] {
                    continue;
                }
                let root: PathBuf = fcomps[..split].iter().collect();
                *votes.entry(root).or_default() += count as u64;
            }
        }

        let mut ranked: Vec<(PathBuf, u64)> = votes.into_iter().collect();
        // Deeper roots first among equals: `arch/x86/include` should beat `.`
        // when both resolve a target, since it is the more specific answer.
        ranked.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.0.components().count().cmp(&a.0.components().count()))
        });
        ranked.truncate(MAX_ROOTS);

        Self {
            roots: ranked.into_iter().map(|(p, _)| p).collect(),
        }
    }

    /// Resolves one include directive. Quoted form searches the including
    /// file's directory first, as the C standard requires.
    pub fn resolve(
        &self,
        reg: &FileRegistry,
        from_dir: &Path,
        target: &str,
        angled: bool,
    ) -> Option<FileId> {
        let t = Path::new(target);

        if !angled {
            let cand = normalize(&from_dir.join(t));
            if let Some(id) = reg.get(&cand) {
                return Some(id);
            }
        }

        // Among roots that can satisfy the include, prefer the one sharing the
        // most path with the including file, and only then the better-voted one.
        //
        // Taking the first match instead lets an unrelated subtree win: the
        // kernel's `tools/include/nolibc` is a legitimate root, so a bare
        // `#include <types.h>` from `fs/` resolved into userspace nolibc and
        // dragged it into that file's include closure.
        let mut best: Option<(usize, usize, FileId)> = None;
        for root in &self.roots {
            let cand = normalize(&root.join(t));
            if let Some(id) = reg.get(&cand) {
                let affinity = shared_prefix_len(root, from_dir);
                let depth = root.components().count();
                // Nearest wins; among equally near roots the shallower one
                // does, because a nested root like `tools/include/nolibc` is a
                // special case while `include` is the canonical one. Root vote
                // order breaks any remaining tie.
                let better =
                    best.is_none_or(|(a, d, _)| affinity > a || (affinity == a && depth < d));
                if better {
                    best = Some((affinity, depth, id));
                }
            }
        }
        if let Some((_, _, id)) = best {
            return Some(id);
        }

        // Angled includes fall back to the including directory. Not standard,
        // but recovers generated and vendor headers laid out unusually.
        if angled {
            let cand = normalize(&from_dir.join(t));
            if let Some(id) = reg.get(&cand) {
                return Some(id);
            }
        }

        None
    }

    /// Last resort for an unresolved include: a file whose path ends with the
    /// target. Ambiguous matches are rejected rather than guessed at.
    pub fn resolve_by_suffix(&self, reg: &FileRegistry, target: &str) -> Option<FileId> {
        let tpath = normalize(Path::new(target));
        let base = tpath.file_name().and_then(|s| s.to_str())?;
        let tcomps: Vec<_> = tpath.components().collect();

        let mut hit = None;
        let mut seen = FxHashSet::default();
        for &fid in reg.with_basename(base) {
            let fcomps: Vec<_> = reg.path(fid).components().collect();
            if fcomps.len() < tcomps.len() {
                continue;
            }
            if fcomps[fcomps.len() - tcomps.len()..] != tcomps[..] {
                continue;
            }
            if seen.insert(fid) && hit.replace(fid).is_some() {
                return None; // more than one candidate: refuse to guess
            }
        }
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg_with(paths: &[&str]) -> FileRegistry {
        let mut r = FileRegistry::default();
        for p in paths {
            r.add(PathBuf::from(p));
        }
        r
    }

    #[test]
    fn normalize_collapses_dot_and_dotdot() {
        assert_eq!(normalize(Path::new("a/./b/../c.h")), PathBuf::from("a/c.h"));
    }

    #[test]
    fn infers_include_root_from_angled_targets() {
        let reg = reg_with(&["linux/include/linux/sched.h", "linux/fs/open.c"]);
        let mut t = FxHashMap::default();
        t.insert("linux/sched.h".to_string(), 10);
        let r = Resolver::infer(&reg, &t);
        assert_eq!(r.roots()[0], PathBuf::from("linux/include"));
    }

    #[test]
    fn relative_and_normalized_paths_resolve_identically() {
        // Indexing with a relative root ("." ) must behave exactly like an
        // absolute one; a mismatch here silently halves include resolution.
        let reg = reg_with(&["./k/include/linux/sched.h", "./k/fs/open.c"]);
        let mut t = FxHashMap::default();
        t.insert("linux/sched.h".to_string(), 1);
        let r = Resolver::infer(&reg, &t);
        let got = r.resolve(&reg, Path::new("./k/fs"), "linux/sched.h", true);
        assert_eq!(got, reg.get(Path::new("k/include/linux/sched.h")));
        assert!(got.is_some(), "relative root must still resolve");
    }

    #[test]
    fn nearer_root_wins_over_an_unrelated_subtree() {
        // Both roots can satisfy `types.h`, but a file under k/fs must not pull
        // in the userspace tools tree — that pollutes its include closure and
        // with it macro scope and result ranking.
        let reg = reg_with(&[
            "k/include/types.h",
            "k/tools/include/nolibc/types.h",
            "k/fs/read_write.c",
        ]);
        let mut t = FxHashMap::default();
        // Give the tools root more votes, so only proximity can save us.
        t.insert("types.h".to_string(), 50);
        let r = Resolver::infer(&reg, &t);
        let got = r.resolve(&reg, Path::new("k/fs"), "types.h", true);
        assert_eq!(
            got,
            reg.get(Path::new("k/include/types.h")),
            "resolution must prefer the root nearer the including file"
        );
    }

    #[test]
    fn resolves_angled_include_through_inferred_root() {
        let reg = reg_with(&["k/include/linux/sched.h"]);
        let mut t = FxHashMap::default();
        t.insert("linux/sched.h".to_string(), 1);
        let r = Resolver::infer(&reg, &t);
        let got = r.resolve(&reg, Path::new("k/fs"), "linux/sched.h", true);
        assert_eq!(got, reg.get(Path::new("k/include/linux/sched.h")));
    }

    #[test]
    fn quoted_include_prefers_including_directory() {
        let reg = reg_with(&["k/fs/internal.h", "k/include/internal.h"]);
        let mut t = FxHashMap::default();
        t.insert("internal.h".to_string(), 1);
        let r = Resolver::infer(&reg, &t);
        let got = r.resolve(&reg, Path::new("k/fs"), "internal.h", false);
        assert_eq!(got, reg.get(Path::new("k/fs/internal.h")));
    }

    #[test]
    fn more_specific_root_wins_on_equal_votes() {
        let reg = reg_with(&["k/arch/x86/include/asm/io.h", "k/include/asm/io.h"]);
        let mut t = FxHashMap::default();
        t.insert("asm/io.h".to_string(), 1);
        let r = Resolver::infer(&reg, &t);
        assert_eq!(r.roots()[0], PathBuf::from("k/arch/x86/include"));
    }

    #[test]
    fn suffix_fallback_refuses_ambiguous_matches() {
        let reg = reg_with(&["a/x/foo.h", "b/x/foo.h"]);
        let r = Resolver::new(vec![]);
        assert_eq!(r.resolve_by_suffix(&reg, "x/foo.h"), None);
    }

    #[test]
    fn suffix_fallback_accepts_a_unique_match() {
        let reg = reg_with(&["a/x/foo.h", "b/y/foo.h"]);
        let r = Resolver::new(vec![]);
        assert_eq!(
            r.resolve_by_suffix(&reg, "x/foo.h"),
            reg.get(Path::new("a/x/foo.h"))
        );
    }
}
