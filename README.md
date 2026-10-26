# codenav

[![CI](https://github.com/sbbz/codenav/actions/workflows/ci.yml/badge.svg)](https://github.com/sbbz/codenav/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Fast code navigation for C and Python. No build config. No setup.**

Jump to definitions, find references, walk call hierarchies — on codebases the size of the Linux kernel. The index keeps itself current as you edit.

---

## Why

| Tool | Problem |
|---|---|
| **clangd / IntelliSense** | Accurate but too slow on big trees. Needs `compile_commands.json`. |
| **ctags** | Instant but textual. No scope. Can't resolve `#define`s. |

**codenav** resolves everything once, at index time, into an on-disk graph — so a query is a lookup, not a computation. Macro-aware, ~2 min to index the kernel, zero config.

---

## Install

**You need:** Rust **1.90+**, a C compiler (tree-sitter grammars are C), and — for the VS Code extension — Node 18+. CI runs on macOS and Linux.

| Platform | C compiler |
|---|---|
| macOS | `xcode-select --install` |
| Debian/Ubuntu | `apt install build-essential` |
| Fedora | `dnf install gcc` |

```bash
git clone https://github.com/sbbz/codenav
cd codenav
cargo install --locked --path crates/cpp
```

That puts `codenav` in `~/.cargo/bin`. A clean build takes about a minute on 8 cores.

**macOS, if you copy the binary somewhere yourself:** delete the old file first rather than overwriting it. Copying over a previously-signed binary invalidates its signature and the kernel `SIGKILL`s it — exit 137, no message. `codesign --force --sign - <path>` repairs it.

The `.codenav` index is portable when built with `codenav index .`: paths are stored relative to the tree root, so it can be copied along with the source. Copying changes mtimes, which costs one hashing pass on the next build — the content hashes still match, so nothing is re-extracted.

---

## Use it

```bash
cd /your/project
codenav index .          # build it once
codenav watch            # optional: keep it current while you work
```

```bash
codenav def vfs_read           # where is this defined
codenav refs vfs_read          # where is it used
codenav callers vfs_read       # who calls it
codenav callees vfs_read       # what does it call
codenav sym ksys_              # find symbols by prefix
```

**Add `--from <file>` for better answers.** It ranks by what that file actually includes/imports:

```bash
codenav def sys_read --from fs/read_write.c
```

---

## VS Code

Build and install the extension from source:

```bash
cd editors/vscode
npm ci
npx @vscode/vsce package --skip-license
code --install-extension codenav-0.1.0.vsix
```

Open a C or Python file and you're done. The extension starts `codenav serve`, which indexes the workspace if it has no index yet and then **re-indexes on every change** — saves in VS Code, but also `git checkout`, `git pull` or anything else that touches the tree. The status bar shows `codenav: indexing` while a build runs; the server keeps answering from the previous index until the new one is ready.

| Setting | Default | |
|---|---|---|
| `codenav.binaryPath` | `codenav` | Point it at `target/release/codenav` if you develop codenav itself, so it never goes stale. After replacing the binary, run **codenav: Restart Server**. |
| `codenav.autoIndex` | `true` | Turn off to index only via **codenav: Build/Rebuild Index**. |
| `codenav.arch` | inferred | Preferred `arch/<name>/`, e.g. `x86_64`. |
| `codenav.maxReferences` | `5000` | Cap on references returned for one symbol. |

| Key | Does |
|---|---|
| `F12` | Go to definition |
| `Shift+F12` | Find references |
| `Ctrl+T` | Search symbols |
| Right-click → Show Call Hierarchy | Both directions |

---

## The thing others can't do

C macros. This line:

```c
SYSCALL_DEFINE3(read, unsigned int, fd, char __user *, buf, size_t, count)
```

generates `sys_read`, `__do_sys_read`, `__se_sys_read`, `__x64_sys_read`.

**None of those strings exist in the source.** codenav finds all of them.

It also indexes every `#define` — 6 million of them in the kernel.

---

## Speed

**Linux kernel** — 64,201 files, 1.36 GB, 15.5M symbols, 91M occurrences:

| | Time |
|---|---|
| First index (8 cores) | ~105 s |
| Re-index, nothing changed | **0.52 s** |
| Re-index, one file edited | **1.27 s** |
| Edit saved → queryable, with the server watching | **~1.5 s** |
| `codenav def vfs_read --from fs/namei.c` (whole process) | **8.1 ms** |
| `codenav callers kfree` — all 27,988 (whole process) | **61 ms** |
| Go to definition in the editor (warm server) | **~0.1 ms** |

Re-index and CLI figures are hyperfine medians (re-index) and means (CLI) on an Apple M2, 8 cores; reproduce them from inside a tree with:

```bash
hyperfine -N --warmup 3 'codenav def vfs_read --from fs/namei.c' 'codenav callers kfree'
hyperfine -N --warmup 3 'codenav index .'
hyperfine --warmup 2 --prepare 'echo "/* $RANDOM */" >> fs/read_write.c' 'codenav index .'
git checkout fs/read_write.c && codenav index .   # undo the edits
```

**Python 3.12 stdlib** — 1,804 files:

- First index: **1.3s**
- After an edit: **0.07s**
- 285,231 symbols

---

## Languages

| | |
|---|---|
| **C** | functions, structs, unions, enums, typedefs, fields, params, **macros**, macro-generated symbols |
| **Python** | classes, functions, methods, fields, params, `self.x` attributes, imports |

Both live in one index. A mixed repo just works.

---

## Why no build config

Most tools need `-I` and `-D` flags. codenav infers them:

- **Include roots** — voted on by matching every `#include <...>` against the file tree. Recovers `include/`, `arch/x86/include/`, etc. **96.7% of kernel includes resolve.**
- **`#ifdef` branches** — *all* of them are indexed, so per-arch and per-config code is all navigable. Results carry the condition they live under.
- **Python imports** — same machinery. `a.b.c` → `a/b/c.py` or `a/b/c/__init__.py`.

---

## Staying current

`codenav serve` (what the editor runs) and `codenav watch` watch the tree with the OS's native file events and re-index after every change. On the kernel a saved edit is queryable **~1.5 s later**. Without either, run `codenav index .` again — it only redoes what changed.

**Why it's fast:** a file's symbols depend only on its own text plus the macros
its includes provide. Headers are indexed as their own files, never pasted into
every `.c` that includes them — so editing a header's body dirties that header
alone. Re-indexed files go into a small overlay; the main index isn't rewritten.

**Falls back to a full rebuild when:**

- a file's `#define`s changed (dependents may expand differently)
- includes/imports changed (closures shift)
- files were added or removed
- the overlay grows past 15% of the tree

**Safe to rebuild under a running server.** A build never touches the index files a server has mapped: it writes a new generation and swaps a pointer to it, and the server switches over on its next request. Builds take a lock, so the watcher and a `codenav index` in a terminal never interleave. A burst of changes — a branch switch touching thousands of files — becomes one build, and builds run as a child process, so a full rebuild's memory (5.4 GB peak on the kernel) never stays with the server.

---

## Known gaps

- `READ_ONCE` ranks a `tools/` copy above the real one. The kernel reaches it via a **build-time generated header** we don't have.
- A macro a file defines *before* including a header doesn't reach that header (kernel `#define pr_fmt` pattern). Costs string contents, not symbols.
- Python import changes don't trigger a rebuild. Symbols stay correct — Python extraction reads only the file's own text — but ranking refreshes on the next full rebuild.

---

## Develop

```bash
cargo test --workspace                    # unit tests + end-to-end watcher tests
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
```

The Phase-0 measurement tools (`expand-bench`, `parse-bench`, `pipeline`, …) build with `--features dev-tools`.

Regression suites need a real tree:

```bash
./scripts/golden.sh /path/to/linux       # 27 C navigation queries
./scripts/golden-py.sh /path/to/pylib    # 8 Python queries
cd editors/vscode && node test-protocol.mjs /path/to/linux   # 17 protocol tests
```

These assert **top-N position**, not just presence — an answer you have to scroll to isn't navigation.

---

## How it works

Short version below. Full design doc: **[DESIGN.md](DESIGN.md)**.

Four phases:

1. **Scan** every file's directives — `#define`, `#include`, `import`.
2. **Resolve** includes/imports → build the graph → compute each file's closure.
3. **Expand** macros, parse with tree-sitter, extract symbols.
4. **Write** a memory-mapped index. Resolution happens *here*, not at query time.

That last point is the whole trick: **the on-disk graph is the answer.** A query is a couple of page touches, not a computation.

Each file is expanded exactly **once** — not once per translation unit like clangd. That's worth ~1000x on the kernel.

---

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
