#!/usr/bin/env bash
# Golden navigation queries against a real Python tree (the 3.12 stdlib).
#
# Mirrors scripts/golden.sh: each case asserts the expected file appears in the
# top N, because an answer you have to scroll to is not navigation.
#
# Usage: scripts/golden-py.sh [python-lib-dir]

set -uo pipefail

LIB="${1:-/tmp/pystdlib}"
CN="$(cd "$(dirname "$0")/.." && pwd)/target/release/codenav"

cd "$LIB" || { echo "no python tree at $LIB"; exit 1; }
[ -d .codenav ] || { echo "no index; run: codenav index ."; exit 1; }

pass=0; fail=0

check() {
  local desc="$1" want="$2" topn="$3"; shift 3
  local got
  got="$("$@" 2>/dev/null | head -n "$topn")"
  if grep -qF "$want" <<<"$got"; then
    pass=$((pass+1)); printf '  ok   %s\n' "$desc"
  else
    fail=$((fail+1))
    printf '  FAIL %s\n       want %s in top %s, got:\n' "$desc" "$want" "$topn"
    sed 's/^/         /' <<<"${got:-<empty>}"
  fi
}

echo "== classes =="
check "OrderedDict -> collections"   "collections/__init__.py" 1 "$CN" def OrderedDict
check "TextIOWrapper -> _pyio"       "_pyio.py"                1 "$CN" def TextIOWrapper
check "Path is a class not a var"    "class Path"              2 "$CN" def Path

echo "== functions and methods =="
check "json dumps"                   "json/__init__.py"        3 "$CN" def dumps
# A method defined in a class body must be reported as a method, not a function.
check "method kind is distinguished" "method"                  3 "$CN" def readline

echo "== call hierarchy across modules =="
# json.dumps is `return cls(...).encode(obj)` — `encode` is the call. JSONEncoder
# appears only as a default parameter value, which is correctly not a call.
check "json dumps calls encode"      "encode"                 10 \
  "$CN" callees dumps --from json/__init__.py

echo "== import-closure ranking =="
# os.walk is real Python. (os.getcwd deliberately is not: os.py does
# `from posix import *`, so it is a C builtin with no Python definition —
# a good reminder to check the source before trusting an expectation.)
check "os.walk resolves to os.py"    "os.py"                   2 "$CN" def walk --from shutil.py

echo "== prefix search =="
check "sym OrderedD"                 "OrderedDict"            10 "$CN" sym OrderedD

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
