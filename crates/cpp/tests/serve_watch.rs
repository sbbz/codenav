//! End to end through `codenav serve`, the way the editor talks to it: the
//! server must notice edits on its own and answer from the new index, with no
//! manual rebuild and no restart.
//!
//! Queries carry `file` context, as every editor query does — ranking bugs have
//! hidden from context-free queries before.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(60);

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
    next_id: u64,
}

impl Server {
    fn start(root: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codenav"))
            .arg("serve")
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn codenav serve");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(serde_json::from_str(&l).unwrap()).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next_id: 1,
        }
    }

    /// Waits for the next completed build and returns its event.
    fn indexed(&self) -> Value {
        loop {
            let v = self.lines.recv_timeout(TIMEOUT).expect("no indexed event");
            if v["event"] == "indexed" {
                assert_eq!(v["ok"], true, "build failed: {v}");
                return v;
            }
        }
    }

    /// Fails if a build starts with nothing having changed. A build reads every
    /// source file, and counting those reads as changes made each build start
    /// the next one, forever.
    ///
    /// One trailing build is allowed: FSEvents can deliver the writes that
    /// created the tree, made just before the server started watching. A loop
    /// keeps going, so a second unprompted build fails.
    fn assert_settles(&self) {
        for allowed in [true, false] {
            let Ok(v) = self.lines.recv_timeout(Duration::from_millis(1500)) else {
                return;
            };
            assert!(allowed, "builds keep starting with nothing changed: {v}");
            self.indexed();
        }
    }

    fn request(&mut self, mut req: Value) -> Vec<Value> {
        let id = self.next_id;
        self.next_id += 1;
        req["id"] = json!(id);
        writeln!(self.stdin, "{req}").unwrap();
        loop {
            let v = self.lines.recv_timeout(TIMEOUT).expect("no reply");
            if v["id"] == json!(id) {
                return v["results"].as_array().cloned().unwrap_or_default();
            }
        }
    }

    fn def(&mut self, name: &str, from: &str) -> Vec<String> {
        self.request(json!({"op": "def", "name": name, "file": from}))
            .iter()
            .map(|r| format!("{}:{}", r["path"].as_str().unwrap(), r["line"]))
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tree(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("codenav-it-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(
        d.join("src/util.h"),
        "#define LIMIT 4\nint helper(int x);\n",
    )
    .unwrap();
    std::fs::write(
        d.join("src/util.c"),
        "#include \"util.h\"\nint helper(int x) { return x + LIMIT; }\n",
    )
    .unwrap();
    std::fs::write(
        d.join("src/main.c"),
        "#include \"util.h\"\nint main(void) { return helper(1); }\n",
    )
    .unwrap();
    // Past 15% of the tree an edit is a full rebuild by design, so a tree this
    // small needs filler for a one-file edit to take the overlay path.
    for i in 0..10 {
        std::fs::write(
            d.join(format!("src/f{i}.c")),
            format!("int f{i}(void) {{ return {i}; }}\n"),
        )
        .unwrap();
    }
    d
}

/// Rewrites a file so its mtime visibly moves even on coarse-grained
/// filesystems, where an edit inside the same tick would look unchanged.
fn edit(path: &Path, text: &str) {
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(path, text).unwrap();
}

#[test]
fn the_server_follows_edits_without_a_restart() {
    let root = tree("follow");
    let mut s = Server::start(&root, &[]);

    // No index existed: the server builds one on start.
    s.indexed();
    s.assert_settles();
    assert_eq!(s.def("helper", "src/main.c"), ["./src/util.c:2"]);

    // A body edit: the incremental path, which writes an overlay.
    edit(
        &root.join("src/util.c"),
        "#include \"util.h\"\n\nint helper(int x) { return x + LIMIT; }\nint renamed(void) { return 0; }\n",
    );
    let ev = s.indexed();
    assert!(
        ev["summary"].as_str().unwrap().contains("incremental"),
        "{ev}"
    );
    assert_eq!(s.def("helper", "src/main.c"), ["./src/util.c:3"]);
    assert_eq!(s.def("renamed", "src/main.c"), ["./src/util.c:4"]);

    // A second edit on top of the overlay must still supersede the first.
    edit(
        &root.join("src/util.c"),
        "#include \"util.h\"\nint helper(int x) { return x + LIMIT; }\n",
    );
    s.indexed();
    assert!(s.def("renamed", "src/main.c").is_empty());

    // A new file gets an id past the end of the table: still incremental.
    std::fs::write(
        root.join("src/extra.c"),
        "#include \"util.h\"\nint extra(void) { return helper(2); }\n",
    )
    .unwrap();
    let ev = s.indexed();
    assert!(
        ev["summary"].as_str().unwrap().contains("incremental"),
        "{ev}"
    );
    assert_eq!(s.def("extra", "src/main.c"), ["./src/extra.c:2"]);
    let callers = s.request(json!({"op": "callers", "name": "helper", "file": "src/main.c"}));
    assert!(callers.iter().any(|c| c["name"] == "extra"), "{callers:?}");

    // And a deleted one disappears.
    std::fs::remove_file(root.join("src/extra.c")).unwrap();
    let ev = s.indexed();
    assert!(
        ev["summary"].as_str().unwrap().contains("incremental"),
        "{ev}"
    );
    assert!(s.def("extra", "src/main.c").is_empty());

    // Superseded generations do not pile up on disk: only what CURRENT
    // names — main and its overlay — is left.
    let current = std::fs::read_to_string(root.join(".codenav/CURRENT")).unwrap();
    let mut gens: Vec<String> = std::fs::read_dir(root.join(".codenav"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("main-") || n.starts_with("overlay-"))
        .collect();
    gens.sort();
    let mut named: Vec<String> = current.lines().map(str::to_string).collect();
    named.sort();
    assert_eq!(gens, named);
}

#[test]
fn a_build_from_elsewhere_is_picked_up_too() {
    // `--no-watch`: someone runs `codenav index` in a terminal instead.
    let root = tree("external");
    let built = Command::new(env!("CARGO_BIN_EXE_codenav"))
        .args(["index", "."])
        .current_dir(&root)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(built.success());

    let mut s = Server::start(&root, &["--no-watch"]);
    assert_eq!(s.def("helper", "src/main.c"), ["./src/util.c:2"]);

    // A call hierarchy entry carries where the call is, not just who makes
    // it: `main` calls `helper` at 2:25, and that is what the editor marks.
    let at = |v: &Value| format!("{}:{}", v["line"], v["column"]);
    let callers = s.request(json!({"op": "callers", "name": "helper", "file": "src/util.c"}));
    assert_eq!(callers.len(), 1);
    assert_eq!(callers[0]["name"], "main");
    let calls: Vec<String> = callers[0]["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(at)
        .collect();
    assert_eq!(calls, ["2:25"]);
    let callees = s.request(json!({"op": "callees", "name": "main", "file": "src/main.c"}));
    assert_eq!(callees[0]["name"], "helper");
    let calls: Vec<String> = callees[0]["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(at)
        .collect();
    assert_eq!(calls, ["2:25"]);

    std::fs::write(
        root.join("src/later.c"),
        "#include \"util.h\"\nint later(void) { return 0; }\n",
    )
    .unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_codenav"))
        .args(["index", "."])
        .current_dir(&root)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(built.success());

    // The server mapped the old generation; it must move to the new one, not
    // answer from a mix of the two.
    assert_eq!(s.def("later", "src/main.c"), ["./src/later.c:2"]);
    assert_eq!(s.def("helper", "src/main.c"), ["./src/util.c:2"]);
}
