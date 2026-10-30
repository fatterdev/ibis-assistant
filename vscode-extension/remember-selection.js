"use strict";

/**
 * Editor/Explorer capture with provenance: MCP `memory_add` entries bound to
 * the source file via `source_path` + `source_hash` (sha256:<hex> of the raw
 * file bytes — same hash scheme as HEAD ingest, so git-driven staleness can
 * invalidate these memories when the file changes). The selection variant
 * records the file:line range in the content; the file variant covers 1..N.
 */

const { createHash } = require("node:crypto");
const path = require("node:path");
const vscode = require("vscode");

/**
 * @param {vscode.Uri} uri
 * @returns {string}
 */
function toRelativePath(uri) {
  const folder = vscode.workspace.getWorkspaceFolder(uri);
  const rel = folder ? path.relative(folder.uri.fsPath, uri.fsPath) : uri.fsPath;
  return rel.split(path.sep).join("/");
}

/**
 * @param {vscode.Uri} uri
 * @returns {Promise<{ hash: string, lines: number, bytes: Uint8Array }>}
 */
async function sourceFileProvenance(uri) {
  const bytes = await vscode.workspace.fs.readFile(uri);
  const hash = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
  const lines = bytes.toString("utf8").split("\n").length;
  return { hash, lines, bytes };
}

/**
 * Kind quick pick honoring ibis.memory.defaultKind ("ask" => interactive).
 * @param {{ kinds: Array<{ label: string, description: string }>,
 *           getConfig: () => vscode.WorkspaceConfiguration }} api
 * @param {string} title
 * @returns {Promise<string | undefined>}
 */
async function pickKind(api, title) {
  const cfg = api.getConfig();
  let kind = cfg.get("memory.defaultKind", "ask");
  if (kind === "ask") {
    const pick = await vscode.window.showQuickPick(api.kinds, {
      placeHolder: "Memory kind",
      title,
    });
    if (!pick) {
      return undefined;
    }
    kind = pick.label;
  }
  return kind;
}

/**
 * Shared title prompt.
 * @param {string} kind
 * @param {string} title
 * @returns {Promise<string | undefined>}
 */
async function askTitle(kind, title) {
  const value = await vscode.window.showInputBox({
    title,
    prompt: `Short title (${kind})`,
    placeHolder: "e.g. Prefer Streamable HTTP for MCP",
    ignoreFocusOut: true,
  });
  if (value === undefined || !value.trim()) {
    return undefined;
  }
  return value.trim();
}

/**
 * Editor context: remember the active selection with file+line provenance.
 * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
 *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
 *           logLine: (message: string) => void,
 *           kinds: Array<{ label: string, description: string }> }} api
 * @param {Record<string, any>} host
 */
async function rememberSelection(api, host) {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.selection.isEmpty) {
    vscode.window.showInformationMessage(
      "Ibis Remember Selection needs a non-empty selection in the active editor.",
    );
    return;
  }
  const title = "Ibis Assistant: Remember Selection";
  const cfg = api.getConfig();
  const kind = await pickKind(api, title);
  if (!kind) {
    return;
  }
  const heading = await askTitle(kind, title);
  if (!heading) {
    return;
  }

  const selection = editor.selection;
  const rel = toRelativePath(editor.document.uri);
  const startLine = selection.start.line + 1;
  const endLine = selection.end.line + 1;
  const content = `${editor.document
    .getText(selection)
    .trimEnd()}\n\nSource: ${rel}:${startLine}-${endLine}`;
  const visibility = cfg.get("memory.defaultVisibility", "team");
  const { hash } = await sourceFileProvenance(editor.document.uri);

  const backend = await api.resolveBackend(host, cfg);
  const text = await host.callMcpTool(backend, "memory_add", {
    kind,
    title: heading,
    content,
    visibility,
    source_path: rel,
    source_hash: hash,
  });
  api.logLine(`memory_add (selection ${rel}:${startLine}-${endLine}, ${kind}, ${visibility}): ${text}`);
  vscode.window.showInformationMessage(
    `Ibis Remembered (${kind}, ${visibility}) from ${rel}:${startLine}-${endLine}: ${text}`,
  );
}

/**
 * Explorer/editor context: remember a fact about a whole file with
 * file-level provenance (content is user-provided, not the file body).
 * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
 *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
 *           logLine: (message: string) => void,
 *           kinds: Array<{ label: string, description: string }> }} api
 * @param {Record<string, any>} host
 * @param {vscode.Uri} [uriArg] resource passed by the Explorer context menu
 */
async function rememberFile(api, host, uriArg) {
  let fileUri = uriArg;
  if (!fileUri) {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      vscode.window.showInformationMessage(
        "Ibis Remember File needs an open file or an Explorer selection.",
      );
      return;
    }
    fileUri = editor.document.uri;
  }
  if (fileUri.scheme !== "file") {
    vscode.window.showInformationMessage("Ibis Remember File works on workspace files only.");
    return;
  }

  const title = "Ibis Assistant: Remember File";
  const cfg = api.getConfig();
  const kind = await pickKind(api, title);
  if (!kind) {
    return;
  }
  const heading = await askTitle(kind, title);
  if (!heading) {
    return;
  }
  const body = await vscode.window.showInputBox({
    title,
    prompt: `What should be remembered about ${path.basename(fileUri.fsPath)}?`,
    placeHolder: "Durable statement about this file (the file is linked, not copied)",
    ignoreFocusOut: true,
  });
  if (!body || !body.trim()) {
    return;
  }

  const rel = toRelativePath(fileUri);
  const { hash, lines } = await sourceFileProvenance(fileUri);
  const content = `${body.trim()}\n\nSource: ${rel}:1-${lines} (whole file)`;
  const visibility = cfg.get("memory.defaultVisibility", "team");

  const backend = await api.resolveBackend(host, cfg);
  const text = await host.callMcpTool(backend, "memory_add", {
    kind,
    title: heading,
    content,
    visibility,
    source_path: rel,
    source_hash: hash,
  });
  api.logLine(`memory_add (file ${rel}, ${kind}, ${visibility}): ${text}`);
  vscode.window.showInformationMessage(
    `Ibis Remembered (${kind}, ${visibility}) for ${rel}: ${text}`,
  );
}

/**
 * Wire both commands.
 * @param {vscode.ExtensionContext} context
 * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
 *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
 *           logLine: (message: string) => void,
 *           kinds: Array<{ label: string, description: string }> }} api
 * @param {Record<string, any>} host
 */
function registerRememberSelection(context, api, host) {
  context.subscriptions.push(
    vscode.commands.registerCommand(
      "ibis.rememberSelection",
      () =>
        rememberSelection(api, host).catch((err) =>
          vscode.window.showErrorMessage(
            `Ibis Remember Selection failed: ${err && err.message ? err.message : err}`,
          ),
        ),
    ),
    vscode.commands.registerCommand(
      "ibis.rememberFile",
      (uri) =>
        rememberFile(api, host, uri).catch((err) =>
          vscode.window.showErrorMessage(
            `Ibis Remember File failed: ${err && err.message ? err.message : err}`,
          ),
        ),
    ),
  );
}

module.exports = { registerRememberSelection };