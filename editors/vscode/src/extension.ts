import * as vscode from "vscode";
import * as path from "path";
import { Client, Loc } from "./client";

let client: Client | undefined;
let out: vscode.OutputChannel;
let status: vscode.StatusBarItem;

function root(): string {
  return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? process.cwd();
}

/** Resolves a server-reported path, which may be relative to the index root. */
function absolute(p: string): string {
  return path.isAbsolute(p) ? p : path.resolve(root(), p);
}

/**
 * The server sends 1-based line/column, so the extension never touches the
 * filesystem. Reading each result's file over the extension-host RPC was what
 * made go-to-definition slow.
 */
function toLocation(l: Loc): vscode.Location {
  const start = new vscode.Position(Math.max(0, l.line - 1), Math.max(0, l.column - 1));
  const end = start.translate(0, l.length);
  return new vscode.Location(vscode.Uri.file(absolute(l.path)), new vscode.Range(start, end));
}

function wordAt(doc: vscode.TextDocument, pos: vscode.Position): string | undefined {
  const range = doc.getWordRangeAtPosition(pos, /[A-Za-z_][A-Za-z0-9_]*/);
  return range ? doc.getText(range) : undefined;
}

function arch(): string | undefined {
  const a = vscode.workspace.getConfiguration("codenav").get<string>("arch");
  return a && a.length > 0 ? a : undefined;
}

function ctx(doc: vscode.TextDocument, pos?: vscode.Position): Record<string, unknown> {
  // The cursor line lets the server disambiguate same-file candidates, e.g.
  // which function's parameter a common name like `file` refers to.
  return { file: doc.uri.fsPath, arch: arch(), line: pos ? pos.line + 1 : undefined };
}

const KIND: Record<string, vscode.SymbolKind> = {
  function: vscode.SymbolKind.Function,
  variable: vscode.SymbolKind.Variable,
  struct: vscode.SymbolKind.Struct,
  union: vscode.SymbolKind.Struct,
  enum: vscode.SymbolKind.Enum,
  "enum-const": vscode.SymbolKind.EnumMember,
  typedef: vscode.SymbolKind.Interface,
  field: vscode.SymbolKind.Field,
  param: vscode.SymbolKind.Variable,
  macro: vscode.SymbolKind.Constant,
  class: vscode.SymbolKind.Class,
  method: vscode.SymbolKind.Method,
};

function symbolKind(k?: string): vscode.SymbolKind {
  return (k !== undefined ? KIND[k] : undefined) ?? vscode.SymbolKind.Variable;
}

/** Whether the server is mid-build, and how the last build went. */
let indexing = false;
let lastBuild: string | undefined;
let lastBuildFailed = false;

function binaryPath(): string {
  return vscode.workspace.getConfiguration("codenav").get<string>("binaryPath") || "codenav";
}

/**
 * Starts a server for the workspace. Unless auto-indexing is off, the server
 * watches the tree and rebuilds the index on every change, reporting each
 * build as an event.
 */
function startClient(): Client {
  const auto = vscode.workspace.getConfiguration("codenav").get<boolean>("autoIndex", true);
  const c = new Client(
    binaryPath(),
    root(),
    (m) => out.appendLine(m),
    (ev) => {
      if (ev.event === "indexing") {
        indexing = true;
      } else {
        indexing = false;
        lastBuild = ev.summary;
        lastBuildFailed = !ev.ok;
      }
      updateStatus();
    },
    auto ? [] : ["--no-watch"],
  );
  c.start();
  return c;
}

function restart() {
  client?.dispose();
  indexing = false;
  client = startClient();
  updateStatus();
}

