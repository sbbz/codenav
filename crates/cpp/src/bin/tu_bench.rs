use codenav_cpp::expand::Keywords;
use codenav_cpp::files::{FileRegistry, Resolver};
use codenav_cpp::intern::Interner;
use codenav_cpp::lex::{lex, WellKnown};
use codenav_cpp::render::render;
use codenav_cpp::tu::{scan_angled_targets, Preprocessor, Session};
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let n_tus: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

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
        "discovery       {} files in {:.2}s",
        reg.len(),
        t.elapsed().as_secs_f64()
    );

    // Directive pre-pass: collect angled include targets for root inference.
    let t = Instant::now();
    let mut targets: FxHashMap<String, u32> = FxHashMap::default();
    {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = Keywords::new(&mut it);
        for p in &paths {
            let Ok(src) = std::fs::read(p) else { continue };
            let toks = lex(&src, &mut it);
            scan_angled_targets(&toks, &wk, &kw, &it, &mut targets);
        }
    }
    let distinct: usize = targets.len();
    let total: u64 = targets.values().map(|&v| v as u64).sum();
    println!(
        "pre-pass        {distinct} distinct angled targets ({total} uses) in {:.2}s",
        t.elapsed().as_secs_f64()
    );

    let t = Instant::now();
    let resolver = Resolver::infer(&reg, &targets);
    println!(
        "root inference  {} roots in {:.2}s",
        resolver.roots().len(),
        t.elapsed().as_secs_f64()
    );
    for r in resolver.roots().iter().take(12) {
        println!("                  {}", r.display());
    }

    // How many distinct angled targets resolve through the inferred roots?
    let base = PathBuf::from(&root);
    let mut ok = 0u64;
    let mut ok_uses = 0u64;
    for (tgt, &uses) in &targets {
        if resolver.resolve(&reg, &base, tgt, true).is_some() {
            ok += 1;
            ok_uses += uses as u64;
        }
    }
    println!(
        "resolution      {ok}/{distinct} distinct ({:.1}%), {ok_uses}/{total} uses ({:.1}%)",
        ok as f64 * 100.0 / distinct.max(1) as f64,
        ok_uses as f64 * 100.0 / total.max(1) as f64
    );

    // Preprocess a sample of translation units with includes followed.
    let sources: Vec<PathBuf> = paths
        .iter()
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("c"))
        .take(n_tus)
        .cloned()
        .collect();

    let mut sess = Session::new(reg, resolver);
    let t = Instant::now();
    let (mut entered, mut res, mut unres, mut skipped, mut deep) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut out_tokens = 0u64;
    let mut rendered_bytes = 0u64;
    let mut raw_bytes = 0u64;

    for p in &sources {
        let Some(id) = sess.reg.get(p) else { continue };
        raw_bytes += std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let (toks, st, _edges) = Preprocessor::new(&mut sess).run(id);
        entered += st.files_entered;
        res += st.includes_resolved;
        unres += st.includes_unresolved;
        skipped += st.includes_skipped_guard;
        deep += st.includes_too_deep;
        out_tokens += toks.len() as u64;
        rendered_bytes += render(&toks, &sess.it).len() as u64;
    }
    let secs = t.elapsed().as_secs_f64();

    let n = sources.len().max(1) as f64;
    println!();
    println!("translation units {}", sources.len());
    println!("  raw            {:.1} MiB", raw_bytes as f64 / 1048576.0);
    println!(
        "  expanded       {:.1} MiB",
        rendered_bytes as f64 / 1048576.0
    );
    println!("  tokens out     {out_tokens}");
    println!(
        "  time           {secs:.2}s  ({:.1} MiB/s/core of raw)",
        raw_bytes as f64 / 1048576.0 / secs
    );
    println!(
        "  per TU         {:.0} files entered, {:.0} includes",
        entered as f64 / n,
        res as f64 / n
    );
    println!(
        "  includes       {res} resolved, {unres} unresolved ({:.1}% miss)",
        unres as f64 * 100.0 / (res + unres).max(1) as f64
    );
    println!("  guard skips    {skipped}");
    println!("  depth-capped   {deep}");
    println!(
        "  files lexed    {} (cache holds every file touched once)",
        sess.cached_files()
    );
    println!();
    println!(
        "  reparse factor {:.1}x   <- headers re-expanded per TU; the fragment cache must remove this",
        entered as f64 / n
    );
}
