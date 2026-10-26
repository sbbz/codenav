# codenav — design

How it works and, more usefully, why. For the user-facing summary see [README.md](README.md).

---

## 1. Problem

Navigating large C codebases forces a choice between two unsatisfying kinds of tool:

- **clangd / IntelliSense** — accurate, but too slow on kernel-scale trees, and needs `compile_commands.json`.
- **ctags** — instant, but textual: no scope, no preprocessor, cannot resolve `#define`s or anything a macro generates.

The goal: precise, macro-aware navigation, a build measured in minutes, and **zero configuration**.

Two observations make that possible:

- **Queries are fast when the on-disk index *is* the answer graph.** Resolve names at index time and store the result memory-mapped and packed, and a query touches a few pages. clangd is slow for the mirror-image reason: it re-parses every header once per translation unit and merges the whole index in RAM.
- **Building that graph does not have to be slow.** Expand each file once rather than once per includer (§2.3), extract in parallel, and write sorted, fixed-width arrays once — no allocator, no B-tree, no rewriting.

---

## 2. The three decisions everything else follows from

### 2.1 Config-less: index *all* `#ifdef` branches

No `compile_commands.json`, no build-log scraping. `-I` paths are inferred; `CONFIG_*` is never defined *and* never undefined, so both arms of every conditional are indexed.

This forfeits exact per-arch disambiguation, which is bought back at query time by ranking. In exchange the tool works on any tree with no setup, which was a hard requirement.

### 2.2 Full macro expansion, with a location map

Macro-generated symbols must be navigable. `SYSCALL_DEFINE3(read, ...)` produces `sys_read`, `__do_sys_read`, `__se_sys_read` and `__x64_sys_read` — none of which appear anywhere in the source text. This is the single capability that separates codenav from ctags, and it requires real expansion.

Tokens produced from a macro body carry the **invocation** offset, so navigation lands on the use site rather than inside the `#define`.

### 2.3 Every file is expanded exactly once

This is the load-bearing one, and it falls out of being config-less.

Because no `#ifdef` is ever *decided*, a header's macro environment can be defined canonically as **its own transitive include closure** rather than per translation unit. That makes it TU-independent, so each file is expanded once — not once per includer.

Measured: per-TU header expansion costs **623–1006×** on the kernel. Closures cost 83 MB and 0.5 s.

This also **removes the need for a signature-keyed header fragment cache entirely**, the usual way per-TU indexers avoid re-expanding headers: no read-set hashing, no variant counting, no hard-freeze at N variants. The plan called for building all of that; it turned out to be unnecessary.

