// Exercises the same request/response path the extension uses, without VS Code.
// Verifies the protocol contract and the byte-offset -> line/column conversion
// the extension performs, which is the part most likely to be silently wrong.

import { spawn } from "child_process";
import * as readline from "readline";
import * as fs from "fs";
import * as path from "path";

const KERNEL = process.argv[2] ?? path.join(process.env.HOME, "workspaces/linux");
const BIN = path.join(process.cwd(), "../../target/release/codenav");

const proc = spawn(BIN, ["serve"], { cwd: KERNEL, stdio: ["pipe", "pipe", "inherit"] });
const rl = readline.createInterface({ input: proc.stdout });
const pending = new Map();
let nextId = 1;

rl.on("line", (line) => {
  const msg = JSON.parse(line);
  const done = pending.get(msg.id);
  if (done) {
    pending.delete(msg.id);
    done(msg);
  }
});

function request(op, params) {
  const id = nextId++;
  const p = new Promise((res) => pending.set(id, res));
  proc.stdin.write(JSON.stringify({ id, op, ...params }) + "\n");
  return p;
}

// Mirrors OffsetMap.positionAt in the extension.
function positionAt(file, offset) {
  const bytes = fs.readFileSync(path.resolve(KERNEL, file));
  const end = Math.min(offset, bytes.length);
  let line = 0;
  let lineStart = 0;
  for (let i = 0; i < end; i++) {
    if (bytes[i] === 0x0a) {
      line++;
      lineStart = i + 1;
    }
  }
  return { line: line + 1, col: end - lineStart + 1 };
}

let pass = 0;
let fail = 0;

function check(desc, cond, detail = "") {
  if (cond) {
    pass++;
    console.log(`  ok   ${desc}`);
  } else {
    fail++;
    console.log(`  FAIL ${desc}${detail ? "\n       " + detail : ""}`);
  }
}

const ping = await request("ping", {});
check("server responds to ping", ping.ok === true, JSON.stringify(ping));
check("index is loaded", ping.symbols > 1_000_000, `symbols=${ping.symbols}`);

// Go-to-definition on a macro-generated symbol, and confirm the offset the
// extension would jump to really lands on the SYSCALL_DEFINE3 line.
const def = await request("def", {
  name: "__x64_sys_read",
  file: "./fs/read_write.c",
});
const top = def.results[0];
check("definition found for __x64_sys_read", !!top, JSON.stringify(def).slice(0, 200));
if (top) {
  const pos = positionAt(top.path, top.offset);
  const text = fs.readFileSync(path.resolve(KERNEL, top.path), "utf8").split("\n")[pos.line - 1];
  check(
    `offset resolves to the SYSCALL_DEFINE3 line (${top.path}:${pos.line})`,
    text.includes("SYSCALL_DEFINE3(read"),
    `line reads: ${text?.trim().slice(0, 70)}`,
  );
}

// Call hierarchy, both directions.
const callers = await request("callers", { name: "vfs_read", file: "./fs/read_write.c" });
check(
  "incoming calls include ksys_read",
  callers.results.some((r) => r.name === "ksys_read"),
  callers.results.slice(0, 5).map((r) => r.name).join(", "),
);

const callees = await request("callees", { name: "ksys_read", file: "./fs/read_write.c" });
check(
  "outgoing calls include vfs_read",
  callees.results.some((r) => r.name === "vfs_read"),
  callees.results.map((r) => r.name).join(", "),
);

// Workspace symbols.
const syms = await request("symbols", { name: "ksys_re", limit: 10 });
check(
  "workspace symbols finds ksys_read",
  syms.results.some((r) => r.name === "ksys_read"),
  syms.results.map((r) => r.name).join(", "),
);

// Every result must carry what the extension needs to build a Location.
const shaped = [...def.results, ...callers.results, ...syms.results].every(
  (r) => typeof r.path === "string" && Number.isInteger(r.offset) && Number.isInteger(r.length),
);
check("all results carry path/offset/length", shaped);

// VS Code sends absolute paths; the CLI sends relative ones. If the server
// fails to map an absolute path it does not error — it silently drops the query
// context and with it the include-closure ranking, so assert both agree.
const relRanked = await request("def", { name: "sys_read", file: "./fs/read_write.c" });
const absRanked = await request("def", {
  name: "sys_read",
  file: path.join(KERNEL, "fs/read_write.c"),
});
check(
  "absolute and relative paths rank identically",
  JSON.stringify(relRanked.results) === JSON.stringify(absRanked.results),
  `rel top=${relRanked.results[0]?.path}  abs top=${absRanked.results[0]?.path}`,
);
check(
  "closure ranking puts an included header above an unrelated selftest",
  absRanked.results[0]?.path.includes("syscalls.h"),
  absRanked.results.slice(0, 2).map((r) => r.path).join(", "),
);

// A common word like `file` names ~235k symbols, nearly all of them parameters
// in unrelated functions. Uncapped, the editor pays a file read per result and
// go-to-definition took over ten seconds. Results must stay bounded and quick.
// Warm first: run immediately after an index build, the mmap pages are still
// being flushed and a cold timing here fails for reasons unrelated to the code.
await request("def", { name: "file", file: path.join(KERNEL, "fs/read_write.c") });
const tCommon = Date.now();
const common = await request("def", { name: "file", file: path.join(KERNEL, "fs/read_write.c") });
const commonMs = Date.now() - tCommon;
check("common symbol result count is bounded", common.results.length <= 100, `got ${common.results.length}`);
check("server supplies line and column", Number.isInteger(common.results[0]?.line) && Number.isInteger(common.results[0]?.column), "extension must not have to read files");
check("common symbol responds quickly", commonMs < 500, `took ${commonMs} ms`);
check(
  "foreign parameters are excluded",
  !common.results.some((r) => r.kind === "param" && !r.path.includes("read_write.c")),
  "a parameter from an unrelated file leaked into definitions",
);

// An unambiguous symbol must come back as exactly one result, so the editor
// jumps straight there instead of opening a peek list.
const single = await request("def", {
  name: "vfs_read",
  file: path.join(KERNEL, "fs/read_write.c"),
  line: 600,
});
check("unambiguous definition returns exactly one result", single.results.length === 1, `got ${single.results.length}`);

// Cursor line disambiguates same-file candidates: `file` inside vfs_read must
// resolve to that function's own parameter, not to 100 others.
const param = await request("def", {
  name: "file",
  file: path.join(KERNEL, "fs/read_write.c"),
  line: 556,
});
check(
  "cursor line picks the enclosing function's parameter",
  param.results.length === 1 && param.results[0].line === 554,
  `got ${param.results.length} results, first at line ${param.results[0]?.line}`,
);

// Unknown symbols must be empty, not an error.
const missing = await request("def", { name: "definitely_not_a_symbol_xyz", file: "./fs/read_write.c" });
check("unknown symbol returns empty results", Array.isArray(missing.results) && missing.results.length === 0);

console.log(`\npassed ${pass}, failed ${fail}`);
proc.kill();
process.exit(fail === 0 ? 0 : 1);
