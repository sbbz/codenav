//! Line-delimited JSON server over stdio.
//!
//! Deliberately not LSP. The editor sends one JSON object per line and gets one
//! back, keyed by `id`. The index stays memory-mapped for the process lifetime,
//! so every request after the first is a few page touches.
//!
//! Requests take a symbol *name* plus the file the query came from, rather than
//! a byte offset. The editor already knows the word under the cursor, and this
//! sidesteps UTF-16-vs-byte offset conversion entirely.

use crate::watch::Status;
use codenav_index::db::Hit;
use codenav_index::{store, Db, SymbolRec, ROLE_CALL, ROLE_DECL, ROLE_DEF};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Everything a query needs to rank results.
pub struct Ctx {
    pub file: Option<u32>,
    pub closure: Option<roaring::RoaringBitmap>,
    pub arch: Option<String>,
    pub path: String,
    /// 1-based cursor line, when the editor supplies it. Disambiguates
    /// same-file candidates such as which function's parameter you meant.
    pub line: Option<u32>,
}

/// The index the server answers from, reopened whenever a build publishes a
/// new generation — whether the watcher ran it or someone typed
/// `codenav index` in a terminal.
struct Live {
    dir: PathBuf,
    state: Mutex<(Option<store::Stamp>, Option<Arc<Db>>)>,
}

impl Live {
    /// The current index, reopening it first if `CURRENT` has moved. Costs one
    /// stat per request.
    fn get(&self) -> Option<Arc<Db>> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = store::stamp(&self.dir);
        if s.1.is_none() || stamp != s.0 {
            // On failure keep answering from the previous generation, which
            // stays mapped and consistent.
            if let Ok(db) = Db::open(&self.dir) {
                *s = (stamp, Some(Arc::new(db)));
            }
        }
        s.1.clone()
    }
}

/// One JSON object per line. Requests are answered on the main thread and
/// build events come from the watcher's, so each line is written under the
/// lock to keep the two from interleaving.
fn emit(v: &Value) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}")?;
    out.flush()
}

/// Serves the index in `dir`. With `watch`, also keeps it current with the tree
/// at that root, announcing each build as `{"event": "indexing"}` and then
/// `{"event": "indexed", ...}` — lines without an `id`, which a client that
/// does not know them simply drops.
pub fn run(dir: &Path, watch: Option<&Path>) -> std::io::Result<()> {
    let live = Arc::new(Live {
        dir: dir.to_path_buf(),
        state: Mutex::new((None, None)),
    });

    let _watcher = watch.and_then(|root| {
        let l = Arc::clone(&live);
        let started = crate::watch::start(root, move |status| {
            let ev = match status {
                Status::Indexing => json!({ "event": "indexing" }),
                Status::Indexed { ok, secs, summary } => {
                    // Swap before announcing, so a client that re-queries on
                    // the event gets the new index. It also unmaps the old
                    // generation, which is what frees its disk space.
                    l.get();
                    json!({ "event": "indexed", "ok": ok, "seconds": secs, "summary": summary })
                }
            };
            let _ = emit(&ev);
        });
        match started {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("codenav: not watching for changes: {e}");
                None
            }
        }
    });

    let stdin = BufReader::new(std::io::stdin());
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                emit(&json!({"error": e.to_string()}))?;
                continue;
            }
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        // Before the first build finishes there is nothing to answer from.
        // Empty results rather than an error, so the editor does not log one
        // per keystroke while the index is being created.
        let Some(ix) = live.get() else {
            emit(&json!({ "id": id, "results": [], "indexing": true }))?;
            continue;
        };
        let ix = &*ix;
        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let name = req.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let ctx = make_ctx(ix, &req);
        let explain = req
            .get("explain")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let result = match op {
            "def" => {
                let limit = req
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(MAX_DEFS);
                json!({ "results": defs(ix, name, ctx.as_ref(), limit, explain) })
            }
            "refs" => {
                let limit = req
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(MAX_LIST);
                json!({ "results": refs(ix, name, limit) })
            }
            "callers" => {
                let limit = req
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(MAX_LIST);
                json!({ "results": callers(ix, name, limit) })
            }
            "callees" => json!({ "results": callees(ix, name, ctx.as_ref()) }),
            "symbols" => {
                let limit = req.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
                json!({ "results": symbols(ix, name, limit) })
            }
            "ping" => json!({ "ok": true, "symbols": ix.symbol_count(), "files": ix.file_count() }),
            other => json!({ "error": format!("unknown op {other}") }),
        };

        let mut out = result;
        out["id"] = id;
        emit(&out)?;
    }
    Ok(())
}