**Accepted tradeoff:** a macro a file defines *before* including a header (the kernel's `#define pr_fmt(fmt) ...` pattern) does not reach that header. That costs string contents, not symbols.

---

## 3. Resolving the central tension

"Index all branches" and "fully expand macros" conflict: expansion needs a definite macro environment, and all-branches has none. Naive forking at every `#if` is exponential.

The resolution is a deliberately shallow slice of variability-aware parsing (the SuperC / TypeChef line of work) — roughly 20% of that machinery. A navigation index does not need soundness for every configuration the way a type checker does. A human reads a ranked list: duplicates are cheap, only false negatives really hurt.

**Presence conditions are cubes, not BDDs.** A `PcId` interns a sorted conjunction of literals. Anything that will not reduce to a cube (notably `||`) collapses to TOP with an `approx` flag. The only logical query needed is contradiction detection, which on sorted cubes is a linear merge-scan.

**Branches are concatenated, not forked.** All arms of every conditional are emitted sequentially into one token stream, each token tagged with its presence condition. At top level, struct-member and enum-member scope, concatenated mutually-exclusive alternates remain grammatical and tree-sitter handles them. Cost is O(file), not exponential.

This was gated on measurement before any indexing code was written:

| Kernel conditionals (68,320 non-guard) | |
|---|---|
| Unbalanced delimiter delta | 297 (0.43%) |
| Inside parens/brackets | 658 (0.96%) |
| **Problematic** | **942 (1.38%)** vs a 5% gate |

And on quality, measured by **absolute** error-node count (per-MiB misleads, since expanded output is 0.71× raw):

| Configuration | Error nodes |
|---|---|
| Raw source (baseline) | 8,456 |
| All branches concatenated, no macros | **8,260** (−2%) |
| + correctly scoped macros | 10,339 (+22%) |

Concatenating every `#ifdef` branch produces *fewer* absolute parse errors than parsing raw source. The core bet is confirmed on quality, not just structure.

---

## 4. Pipeline

```
P0  discover        walk the tree, hash every file
P1  scan            lex; collect #define / #include / import
P1b resolve         infer roots → include graph → per-file closures
P2  extract         expand macros → render → tree-sitter → symbols
P3  write           memory-mapped index
```

**Where the time goes** (kernel, CPU): parse 77%, extract 16%, expand 4%, lex 2%, render 1%.

tree-sitter dominates and is irreducible without replacing the parser — the slowest 1% of files account for only 11% of parse time, so there is no pathological tail to cap. Expansion, which the original plan flagged as the risk, turned out ~50× cheaper than parsing.

### Attributes are dropped before parsing

tree-sitter-c cannot parse `T * __attribute__((...)) name(...)` — an attribute between the `*` and the declarator loses the entire function. The kernel writes exactly this whenever `__must_check` decorates a pointer-returning function, so `ERR_PTR` and `ERR_CAST` had **no definition in the index at all**, while their non-pointer neighbours in the same header resolved fine.

Worse, the damage was not just absence. With the signature unparseable, error recovery turned call sites inside those bodies into symbols, so `def ERR_PTR` confidently answered with `return ERR_PTR(-ENOSYS);` in a GPIO header — a plausible-looking result that was pure noise.

Attributes carry nothing anyone navigates to, so the whole `__attribute__((...))` run is skipped at render time. An unbalanced one is emitted verbatim rather than swallowing the rest of the file. This removed 4.9% of symbols and 4.8% of occurrences — all of it attribute-argument noise — and cut build time from 122 s to 110 s.

### `__ASSEMBLY__` is the one conditional that gets decided

`CONFIG_*` is deliberately left undecided — that is the whole config-less premise. `__ASSEMBLY__` is different: it has a single correct value for every file codenav reads, because codenav parses everything as C and only `.S` files ever see it defined.

Leaving it undecided is actively harmful, because those arms are not C. `include/linux/annotate.h` defines `.macro ANNOTATE type` in its `#else /* __ASSEMBLY__ */` branch, and that assembler syntax leaked into C expansions as `ANNOTATE type= 8(1b)`. In `instrumented.h` the resulting error node swallowed everything after line 59 — eight functions, including `instrument_atomic_read`, simply absent from the index.

`PcStore::for_c` marks `__ASSEMBLY__` and `__ASSEMBLER__` false, so any definition guarded by them contradicts and is never selected. A plain `PcStore` still leaves them undecided; the decision belongs to reading-as-C, not to the store.

The same file also needed `asm __inline` handled — the kernel's `asm_inline`, another construct tree-sitter-c rejects and another pure compiler hint, dropped alongside `__attribute__`.

### `__builtin_offsetof` loses only its type keyword

`__builtin_offsetof(struct T, m)` takes a **type** as its first argument, which tree-sitter-c cannot parse in expression position — and the resulting error node spans the entire enclosing function. This is not niche: `offsetof` expands to it and `container_of` expands to `offsetof`, so it reaches a large share of kernel functions.

Dropping just the `struct`/`union`/`enum` keyword leaves an ordinary call, which parses, and keeps *both* the type name and the member navigable — better than dropping the construct. Measured back to back, it also costs slightly less CPU than leaving the errors in place (768 s vs 814 s), since error recovery is not free.

### A macro can shadow the variable it guards

Not everything here is fixable by rendering. `include/linux/buffer_head.h` declares `extern int buffer_heads_over_limit;` under `#ifdef CONFIG_BLOCK` and `#define buffer_heads_over_limit 0` under the `#else`. Being config-less, both arms live — so the macro rewrites the variable's own definition in `fs/buffer.c` into `int 0;`, which does not parse.

This is the config-less tradeoff showing its teeth rather than a bug in a pass, and it is why `fs/buffer.c` still reports an error node. Deciding it would mean deciding `CONFIG_BLOCK`, which is exactly what the design refuses to do.

### Finding these without guessing

`codenav diagnose <file>` re-expands one file exactly as the build does and reports each tree-sitter error at its **original source line**, with `--dump` for the expanded text. This class of bug is invisible from a query — the symbol is simply not there — and before the tool the only method was to guess a construct and rebuild for two minutes. It found the `__ASSEMBLY__` leak in one run.

### Root inference (why no `-I` flags are needed)

Every `#include <...>` target is matched against the file tree by path suffix, and the directory prefixes that would make it resolve are voted on. On the kernel this recovers `include/`, `include/uapi/`, `arch/x86/include/`, `arch/powerpc/include/` and others in 0.03 s, resolving **96.7% of include uses**.

Resolution prefers the root sharing the most path with the including file, then the shallower one. Without that, `tools/include/nolibc` — a legitimate inferred root — would swallow a bare `#include <types.h>` from `fs/` and drag userspace headers into kernel closures.

**Include guards are honoured structurally, by file identity.** With all branches taken, a guard's own `#ifndef` suppresses nothing, so re-inclusion has to be stopped by identity rather than by evaluating the conditional.

---

## 5. On-disk index

Write-once, sorted and packed, memory-mapped, never fully loaded. The full dataset is known at write time, so a B-tree would buy nothing.

| File | Contents |
|---|---|
| `names.fst`, `names.bin/.idx` | name → id, and the reverse table |
| `paths.fst`, `paths.bin/.idx` | path → FileId, and the reverse |
| `symbols.bin`, `occs.bin` | fixed-width 32-byte records |
| `sym_by_name`, `occ_by_name` | postings, name → records |
| `callers`, `calls` | call graph, both directions |
| `sym_by_file`, `occ_by_file` | per-file runs |
| `closures.bin/.idx` | per-file include closure (roaring bitmaps) |
| `pcs.bin/.idx` | presence-condition cubes |
| `roots.txt` | inferred include roots |

Those live in a **generation directory**. `.codenav/` itself holds:

| Entry | Contents |
|---|---|
| `CURRENT` | names the live generation: `main-N`, and `overlay-M` if there is one |
| `main-N/` | a full index, as above |
| `overlay-M/` | files re-indexed since `main-N` was built, plus `tombstones.bin` |
| `manifest.tsv` | per-file hashes for incremental |
| `lock` | held for the whole of a build |
| `last-build.log` | output of the last build the watcher ran |

Nothing is rewritten in place. A build writes a fresh generation, publishes it by renaming a new `CURRENT` over the old one, and then deletes the generations it replaced. A server that mapped the old generation keeps a consistent view — an unlinked file stays valid while mapped — and reopens when it sees `CURRENT` change, which it checks with one `stat` per request (2 µs).

**Fixed-width records** mean an id is an array index, so lookup is O(1). **Postings are CSR** — an offsets array indexed by key plus one flat values array: two loads and a slice, no hashing, no per-key allocation. **Occurrences are sorted by `(file, offset)`**, so a file's occurrences form one contiguous run, which is what makes "what is at this cursor position" a single seek.

**Every occurrence stores its enclosing `SymbolId`.** That one field makes call hierarchy a single seek in *both* directions, with no query-time graph traversal.

Kernel index: ~4.8 GB, of which `occs.bin` is 2.9 GB (95M records).

### Result caps have to reach the postings

A capped query must not first build the uncapped answer. Postings are exposed as a borrowed view over the mapping, so the count is the run length and only the first N records are ever decoded. `refs file` matches 1,998,373 occurrences in the kernel; materialising them — each with a heap-allocated enclosing-symbol name — to return 2,000 cost 170 ms, against 3.1 ms once the cap reaches the postings.

Tombstones complicate the count, and the obvious handling is a trap. Falling back to "scan every posting and filter" whenever an overlay exists makes the slow path the *normal* path, because an overlay exists after any edit — `refs file` went back to 33 ms and 3 GB resident. Instead the dead count comes from walking the tombstoned files' own occurrence runs, which is a few thousand records rather than two million.

### Line and column are stored, not computed

Both records had spare bytes — `SymbolRec` 6, `OccRec` 7 — so a `u32` line and `u16` column fit without growing the index at all (4,798 MiB before and after). They are resolved once at write time, reading only files that actually carry a record, which keeps the incremental path from touching the whole tree.

This removed the line-table cache entirely. That cache had been the second bottleneck: bounded at 512 files and cleared wholesale on overflow, against `callers` result sets spanning thousands of distinct files, so it re-read nearly all of them on every request and never warmed up.

Warm server-path latency on the kernel:

| Query | Matches | Before | After |
|---|---|---|---|
| `refs file` | 1,998,373 | 170 ms | **3.1 ms** |
| `refs dev` | 889,138 | 630 ms | **3.2 ms** |
| `refs inode` | 90,694 | 8.9 ms | **3.3 ms** |
| `callers kfree` | 27,900 | 28 ms | **4.0 ms** |
| `def vfs_read` | 2 | 0.05 ms | 0.02 ms |

The CLI, which formats every result rather than a window, improved for the same reason: `refs inode` 1,450 ms to 490 ms, `callers kfree` 543 ms to 60 ms.

What is left at ~3 ms is JSON serialisation of 2,000 results, not lookup — it scales with the cap, not with how many matches exist. Worth knowing before optimising further: storing positions did *not* move the warm numbers, because the cache fix had already absorbed that cost. It paid off in cold queries, in the CLI, and in deleting a whole cache.

---

## 6. Ranking

With all branches indexed and no build config, a name legitimately has many definitions. Resolution happens at index time; ranking picks among the survivors.

| Signal | Weight |
|---|---|
| Defined in the querying file | +18 |
| Reachable through its include closure (definitions only) | +10 |
| Cursor proximity (same file, decaying with distance) | +10 → +1 |
| Kind (class/function/macro 6, field 2, variable 1, param 0) | +6 → 0 |
| Under the canonical include root | +5 / +2 |
| Arch match / mismatch | +5 / −5 |
| Word-size match on `_32`/`_64` files | +2 / −2 |
| Path proximity | up to +6 |
| Definition over declaration | +6 |
| Looks like test code | −4 |
| Contradictory presence condition | −6 |

Two rules matter more than the numbers:

- **A contradictory guard is demoted, never removed.** You may be reading the other arm of the `#ifdef` — which is precisely the case the all-branches model exists to serve.
- **Never silently pick when the top two are close.** A clear winner is returned alone so the editor jumps; otherwise the tied group is returned.

Own-file at +18 deliberately beats anything a header can score (closure 10 + canonical root 5): `pr_fmt` has 2,423 definition sites, and a file that overrides it must get its own.

### The arch hint has to accept the names people use

`uname -m` prints `x86_64`; the kernel directory is `arch/x86`. Comparing the two raw meant the *mismatch* branch fired, so selecting `x86_64` actively demoted every file it was meant to promote — `raw_copy_to_user` went from 30 to 20 and nearly lost to `asm-generic`. Arch names are now canonicalised (`x86_64`/`amd64`/`i686` to `x86`, `aarch64` to `arm64`, and so on).

An arch on its own is also context. Both the CLI and the server previously required a file before building any context at all, so `--arch` was accepted and then silently ignored.

Finally, `arch/x86` ships both `uaccess_32.h` and `uaccess_64.h`, guarded by `CONFIG_X86_32` — genuinely undecided, so the two tie and the editor offers both. When the caller says `x86_64` rather than plain `x86` they have said which one they mean, so a deliberately weak ±2 separates them. It breaks a tie; it does not outvote the include graph.

### The closure bonus applies to definitions only

A `.c` file is never in anyone's include closure — only headers are includable. So granting the reachability bonus to declarations meant a prototype in a header you include *structurally outranked the body that defines it*: `folio_unlock` scored 22 in `pagemap.h` against 15 in `mm/filemap.c`.

This only showed up through the editor. The CLI, queried without `--from`, ranked both correctly, so the raw `is_def` weight looked sufficient — and every query the editor sends carries context. Measured on 400 real kernel functions: **0 of 400** resolved to a definition with context before the fix, **396 of 400** after (the other 4 do not exist in this tree).

The signal is meant to pick *which* definition you mean, not to promote a prototype over one.

---

## 7. Incremental updates

A file's symbols depend on exactly two things: its own text, and the macros visible in its closure. Headers are indexed as *their own files*, never inlined into including TUs — so a body edit, **even to a header**, dirties that file alone.

Only two kinds of change escape a file:

- its **macro exports** changed → anything whose closure contains it may expand differently;
- its **includes** changed → closures shift downstream.

`manifest.tsv` records a content hash, an export signature and an edge hash per file. The planner falls back to a full rebuild when exports change, edges change, files are added or removed (FileIds shift), or the blast radius exceeds 25%.

To re-expand a dirty file, only its **closure** needs rescanning — ~1,020 files on the kernel rather than 63,779.

Results go to a small **overlay** index with the superseded FileIds tombstoned. Queries read both shards and skip tombstoned results from main, so a one-file edit never rewrites the 4.8 GB main index. `SymbolId`s are namespaced by offsetting overlay ids past the end of main. Compaction is a full rebuild, triggered past 15% of the tree.

### Finding the edit cost more than applying it

The manifest also records each file's **mtime and size**, and a rescan reads a file's bytes only when its stat differs. Without that, discovering which single file changed meant reading and hashing the whole 1.36 GiB tree — 1.2 s of a 2.15 s update, against 0.17 s of work actually proportional to the edit. The remaining fixed cost is the directory walk and rebuilding the 64,201-entry `FileRegistry`.

This trusts stat the way git and ninja do: a file rewritten with its mtime and size preserved is missed. That requires a deliberate `touch -r`; a zero mtime always forces a read.

| | Kernel | Python stdlib |
|---|---|---|
| Nothing changed | **0.52 s** | 0.03 s |
| One file edited | **1.27 s** | **0.07 s** |
| Full rebuild | ~116 s | 1.3 s |

Kernel rows are hyperfine medians editing `fs/read_write.c`, whose large include closure makes it a slow case; an earlier measurement of a different file gave 0.83 s. Publishing each update as a fresh generation (§5) costs ~23 ms over the old in-place overlay write, measured A/B on a 10,000-file kernel subset.

---

## 8. Multi-language

Language support forks in the **build only**. The registry, closures, index schema, ranking and queries are language-neutral.

Python is a much simpler shape: no preprocessor means no macro table, no presence conditions, no expansion and no location map — tree-sitter offsets are already source offsets. And unlike C, the import graph is *not* needed during extraction, only for ranking, so each file is parsed once and yields its symbols and imports together.

**Import resolution reuses the C include resolver unchanged**, because that resolver only ever matched path suffixes and `a.b.c` rewritten to `a/b/c` is one. `module_candidates` covers both `a/b/c.py` and the package form `a/b/c/__init__.py`; relative imports walk up from the file's own package.

---

## 9. Editor integration

Deliberately **not LSP** — bypassing slow C language servers was the point. `codenav serve` speaks line-delimited JSON over stdio; one long-lived process keeps the index mapped.

Requests carry the **word under the cursor plus the file**, not a byte offset, which sidesteps UTF-16-vs-byte conversion. Responses carry **line and column**, computed server-side, so the extension does no filesystem I/O — reading a file per result over the extension-host RPC was what made go-to-definition feel slow.

### Keeping the index current

`codenav serve` watches the tree (FSEvents, inotify or ReadDirectoryChangesW, via the `notify` crate) and runs a build after every change. It announces each one as `{"event":"indexing"}` and `{"event":"indexed","ok":…,"seconds":…,"summary":…}` — lines without an `id`, so a client that predates them drops them.

Events are **only a trigger**. Every build is an ordinary `codenav index .`, which re-stats the whole tree against the manifest, so a dropped, coalesced or misattributed event can delay an update but never lose one. That is what makes it safe to filter loosely (any `.c`/`.h`/`.py`, any directory, anything that vanished; never `.codenav/` or `.git/`) and to fold a burst into one build: wait for 200 ms of quiet, at most 2 s. Changes that land mid-build queue one more build behind it.

Reads are never changes. inotify reports opens, and a build opens every source file, so counting access events made each build trigger the next one forever. FSEvents does not report reads, so this was invisible on macOS; the Linux run of the end-to-end test caught it, and the test now asserts that the server goes quiet after a build.

The build runs as a **child process**. A full kernel rebuild peaks at 5.4 GB resident, which a long-lived server would otherwise keep; an indexer crash cannot take the server down; and its output goes to `last-build.log` rather than a pipe, so a server that exits mid-build does not kill a build that would otherwise finish and publish.

On the kernel, a saved edit to `fs/read_write.c` is queryable 1.46 s later: the 200 ms quiet period, 0.1 s to find the change, 0.74 s to re-index the file, and the rest process start and the write.

---

## 10. Things tried and rejected

Worth recording, because each cost real time:

- **Header fragment cache** (caching each header's expansion per macro environment, keyed by signature). Unnecessary once closures made expansion TU-independent.
- **Declaration-level forking** for macros with 2–4 live variants. Measured 92.8% of macro names have exactly one meaning and true ambiguity is 0.5% of lookups — it would have served almost nothing.
- **Parallel P1 pre-pass.** Worked, 3× faster, but produced 0.2% fewer symbols with every aggregate matching exactly, and the cause was never localised. Reverted: a 12 s saving on a 105 s build is not worth an unexplained index difference.
- **Expanding out-of-scope macros** when a name has one meaning tree-wide. Raised parse errors 50% — an unexpanded macro usually parses harmlessly as an identifier, a wrongly expanded one injects garbage.

---

## 11. Known gaps

- `READ_ONCE` ranks a `tools/` copy above the real one; the kernel reaches it through `asm/rwonce.h`, a **build-time generated** header that does not exist in a clean checkout.
- A macro defined before an `#include` does not reach that header (§2.3).
- Python import changes do not trigger a rebuild. Symbols stay correct — Python extraction reads only the file's own text — but ranking refreshes on the next full rebuild.
- Presence conditions are per-shard, so overlay symbols are treated as unconditional until the next compaction.
- **Extraction coverage is not 100%.** With editor context, 396 of 400 real kernel functions resolve to a definition (the other 4 do not exist in this tree); without context 380 do. The rest lose their body to tree-sitter error recovery triggered elsewhere in the file — `fs/buffer.c` still reports one error node and 48 of 54 resolve, from a macro shadowing the variable it guards (above) — a config-less consequence, not a pass to fix. The ranking weight cannot help here; the symbol is not in the index.

---

## 12. Testing

131 unit tests, plus three suites that run against real trees:

```
scripts/golden.sh      16 kernel navigation queries
scripts/golden-py.sh    8 Python queries
test-protocol.mjs      17 editor-protocol tests
```

Golden queries assert **top-N position**, not just presence — an answer you have to scroll to is not navigation.

**The dominant failure mode in this codebase is plausible-but-wrong output**, and it drove the testing approach. Bugs found only by querying real code, all of which passed unit tests and returned believable answers:

| Bug | Symptom | Cost |
|---|---|---|
| Registry stored raw paths, resolution normalized them | a relative root silently halved include resolution | 213k vs 401k edges |
| Guarded variants picked by source order | `#ifdef X / real / #else / empty` always chose the empty arm | 24% of all symbols |
| Macros never indexed at all | `#define` lines stripped before parsing, so the extractor's macro branch was dead code | 6.1M symbols missing |
| Cross-shard NameIds | overlay records resolved against main's name table | wrong names, right offsets |
| Full rebuild kept the previous overlay | stale tombstones named `FileId`s from the *old* main index and masked symbols the rebuild had just written correctly | `def pr_fmt --from fs/aio.c` lost its own definition among 2,375 others |
| Builds rewrote index files in place | a running server kept answering from its mappings — old offsets into new contents | after a rebuild, `def helper` returned nothing although `helper` was unchanged |
| Watcher counted inotify opens as changes | each build's own reads triggered the next build | Linux only: a permanent rebuild loop, ~0.5 s of CPU per cycle on the kernel |

The full-rebuild overlay bug is worth dwelling on: it fires on the *fallback* path, which is common — any header whose macro exports change forces a full rebuild — and the symptom is a query that still returns thousands of correct-looking results, just not the right one. It was found only because the golden suite pins a specific expected file, and it is why a full build publishes a main index with *no* overlay rather than carrying the old one forward.

The in-place rewrite was latent for as long as rebuilding meant restarting the server; re-indexing on every save would have made it the normal case. It produced no error, just empty answers — and reading a mapping past the end of a file that shrank faults the process.

The practices that catch these: **check absolute counts against a known baseline** after any change that could affect coverage, **verify against real code with named symbols** rather than trusting that results came back, and when a golden expectation fails, **check the source before changing the tool** — twice the tool was right and the expectation was wrong.