export function activate(context: vscode.ExtensionContext) {
  out = vscode.window.createOutputChannel("codenav");
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  status.command = "codenav.setArch";
  client = startClient();
  context.subscriptions.push({ dispose: () => client?.dispose() }, out);
  updateStatus();
  status.show();
  context.subscriptions.push(status);

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("codenav.autoIndex") || e.affectsConfiguration("codenav.binaryPath")) {
        restart();
      }
    }),
  );

  const selector: vscode.DocumentSelector = [
    { language: "c", scheme: "file" },
    { language: "cpp", scheme: "file" },
    { language: "python", scheme: "file" },
  ];

  context.subscriptions.push(
    vscode.languages.registerDefinitionProvider(selector, {
      async provideDefinition(doc, pos) {
        const name = wordAt(doc, pos);
        if (!name || !client) return undefined;
        const hits = await client.request("def", { name, ...ctx(doc, pos) });
        // Definitions before declarations, order otherwise preserved from the
        // server, which has already ranked by include-graph reachability.
        const ranked = [...hits].sort(
          (a, b) => Number(b.isDefinition) - Number(a.isDefinition),
        );
        return ranked.map(toLocation);
      },
    }),

    vscode.languages.registerReferenceProvider(selector, {
      async provideReferences(doc, pos) {
        const name = wordAt(doc, pos);
        if (!name || !client) return [];
        const cap = vscode.workspace
          .getConfiguration("codenav")
          .get<number>("maxReferences", 5000);
        const hits = await client.request("refs", { name, ...ctx(doc, pos) });
        if (hits.length > cap) {
          out.appendLine(`${name}: ${hits.length} references, showing first ${cap}`);
        }
        return hits.slice(0, cap).map(toLocation);
      },
    }),

    vscode.languages.registerWorkspaceSymbolProvider({
      async provideWorkspaceSymbols(query) {
        if (!client || query.length === 0) return [];
        const hits = await client.request("symbols", { name: query, limit: 200 });
        return hits.map(
          (h) =>
            new vscode.SymbolInformation(h.name ?? query, symbolKind(h.kind), "", toLocation(h)),
        );
      },
    }),

    vscode.languages.registerCallHierarchyProvider(selector, new CallHierarchy()),

    vscode.commands.registerCommand("codenav.buildIndex", buildIndex),
    vscode.commands.registerCommand("codenav.setArch", setArch),
    vscode.commands.registerCommand("codenav.restart", () => {
      restart();
      vscode.window.showInformationMessage("codenav: server restarted");
    }),
  );
}

/**
 * Call hierarchy. Both directions are one index seek, because every occurrence
 * stored its enclosing symbol at index time.
 */
class CallHierarchy implements vscode.CallHierarchyProvider {
  async prepareCallHierarchy(
    doc: vscode.TextDocument,
    pos: vscode.Position,
  ): Promise<vscode.CallHierarchyItem | undefined> {
    const name = wordAt(doc, pos);
    if (!name || !client) return undefined;
    const hits = await client.request("def", { name, ...ctx(doc, pos) });
    const best = hits.find((h) => h.isDefinition) ?? hits[0];
    if (!best) return undefined;
    const loc = toLocation(best);
    return new vscode.CallHierarchyItem(
      symbolKind(best.kind),
      name,
      vscode.workspace.asRelativePath(loc.uri),
      loc.uri,
      loc.range,
      loc.range,
    );
  }

  async provideCallHierarchyIncomingCalls(
    item: vscode.CallHierarchyItem,
  ): Promise<vscode.CallHierarchyIncomingCall[]> {
    if (!client) return [];
    const hits = await client.request("callers", {
      name: item.name,
      file: item.uri.fsPath,
      arch: arch(),
    });
    return hits.map((h) => {
      const loc = toLocation(h);
      {
        const from = new vscode.CallHierarchyItem(
          symbolKind(h.kind),
          h.name ?? "?",
          vscode.workspace.asRelativePath(loc.uri),
          loc.uri,
          loc.range,
          loc.range,
        );
        return new vscode.CallHierarchyIncomingCall(from, [loc.range]);
      }
    });
  }

  async provideCallHierarchyOutgoingCalls(
    item: vscode.CallHierarchyItem,
  ): Promise<vscode.CallHierarchyOutgoingCall[]> {
    if (!client) return [];
    const hits = await client.request("callees", {
      name: item.name,
      file: item.uri.fsPath,
      arch: arch(),
    });
    return hits.map((h) => {
      const loc = toLocation(h);
      {
        const to = new vscode.CallHierarchyItem(
          symbolKind(h.kind),
          h.name ?? "?",
          vscode.workspace.asRelativePath(loc.uri),
          loc.uri,
          loc.range,
          loc.range,
        );
        return new vscode.CallHierarchyOutgoingCall(to, [loc.range]);
      }
    });
  }
}

function updateStatus() {
  const a = arch();
  const icon = indexing ? "$(sync~spin)" : lastBuildFailed ? "$(warning)" : "$(symbol-namespace)";
  const label = indexing ? "codenav: indexing" : a ? `codenav: ${a}` : "codenav";
  status.text = `${icon} ${label}`;
  const build = lastBuild ? `\nLast index build: ${lastBuild}` : "";
  status.tooltip = `codenav: click to select architecture${build}`;
}

async function setArch() {
  const picked = await vscode.window.showInputBox({
    prompt: "Preferred architecture (blank to infer from the current file)",
    value: arch() ?? "",
    placeHolder: "x86",
  });
  if (picked === undefined) return;
  await vscode.workspace
    .getConfiguration("codenav")
    .update("arch", picked, vscode.ConfigurationTarget.Workspace);
  updateStatus();
}

function buildIndex() {
  const term = vscode.window.createTerminal({ name: "codenav index", cwd: root() });
  term.show();
  // Run in a terminal rather than a progress bar: the build prints per-phase
  // timings that are worth seeing. The server picks up the result on its own,
  // and a build it is already running makes this one wait rather than clash.
  term.sendText(`${binaryPath()} index .`);
}

export function deactivate() {
  client?.dispose();
}
