//! Include graph and per-file include closures.
//!
//! Being config-less buys a simplification that a build-config-driven indexer
//! cannot have: a header's macro environment can be defined canonically as its
//! own transitive include closure, independent of which translation unit pulled
//! it in. Every file is therefore expanded exactly once rather than once per
//! including TU — which is the whole difference from clangd.
//!
//! The tradeoff: a macro a TU defines *before* including a header (the kernel's
//! `#define pr_fmt(...)` pattern) does not reach that header. For navigation
//! that costs string contents, not symbols.

use crate::expand::Keywords;
use crate::files::{FileId, FileRegistry, Resolver};
use crate::intern::Interner;
use crate::lex::{Kind, Token, WellKnown};
use roaring::RoaringBitmap;

/// One `#include` directive as written, before resolution.
pub struct RawInclude {
    pub target: String,
    pub angled: bool,
    pub off: u32,
}

/// Extracts include directives without expanding anything. Computed includes
/// (`#include SOME_MACRO`) are skipped here and resolved later during expansion.
pub fn scan_includes(
    toks: &[Token],
    wk: &WellKnown,
    kw: &Keywords,
    it: &Interner,
) -> Vec<RawInclude> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let t = toks[i];
        if !(t.bol() && t.kind == Kind::Punct && t.sym == wk.hash) {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < toks.len() && !toks[j].bol() {
            j += 1;
        }
        let line = &toks[start..j];
        i = j;

        if line.first().is_none_or(|k| k.sym != kw.include) {
            continue;
        }
        let rest = &line[1..];
        let Some(f) = rest.first() else { continue };

        if f.kind == Kind::Str {
            out.push(RawInclude {
                target: it.text(f.sym).trim_matches('"').to_string(),
                angled: false,
                off: f.off,
            });
        } else if f.kind == Kind::Punct && it.text(f.sym) == "<" {
            let mut s = String::new();
            for t in &rest[1..] {
                let txt = it.text(t.sym);
                if txt == ">" {
                    if !s.is_empty() {
                        out.push(RawInclude {
                            target: s,
                            angled: true,
                            off: f.off,
                        });
                    }
                    break;
                }
                s.push_str(txt);
            }
        }
    }
    out
}

#[derive(Default)]
pub struct IncludeGraph {
    /// Direct include edges, indexed by including file.
    pub edges: Vec<Vec<FileId>>,
    pub unresolved: u64,
    pub resolved: u64,
}

impl IncludeGraph {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            edges: vec![Vec::new(); n],
            unresolved: 0,
            resolved: 0,
        }
    }

    pub fn add(
        &mut self,
        from: FileId,
        incs: &[RawInclude],
        reg: &FileRegistry,
        resolver: &Resolver,
    ) {
        let dir = reg
            .path(from)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        for inc in incs {
            match resolver
                .resolve(reg, &dir, &inc.target, inc.angled)
                .or_else(|| resolver.resolve_by_suffix(reg, &inc.target))
            {
                Some(to) => {
                    self.resolved += 1;
                    if to != from {
                        self.edges[from as usize].push(to);
                    }
                }
                None => self.unresolved += 1,
            }
        }
        self.edges[from as usize].sort_unstable();
        self.edges[from as usize].dedup();
    }

    /// Transitive include closure of every file, computed in DFS post-order so
    /// each node unions the already-finished closures of its children.
    ///
    /// Include cycles are broken by skipping nodes still on the stack. That
    /// under-approximates the closure for files inside a cycle, which is
    /// acceptable: real cycles are rare and guarded.
    pub fn closures(&self) -> (Vec<RoaringBitmap>, ClosureStats) {
        let n = self.edges.len();
        let mut closure: Vec<RoaringBitmap> = vec![RoaringBitmap::new(); n];
        let mut done = vec![false; n];
        let mut on_stack = vec![false; n];
        let mut stats = ClosureStats::default();

        // Explicit stack: the kernel's include DAG is far too deep to recurse.
        let mut stack: Vec<(FileId, usize)> = Vec::with_capacity(256);

        for root in 0..n as FileId {
            if done[root as usize] {
                continue;
            }
            stack.push((root, 0));
            on_stack[root as usize] = true;

            while let Some(&mut (node, ref mut child_idx)) = stack.last_mut() {
                let kids = &self.edges[node as usize];
                if *child_idx < kids.len() {
                    let kid = kids[*child_idx];
                    *child_idx += 1;
                    if done[kid as usize] {
                        continue;
                    }
                    if on_stack[kid as usize] {
                        stats.cycle_edges += 1;
                        continue;
                    }
                    stack.push((kid, 0));
                    on_stack[kid as usize] = true;
                    continue;
                }

                // All children finished: union their closures into this one.
                let mut acc = RoaringBitmap::new();
                for &kid in kids {
                    acc.insert(kid);
                    if done[kid as usize] {
                        acc |= &closure[kid as usize];
                    }
                }
                closure[node as usize] = acc;
                done[node as usize] = true;
                on_stack[node as usize] = false;
                stack.pop();
            }
        }

        stats.total_entries = closure.iter().map(|c| c.len()).sum();
        stats.max_closure = closure.iter().map(|c| c.len()).max().unwrap_or(0);
        stats.files = n as u64;
        (closure, stats)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ClosureStats {
    pub files: u64,
    pub total_entries: u64,
    pub max_closure: u64,
    pub cycle_edges: u64,
}

impl ClosureStats {
    pub fn mean_closure(&self) -> f64 {
        self.total_entries as f64 / self.files.max(1) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[&[FileId]]) -> IncludeGraph {
        let mut g = IncludeGraph::with_capacity(edges.len());
        for (i, e) in edges.iter().enumerate() {
            g.edges[i] = e.to_vec();
        }
        g
    }

    fn closure_of(g: &IncludeGraph, id: FileId) -> Vec<FileId> {
        let (c, _) = g.closures();
        c[id as usize].iter().collect()
    }

    #[test]
    fn direct_include_is_in_closure() {
        let g = graph(&[&[1], &[]]);
        assert_eq!(closure_of(&g, 0), vec![1]);
    }

    #[test]
    fn closure_is_transitive() {
        let g = graph(&[&[1], &[2], &[3], &[]]);
        assert_eq!(closure_of(&g, 0), vec![1, 2, 3]);
    }

    #[test]
    fn diamond_includes_are_not_double_counted() {
        let g = graph(&[&[1, 2], &[3], &[3], &[]]);
        assert_eq!(closure_of(&g, 0), vec![1, 2, 3]);
    }

    #[test]
    fn cycles_terminate() {
        let g = graph(&[&[1], &[2], &[0]]);
        let (c, st) = g.closures();
        assert!(st.cycle_edges > 0, "cycle should have been detected");
        // Every node must still have been finished.
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn deep_chain_does_not_overflow_the_stack() {
        let n = 100_000usize;
        let mut g = IncludeGraph::with_capacity(n);
        for i in 0..n - 1 {
            g.edges[i] = vec![i as FileId + 1];
        }
        let (c, st) = g.closures();
        assert_eq!(c[0].len(), n as u64 - 1);
        assert_eq!(st.max_closure, n as u64 - 1);
    }
}