fn make_ctx(ix: &Db, req: &Value) -> Option<Ctx> {
    context(
        ix,
        req.get("file").and_then(|v| v.as_str()),
        req.get("arch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        req.get("line").and_then(|v| v.as_u64()).map(|v| v as u32),
    )
}

/// Ranking context for a query made from `from`. Shared with the CLI, so the
/// two can never rank differently.
pub fn context(
    ix: &Db,
    from: Option<&str>,
    arch: Option<String>,
    line: Option<u32>,
) -> Option<Ctx> {
    // An arch on its own is still context. Requiring a file meant the setting
    // was silently ignored whenever the caller had no particular file in hand.
    if from.is_none() && arch.is_none() {
        return None;
    }
    let id = from.and_then(|f| resolve_file(ix, f));
    Some(Ctx {
        file: id,
        closure: id.and_then(|i| ix.closure(i)),
        arch: arch.or_else(|| id.and_then(|i| arch_of(ix.path(i)))),
        path: id
            .and_then(|i| ix.path(i))
            .or(from)
            .unwrap_or_default()
            .to_string(),
        line,
    })
}

/// Maps an editor-supplied path onto a FileId.
///
/// Editors send absolute paths while the index stores whatever form it was
/// built with (`./fs/x.c` when built as `codenav index .`). Getting this wrong
/// does not fail loudly — it silently drops the query context and with it the
/// include-closure ranking, so every form is tried.
fn resolve_file(ix: &Db, from: &str) -> Option<u32> {
    if let Some(id) = ix.file_id(from) {
        return Some(id);
    }
    let trimmed = from.trim_start_matches("./");
    if let Some(id) = ix
        .file_id(&format!("./{trimmed}"))
        .or_else(|| ix.file_id(trimmed))
    {
        return Some(id);
    }
    // Absolute path: re-express it relative to the directory the server was
    // started in, which is the workspace root.
    let cwd = std::env::current_dir().ok()?;
    let rel = std::path::Path::new(from).strip_prefix(&cwd).ok()?;
    let rel = rel.to_str()?;
    ix.file_id(&format!("./{rel}")).or_else(|| ix.file_id(rel))
}

/// Maps the arch names people actually type onto the kernel's `arch/<name>`
/// directory.
///
/// Without this, selecting `x86_64` — which is what `uname -m` prints and what
/// the UI suggests — *penalises* every file under `arch/x86/`, because the
/// mismatch branch fires. That is the opposite of what the setting is for.
pub fn canonical_arch(a: &str) -> &str {
    match a {
        "x86_64" | "amd64" | "x64" | "i386" | "i686" | "x86_32" => "x86",
        "aarch64" => "arm64",
        "armv6l" | "armv7l" => "arm",
        "ppc" | "ppc64" | "ppc64le" | "powerpc64" => "powerpc",
        "riscv32" | "riscv64" => "riscv",
        "s390x" => "s390",
        "mips64" => "mips",
        other => other,
    }
}

/// Word size implied by an arch name, when it carries one.
///
/// `arch/x86` holds both `uaccess_32.h` and `uaccess_64.h`, guarded by
/// `CONFIG_X86_32`. That is genuinely undecided, so the two tie — but if the
/// caller said `x86_64`, they have told us which one they mean.
pub fn arch_bits(a: &str) -> Option<u8> {
    match a {
        "i386" | "i686" | "x86_32" | "riscv32" | "armv6l" | "armv7l" | "arm" => Some(32),
        "x86_64" | "amd64" | "x64" | "aarch64" | "arm64" | "ppc64" | "ppc64le" | "powerpc64"
        | "riscv64" | "s390x" | "mips64" => Some(64),
        _ => None,
    }
}

/// Small tiebreak between per-word-size files such as `uaccess_32.h` and
/// `uaccess_64.h`. Deliberately weak: it should separate a tie, not outvote
/// the include graph.
pub fn bitness_bonus(path: &str, want: Option<u8>) -> i32 {
    let Some(bits) = want else { return 0 };
    let stem = path.rsplit('/').next().unwrap_or(path);
    let has = |n: &str| stem.contains(&format!("_{n}."));
    match (has("64"), has("32")) {
        (true, false) => {
            if bits == 64 {
                2
            } else {
                -2
            }
        }
        (false, true) => {
            if bits == 32 {
                2
            } else {
                -2
            }
        }
        _ => 0,
    }
}

fn arch_of(path: Option<&str>) -> Option<String> {
    let p = path?;
    let i = p.find("arch/")?;
    p[i + 5..].split('/').next().map(|s| s.to_string())
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

/// Include-graph reachability dominates, because a definition in a file you
/// actually include is almost always the one you meant. The CLI ranks with
/// this too.
pub fn score(ix: &Db, s: &SymbolRec, ctx: Option<&Ctx>) -> i32 {
    let mut n = 0;
    if let Some(c) = ctx {
        if Some(s.file) == c.file {
            // Beats anything a header can score (closure 10 + canonical root 5),
            // because a definition in the file you are editing is the answer.
            n += 18;
        } else if s.is_def && c.closure.as_ref().is_some_and(|b| b.contains(s.file)) {
            // Definitions only. A `.c` file is never in anyone's include
            // closure, so granting this to declarations meant a prototype in a
            // header you include always beat the body that defines it —
            // `folio_unlock` scored 22 in pagemap.h against 15 in mm/filemap.c.
            // The signal is meant to pick *which* definition you mean, not to
            // promote a prototype over one.
            n += 10;
        }
        // Querying a file, not a position, so the context condition is TRUE.
        // Contradiction then means the candidate is unreachable in every
        // configuration; demote it, but keep it visible.
        if ix.pc_contradicts(s.pc, 0) {
            n -= 6;
        }
        if let (Some(a), Some(b)) = (arch_of(ix.path(s.file)), c.arch.as_deref()) {
            n += if a == canonical_arch(b) { 5 } else { -5 };
        }
        if let (Some(p), Some(b)) = (ix.path(s.file), c.arch.as_deref()) {
            n += bitness_bonus(p, arch_bits(b));
        }
        if let Some(p) = ix.path(s.file) {
            n += common_prefix_len(p, &c.path).min(6) as i32;
        }
    }
    // A definition under the project's canonical include root outranks a copy
    // in a driver, sample or test tree. Several files legitimately define `min`
    // and all are reachable; this is what separates the real one.
    if let Some(p) = ix.path(s.file) {
        n += match ix.root_rank(p) {
            Some(0) => 5,
            Some(_) => 2,
            None => 0,
        };
    }

    n += kind_weight(s.kind);
    if ix.path(s.file).is_some_and(looks_like_test) {
        n -= 4;
    }
    // A definition must outweigh the canonical-root bonus a competing header
    // declaration gets (+5), or `inode_owner_or_capable` answers with the
    // prototype in fs.h instead of the body in fs/inode.c. Deliberately a
    // weight and not a filter: `sys_read` has no real definition on x86, and
    // its only defining symbol is a selftest's local helper — which a hard
    // "definitions only" rule would promote over the canonical declaration.
    if s.is_def {
        n += 6;
    }
    if ix.path(s.file).is_some_and(|p| p.ends_with(".c")) {
        n += 2;
    }
    n
}

/// Go-to-definition never needs more than a screenful, and common kernel words
/// like `file` name a quarter of a million symbols — every one of which would
/// have to be scored to order them.
const MAX_DEFS: usize = 100;

/// Cap on reference and caller lists. `file` matches ~2M occurrences; the editor
/// shows a window of them, and the cap reaches the postings so the rest are
/// never decoded. Callers can raise it explicitly.
const MAX_LIST: usize = 2000;

/// `SymKind::Param` — a parameter in some other function is never the answer to
/// go-to-definition, and these dominate the result count for common words.
const KIND_PARAM: u8 = 8;
/// True for paths that look like test code.
///
/// A local subclass in a test file should not outrank the real implementation.
/// This only breaks ties: a query made *from* a test file still wins on the
/// own-file and closure bonuses, which are far larger.
fn looks_like_test(path: &str) -> bool {
    path.split('/').any(|c| {
        c == "test"
            || c == "tests"
            || c == "testing"
            || c.starts_with("test_")
            || c.ends_with("_test.py")
            || c.ends_with("_test.c")
    })
}

/// How likely a definition of this kind is to be what someone meant.
///
/// A class, function or macro is nearly always the target; an assignment or a
/// parameter that happens to share the name almost never is. In C this is also
/// what lets a macro beat a same-named symbol, since the preprocessor gets
/// there first.
fn kind_weight(kind: u8) -> i32 {
    match kind {
        // function, struct, union, enum, typedef, macro, class, method
        0 | 2 | 3 | 4 | 6 | 9 | 11 | 12 => 6,
        // field, enum constant
        5 | 7 => 2,
        // variable
        1 => 1,
        // parameter, label
        _ => 0,
    }
}

pub fn kind_name(k: u8) -> &'static str {
    match k {
        0 => "function",
        1 => "variable",
        2 => "struct",
        3 => "union",
        4 => "enum",
        5 => "enum-const",
        6 => "typedef",
        7 => "field",
        8 => "param",
        9 => "macro",
        11 => "class",
        12 => "method",
        _ => "symbol",
    }
}

/// Line and column come from the record, resolved when the index was built.
/// Deriving them here meant reading the source file per result, which was the
/// entire cost of a large `refs` or `callers` list.
fn loc(ix: &Db, file: u32, off: u32, len: u32, line: u32, col: u16) -> Value {
    json!({
        "path": ix.path(file).unwrap_or(""),
        "offset": off,
        "length": len,
        "line": line,
        "column": col,
    })
}

fn defs(ix: &Db, name: &str, ctx: Option<&Ctx>, limit: usize, explain: bool) -> Vec<Value> {
    let mut v: Vec<Hit> = ix.symbols_named(name);

    // Parameters are only meaningful in their own file. Keeping the rest turns
    // a two-result lookup into a 235k-result one for words like `file`.
    if let Some(c) = ctx {
        v.retain(|h| h.rec.kind != KIND_PARAM || Some(h.rec.file) == c.file);
    }

    // Cached, not sort_by_key: scoring reads paths out of the mmap, and a
    // plain comparator would re-derive them O(n log n) times.
    // Distance from the cursor is a first-class signal, not a tiebreak: a
    // declaration two lines above is far likelier than one a thousand lines
    // away, and it is what makes a parameter beat a same-named function
    // elsewhere in the file.
    let cursor = ctx.and_then(|c| c.line);
    v.sort_by_cached_key(|s| {
        let mut sc = score(ix, &s.rec, ctx);
        if let (Some(cur), Some(c)) = (cursor, ctx) {
            if Some(s.rec.file) == c.file {
                let line = s.rec.line;
                if line <= cur {
                    let d = cur - line;
                    sc += match d {
                        0..=50 => 10,
                        51..=200 => 6,
                        201..=1000 => 3,
                        _ => 1,
                    };
                }
            }
        }
        -sc
    });

    // The plan's rule: never silently pick when the top two are close. A single
    // clear winner is returned alone so the editor jumps straight there instead
    // of opening a peek list; otherwise the tied group is returned.
    let clear_winner = match (v.first(), v.get(1)) {
        (Some(a), Some(b)) => score(ix, &a.rec, ctx) > score(ix, &b.rec, ctx),
        _ => true,
    };
    if clear_winner {
        v.truncate(1);
    } else if let Some((c, cursor)) = ctx.and_then(|c| c.line.map(|l| (c, l))) {
        // Tied candidates in the file being edited: the one declared closest
        // above the cursor is the one meant — this is how `file` resolves to
        // the enclosing function's own parameter rather than 100 others.
        let mut best: Option<(u32, usize)> = None;
        for (i, s) in v.iter().enumerate() {
            if Some(s.rec.file) != c.file {
                continue;
            }
            let line = s.rec.line;
            if line > cursor {
                continue;
            }
            let d = cursor - line;
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i));
            }
        }
        if let Some((_, i)) = best {
            let pick = v[i].clone();
            v.clear();
            v.push(pick);
        } else {
            v.truncate(limit);
        }
    } else {
        v.truncate(limit);
    }

    let mut out = Vec::with_capacity(v.len());
    for h in &v {
        let s = &h.rec;
        let mut o = loc(ix, s.file, s.off, s.len, s.line, s.col);
        o["name"] = json!(name);
        o["kind"] = json!(kind_name(s.kind));
        o["isDefinition"] = json!(s.is_def);
        if explain {
            o["score"] = json!(score(ix, s, ctx));
            o["inClosure"] = json!(ctx
                .and_then(|c| c.closure.as_ref())
                .is_some_and(|b| b.contains(s.file)));
            o["ctxFile"] = json!(ctx.and_then(|c| c.file));
        }
        out.push(o);
    }
    out
}

