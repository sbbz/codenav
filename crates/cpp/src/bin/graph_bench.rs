use codenav_cpp::expand::Keywords;
use codenav_cpp::files::{FileId, FileRegistry, Resolver};
use codenav_cpp::graph::{scan_includes, IncludeGraph, RawInclude};
use codenav_cpp::intern::Interner;
use codenav_cpp::lex::{lex, WellKnown};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::time::Instant;

struct Scanned {
    includes: Vec<RawInclude>,
    angled: Vec<(String, u32)>,
}

/// Lexes and scans one file with a thread-local interner, returning plain
/// strings so nothing crosses threads that depends on interner identity.
fn scan_one(path: &PathBuf) -> Option<Scanned> {
    let src = std::fs::read(path).ok()?;
    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let toks = lex(&src, &mut it);
    let includes = scan_includes(&toks, &wk, &kw, &it);
    let angled = includes
        .iter()
        .filter(|i| i.angled)
        .map(|i| (i.target.clone(), 1u32))
        .collect();
    Some(Scanned { includes, angled })
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());

    let t = Instant::now();
    let paths: Vec<PathBuf> = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(false)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("c") | Some("h")
            )
        })
        .collect();
    let mut reg = FileRegistry::default();
    for p in &paths {
        reg.add(p.clone());
    }
    println!(
        "P0 discovery    {} files in {:.2}s",
        reg.len(),
        t.elapsed().as_secs_f64()
    );

    // P1: directive pre-pass, parallel.
    let t = Instant::now();
    let scanned: Vec<Option<Scanned>> = paths.par_iter().map(scan_one).collect();
    println!(
        "P1 pre-pass     {:.2}s  ({} threads)",
        t.elapsed().as_secs_f64(),
        rayon::current_num_threads()
    );

    let t = Instant::now();
    let mut targets: FxHashMap<String, u32> = FxHashMap::default();
    for s in scanned.iter().flatten() {
        for (tgt, n) in &s.angled {
            *targets.entry(tgt.clone()).or_default() += n;
        }
    }
    let resolver = Resolver::infer(&reg, &targets);
    println!(
        "   roots        {} inferred in {:.2}s",
        resolver.roots().len(),
        t.elapsed().as_secs_f64()
    );

    let t = Instant::now();
    let mut g = IncludeGraph::with_capacity(reg.len());
    for (i, s) in scanned.iter().enumerate() {
        if let Some(s) = s {
            g.add(i as FileId, &s.includes, &reg, &resolver);
        }
    }
    let edges: usize = g.edges.iter().map(|e| e.len()).sum();
    println!(
        "P1 graph        {edges} edges in {:.2}s   ({} resolved, {} unresolved, {:.1}% miss)",
        t.elapsed().as_secs_f64(),
        g.resolved,
        g.unresolved,
        g.unresolved as f64 * 100.0 / (g.resolved + g.unresolved).max(1) as f64
    );

    let t = Instant::now();
    let (closures, st) = g.closures();
    let secs = t.elapsed().as_secs_f64();

    let bytes: usize = closures.iter().map(|c| c.serialized_size()).sum();
    println!("P2 closures     {secs:.2}s");
    println!("   mean         {:.0} files per closure", st.mean_closure());
    println!("   max          {}", st.max_closure);
    println!("   total        {} entries", st.total_entries);
    println!("   memory       {:.0} MiB", bytes as f64 / 1048576.0);
    println!("   cycle edges  {}", st.cycle_edges);

    // Distribution, to see whether a few huge headers dominate.
    let mut sizes: Vec<u64> = closures.iter().map(|c| c.len()).collect();
    sizes.sort_unstable();
    let pct = |p: f64| sizes[((sizes.len() as f64 * p) as usize).min(sizes.len() - 1)];
    println!(
        "   p50/p90/p99  {} / {} / {}",
        pct(0.50),
        pct(0.90),
        pct(0.99)
    );

    println!();
    println!(
        "Every file is expanded ONCE under this model, versus {:.0} times per TU\nwith per-TU header expansion.",
        st.mean_closure()
    );
}
