use codenav_cpp::expand::{preprocess, ExpStats, Keywords, MacroTable};
use codenav_cpp::intern::Interner;
use codenav_cpp::lex::{lex, WellKnown};
use codenav_cpp::pc::PcStore;
use rayon::prelude::*;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Default, Clone, Copy)]
struct Agg {
    files: u64,
    bytes: u64,
    toks_in: u64,
    toks_out: u64,
    defines: u64,
    lex_ns: u64,
    pp_ns: u64,
    exp: ExpStats,
}

fn run(path: &PathBuf) -> Option<Agg> {
    let src = std::fs::read(path).ok()?;

    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let mut pcs = PcStore::new();

    let t0 = Instant::now();
    let toks = lex(&src, &mut it);
    let lex_ns = t0.elapsed().as_nanos() as u64;

    let t1 = Instant::now();
    let mut mt = MacroTable::default();
    let (out, exp) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
    let pp_ns = t1.elapsed().as_nanos() as u64;

    Some(Agg {
        files: 1,
        bytes: src.len() as u64,
        toks_in: toks.len() as u64,
        toks_out: out.len() as u64,
        defines: mt.len() as u64,
        lex_ns,
        pp_ns,
        exp,
    })
}

fn merge(mut a: Agg, b: Agg) -> Agg {
    a.files += b.files;
    a.bytes += b.bytes;
    a.toks_in += b.toks_in;
    a.toks_out += b.toks_out;
    a.defines += b.defines;
    a.lex_ns += b.lex_ns;
    a.pp_ns += b.pp_ns;
    a.exp.expansions += b.exp.expansions;
    a.exp.pastes += b.exp.pastes;
    a.exp.stringizes += b.exp.stringizes;
    a.exp.budget_hits += b.exp.budget_hits;
    a
}

/// Isolated-file mode understates expansion density badly, because a header
/// alone defines macros but barely uses any. This builds one global macro table
/// from every header first, then expands the `.c` files against it — which is
/// what the real indexer does once includes are followed.
fn global_table_mode(root: &str) {
    let all: Vec<PathBuf> = collect(root);
    let (headers, sources): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|p| p.extension().and_then(|s| s.to_str()) == Some("h"));

    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let mut pcs = PcStore::new();
    let mut mt = MacroTable::default();

    let t0 = Instant::now();
    let mut hdr_bytes = 0u64;
    for p in &headers {
        let Ok(src) = std::fs::read(p) else { continue };
        hdr_bytes += src.len() as u64;
        let toks = lex(&src, &mut it);
        let _ = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
    }
    let hdr_s = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let mut bytes = 0u64;
    let mut toks_in = 0u64;
    let mut toks_out = 0u64;
    let mut exp = ExpStats::default();
    for p in &sources {
        let Ok(src) = std::fs::read(p) else { continue };
        bytes += src.len() as u64;
        let toks = lex(&src, &mut it);
        toks_in += toks.len() as u64;
        let (out, s) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
        toks_out += out.len() as u64;
        exp.expansions += s.expansions;
        exp.pastes += s.pastes;
        exp.stringizes += s.stringizes;
        exp.budget_hits += s.budget_hits;
    }
    let src_s = t1.elapsed().as_secs_f64();

    let hmb = hdr_bytes as f64 / (1024.0 * 1024.0);
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let per_core = mb / src_s;

    println!("mode            global-table (single thread)");
    println!("headers         {} files, {hmb:.1} MiB", headers.len());
    println!(
        "  scan+define   {hdr_s:.2}s  ({:.1} MiB/s/core)",
        hmb / hdr_s
    );
    println!("  macros        {}", mt.len());
    println!("interner        {} symbols", it.len());
    println!();
    println!("sources         {} files, {mb:.1} MiB", sources.len());
    println!("  lex+expand    {src_s:.2}s  ({per_core:.1} MiB/s/core)");
    println!("  tokens in     {toks_in}");
    println!("  tokens out    {toks_out}");
    println!(
        "  growth        {:.2}x",
        toks_out as f64 / toks_in.max(1) as f64
    );
    println!();
    println!("expansions      {}", exp.expansions);
    println!(
        "  per KiB src   {:.1}",
        exp.expansions as f64 / (bytes as f64 / 1024.0)
    );
    println!("pastes          {}", exp.pastes);
    println!("stringizes      {}", exp.stringizes);
    println!(
        "budget hits     {}   ({:.4}% of expansions)",
        exp.budget_hits,
        exp.budget_hits as f64 * 100.0 / exp.expansions.max(1) as f64
    );
    println!();
    println!("THROUGHPUT      {per_core:.1} MiB/s/core   gate: > 3.0");
    println!(
        "verdict         {}",
        if per_core > 3.0 { "PASS" } else { "FAIL" }
    );
}

fn collect(root: &str) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(root)
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
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut root = ".".to_string();
    let mut global = false;
    for a in args.by_ref() {
        if a == "--global" {
            global = true;
        } else {
            root = a;
        }
    }
    if global {
        global_table_mode(&root);
        return;
    }

    let files: Vec<PathBuf> = ignore::WalkBuilder::new(&root)
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

    // Warm the page cache so this measures CPU, not disk.
    let _: u64 = files
        .par_iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();

    let threads = rayon::current_num_threads();
    let t = Instant::now();
    let agg = files.par_iter().filter_map(run).reduce(Agg::default, merge);
    let wall = t.elapsed().as_secs_f64();

    let mb = agg.bytes as f64 / (1024.0 * 1024.0);
    let cpu_s = (agg.lex_ns + agg.pp_ns) as f64 / 1e9;
    let per_core = mb / cpu_s;

    println!("root            {root}");
    println!("files           {}", agg.files);
    println!("size            {mb:.1} MiB");
    println!("tokens in       {}", agg.toks_in);
    println!("tokens out      {}", agg.toks_out);
    println!("macros defined  {}", agg.defines);
    println!();
    println!(
        "lex             {:.2}s cpu   ({:.1} MiB/s/core)",
        agg.lex_ns as f64 / 1e9,
        mb / (agg.lex_ns as f64 / 1e9)
    );
    println!(
        "preprocess      {:.2}s cpu   ({:.1} MiB/s/core)",
        agg.pp_ns as f64 / 1e9,
        mb / (agg.pp_ns as f64 / 1e9)
    );
    println!(
        "wall            {wall:.2}s  ({:.1} MiB/s, {threads} threads)",
        mb / wall
    );
    println!();
    println!("expansions      {}", agg.exp.expansions);
    println!("pastes          {}", agg.exp.pastes);
    println!("stringizes      {}", agg.exp.stringizes);
    println!(
        "budget hits     {}   ({:.4}% of expansions)",
        agg.exp.budget_hits,
        agg.exp.budget_hits as f64 * 100.0 / agg.exp.expansions.max(1) as f64
    );
    println!();
    println!("THROUGHPUT      {per_core:.1} MiB/s/core   gate: > 3.0");
    println!(
        "verdict         {}",
        if per_core > 3.0 {
            "PASS"
        } else {
            "FAIL - expansion cost blows the build-time budget"
        }
    );
}