fn refs(ix: &Db, name: &str, limit: usize) -> Vec<Value> {
    let (occs, total) = ix.references_limited(name, limit);
    let mut out = Vec::with_capacity(occs.len());
    for oh in occs.iter() {
        let o = &oh.rec;
        {
            let mut v = loc(ix, o.file, o.off, o.len, o.line, o.col);
            v["role"] = json!(match o.role {
                ROLE_DECL => "declaration",
                ROLE_DEF => "definition",
                ROLE_CALL => "call",
                _ => "reference",
            });
            v["enclosing"] = match &oh.enclosing {
                Some(n) => json!(n),
                None => Value::Null,
            };
            out.push(v);
        }
    }
    if let (true, Some(first)) = (total > limit, out.first_mut()) {
        first["truncated"] = json!(total);
    }
    out
}

fn callers(ix: &Db, name: &str, limit: usize) -> Vec<Value> {
    let (syms, total) = ix.callers_limited(name, limit);
    let mut out = Vec::with_capacity(syms.len());
    for h in syms.iter() {
        let s = &h.rec;
        let mut v = loc(ix, s.file, s.off, s.len, s.line, s.col);
        v["name"] = json!(h.name);
        v["kind"] = json!(kind_name(s.kind));
        out.push(v);
    }
    if let (true, Some(first)) = (total > limit, out.first_mut()) {
        first["truncated"] = json!(total);
    }
    out
}

