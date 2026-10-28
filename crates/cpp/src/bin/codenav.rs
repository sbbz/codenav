//! codenav — C and Python navigation from an on-disk graph, without a build config.
//!
//!   codenav index [dir]          build or update the index in <dir>/.codenav
//!   codenav watch                keep it current as files change
//!   codenav serve                JSON over stdio for editors; also watches
//!   codenav def <name>           where is this defined
//!   codenav refs <name>          where is this used
//!   codenav callers <name>       who calls this
//!   codenav callees <name>       what does this call
//!   codenav sym <prefix>         find symbols by prefix

use codenav_cpp::expand::{Expander, Keywords};
use codenav_cpp::files::{is_source, FileId, FileRegistry, Resolver};
use codenav_cpp::global::{collect_defines, GlobalMacros, ScopedMacros};
use codenav_cpp::graph::{scan_includes, IncludeGraph, RawInclude};
use codenav_cpp::incremental::strip_directives;
use codenav_cpp::intern::{Interner, Symbols, ThreadInterner};
use codenav_cpp::lex::{lex, WellKnown};
use codenav_cpp::pc::PcStore;
use codenav_cpp::progress::Progress;
use codenav_cpp::py::{module_candidates, resolve_import, PyExtractor, PyKinds, RawImport};
use codenav_cpp::render::render;
use codenav_cpp::symbols::{Extractor, Kinds, Role, SymKind};
use codenav_cpp::{serve, INDEX_DIR};
use codenav_index::{
    manifest, store, Db, IndexWriter, OccRec, SymbolRec, NO_SYMBOL, ROLE_CALL, ROLE_DECL, ROLE_DEF,
    ROLE_REF,
};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;
use tree_sitter::Parser;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(|s| s.as_str()) else {
        usage();
        return ExitCode::FAILURE;
    };

    match cmd {
        "version" | "--version" | "-V" => {
            println!("codenav {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "index" => {
            let root = args
                .iter()
                .skip(1)
                .find(|a| !a.starts_with("--"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            // The watcher names the files that changed, one per line, so the
            // build can skip walking the tree. Not meant to be typed.
            let hint = args.iter().any(|a| a == "--paths-from-stdin").then(|| {
                std::io::stdin()
                    .lines()
                    .map_while(Result::ok)
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<String>>()
            });
            match build_index(&root, hint) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("index failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "serve" => {
            let (root, dir) = locate();
            let watch = !args.iter().any(|a| a == "--no-watch");
            if !watch && !dir.is_dir() {
                eprintln!("no index found; run `codenav index <dir>` first");
                return ExitCode::FAILURE;
            }
            match codenav_cpp::serve::run(&dir, watch.then_some(root.as_path())) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("serve failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "watch" => {
            let (root, _) = locate();
            let shown = root.display().to_string();
            let started = codenav_cpp::watch::start(&root, move |status| {
                if let codenav_cpp::watch::Status::Indexed { ok: true, secs, .. } = status {
                    eprintln!("-- up to date ({secs:.1}s); watching {shown}");
                }
            });
            match started {
                Ok(_watcher) => loop {
                    std::thread::park();
                },
                Err(e) => {
                    eprintln!("watch failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "diagnose" => {
            let Some(path) = args.iter().skip(1).find(|a| !a.starts_with("--")) else {
                eprintln!("diagnose: expected a file path");
                return ExitCode::FAILURE;
            };
            let dump = args.iter().any(|a| a == "--dump");
            match diagnose(path, dump) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(e) => {
                    eprintln!("diagnose failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "def" | "refs" | "callers" | "callees" | "sym" => {
            let Some(name) = args.iter().skip(1).find(|a| !a.starts_with("--")) else {
                eprintln!("{cmd}: expected a name");
                return ExitCode::FAILURE;
            };
            let from = flag(&args, "--from");
            let arch = flag(&args, "--arch");
            match query(cmd, name, from.as_deref(), arch) {
                Ok(found) => {
                    if found {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("query failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            usage();
            ExitCode::FAILURE
        }
    }
}

/// Value of `--name value` or `--name=value`.
fn flag(args: &[String], name: &str) -> Option<String> {
    let eq = format!("{name}=");
    for (i, a) in args.iter().enumerate() {
        if let Some(v) = a.strip_prefix(&eq) {
            return Some(v.to_string());
        }
        if a == name {
            return args.get(i + 1).cloned();
        }
    }
    None
}

fn usage() {
    eprintln!(
        "codenav — C and Python code navigation\n\n\
         \x20 codenav index [dir]      build or update the index (default: .)\n\
         \x20 codenav watch            keep the index current as files change\n\
         \x20 codenav def <name>       where is this defined\n\
         \x20 codenav refs <name>      where is this used\n\
         \x20 codenav callers <name>   who calls this\n\
         \x20 codenav callees <name>   what does this call\n\
         \x20 codenav sym <prefix>     find symbols by prefix\n\
         \x20 codenav serve            line-delimited JSON over stdio; watches\n\
         \x20                          the tree unless --no-watch\n\
         \x20 codenav diagnose <file>  show where the parse breaks (--dump for text)\n\
         \x20 codenav version          print the version\n\n\
         ranking:\n\
         \x20 --from <file>            rank by what <file> actually includes\n\
         \x20 --arch <name>            prefer arch/<name>/ over other arches"
    );
}

/// The tree root and its index directory: the nearest ancestor of the current
/// directory that already has an index, so commands work from subdirectories,
/// or else the current directory itself.
fn locate() -> (PathBuf, PathBuf) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut d = cwd.clone();
    loop {
        let c = d.join(INDEX_DIR);
        if c.is_dir() {
            return (d, c);
        }
        if !d.pop() {
            return (cwd.clone(), cwd.join(INDEX_DIR));
        }
    }
}

fn index_dir() -> PathBuf {
    locate().1
}

fn build_index(root: &Path, hint: Option<Vec<String>>) -> std::io::Result<()> {
    let t_all = Instant::now();

    // Held to the end, so a second build — the server's watcher and a terminal,
    // say — waits instead of interleaving with this one.
    let out_dir = root.join(INDEX_DIR);
    let _lock = store::BuildLock::acquire(&out_dir, || {
        eprintln!("waiting for another codenav build to finish...")
    })?;

    // An index from before generations existed is rebuilt rather than updated:
    // the incremental path only knows how to publish on top of a generation.
    let old = match store::current(&out_dir) {
        Ok(Some(_)) => manifest::Manifest::load(&out_dir),
        _ => None,
    };

    // With every changed file named, re-stat just those against the manifest
    // instead of walking the tree. Only when the manifest names files the way
    // this walk would (`./a/b.c`, from `codenav index .`).
    let mut fell_back = false;
    if let (Some(hint), Some(m)) = (hint, &old) {
        if root == Path::new(".") && m.files.keys().next().is_some_and(|k| k.starts_with("./")) {
            let t = Instant::now();
            let mut scans: FxHashMap<String, manifest::Scan> = m
                .files
                .iter()
                .map(|(p, f)| {
                    let s = manifest::Scan {
                        content: f.content,
                        mtime_ns: f.mtime_ns,
                        size: f.size,
                    };
                    (p.clone(), s)
                })
                .collect();
            let named = hint.len();
            for p in hint {
                let path = Path::new(&p);
                match (is_source(path) && path.is_file())
                    .then(|| manifest::rescan(path, m.get(&p)))
                    .flatten()
                {
                    Some(s) => scans.insert(p, s),
                    None => scans.remove(&p),
                };
            }
            let hashes: Vec<(String, manifest::Scan)> = scans.into_iter().collect();
            let changes = manifest::diff(m, &hashes);
            if changes.is_empty() {
                eprintln!("index up to date ({named} changed path(s) checked)");
                return Ok(());
            }
            eprintln!(
                "changed: {} modified, {} added, {} removed ({named} path(s) from the watcher, checked in {:.2}s)",
                changes.modified.len(),
                changes.added.len(),
                changes.removed.len(),
                t.elapsed().as_secs_f64()
            );
            let t = Instant::now();
            match codenav_cpp::incremental::try_update(&out_dir, &changes, &hashes, m)? {
                codenav_cpp::incremental::Outcome::Applied { files } => {
                    eprintln!(
                        "incremental: {files} file(s) re-indexed in {:.2}s",
                        t.elapsed().as_secs_f64()
                    );
                    return Ok(());
                }
                codenav_cpp::incremental::Outcome::Fallback(why) => {
                    eprintln!("full rebuild: {why}");
                    fell_back = true;
                }
            }
        }
    }

    // No filters: every source under the root, exactly as the watcher sees it.
    let paths: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| is_source(p))
        .collect();

    // Python takes a separate path: no preprocessor, so no macro table and no
    // expansion. Both languages share the registry, the closure vector and the
    // index, so queries and ranking stay language-neutral.
    let (paths, py_paths): (Vec<PathBuf>, Vec<PathBuf>) = paths
        .into_iter()
        .partition(|p| p.extension().and_then(|s| s.to_str()) != Some("py"));

    let mut reg = FileRegistry::default();
    for p in &paths {
        reg.add(p.clone());
    }
    let py_base = reg.len() as FileId;
    for p in &py_paths {
        reg.add(p.clone());
    }
    eprintln!(
        "discovered {} files ({} C, {} Python)",
        reg.len(),
        paths.len(),
        py_paths.len()
    );

    // Hash everything and compare against the last build. Reading the tree is
    // cheap next to parsing it, and it is what lets an unchanged tree exit
    // without doing any work at all.
    let all_paths: Vec<&PathBuf> = paths.iter().chain(py_paths.iter()).collect();
    let t = Instant::now();
    let hashes: Vec<(String, manifest::Scan)> = all_paths
        .par_iter()
        .filter_map(|p| {
            let path = p.to_string_lossy().into_owned();
            let previous = old.as_ref().and_then(|m| m.get(&path));
            Some((path, manifest::rescan(p, previous)?))
        })
        .collect();
    let changes = old
        .as_ref()
        .map(|m| manifest::diff(m, &hashes))
        .unwrap_or_else(|| manifest::Changes {
            added: hashes.iter().map(|(p, _)| p.clone()).collect(),
            ..Default::default()
        });

    if old.is_some() && changes.is_empty() && !fell_back {
        eprintln!(
            "index up to date ({} files checked in {:.1}s)",
            hashes.len(),
            t.elapsed().as_secs_f64()
        );
        return Ok(());
    }
    if let Some(old_man) = old.as_ref().filter(|_| !fell_back) {
        eprintln!(
            "changed: {} modified, {} added, {} removed (checked in {:.1}s)",
            changes.modified.len(),
            changes.added.len(),
            changes.removed.len(),
            t.elapsed().as_secs_f64()
        );

        let t = Instant::now();
        match codenav_cpp::incremental::try_update(&out_dir, &changes, &hashes, old_man)? {
            codenav_cpp::incremental::Outcome::Applied { files } => {
                eprintln!(
                    "incremental: {files} file(s) re-indexed in {:.2}s",
                    t.elapsed().as_secs_f64()
                );
                return Ok(());
            }
            codenav_cpp::incremental::Outcome::Fallback(why) => {
                eprintln!("full rebuild: {why}");
            }
        }
    }

    // Signatures recorded for the next build's incremental decision.
    let mut file_exports: FxHashMap<String, u64> = FxHashMap::default();
    let mut file_edges: FxHashMap<String, u64> = FxHashMap::default();

    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let mut pcs = PcStore::for_c(&mut it);
    let mut gm = GlobalMacros::default();

    // P1 is serial. A parallel version (each worker interning independently,
    // then merging) ran 3x faster but produced 0.2% fewer symbols, and every
    // aggregate — names, definitions, presence conditions, interned symbols —
    // matched exactly, so the cause was never localised. Not worth an
    // unexplained index difference for 12s of a 90s build.
    let t = Instant::now();
    let mut all_includes: Vec<Vec<RawInclude>> = (0..reg.len()).map(|_| Vec::new()).collect();
    // Each file's directive lines, so an update can read its closure's
    // macros without re-lexing ~1,000 headers.
    let mut directives: Vec<Vec<u8>> = vec![Vec::new(); reg.len()];
    let mut targets: FxHashMap<String, u32> = FxHashMap::default();

    let pg = Progress::new("scan", paths.len());
    for (i, p) in paths.iter().enumerate() {
        pg.inc(1);
        let Ok(src) = std::fs::read(p) else { continue };
        let toks = lex(&src, &mut it);
        let incs = scan_includes(&toks, &wk, &kw, &it);
        for inc in &incs {
            if inc.angled {
                *targets.entry(inc.target.clone()).or_default() += 1;
            }
        }
        collect_defines(i as FileId, &toks, &wk, &kw, &mut it, &mut pcs, &mut gm);
        all_includes[i] = incs;
        directives[i] = codenav_cpp::directives::encode(&toks, &wk, &it);
    }
    pg.finish();
    eprintln!(
        "scanned directives in {:.1}s ({} names, {} defs, {} pcs)",
        t.elapsed().as_secs_f64(),
        gm.names(),
        gm.total_defs(),
        pcs.len()
    );

    for (f, sig) in gm.export_signatures() {
        if let Some(p) = all_paths.get(f as usize) {
            file_exports.insert(p.to_string_lossy().into_owned(), sig);
        }
    }

    let t = Instant::now();
    let resolver = Resolver::infer(&reg, &targets);
    let mut g = IncludeGraph::with_capacity(reg.len());
    for (i, incs) in all_includes.iter().enumerate() {
        g.add(i as FileId, incs, &reg, &resolver);
    }
    // Keyed by the walk path, which is what the manifest and the index path
    // table use. The registry stores a normalized form, and keying by that
    // silently misses every lookup.
    for (i, e) in g.edges.iter().enumerate() {
        if let Some(p) = all_paths.get(i) {
            file_edges.insert(
                p.to_string_lossy().into_owned(),
                manifest::combine_unordered(e.iter().map(|&t| t as u64)),
            );
        }
    }
    let (mut closures, _) = g.closures();
    // Stored so an update can recompute closures after an include changes.
    let mut edges = g.edges.clone();
    let missing = std::mem::take(&mut g.missing);
    eprintln!(
        "resolved includes in {:.1}s ({} roots, {} edges)",
        t.elapsed().as_secs_f64(),
        resolver.roots().len(),
        g.resolved
    );

    let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
    let kinds = Kinds::new(&lang);

    let mut w = IndexWriter::new();
    for p in &paths {
        w.add_file(&p.to_string_lossy());
    }
    for p in &py_paths {
        w.add_file(&p.to_string_lossy());
    }
    // Ranking needs these at query time; both are cheap to store.
    w.set_pcs(pcs.export());
    w.set_roots(
        resolver
            .roots()
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect(),
    );

    // P2 runs in parallel against a read-only global symbol table built by the
    // pre-pass, with each worker holding a private overflow for the handful of
    // strings that `##` paste creates. Files are processed in batches so the
    // intermediate records never all exist at once.
    struct FileOut {
        file: u32,
        too_deep: u64,
        lex_us: u64,
        expand_us: u64,
        render_us: u64,
        parse_us: u64,
        extract_us: u64,
        out_bytes: u64,
        symbols: Vec<codenav_cpp::symbols::Symbol>,
        occurrences: Vec<codenav_cpp::symbols::Occurrence>,
        names: Vec<(u32, String)>,
    }

    const BATCH: usize = 4096;
    let t = Instant::now();
    // Worker threads default to a far smaller stack than the main thread, and
    // the extractor's tree walk is recursive — a default pool overflows on the
    // kernel's deepest expanded files.
    let pool = rayon::ThreadPoolBuilder::new()
        .stack_size(64 << 20)
        .build()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let threads = pool.current_num_threads();
    let mut too_deep = 0u64;
    let profile = std::env::var("CODENAV_PROFILE").is_ok();
    let mut prof: Vec<(u64, u32, u64)> = Vec::new();
    let (mut tot_lex, mut tot_expand, mut tot_render, mut tot_parse, mut tot_extract) =
        (0u64, 0u64, 0u64, 0u64, 0u64);

    let pg = Progress::new("extract", paths.len());
    for (batch, chunk) in paths.chunks(BATCH).enumerate() {
        let base_idx = batch * BATCH;

        let outs: Vec<FileOut> = pool.install(|| {
            chunk
                .par_iter()
                .enumerate()
                .filter_map(|(k, p)| {
                    pg.inc(1);
                    let i = (base_idx + k) as u32;
                    let src = std::fs::read(p).ok()?;

                    let mut ti = ThreadInterner::new(&it);
                    let t_lex = Instant::now();
                    let toks = lex(&src, &mut ti);
                    let lex_us = t_lex.elapsed().as_micros() as u64;

                    let t_exp = Instant::now();
                    let mut expanded = Vec::with_capacity(toks.len());
                    {
                        let scoped = ScopedMacros::new(&gm, i, &closures[i as usize]);
                        let mut ex = Expander::new(&scoped, &mut ti, &wk, &pcs);
                        let body = strip_directives(&toks, &wk);
                        ex.expand_seq(&body, &mut expanded);
                    }
                    let expand_us = t_exp.elapsed().as_micros() as u64;

                    let t_ren = Instant::now();
                    let r = render(&expanded, &ti);
                    let render_us = t_ren.elapsed().as_micros() as u64;

                    let mut parser = Parser::new();
                    parser.set_language(&lang).ok()?;
                    let t_parse = Instant::now();
                    let tree = parser.parse(r.text.as_bytes(), None)?;
                    let parse_us = t_parse.elapsed().as_micros() as u64;

                    let t_ext = Instant::now();
                    let (ex, xs) = Extractor::new(&kinds, &r, i).run(&tree);
                    let extract_us = t_ext.elapsed().as_micros() as u64;

                    // Resolve each distinct name once here, on the worker, so the
                    // serial merge only does hash lookups on short strings.
                    let mut seen: FxHashMap<u32, String> = FxHashMap::default();
                    for sym in &ex.symbols {
                        seen.entry(sym.name)
                            .or_insert_with(|| ti.text(sym.name).to_string());
                    }
                    for o in &ex.occurrences {
                        seen.entry(o.name)
                            .or_insert_with(|| ti.text(o.name).to_string());
                    }

                    Some(FileOut {
                        file: i,
                        too_deep: xs.too_deep,
                        lex_us,
                        expand_us,
                        render_us,
                        parse_us,
                        extract_us,
                        out_bytes: r.len() as u64,
                        symbols: ex.symbols,
                        occurrences: ex.occurrences,
                        names: seen.into_iter().collect(),
                    })
                })
                .collect()
        });

        for out in outs {
            too_deep += out.too_deep;
            if profile {
                prof.push((out.parse_us, out.file, out.out_bytes));
                tot_lex += out.lex_us;
                tot_expand += out.expand_us;
                tot_render += out.render_us;
                tot_parse += out.parse_us;
                tot_extract += out.extract_us;
            }
            let mut map: FxHashMap<u32, u32> = FxHashMap::default();
            for (local, text) in &out.names {
                map.insert(*local, w.intern(text));
            }

            // SymbolIds are file-local coming out of extraction; rebase them
            // onto the index-wide numbering as records are appended.
            let base = w.symbol_count() as u32;
            let rebase = |e: u32| if e == NO_SYMBOL { NO_SYMBOL } else { base + e };

            for sym in &out.symbols {
                w.push_symbol(SymbolRec {
                    name: map[&sym.name],
                    file: out.file,
                    off: sym.off,
                    len: sym.len,
                    enclosing: rebase(sym.enclosing),
                    pc: sym.pc,
                    kind: sym.kind as u8,
                    is_def: sym.is_def,
                    ..Default::default()
                });
            }
            for o in &out.occurrences {
                w.push_occurrence(OccRec {
                    name: map[&o.name],
                    file: out.file,
                    off: o.off,
                    len: o.len,
                    enclosing: rebase(o.enclosing),
                    pc: o.pc,
                    role: match o.role {
                        Role::Decl => ROLE_DECL,
                        Role::Def => ROLE_DEF,
                        Role::Ref => ROLE_REF,
                        Role::Call => ROLE_CALL,
                    },
                    ..Default::default()
                });
            }
        }
    }
    pg.finish();
    if too_deep > 0 {
        eprintln!("  warning: {too_deep} subtrees hit the depth cap");
    }
    if profile {
        let total: u64 = tot_lex + tot_expand + tot_render + tot_parse + tot_extract;
        let pct = |x: u64| x as f64 * 100.0 / total.max(1) as f64;
        eprintln!(
            "  cpu: lex {:.0}s ({:.0}%) | expand {:.0}s ({:.0}%) | render {:.0}s ({:.0}%) | parse {:.0}s ({:.0}%) | extract {:.0}s ({:.0}%)",
            tot_lex as f64 / 1e6, pct(tot_lex),
            tot_expand as f64 / 1e6, pct(tot_expand),
            tot_render as f64 / 1e6, pct(tot_render),
            tot_parse as f64 / 1e6, pct(tot_parse),
            tot_extract as f64 / 1e6, pct(tot_extract),
        );
        prof.sort_unstable_by_key(|x| std::cmp::Reverse(x.0));
        let top1pct: u64 = prof.iter().take(prof.len() / 100).map(|x| x.0).sum();
        eprintln!(
            "  slowest 1% of files account for {:.0}% of parse time",
            top1pct as f64 * 100.0 / tot_parse.max(1) as f64
        );
        for (us, f, bytes) in prof.iter().take(12) {
            eprintln!(
                "    {:>7.2}s  {:>6.1} MiB  {}",
                *us as f64 / 1e6,
                *bytes as f64 / 1048576.0,
                paths[*f as usize].display()
            );
        }
    }
    eprintln!("  ({threads} threads)");
    // Macros come from the macro table rather than the parse tree, since the
    // directive lines that define them were stripped before parsing.
    let mut n_macros = 0u64;
    for (name, file, off, pc) in gm.definition_sites() {
        let text = it.text(name);
        let nid = w.intern(text);
        w.push_symbol(SymbolRec {
            name: nid,
            file,
            off,
            len: text.len() as u32,
            enclosing: NO_SYMBOL,
            pc,
            kind: SymKind::Macro as u8,
            is_def: true,
            ..Default::default()
        });
        n_macros += 1;
    }
    eprintln!("  {n_macros} macro definitions");

    eprintln!(
        "extracted {} symbols, {} occurrences in {:.1}s",
        w.symbol_count(),
        w.occurrence_count(),
        t.elapsed().as_secs_f64()
    );

    let mut py_roots: Vec<String> = Vec::new();
    let mut py_missing: Vec<(u32, u32, String)> = Vec::new();
    // ---- Python phase ----
    // No preprocessor, so no macro table and no expansion: each file is parsed
    // once and yields its symbols and its imports together. The import graph is
    // only needed for query-time ranking, not for extraction.
    if !py_paths.is_empty() {
        let t = Instant::now();
        let py_lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        let py_kinds = PyKinds::new(&py_lang);

        struct PyOut {
            file: u32,
            ex: codenav_cpp::symbols::Extracted,
            names: Vec<(u32, String)>,
            imports: Vec<RawImport>,
        }

        let mut all_imports: Vec<(u32, Vec<RawImport>)> = Vec::with_capacity(py_paths.len());

        let pg = Progress::new("python", py_paths.len());
        for (batch, chunk) in py_paths.chunks(BATCH).enumerate() {
            let base_idx = batch * BATCH;
            let outs: Vec<PyOut> = pool.install(|| {
                chunk
                    .par_iter()
                    .enumerate()
                    .filter_map(|(k, p)| {
                        pg.inc(1);
                        let file = py_base + (base_idx + k) as u32;
                        let src = std::fs::read(p).ok()?;
                        let mut ti = ThreadInterner::new(&it);
                        let mut parser = Parser::new();
                        parser.set_language(&py_lang).ok()?;
                        let tree = parser.parse(&src, None)?;
                        let (ex, imports, _) =
                            PyExtractor::new(&py_kinds, &src, file, &mut ti).run(&tree);

                        // Resolve each distinct name once on the worker, as the
                        // C phase does, so the serial merge only hashes strings.
                        let mut seen: FxHashMap<u32, String> = FxHashMap::default();
                        for s in &ex.symbols {
                            seen.entry(s.name)
                                .or_insert_with(|| ti.text(s.name).to_string());
                        }
                        for o in &ex.occurrences {
                            seen.entry(o.name)
                                .or_insert_with(|| ti.text(o.name).to_string());
                        }
                        Some(PyOut {
                            file,
                            ex,
                            names: seen.into_iter().collect(),
                            imports,
                        })
                    })
                    .collect()
            });

            for out in outs {
                let mut map: FxHashMap<u32, u32> = FxHashMap::default();
                for (local, text) in &out.names {
                    map.insert(*local, w.intern(text));
                }
                let base = w.symbol_count() as u32;
                let rebase = |e: u32| if e == NO_SYMBOL { NO_SYMBOL } else { base + e };

                for s in &out.ex.symbols {
                    w.push_symbol(SymbolRec {
                        name: map[&s.name],
                        file: out.file,
                        off: s.off,
                        len: s.len,
                        enclosing: rebase(s.enclosing),
                        pc: s.pc,
                        kind: s.kind as u8,
                        is_def: s.is_def,
                        ..Default::default()
                    });
                }
                for o in &out.ex.occurrences {
                    w.push_occurrence(OccRec {
                        name: map[&o.name],
                        file: out.file,
                        off: o.off,
                        len: o.len,
                        enclosing: rebase(o.enclosing),
                        pc: o.pc,
                        role: match o.role {
                            Role::Decl => ROLE_DECL,
                            Role::Def => ROLE_DEF,
                            Role::Ref => ROLE_REF,
                            Role::Call => ROLE_CALL,
                        },
                        ..Default::default()
                    });
                }
                all_imports.push((out.file, out.imports));
            }
        }

        // Import graph. Absolute imports vote for roots exactly as C includes
        // do — the resolver only ever matched path suffixes, and `a.b.c`
        // rewritten to `a/b/c` is one.
        let mut py_targets: FxHashMap<String, u32> = FxHashMap::default();
        for (_, imps) in &all_imports {
            for i in imps.iter().filter(|i| i.level == 0) {
                for c in module_candidates(&i.path) {
                    *py_targets.entry(c).or_default() += 1;
                }
            }
        }
        let py_resolver = Resolver::infer(&reg, &py_targets);
        py_roots = py_resolver
            .roots()
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();

        let mut pyg = IncludeGraph::with_capacity(reg.len());
        let (mut resolved, mut unresolved) = (0u64, 0u64);
        for (file, imps) in &all_imports {
            let dir = reg
                .path(*file)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            for imp in imps {
                match resolve_import(&reg, &py_resolver, &dir, imp) {
                    Some(to) => {
                        resolved += 1;
                        if to != *file {
                            pyg.edges[*file as usize].push(to);
                        }
                    }
                    // Third-party and stdlib-external imports simply are not in
                    // the tree; that is expected, not an error.
                    None => {
                        unresolved += 1;
                        py_missing.push((*file, imp.level, imp.path.clone()));
                    }
                }
            }
            pyg.edges[*file as usize].sort_unstable();
            pyg.edges[*file as usize].dedup();
        }

        // Python import edges are deliberately not recorded in the manifest.
        // They feed closures, and for Python closures only affect *ranking* —
        // extraction reads nothing but the file's own text. So an import change
        // never invalidates another file's symbols, and the incremental check
        // treats Python edges as constant. Ranking refreshes on a full rebuild.
        let (py_closures, _) = pyg.closures();
        // Python file ids occupy the tail of the registry, so only that range
        // is replaced; the C closures computed earlier stay untouched.
        let tail = py_base as usize..reg.len();
        closures[tail.clone()].clone_from_slice(&py_closures[tail.clone()]);
        edges[tail.clone()].clone_from_slice(&pyg.edges[tail]);

        pg.finish();
        eprintln!(
            "python: {} files, {} symbols, imports {} resolved / {} external, {} roots in {:.1}s",
            py_paths.len(),
            w.symbol_count(),
            resolved,
            unresolved,
            py_resolver.roots().len(),
            t.elapsed().as_secs_f64()
        );
    }

    w.set_closures(closures);
    w.set_edges(edges);
    w.set_unresolved(missing);
    w.set_directives(directives);
    w.set_python_imports(py_roots, py_missing);

    let out = out_dir;
    let t = Instant::now();
    // The write is a single opaque call of ~15s on the kernel, with no loop to
    // hang a bar on — at least say what it is doing.
    codenav_cpp::progress::note("writing index...");
    let gen = store::fresh(&out, store::Kind::Main)?;
    let stats = w.write(&out.join(&gen))?;
    codenav_cpp::progress::clear();

    // Publishing a main with no overlay is what discards the old overlay. Its
    // tombstones name FileIds from the *previous* main; carried over, they
    // would silently mask symbols this build just wrote correctly.
    let live = store::Pointer {
        main: gen,
        overlay: None,
    };
    store::publish(&out, &live)?;

    // Only after a successful write, so a failed build does not look current.
    let mut man = manifest::Manifest::default();
    for (path, scan) in &hashes {
        man.insert(
            path.clone(),
            manifest::FileFacts {
                content: scan.content,
                exports: file_exports.get(path).copied().unwrap_or(0),
                edges: file_edges.get(path).copied().unwrap_or(0),
                mtime_ns: scan.mtime_ns,
                size: scan.size,
            },
        );
    }
    man.save(&out)?;
    store::remove_superseded(&out, &live);
    eprintln!(
        "wrote {} in {:.1}s ({:.0} MiB)",
        out.display(),
        t.elapsed().as_secs_f64(),
        stats.bytes_on_disk as f64 / 1048576.0
    );
    eprintln!("total {:.1}s", t_all.elapsed().as_secs_f64());
    Ok(())
}

fn role_name(r: u8) -> &'static str {
    match r {
        ROLE_DECL => "decl",
        ROLE_DEF => "def",
        ROLE_CALL => "call",
        _ => "ref",
    }
}

fn query(cmd: &str, name: &str, from: Option<&str>, arch: Option<String>) -> std::io::Result<bool> {
    let dir = index_dir();
    if !dir.is_dir() {
        eprintln!("no index found; run `codenav index <dir>` first");
        return Ok(false);
    }
    let ix = Db::open(&dir)?;

    // Without a querying file there is no include graph to score against, so
    // ranking falls back to the always-available signals. Built exactly as the
    // server builds it: ranking bugs used to hide in the gap between the two.
    let ctx = serve::context(&ix, from, arch, None);
    if let (Some(f), Some(None)) = (from, ctx.as_ref().map(|c| c.file)) {
        eprintln!("warning: {f} is not in the index; ranking without it");
    }

    match cmd {
        "def" => {
            let mut defs = ix.symbols_named(name);
            if defs.is_empty() {
                eprintln!("no symbol named {name}");
                return Ok(false);
            }
            defs.sort_by_cached_key(|h| -serve::score(&ix, &h.rec, ctx.as_ref()));
            for h in &defs {
                let s = &h.rec;
                let p = ix.path(s.file).unwrap_or("?");
                let (l, c) = (s.line, s.col);
                println!(
                    "{p}:{l}:{c}  {} {}{}",
                    serve::kind_name(s.kind),
                    name,
                    if s.is_def { "" } else { "  (declaration)" }
                );
            }
        }
        "refs" => {
            let refs = ix.references(name);
            if refs.is_empty() {
                eprintln!("no references to {name}");
                return Ok(false);
            }
            for oh in &refs {
                let o = &oh.rec;
                let p = ix.path(o.file).unwrap_or("?");
                let (l, c) = (o.line, o.col);
                let ctx = oh
                    .enclosing
                    .as_ref()
                    .map(|n| format!("  in {n}"))
                    .unwrap_or_default();
                println!("{p}:{l}:{c}  {}{ctx}", role_name(o.role));
            }
        }
        "callers" => {
            let callers = ix.callers_of(name);
            if callers.is_empty() {
                eprintln!("nothing calls {name}");
                return Ok(false);
            }
            for h in &callers {
                let p = ix.path(h.rec.file).unwrap_or("?");
                let l = h.rec.line;
                println!("{}  {p}:{l}", h.name);
            }
        }
        "callees" => {
            // Only a definition has call edges, so declarations are excluded
            // before ranking.
            let mut ids: Vec<u32> = ix
                .symbol_ids_named(name)
                .into_iter()
                .filter(|&i| ix.symbol(i).is_some_and(|s| s.is_def))
                .collect();
            ids.sort_by_cached_key(|&i| {
                ix.symbol(i)
                    .map(|s| -serve::score(&ix, &s, ctx.as_ref()))
                    .unwrap_or(0)
            });
            let Some(id) = ids.first().copied() else {
                eprintln!("no definition of {name} to inspect");
                return Ok(false);
            };
            let callees = ix.callees_of(id);
            if callees.is_empty() {
                eprintln!("{name} calls nothing");
                return Ok(false);
            }
            for c in callees {
                println!("{c}");
            }
        }
        "sym" => {
            let hits = ix.names_with_prefix(name, 50);
            if hits.is_empty() {
                eprintln!("no symbols starting with {name}");
                return Ok(false);
            }
            for n in hits {
                let defs = ix.definitions(&n);
                if let Some(h) = defs.first() {
                    let s = &h.rec;
                    let p = ix.path(s.file).unwrap_or("?");
                    let l = s.line;
                    println!("{n}  {} {p}:{l}", serve::kind_name(s.kind));
                } else {
                    println!("{n}");
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(true)
}

/// Re-expands one file exactly as the build does and reports where tree-sitter
/// fails on the result.
///
/// Symbols disappear when a construct earlier in the file breaks the parse and
/// error recovery swallows everything after it. That is invisible from a query
/// — the name simply is not there — so without this the only way to find the
/// offending construct was to guess and rebuild.
fn diagnose(path: &str, dump: bool) -> std::io::Result<bool> {
    let dir = index_dir();
    let ix = Db::open(&dir)?;

    let mut reg = FileRegistry::default();
    for i in 0..ix.file_count() as FileId {
        reg.add(PathBuf::from(ix.path(i).unwrap_or("")));
    }
    let Some(file) = ix
        .file_id(path)
        .or_else(|| ix.file_id(&format!("./{}", path.trim_start_matches("./"))))
    else {
        eprintln!("{path} is not in the index");
        return Ok(false);
    };

    let mut it = Interner::new();
    let wk = WellKnown::new(&mut it);
    let kw = Keywords::new(&mut it);
    let mut pcs = PcStore::for_c(&mut it);

    let mut closures = vec![roaring::RoaringBitmap::new(); ix.file_count()];
    closures[file as usize] = ix.closure(file).unwrap_or_default();
    let scope = codenav_cpp::incr::scan_scope(&[file], &closures);

    let mut gm = GlobalMacros::default();
    gm.scan_files(&scope, &reg, &mut it, &wk, &kw, &mut pcs);

    let src = std::fs::read(reg.path(file))?;
    let toks = lex(&src, &mut it);
    let mut expanded = Vec::with_capacity(toks.len());
    {
        let scoped = ScopedMacros::new(&gm, file, &closures[file as usize]);
        let mut ex = Expander::new(&scoped, &mut it, &wk, &pcs);
        let body = strip_directives(&toks, &wk);
        ex.expand_seq(&body, &mut expanded);
    }
    let r = render(&expanded, &it);

    if dump {
        println!("{}", r.text);
        return Ok(true);
    }

    let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
    let mut p = Parser::new();
    p.set_language(&lang).map_err(std::io::Error::other)?;
    let Some(tree) = p.parse(r.text.as_bytes(), None) else {
        eprintln!("parse failed outright");
        return Ok(false);
    };

    // Line starts of the original file, so an error is reported where the
    // author can see it rather than at an offset into expanded text.
    let mut starts = vec![0u32];
    starts.extend(
        src.iter()
            .enumerate()
            .filter(|(_, &c)| c == b'\n')
            .map(|(i, _)| i as u32 + 1),
    );
    let line_of = |off: u32| starts.partition_point(|&s| s <= off);

    let mut cur = tree.walk();
    let mut stack = vec![tree.root_node()];
    let mut errors = 0usize;
    while let Some(n) = stack.pop() {
        if n.is_error() || n.is_missing() {
            errors += 1;
            let src_off = r.to_source(n.start_byte() as u32).unwrap_or(0);
            let end_off = r
                .to_source(n.end_byte().saturating_sub(1) as u32)
                .unwrap_or(src_off);
            let text =
                r.text[n.start_byte()..n.end_byte().min(n.start_byte() + 80)].replace('\n', " ");
            println!(
                "{}:{}-{}  {}  ({} bytes expanded)  {}",
                path,
                line_of(src_off),
                line_of(end_off),
                if n.is_missing() { "MISSING" } else { "ERROR" },
                n.end_byte() - n.start_byte(),
                text.trim()
            );
            continue; // the subtree below an error is noise
        }
        for c in n.children(&mut cur) {
            stack.push(c);
        }
    }
    eprintln!(
        "{errors} error node(s); {} tokens expanded from {} source bytes",
        r.tokens(),
        src.len()
    );
    Ok(errors == 0)
}
