//! End-to-end index build: discovery, pre-pass, closures, expansion, parse.
//!
//! Single-threaded on purpose, so the numbers are MiB/s/core and comparable
//! with every earlier measurement. Parallelising needs a shared read-only
//! symbol table with per-thread overflow, which is separate work.

use codenav_cpp::expand::{ExpStats, Expander, Keywords};
use codenav_cpp::files::{FileId, FileRegistry, Resolver};
use codenav_cpp::global::{collect_defines, GlobalMacros, LookupStats, ScopedMacros};
use codenav_cpp::graph::{scan_includes, IncludeGraph, RawInclude};
use codenav_cpp::intern::Interner;
use codenav_cpp::lex::{lex, WellKnown};
use codenav_cpp::pc::{PcStore, PC_TRUE};
use codenav_cpp::render::render;
use codenav_cpp::symbols::{call_edges, ExtractStats, Extractor, Kinds};
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::time::Instant;
use tree_sitter::{Node, Parser};

fn count_errors(n: Node) -> (u64, u64) {
    let mut cur = n.walk();
    let mut stack = vec![n];
    let (mut errors, mut missing) = (0u64, 0u64);
    while let Some(node) = stack.pop() {
        if node.is_error() {
            errors += 1;
            continue;
        }
        if node.is_missing() {
            missing += 1;
        }
        if node.has_error() {
            for c in node.children(&mut cur) {
                stack.push(c);
            }
        }
    }
    (errors, missing)
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let limit: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let t_all = Instant::now();

    // ---- P0 discovery ----
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
    let p0 = t.elapsed().as_secs_f64();
    println!("P0 discovery      {:>7.2}s   {} files", p0, reg.len());

    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let mut pcs = PcStore::new();
    let mut gm = GlobalMacros::default();

    // ---- P1 pre-pass: lex once, harvest includes and #defines ----
    let t = Instant::now();
    let mut all_includes: Vec<Vec<RawInclude>> = Vec::with_capacity(reg.len());
    let mut targets: FxHashMap<String, u32> = FxHashMap::default();
    let mut raw_bytes = 0u64;
    for (i, p) in paths.iter().enumerate() {
        let Ok(src) = std::fs::read(p) else {
            all_includes.push(Vec::new());
            continue;
        };
        raw_bytes += src.len() as u64;
        let toks = lex(&src, &mut it);
        let incs = scan_includes(&toks, &wk, &kw, &it);
        for inc in &incs {
            if inc.angled {
                *targets.entry(inc.target.clone()).or_default() += 1;
            }
        }
        collect_defines(i as FileId, &toks, &wk, &kw, &mut it, &mut pcs, &mut gm);
        all_includes.push(incs);
    }
    let p1 = t.elapsed().as_secs_f64();
    let rmb = raw_bytes as f64 / 1048576.0;
    println!(
        "P1 pre-pass       {:>7.2}s   {:.0} MiB/s/core   {} macro names, {} defs",
        p1,
        rmb / p1,
        gm.names(),
        gm.total_defs()
    );

    if std::env::args().any(|a| a == "--diag") {
        println!();
        println!("distinct meanings per macro name:");
        for (n, count) in gm.distinct_histogram() {
            println!("   {n:>3} -> {count}");
        }
        println!("worst offenders (name, defs, distinct):");
        for (name, defs, distinct) in gm.top_ambiguous(25) {
            println!("   {:<32} {defs:>5} {distinct:>5}", it.text(name));
        }
        println!();
    }

    // ---- P1b roots, graph, closures ----
    let t = Instant::now();
    let resolver = Resolver::infer(&reg, &targets);
    let mut g = IncludeGraph::with_capacity(reg.len());
    for (i, incs) in all_includes.iter().enumerate() {
        g.add(i as FileId, incs, &reg, &resolver);
    }
    let (closures, cst) = g.closures();
    let p1b = t.elapsed().as_secs_f64();
    println!(
        "P1b graph+closure {:>7.2}s   {} roots, {} edges, mean closure {:.0}",
        p1b,
        resolver.roots().len(),
        g.resolved,
        cst.mean_closure()
    );

    // ---- P2 expand + render + parse, once per file ----
    let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&lang).expect("tree-sitter-c");
    let kinds = Kinds::new(&lang);

    let n = paths.len().min(limit);
    let (mut lex_s, mut exp_s, mut ren_s, mut parse_s) = (0f64, 0f64, 0f64, 0f64);
    let mut done_bytes = 0u64;
    let mut out_bytes = 0u64;
    let mut exp = ExpStats::default();
    let mut lk = LookupStats::default();
    let (mut errors, mut missing) = (0u64, 0u64);
    let mut biggest = 0usize;
    let mut extract_s = 0f64;
    let (mut n_symbols, mut n_occurrences, mut n_calls, mut n_defs) = (0u64, 0u64, 0u64, 0u64);
    let mut kind_counts = [0u64; 16];
    let mut xs = ExtractStats::default();

    let t = Instant::now();
    for (i, p) in paths.iter().take(n).enumerate() {
        let Ok(src) = std::fs::read(p) else { continue };
        done_bytes += src.len() as u64;

        let t0 = Instant::now();
        let toks = lex(&src, &mut it);
        lex_s += t0.elapsed().as_secs_f64();

        let t0 = Instant::now();
        let mut expanded = Vec::with_capacity(toks.len());
        {
            let scoped = ScopedMacros::new(&gm, i as FileId, &closures[i]);
            let mut ex = Expander::new(&scoped, &mut it, &wk, &pcs);
            // Directives are skipped here; only real code is expanded. Every
            // conditional branch stays in the stream.
            let body: Vec<_> = strip_directives(&toks, &wk, PC_TRUE);
            ex.expand_seq(&body, &mut expanded);
            let s = ex.stats;
            exp.expansions += s.expansions;
            exp.pastes += s.pastes;
            exp.stringizes += s.stringizes;
            exp.budget_hits += s.budget_hits;
            exp.unexpanded_known += s.unexpanded_known;
            let ls = scoped.stats();
            lk.hits += ls.hits;
            lk.out_of_scope += ls.out_of_scope;
            lk.ambiguous += ls.ambiguous;
        }
        exp_s += t0.elapsed().as_secs_f64();

        let t0 = Instant::now();
        let r = render(&expanded, &it);
        ren_s += t0.elapsed().as_secs_f64();
        out_bytes += r.len() as u64;
        biggest = biggest.max(r.len());

        let t0 = Instant::now();
        if let Some(tree) = parser.parse(r.text.as_bytes(), None) {
            parse_s += t0.elapsed().as_secs_f64();
            let (e, m) = count_errors(tree.root_node());
            errors += e;
            missing += m;

            let t0 = Instant::now();
            let (ex, es) = Extractor::new(&kinds, &r, i as FileId).run(&tree);
            extract_s += t0.elapsed().as_secs_f64();
            n_symbols += ex.symbols.len() as u64;
            n_occurrences += ex.occurrences.len() as u64;
            n_calls += call_edges(&ex).len() as u64;
            n_defs += ex.symbols.iter().filter(|s| s.is_def).count() as u64;
            for s in &ex.symbols {
                kind_counts[s.kind as usize] += 1;
            }
            xs.identifiers += es.identifiers;
            xs.in_error_nodes += es.in_error_nodes;
        }
    }
    let p2 = t.elapsed().as_secs_f64();

    let dmb = done_bytes as f64 / 1048576.0;
    let omb = out_bytes as f64 / 1048576.0;
    println!(
        "P2 expand+parse   {:>7.2}s   {:.1} MiB/s/core   {n} files, {dmb:.0} MiB in, {omb:.0} MiB out ({:.2}x)",
        p2,
        dmb / p2,
        omb / dmb
    );
    println!(
        "   lex {:.1}s | expand {:.1}s | render {:.1}s | parse {:.1}s  ({:.0}% parse)",
        lex_s,
        exp_s,
        ren_s,
        parse_s,
        parse_s * 100.0 / p2
    );
    println!();
    println!("expansions        {}", exp.expansions);
    println!(
        "  per KiB         {:.1}",
        exp.expansions as f64 / (done_bytes as f64 / 1024.0)
    );
    println!("  budget hits     {}", exp.budget_hits);
    println!(
        "  unexpanded      {}   (macro somewhere, not expanded here)",
        exp.unexpanded_known
    );
    println!(
        "  of which AMBIG  {}   <- too many competing meanings",
        lk.ambiguous
    );
    println!("lookups in scope  {}", lk.hits);
    println!(
        "  out of scope    {}   <- defined elsewhere but not included",
        lk.out_of_scope
    );
    println!(
        "parse errors      {errors} error nodes, {missing} missing   ({:.0}/MiB)",
        errors as f64 / omb.max(0.001)
    );
    println!("largest output    {:.1} MiB", biggest as f64 / 1048576.0);
    println!();
    println!("symbols           {n_symbols}   ({n_defs} definitions)");
    println!("occurrences       {n_occurrences}");
    println!("call edges        {n_calls}");
    println!(
        "  identifiers     {}   ({} inside error nodes, {:.1}% lost)",
        xs.identifiers,
        xs.in_error_nodes,
        xs.in_error_nodes as f64 * 100.0 / xs.identifiers.max(1) as f64
    );
    println!("extract           {extract_s:.2}s");
    for (i, name) in [
        "function",
        "variable",
        "struct",
        "union",
        "enum",
        "enum_const",
        "typedef",
        "field",
        "param",
        "macro",
        "label",
    ]
    .iter()
    .enumerate()
    {
        if kind_counts[i] > 0 {
            println!("  {name:<12}  {}", kind_counts[i]);
        }
    }

    let total = t_all.elapsed().as_secs_f64();
    println!();
    println!("TOTAL             {total:>7.2}s single-threaded");
    let cores = std::thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(1);
    // P1 and P2 parallelise cleanly; P1b is graph work and stays serial.
    let par = p0 + p1b + (p1 + p2) / (cores as f64 * 0.7);
    println!("PROJECTED         {par:>7.2}s on {cores} cores at 70% scaling");
}

/// Drops directive lines, leaving the code of every conditional branch.
fn strip_directives(
    toks: &[codenav_cpp::lex::Token],
    wk: &WellKnown,
    pc: u32,
) -> Vec<codenav_cpp::lex::Token> {
    use codenav_cpp::lex::Kind;
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0usize;
    while i < toks.len() {
        let t = toks[i];
        if t.bol() && t.kind == Kind::Punct && t.sym == wk.hash {
            i += 1;
            while i < toks.len() && !toks[i].bol() {
                i += 1;
            }
            continue;
        }
        let mut t = t;
        t.pc = pc;
        out.push(t);
        i += 1;
    }
    out
}