fn callees(ix: &Db, name: &str, ctx: Option<&Ctx>) -> Vec<Value> {
    // Resolve the id straight from the posting list; scanning every record to
    // recover it would be O(symbols) per request.
    // Only a definition has call edges, so declarations are excluded before
    // ranking — a header declaration can otherwise tie with the real body.
    let all = ix.symbol_ids_named(name);
    let mut ids: Vec<u32> = all
        .iter()
        .copied()
        .filter(|&i| ix.symbol(i).is_some_and(|s| s.is_def))
        .collect();
    if ids.is_empty() {
        ids = all;
    }
    ids.sort_by_cached_key(|&i| ix.symbol(i).map(|s| -score(ix, &s, ctx)).unwrap_or(0));
    let Some(id) = ids.first().copied() else {
        return Vec::new();
    };

    let callee_names = ix.callees_of(id);
    let mut out = Vec::with_capacity(callee_names.len());
    for text in callee_names {
        (|| {
            // Point at the callee's definition, ranked the same way go-to-def
            // is — otherwise an unrelated copy in some selftest can win.
            let mut cands = ix.symbols_named(&text);
            if let Some(c) = ctx {
                cands.retain(|h| h.rec.kind != KIND_PARAM || Some(h.rec.file) == c.file);
            }
            cands.sort_by_cached_key(|h| -score(ix, &h.rec, ctx));
            let target = &cands.first()?.rec;
            let mut v = loc(
                ix,
                target.file,
                target.off,
                target.len,
                target.line,
                target.col,
            );
            v["name"] = json!(text);
            v["kind"] = json!(kind_name(target.kind));
            out.push(v);
            Some(())
        })();
    }
    out
}

