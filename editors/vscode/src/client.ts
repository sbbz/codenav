import * as cp from "child_process";
import * as readline from "readline";

export interface Loc {
  path: string;
  offset: number;
  length: number;
  /** 1-based, supplied by the server so the extension does no file I/O. */
  line: number;
  column: number;
  name?: string;
  kind?: string;
  isDefinition?: boolean;
  role?: string;
  enclosing?: string | null;
}

/** A build the server ran because files changed. */
export type ServerEvent =
  | { event: "indexing" }
  | { event: "indexed"; ok: boolean; seconds: number; summary: string };

/**
 * Talks to `codenav serve` over line-delimited JSON.
 *
 * One long-lived process keeps the index memory-mapped, so requests after the
 * first cost a few page touches rather than reopening a multi-gigabyte index.
 */
export class Client {
  private proc: cp.ChildProcess | undefined;
  private pending = new Map<number, (v: any) => void>();
  private nextId = 1;
  private starting: Promise<void> | undefined;

  constructor(
    private binary: string,
    private cwd: string,
    private onError: (msg: string) => void,
    /** Lines the server sends unprompted, such as `{"event": "indexed"}`. */
    private onEvent: (ev: ServerEvent) => void = () => {},
    private args: string[] = [],
  ) {}

  /**
   * Starts the server without waiting for a request. With watching on, it
   * indexes as soon as it starts, so the index is ready before the first jump.
   */
  start(): void {
    this.ensure().catch(() => {});
  }

  private async ensure(): Promise<void> {
    if (this.proc && !this.proc.killed) return;
    if (this.starting) return this.starting;

    this.starting = new Promise<void>((resolve, reject) => {
      const proc = cp.spawn(this.binary, ["serve", ...this.args], {
        cwd: this.cwd,
        stdio: ["pipe", "pipe", "pipe"],
      });

      proc.on("error", (e) => {
        this.proc = undefined;
        this.starting = undefined;
        this.onError(`could not start ${this.binary}: ${e.message}`);
        reject(e);
      });

      proc.stderr?.on("data", (d) => {
        const msg = String(d).trim();
        if (msg) this.onError(msg);
      });

      proc.on("exit", () => {
        // Fail every in-flight request rather than leaving callers hanging.
        for (const [, resolveOne] of this.pending) resolveOne({ results: [] });
        this.pending.clear();
        this.proc = undefined;
        this.starting = undefined;
      });

      const rl = readline.createInterface({ input: proc.stdout! });
      rl.on("line", (line) => {
        let msg: any;
        try {
          msg = JSON.parse(line);
        } catch {
          return;
        }
        if (msg.id === undefined && typeof msg.event === "string") {
          this.onEvent(msg as ServerEvent);
          return;
        }
        const done = this.pending.get(msg.id);
        if (done) {
          this.pending.delete(msg.id);
          done(msg);
        }
      });

      this.proc = proc;
      resolve();
    });

    return this.starting;
  }

  async request(op: string, params: Record<string, unknown>): Promise<Loc[]> {
    try {
      await this.ensure();
    } catch {
      return [];
    }
    if (!this.proc?.stdin) return [];

    const id = this.nextId++;
    const reply = new Promise<any>((resolve) => {
      this.pending.set(id, resolve);
      // Don't let a wedged server freeze the editor.
      setTimeout(() => {
        if (this.pending.delete(id)) resolve({ results: [] });
      }, 15000);
    });

    this.proc.stdin.write(JSON.stringify({ id, op, ...params }) + "\n");
    const msg = await reply;
    if (msg.error) {
      this.onError(String(msg.error));
      return [];
    }
    return (msg.results ?? []) as Loc[];
  }

  dispose(): void {
    this.proc?.kill();
    this.proc = undefined;
  }
}
