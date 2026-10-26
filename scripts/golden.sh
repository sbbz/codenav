#!/usr/bin/env bash
# Golden navigation queries against a real Linux kernel index.
#
# Each case asserts that the expected file appears in the top N results, which
# is the property that actually matters: the right answer must be visible
# without scrolling. Cases are grouped by what they exercise, so a regression
# points at a mechanism rather than just a number.
#
# Usage: scripts/golden.sh [kernel-dir]

set -uo pipefail

KERNEL="${1:-$HOME/workspaces/linux}"
CN="$(cd "$(dirname "$0")/.." && pwd)/target/release/codenav"

cd "$KERNEL" || { echo "no kernel at $KERNEL"; exit 1; }
[ -d .codenav ] || { echo "no index; run: codenav index ."; exit 1; }

pass=0; fail=0

# check <description> <expected-substring> <top-n> <command...>
check() {
  local desc="$1" want="$2" topn="$3"; shift 3
  local got
  got="$("$@" 2>/dev/null | head -n "$topn")"
  if grep -qF "$want" <<<"$got"; then
    pass=$((pass+1))
    printf '  ok   %s\n' "$desc"
  else
    fail=$((fail+1))
    printf '  FAIL %s\n       want %s in top %s, got:\n' "$desc" "$want" "$topn"
    sed 's/^/         /' <<<"${got:-<empty>}"
  fi
}

echo "== arch selection =="
# `uname -m` says x86_64 while the directory is arch/x86, so comparing the two
# raw made the mismatch branch fire and demoted exactly the files it should have
# promoted. And x86 carries both uaccess_32.h and uaccess_64.h.
check "x86_64 picks the 64-bit variant"       "arch/x86/include/asm/uaccess_64.h" 1 \
  "$CN" def raw_copy_to_user --arch x86_64 --from fs/read_write.c
check "an arch alone is enough context"       "arch/x86" 1 \
  "$CN" def raw_copy_to_user --arch x86_64

echo "== assembly-only branches must not leak into C =="
# include/linux/annotate.h defines `.macro ANNOTATE type` in its __ASSEMBLY__
# arm. Indexing every branch pulled that assembler syntax into C expansions,
# which broke the parse of instrumented.h and lost every function after line 59.
check "symbol past an __ASSEMBLY__ arm"       "include/linux/instrumented.h" 1 \
  "$CN" def instrument_atomic_read
check "and the rest of that file"             "include/linux/instrumented.h" 1 \
  "$CN" def instrument_atomic_read_write

echo "== definition beats declaration =="
# The prototype in a canonical header outscored the body on the include-root
# bonus, so go-to-definition landed on the `;` instead of the function.
check "definition beats the prototype"        "fs/inode.c" 1 \
  "$CN" def inode_owner_or_capable
check "definition beats the prototype (vfs)"  "fs/read_write.c" 1 \
  "$CN" def vfs_read
# With --from, the declaration also collects the include-closure bonus while the
# defining .c file cannot — a header is includable, a .c file never is. That made
# the prototype win every query the editor sends, which is every query.
check "definition wins with --from too"       "mm/filemap.c" 1 \
  "$CN" def folio_unlock --from mm/vmscan.c
check "definition wins with --from (compat)"  "mm/folio-compat.c" 1 \
  "$CN" def unlock_page --from mm/vmscan.c
check "definition wins with --from (inode)"   "fs/inode.c" 1 \
  "$CN" def inode_owner_or_capable --from fs/namei.c

echo "== attributes must not eat the declarator =="
# tree-sitter-c cannot parse `T * __attribute__((...)) name(...)`, which is how
# the kernel writes __must_check on a pointer-returning function. Both of these
# had no definition at all while their non-pointer neighbours in the same header
# resolved fine.
check "ERR_PTR (pointer + __must_check)"      "include/linux/err.h" 1 \
  "$CN" def ERR_PTR
check "ERR_CAST (pointer + __must_check)"     "include/linux/err.h" 1 \
  "$CN" def ERR_CAST

echo "== macro-generated symbols (the ctags-impossible case) =="
# These exist only after ## pasting inside SYSCALL_DEFINE3. Note there is NO
# plain sys_read *definition* on x86: __SYSCALL_DEFINEx emits __se_/__do_/__x64_
# and sys_read survives only as a declaration in syscalls.h. Asserting the
# declaration here is deliberate — it pins that distinction.
check "sys_read -> decl in syscalls.h"        "include/linux/syscalls.h" 1 \
  "$CN" def sys_read --from fs/read_write.c
check "__do_sys_read -> SYSCALL_DEFINE3 site" "fs/read_write.c:723" 2 \
  "$CN" def __do_sys_read
check "__se_sys_read -> SYSCALL_DEFINE3 site" "fs/read_write.c:723" 2 \
  "$CN" def __se_sys_read
check "__x64_sys_read (arch-conditional)"     "fs/read_write.c:723" 2 \
  "$CN" def __x64_sys_read

echo "== macro definitions (were missing entirely) =="
# Directive lines are stripped before parsing, so macros cannot come from the
# parse tree; they are emitted from the macro table. These assert top-1, which
# also pins the ranking that keeps a driver or tools/ copy from winning.
check "min -> canonical minmax.h"        "include/linux/minmax.h"      1 \
  "$CN" def min --from fs/read_write.c
check "container_of -> canonical header" "include/linux/container_of.h" 1 \
  "$CN" def container_of --from fs/read_write.c
check "EXPORT_SYMBOL resolves"           "include/linux/export.h"      1 \
  "$CN" def EXPORT_SYMBOL --from fs/read_write.c
# pr_fmt has 2,423 definition sites. A file that overrides it must get its own;
# a file that does not must get the printk.h default.
check "pr_fmt prefers the file's own"    "fs/aio.c"                    1 \
  "$CN" def pr_fmt --from fs/aio.c
check "pr_fmt falls back to the default" "include/linux/printk.h"      1 \
  "$CN" def pr_fmt --from fs/read_write.c

echo "== ordinary definitions =="
check "vfs_read"   "fs/read_write.c"   3 "$CN" def vfs_read
check "vfs_write"  "fs/read_write.c"   3 "$CN" def vfs_write
check "do_sys_open" "fs/open.c"        3 "$CN" def do_sys_open

echo "== call hierarchy =="
check "ksys_read calls vfs_read"     "vfs_read"  40 "$CN" callees ksys_read
check "vfs_read called by ksys_read" "ksys_read" 10 "$CN" callers vfs_read

echo "== ranking by include closure =="
# The selftest defines its own sys_read. When the query comes from a file that
# cannot see it, it must not take the top slot.
check "selftest does not win with --from" "include/linux/syscalls.h" 1 \
  "$CN" def sys_read --from fs/read_write.c

echo "== prefix search =="
check "sym ksys_ finds ksys_read" "ksys_read" 40 "$CN" sym ksys_

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