fn symbols(ix: &Db, query: &str, limit: usize) -> Vec<Value> {
    let names = ix.names_with_prefix(query, limit);
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        if let Some(h) = ix.definitions(&n).into_iter().next() {
            let s = h.rec;
            let mut v = loc(ix, s.file, s.off, s.len, s.line, s.col);
            v["name"] = json!(n);
            v["kind"] = json!(kind_name(s.kind));
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod arch_tests {
    use super::*;

    #[test]
    fn uname_style_names_map_onto_kernel_arch_dirs() {
        // `uname -m` prints x86_64; the kernel directory is arch/x86. Comparing
        // them raw made the mismatch branch fire and *demoted* the right files.
        assert_eq!(canonical_arch("x86_64"), "x86");
        assert_eq!(canonical_arch("amd64"), "x86");
        assert_eq!(canonical_arch("i686"), "x86");
        assert_eq!(canonical_arch("aarch64"), "arm64");
        assert_eq!(canonical_arch("ppc64le"), "powerpc");
        // An exact directory name passes through untouched.
        assert_eq!(canonical_arch("x86"), "x86");
        assert_eq!(canonical_arch("riscv"), "riscv");
        // Unknown names are left alone rather than guessed at.
        assert_eq!(canonical_arch("sparc64"), "sparc64");
    }

    #[test]
    fn bitness_is_only_read_when_the_name_carries_one() {
        assert_eq!(arch_bits("x86_64"), Some(64));
        assert_eq!(arch_bits("i386"), Some(32));
        // Plain "x86" says nothing about word size, so neither variant wins.
        assert_eq!(arch_bits("x86"), None);
        assert_eq!(arch_bits("powerpc"), None);
    }

    #[test]
    fn bitness_separates_the_32_and_64_variants() {
        let p32 = "./arch/x86/include/asm/uaccess_32.h";
        let p64 = "./arch/x86/include/asm/uaccess_64.h";
        assert_eq!(bitness_bonus(p64, Some(64)), 2);
        assert_eq!(bitness_bonus(p32, Some(64)), -2);
        assert_eq!(bitness_bonus(p32, Some(32)), 2);
        // Without a word size, the two must stay tied.
        assert_eq!(bitness_bonus(p32, None), 0);
        assert_eq!(bitness_bonus(p64, None), 0);
        // A file naming neither is unaffected.
        assert_eq!(bitness_bonus("./include/linux/uaccess.h", Some(64)), 0);
    }

    #[test]
    fn a_digit_in_the_directory_is_not_a_word_size() {
        // Only the final component is inspected, and only `_32.`/`_64.`.
        assert_eq!(bitness_bonus("./arch/mips64/kernel/thing.c", Some(32)), 0);
        assert_eq!(
            bitness_bonus("./drivers/net/e1000/e1000_main.c", Some(64)),
            0
        );
    }
}
