use codenav_cpp::scan::{scan, Stats};
use rayon::prelude::*;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());

    let t0 = Instant::now();
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
    let walk = t0.elapsed();

    let t1 = Instant::now();
    let total = files
        .par_iter()
        .filter_map(|p| std::fs::read(p).ok())
        .map(|b| scan(&b))
        .reduce(Stats::default, |mut a, b| {
            a.merge(&b);
            a
        });
    let scan_time = t1.elapsed();

    let mb = total.bytes as f64 / (1024.0 * 1024.0);
    println!("root              {root}");
    println!("files             {}", total.files);
    println!("size              {mb:.1} MiB");
    println!("walk              {:.2}s", walk.as_secs_f64());
    println!(
        "scan              {:.2}s  ({:.0} MiB/s wall, {} threads)",
        scan_time.as_secs_f64(),
        mb / scan_time.as_secs_f64(),
        rayon::current_num_threads()
    );
    println!();
    println!("#include          {}", total.includes);
    println!("#define           {}", total.defines);
    println!("conditionals      {}", total.groups);
    println!("  include guards  {}", total.include_guards);
    println!("  real (denom)    {}", total.real_groups());
    println!("max nesting       {}", total.max_nesting);
    println!("unterminated      {}", total.unterminated);
    println!();
    let d = total.real_groups().max(1) as f64;
    println!(
        "unbalanced delta  {:>8}  {:5.2}%",
        total.unbalanced_delta,
        total.unbalanced_delta as f64 * 100.0 / d
    );
    println!(
        "inside parens     {:>8}  {:5.2}%",
        total.inside_parens,
        total.inside_parens as f64 * 100.0 / d
    );
    println!(
        "mid-declaration   {:>8}  {:5.2}%   (informational)",
        total.mid_declaration,
        total.mid_declaration as f64 * 100.0 / d
    );
    println!();
    println!(
        "PROBLEMATIC       {:>8}  {:5.2}%   gate: < 5.00%",
        total.problematic,
        total.problematic_pct()
    );
    println!(
        "verdict           {}",
        if total.problematic_pct() < 5.0 {
            "PASS - concatenate-all-branches is viable"
        } else {
            "FAIL - needs statement-granularity forking"
        }
    );
}
