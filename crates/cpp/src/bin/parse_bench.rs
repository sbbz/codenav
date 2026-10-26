use codenav_cpp::expand::{preprocess, Keywords, MacroTable};
use codenav_cpp::intern::Interner;
use codenav_cpp::lex::{lex, WellKnown};
use codenav_cpp::pc::PcStore;
use codenav_cpp::render::render;
use std::path::PathBuf;
use std::time::Instant;
use tree_sitter::{Node, Parser};

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

#[derive(Default, Clone, Copy)]
struct Errs {
    nodes: u64,
    errors: u64,
    missing: u64,
}

fn walk(n: Node, e: &mut Errs) {
    let mut cur = n.walk();
    let mut stack = vec![n];
    while let Some(node) = stack.pop() {
        e.nodes += 1;
        if node.is_error() {
            e.errors += 1;
            continue; // don't descend into error subtrees
        }
        if node.is_missing() {
            e.missing += 1;
        }
        if node.has_error() {
            for c in node.children(&mut cur) {
                stack.push(c);
            }
        }
    }
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let no_macros = std::env::args().any(|a| a == "--no-macros");
    let limit: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);

    let all = collect(&root);
    let (headers, mut sources): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|p| p.extension().and_then(|s| s.to_str()) == Some("h"));
    sources.truncate(limit);

    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let mut pcs = PcStore::new();
    let mut mt = MacroTable::default();

    let t = Instant::now();
    for p in &headers {
        if no_macros {
            break;
        }
        let Ok(src) = std::fs::read(p) else { continue };
        let toks = lex(&src, &mut it);
        let _ = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
    }
    println!(
        "macro table     {} macros from {} headers in {:.2}s",
        mt.len(),
        headers.len(),
        t.elapsed().as_secs_f64()
    );

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .expect("load tree-sitter-c");

    let mut raw_bytes = 0u64;
    let mut exp_bytes = 0u64;
    let mut lex_s = 0f64;
    let mut pp_s = 0f64;
    let mut render_s = 0f64;
    let mut parse_raw_s = 0f64;
    let mut parse_exp_s = 0f64;
    let mut walk_s = 0f64;
    let mut e_raw = Errs::default();
    let mut e_exp = Errs::default();
    let mut failed = 0u64;
    let mut biggest = 0usize;

    for p in &sources {
        let Ok(src) = std::fs::read(p) else { continue };
        raw_bytes += src.len() as u64;

        // Baseline: parse the unpreprocessed source, as a ctags-class tool would.
        let t = Instant::now();
        if let Some(tree) = parser.parse(&src, None) {
            parse_raw_s += t.elapsed().as_secs_f64();
            let t = Instant::now();
            walk(tree.root_node(), &mut e_raw);
            walk_s += t.elapsed().as_secs_f64();
        }

        let t = Instant::now();
        let toks = lex(&src, &mut it);
        lex_s += t.elapsed().as_secs_f64();

        let t = Instant::now();
        let (out, _) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
        pp_s += t.elapsed().as_secs_f64();

        let t = Instant::now();
        let r = render(&out, &it);
        render_s += t.elapsed().as_secs_f64();

        exp_bytes += r.len() as u64;
        biggest = biggest.max(r.len());

        let t = Instant::now();
        match parser.parse(r.text.as_bytes(), None) {
            Some(tree) => {
                parse_exp_s += t.elapsed().as_secs_f64();
                let t = Instant::now();
                walk(tree.root_node(), &mut e_exp);
                walk_s += t.elapsed().as_secs_f64();
            }
            None => failed += 1,
        }
    }

    let rmb = raw_bytes as f64 / (1024.0 * 1024.0);
    let emb = exp_bytes as f64 / (1024.0 * 1024.0);

    println!("sources         {} files", sources.len());
    println!("raw             {rmb:.1} MiB");
    println!(
        "expanded        {emb:.1} MiB   ({:.2}x growth, largest file {:.1} MiB)",
        emb / rmb,
        biggest as f64 / (1024.0 * 1024.0)
    );
    println!();
    println!(
        "lex             {lex_s:6.2}s  ({:6.1} MiB/s/core of raw)",
        rmb / lex_s
    );
    println!(
        "preprocess      {pp_s:6.2}s  ({:6.1} MiB/s/core of raw)",
        rmb / pp_s
    );
    println!(
        "render          {render_s:6.2}s  ({:6.1} MiB/s/core of expanded)",
        emb / render_s
    );
    println!(
        "parse RAW       {parse_raw_s:6.2}s  ({:6.1} MiB/s/core)   baseline",
        rmb / parse_raw_s
    );
    println!(
        "parse EXPANDED  {parse_exp_s:6.2}s  ({:6.1} MiB/s/core of expanded)",
        emb / parse_exp_s
    );
    println!("error-walk      {walk_s:6.2}s  (measurement only, not pipeline cost)");
    println!("parse failures  {failed}");
    println!();

    let pipeline_s = lex_s + pp_s + render_s + parse_exp_s;
    println!(
        "PIPELINE        {pipeline_s:6.2}s  ({:6.1} MiB/s/core of raw source)",
        rmb / pipeline_s
    );
    println!();
    println!(
        "errors RAW      {:>9} error nodes, {:>7} missing",
        e_raw.errors, e_raw.missing
    );
    println!(
        "errors EXPANDED {:>9} error nodes, {:>7} missing",
        e_exp.errors, e_exp.missing
    );
    println!(
        "  errors/MiB    raw {:.1}  expanded {:.1}",
        e_raw.errors as f64 / rmb,
        e_exp.errors as f64 / emb
    );

    // Extrapolate to a full kernel index build on this machine.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!();
    println!(
        "extrapolated    {:.0}s single-core per GiB of source; ~{:.0}s on {cores} cores at 70% scaling",
        pipeline_s / rmb * 1024.0,
        pipeline_s / rmb * 1024.0 / (cores as f64 * 0.7)
    );
}
