//! An incremental update must leave the index exactly as a full rebuild of the
//! same tree would. Each step below makes an edit, updates incrementally, then
//! rebuilds a copy of the tree from scratch and compares every symbol,
//! occurrence and include closure.
//!
//! This is the check that matters: the failure mode of an incremental indexer
//! is not a crash but an answer that looks right and is stale.

use codenav_index::{Db, ROLE_CALL, ROLE_DECL, ROLE_DEF};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("codenav-eq-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    // Coarse mtimes must still see the edit.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(p, text).unwrap();
}

/// Runs `codenav index .` and returns its log.
fn index(root: &Path) -> String {
    index_with(root, None)
}

/// With `hint`, hands the build the changed paths the way the watcher does,
/// so it skips the walk.
fn index_with(root: &Path, hint: Option<&[&str]>) -> String {
    use std::io::Write;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codenav"));
    cmd.args(["index", "."]).current_dir(root);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    if hint.is_some() {
        cmd.arg("--paths-from-stdin").stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().unwrap();
    if let (Some(paths), Some(mut stdin)) = (hint, child.stdin.take()) {
        for p in paths {
            writeln!(stdin, "./{p}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "index failed:\n{log}");
    log
}

/// Everything a query can observe, with ids replaced by paths and names so
/// two independently built indexes compare.
fn dump(root: &Path) -> BTreeSet<String> {
    let db = Db::open(&root.join(".codenav")).unwrap();
    let path = |f: u32| db.path(f).unwrap_or("?").to_string();
    let mut out = BTreeSet::new();
    for name in db.names_with_prefix("", usize::MAX) {
        for h in db.symbols_named(&name) {
            let s = h.rec;
            out.insert(format!(
                "sym {name} {}:{}+{} kind={} def={}",
                path(s.file),
                s.off,
                s.len,
                s.kind,
                s.is_def
            ));
        }
        for o in db.references(&name) {
            let r = o.rec;
            let role = match r.role {
                ROLE_DECL => "decl",
                ROLE_DEF => "def",
                ROLE_CALL => "call",
                _ => "ref",
            };
            out.insert(format!(
                "occ {name} {}:{}+{} {role} in={:?} line={}",
                path(r.file),
                r.off,
                r.len,
                o.enclosing,
                r.line
            ));
        }
    }
    for f in 0..db.file_count() as u32 {
        let p = path(f);
        if db.file_id(&p).is_none() {
            continue; // removed
        }
        let mut seen: Vec<String> = db
            .closure(f)
            .unwrap_or_default()
            .iter()
            .filter(|&c| db.file_id(&path(c)).is_some())
            .map(path)
            .collect();
        seen.sort();
        out.insert(format!("closure {p} -> {}", seen.join(",")));
    }
    out
}

/// Rebuilds a copy of `root` from scratch and compares it with the
/// incrementally updated index.
fn assert_matches_full_build(root: &Path, step: &str) {
    // Named after the tree too: scenarios run in parallel and share step names.
    let tree = root.file_name().unwrap().to_string_lossy();
    let copy = tmp(&format!("full-{tree}-{}", step.replace(' ', "-")));
    for e in walk(root) {
        let rel = e.strip_prefix(root).unwrap();
        let to = copy.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(&e, &to).unwrap();
    }
    index(&copy);
    let (inc, full) = (dump(root), dump(&copy));
    let missing: Vec<&String> = full.difference(&inc).collect();
    let extra: Vec<&String> = inc.difference(&full).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "after {step}: the incremental index differs from a full rebuild\n\
         missing (in full, not incremental): {missing:#?}\n\
         extra (in incremental, not full): {extra:#?}"
    );
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.file_name().is_some_and(|n| n == ".codenav") {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// How a scenario updates: walking the tree, or told what changed.
#[derive(Clone, Copy)]
enum Mode {
    Walk,
    Hinted,
}

/// One step: make the edit, update, check it stayed incremental and matches.
/// `changed` is what the watcher would report, used in hinted mode.
fn step(root: &Path, mode: Mode, name: &str, changed: &[&str], edit: impl FnOnce()) {
    edit();
    let log = match mode {
        Mode::Walk => index(root),
        Mode::Hinted => index_with(root, Some(changed)),
    };
    assert!(
        log.contains("incremental:"),
        "{name}: expected an incremental update, got:\n{log}"
    );
    if let Mode::Hinted = mode {
        assert!(
            log.contains("from the watcher"),
            "{name}: walked anyway:\n{log}"
        );
    }
    assert_matches_full_build(root, name);
}

#[test]
fn every_kind_of_edit_matches_a_full_rebuild() {
    scenario("walk", Mode::Walk);
}

#[test]
fn the_same_holds_when_told_what_changed() {
    scenario("hinted", Mode::Hinted);
}

fn scenario(tag: &str, mode: Mode) {
    let root = tmp(tag);
    write(
        &root,
        "include/gen.h",
        "#define MAKE(n) int gen_##n(void)\n#define LIMIT 4\nint shared(int);\n",
    );
    write(
        &root,
        "src/a.c",
        "#include \"gen.h\"\nMAKE(alpha) { return LIMIT; }\nint shared(int x) { return x; }\n",
    );
    write(
        &root,
        "src/b.c",
        "#include \"late.h\"\nLATE(beta)\nint b_main(void) { return shared(1); }\n",
    );
    write(&root, "src/c.c", "int c_only(void) { return 3; }\n");
    write(&root, "py/mod.py", "def py_func():\n    return 1\n");
    // Enough untouched files that no step trips the "too much of the tree"
    // fallbacks, which are tested elsewhere.
    for i in 0..60 {
        write(
            &root,
            &format!("fill/f{i}.c"),
            &format!("int fill{i}(void) {{ return {i}; }}\n"),
        );
    }
    index(&root);

    step(&root, mode, "a body edit", &["src/c.c"], || {
        write(
            &root,
            "src/c.c",
            "\nint c_only(void) { return 4; }\nint c_more(void) { return 5; }\n",
        )
    });
    step(
        &root,
        mode,
        "a comment in a header",
        &["include/gen.h"],
        || {
            write(
            &root,
            "include/gen.h",
            "/* note */\n#define MAKE(n) int gen_##n(void)\n#define LIMIT 4\nint shared(int);\n",
        )
        },
    );
    step(
        &root,
        mode,
        "a macro change reaching includers",
        &["include/gen.h"],
        || {
            write(
                &root,
                "include/gen.h",
                "#define MAKE(n) int made_##n(void)\n#define LIMIT 4\nint shared(int);\n",
            )
        },
    );
    step(&root, mode, "a new file", &["src/d.c"], || {
        write(
            &root,
            "src/d.c",
            "#include \"gen.h\"\nMAKE(delta) { return shared(LIMIT); }\n",
        )
    });
    step(
        &root,
        mode,
        "a new header that satisfies an old include",
        &["src/late.h"],
        || {
            write(
                &root,
                "src/late.h",
                "#define LATE(n) int late_##n(void) { return 0; }\n",
            )
        },
    );
    step(
        &root,
        mode,
        "an include added to a file",
        &["src/c.c"],
        || {
            write(
            &root,
            "src/c.c",
            "#include \"gen.h\"\nMAKE(gamma) { return LIMIT; }\nint c_only(void) { return 4; }\n",
        )
        },
    );
    step(&root, mode, "a header removed", &["src/late.h"], || {
        std::fs::remove_file(root.join("src/late.h")).unwrap()
    });
    step(&root, mode, "a file removed", &["src/d.c"], || {
        std::fs::remove_file(root.join("src/d.c")).unwrap()
    });
    step(&root, mode, "a removed file re-added", &["src/d.c"], || {
        write(
            &root,
            "src/d.c",
            "#include \"gen.h\"\nMAKE(again) { return 1; }\n",
        )
    });
    step(
        &root,
        mode,
        "python edited and added",
        &["py/mod.py", "py/new.py"],
        || {
            write(
                &root,
                "py/mod.py",
                "def py_func():\n    return 2\n\nclass Thing:\n    pass\n",
            );
            write(
                &root,
                "py/new.py",
                "from mod import Thing\n\ndef use():\n    return Thing()\n",
            );
        },
    );
    // Earlier steps' files are now carried forward by copying; one more edit
    // elsewhere must leave all of them intact.
    step(
        &root,
        mode,
        "an unrelated edit after many",
        &["fill/f0.c"],
        || write(&root, "fill/f0.c", "int fill0(void) { return 100; }\n"),
    );
}

#[test]
fn a_header_edit_keeps_its_macros() {
    // The regression: an updated file's macro definitions were not re-added,
    // so editing a header made its macros vanish from go-to-definition.
    let root = tmp("macros");
    let mode = Mode::Hinted;
    write(&root, "a.h", "#define MAXLEN 16\nint helper(int x);\n");
    write(
        &root,
        "a.c",
        "#include \"a.h\"\nint helper(int x) { return x + MAXLEN; }\n",
    );
    for i in 0..20 {
        write(
            &root,
            &format!("g{i}.c"),
            &format!("int g{i}(void) {{ return {i}; }}\n"),
        );
    }
    index(&root);
    step(&root, mode, "a header edit", &["a.h"], || {
        write(
            &root,
            "a.h",
            "#define MAXLEN 16\nint helper(int x);\n/* edited */\n",
        )
    });
    let db = Db::open(&root.join(".codenav")).unwrap();
    assert_eq!(db.definitions("MAXLEN").len(), 1);
}
