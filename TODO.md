# TODO

## Re-index only the files that use a changed macro

Changing a `#define` in a header re-indexes every file whose include closure
contains that header. For the ~5% of kernel headers seen by more than 2,500
files that means a full rebuild — yet most of their macros are used by a
handful of files:

| Macro (header) | Files in the closure | Files that use it |
|---|---|---|
| `kcpustat_cpu` (`kernel_stat.h`) | ~22,000 | 6 |
| `kstat_cpu` (`kernel_stat.h`) | ~22,000 | 1 |
| `SEQ_SKIP` (`seq_file.h`) | >2,500 | 11 |
| `SEQ_START_TOKEN` (`seq_file.h`) | >2,500 | 72 |
| `DEFINE_SHOW_ATTRIBUTE` (`seq_file.h`) | >2,500 | 360 |
| `container_of` (worst case) | — | 12,038 |

"Uses" means the file names the macro, or names a macro whose definition
does, transitively. Grep estimates that miss multi-line definitions, so
slightly low.

Plan:

1. **Record each file's read set.** While expanding a file, note every
   identifier looked up in the macro table, including misses (a newly added
   `#define foo` must reach files that use `foo` as a function). Store hashes,
   not names. Measure the size on the kernel before committing to it.
2. **Diff the header's macros.** The per-definition export hashes already say
   which names were added, removed or changed.
3. **Narrow the dirty set** to files in the closure whose read set contains a
   changed name.
4. **Test it.** Extend `tests/incremental_equivalence.rs` with a macro used
   only through another macro, and a new macro that shadows a function name.

Expected: a `kcpustat_cpu` edit goes from a ~4 min full rebuild to ~0.4 s.
Macros like `container_of` still dirty half the tree; the floor there is the
re-extraction itself.

## Smaller

- **Large overlays.** When an update genuinely dirties more than 2,500 files,
  it falls back to a full rebuild, because every later update copies the
  overlay forward (a 22k-file overlay made later saves take 14.4 s). Either
  stack two overlay layers, or compact into a new main generation.
- **Shrink the directive cache.** It adds ~700 MB to the kernel index
  (4.5 → 5.2 GB); variable-length integers should roughly halve it.
- **Narrow the closure recompute.** Adding or removing a file or include
  recomputes every closure (~1.1 s of the ~1.5 s); only ancestors of the
  changed edges can change.
