"use strict";

/**
 * "Ibis Assistant" activity-bar view: memory kinds -> typed entries via MCP
 * `memory_search`, free-text semantic search, and a read-only detail editor
 * filled from MCP `memory_expand` (fallback: the search summary).
 */

const vscode = require("vscode");

const MEMORY_KINDS = [
  { kind: "decision", icon: "law" },
  { kind: "fix", icon: "wrench" },
  { kind: "rule", icon: "checklist" },
  { kind: "constraint", icon: "lock" },
  { kind: "gotcha", icon: "warning" },
  { kind: "design", icon: "lightbulb" },
  { kind: "fact", icon: "info" },
  { kind: "ask", icon: "question" },
];

const SEARCH_LIMIT = 50;

/**
 * @param {string} text
 * @returns {Array<Record<string, unknown>>}
 */
function parseMemoryArray(text) {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error(`unexpected memory_search output: ${String(text).slice(0, 120)}`);
  }
  if (parsed && Array.isArray(parsed.results)) {
    parsed = parsed.results;
  }
  if (!Array.isArray(parsed)) {
    throw new Error("unexpected memory_search payload (expected an array)");
  }
  return parsed;
}

/**
 * @param {Record<string, any>} memory
 * @returns {vscode.MarkdownString}
 */
function memoryTooltip(memory) {
  const tooltip = new vscode.MarkdownString();
  tooltip.isTrusted = false;
  tooltip.appendMarkdown(
    [
      `**${memory.title || memory.id}**`,
      "",
      String(memory.content || "").slice(0, 400),
      "",
      `_${memory.kind || "memory"}${memory.status ? ` · ${memory.status}` : ""}${
        memory.visibility ? ` · ${memory.visibility}` : ""
      }_`,
    ].join("\n"),
  );
  return tooltip;
}

/**
 * @param {string} label
 * @param {{ collapsible?: vscode.TreeItemCollapsibleState, icon?: string, contextValue?: string,
 *           command?: vscode.Command, description?: string, tooltip?: vscode.MarkdownString }} [opts]
 */
function treeItem(label, opts = {}) {
  const item = new vscode.TreeItem(label, opts.collapsible ?? vscode.TreeItemCollapsibleState.None);
  if (opts.icon) {
    item.iconPath = new vscode.ThemeIcon(opts.icon);
  }
  if (opts.contextValue) {
    item.contextValue = opts.contextValue;
  }
  if (opts.command) {
    item.command = opts.command;
  }
  if (opts.description !== undefined) {
    item.description = opts.description;
  }
  if (opts.tooltip !== undefined) {
    item.tooltip = opts.tooltip;
  }
  return item;
}

/**
 * Shared tree-node wrapper: keeps plain data on the item so refresh logic can
 * re-derive everything from settings without caching stale entries.
 */
class MemoryNode {
  constructor(label, opts, data) {
    this.item = treeItem(label, opts);
    this.data = data || {};
  }
}

class MemoryTreeDataProvider {
  /**
   * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
   *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
   *           logLine: (message: string) => void }} api
   * @param {Record<string, any>} host ide-host exports (callMcpTool, ...)
   */
  constructor(api, host) {
    this.api = api;
    this.host = host;
    this.searchQuery = "";
    this._onDidChangeTreeData = new vscode.EventEmitter();
    this.onDidChangeTreeData = this._onDidChangeTreeData.event;
  }

  refresh() {
    this._onDidChangeTreeData.fire();
  }

  /**
   * @param {string} query
   */
  async search(query) {
    this.searchQuery = query;
    this._onDidChangeTreeData.fire();
  }

  /**
   * @param {MemoryNode | undefined} element
   * @returns {Promise<MemoryNode[]>}
   */
  async getChildren(element) {
    if (!element) {
      const roots = MEMORY_KINDS.map(
        (k) =>
          new MemoryNode(
            k.kind,
            {
              collapsible: vscode.TreeItemCollapsibleState.Collapsed,
              icon: k.icon,
              contextValue: "ibisMemoryKind",
            },
            { type: "kind", kind: k.kind, icon: k.icon },
          ),
      );
      if (this.searchQuery) {
        roots.push(
          new MemoryNode(
            `search: "${this.searchQuery}"`,
            {
              collapsible: vscode.TreeItemCollapsibleState.Collapsed,
              icon: "search",
              contextValue: "ibisMemorySearch",
              description: "search results",
            },
            { type: "search", query: this.searchQuery },
          ),
        );
      }
      return roots;
    }

    const { type, kind, icon, query } = element.data;
    if (type === "kind" || type === "search") {
      try {
        const entries = await this.fetchEntries(type === "kind" ? kind : query, type === "kind" ? kind : "");
        if (entries.length === 0) {
          return [
            new MemoryNode("no memories found", {
              icon: "circle-slash",
              description: type === "kind" ? `no ${kind} memories` : "no matches",
            }),
          ];
        }
        return entries.map((memory) => this.entryNode(memory, icon));
      } catch (err) {
        const message = err && err.message ? err.message : String(err);
        this.api.logLine(`memory tree fetch failed: ${message}`);
        return [new MemoryNode(`error: ${message}`, { icon: "error" })];
      }
    }
    return [];
  }

  /**
   * MCP memory_search; kinds browse with the kind as semantic seed, search
   * uses the free-text query.
   * @param {string} query
   * @param {string} kind
   * @returns {Promise<Array<Record<string, any>>>}
   */
  async fetchEntries(query, kind) {
    const cfg = this.api.getConfig();
    const backend = await this.api.resolveBackend(this.host, cfg);
    const args = { query, limit: SEARCH_LIMIT };
    if (kind) {
      args.kind = kind;
    }
    const text = await this.host.callMcpTool(backend, "memory_search", args);
    return parseMemoryArray(text);
  }

  /**
   * @param {Record<string, any>} memory
   * @param {string} [kindIcon]
   */
  entryNode(memory, kindIcon) {
    const kindMeta = MEMORY_KINDS.find((k) => k.kind === memory.kind);
    const description = [
      memory.status && memory.status !== "current" ? memory.status : "",
      memory.visibility === "personal" ? "personal" : "",
      Array.isArray(memory.tags) && memory.tags.length ? memory.tags.slice(0, 3).join(", ") : "",
    ]
      .filter(Boolean)
      .join(" · ");
    return new MemoryNode(
      memory.title || memory.id,
      {
        icon: kindMeta ? kindMeta.icon : kindIcon || "bookmark",
        contextValue: "ibisMemoryEntry",
        description,
        tooltip: memoryTooltip(memory),
        command: {
          command: "ibis.memory.openDetail",
          title: "Open memory detail",
          arguments: [memory],
        },
      },
      { type: "entry", memory },
    );
  }

  /**
   * @param {MemoryNode} element
   * @returns {vscode.TreeItem}
   */
  getTreeItem(element) {
    return element.item;
  }
}

/**
 * Read-only virtual document for memory details (scheme `ibis-memory`); the
 * payload rides in the Uri query so reopening the same entry re-renders.
 */
function registerMemoryDetailProvider(context) {
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider("ibis-memory", {
      provideTextDocumentContent(uri) {
        let detail = {};
        try {
          detail = JSON.parse(uri.query || "{}");
        } catch {
          return "# Ibis memory\n\n(unreadable detail payload)";
        }
        return renderMemoryMarkdown(detail);
      },
    }),
  );
}

/**
 * @param {Record<string, any>} detail
 * @returns {string}
 */
function renderMemoryMarkdown(detail) {
  const meta = [
    `**Kind:** ${detail.kind || "?"}`,
    `**Status:** ${detail.status || "current"}`,
    `**Visibility:** ${detail.visibility || "team"}`,
    detail.owner ? `**Owner:** ${detail.owner}` : "",
    Array.isArray(detail.tags) && detail.tags.length ? `**Tags:** ${detail.tags.join(", ")}` : "",
    detail.confidence ? `**Confidence:** ${detail.confidence}` : "",
    detail.updated_at ? `**Updated:** ${detail.updated_at}` : "",
    detail.store === "git" ? `**Store:** git claim (repo is source of truth)` : "",
  ].filter(Boolean);

  const chain = Array.isArray(detail.supersedes_chain) ? detail.supersedes_chain : [];
  const extra = [
    chain.length ? `\n_Supersedes chain: ${chain.length} earlier version(s)._` : "",
    Array.isArray(detail.derived_from) && detail.derived_from.length
      ? `\n_Derived from: ${detail.derived_from.join(", ")}._`
      : "",
  ].filter(Boolean);

  return [
    `# ${detail.title || detail.id || "Memory"}`,
    "",
    meta.join(" · "),
    ...extra,
    "",
    "---",
    "",
    String(detail.content || "(no content)"),
    "",
    "",
  ].join("\n");
}

/**
 * Open the detail editor for a memory entry (search summary, enriched with
 * `memory_expand` when the runtime can expand the id).
 * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
 *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
 *           logLine: (message: string) => void }} api
 * @param {Record<string, any>} host
 * @param {Record<string, any>} entry
 */
async function openMemoryDetail(api, host, entry) {
  let detail = { ...entry };
  try {
    const backend = await api.resolveBackend(host, api.getConfig());
    const text = await host.callMcpTool(backend, "memory_expand", { id: entry.id });
    const expanded = JSON.parse(text);
    if (expanded && expanded.memory) {
      detail = { ...detail, ...expanded.memory };
    }
  } catch (err) {
    api.logLine(
      `memory_expand unavailable (${err && err.message ? err.message : err}); showing search summary`,
    );
  }
  const safeTitle = String(detail.title || detail.id || "memory")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 60) || "memory";
  const uri = vscode.Uri.from({
    scheme: "ibis-memory",
    path: `/memory/${detail.kind || "memory"}/${safeTitle}.md`,
    query: JSON.stringify(detail),
  });
  await vscode.window.showTextDocument(uri, { preview: true });
}

/**
 * Wire the activity-bar view, its commands, and the detail provider.
 * @param {vscode.ExtensionContext} context
 * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
 *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
 *           logLine: (message: string) => void }} api
 * @param {Record<string, any>} host
 */
function registerMemoryTree(context, api, host) {
  const provider = new MemoryTreeDataProvider(api, host);
  const treeView = vscode.window.createTreeView("ibis.memoryExplorer", {
    treeDataProvider: provider,
    showCollapseAll: true,
  });
  context.subscriptions.push(
    treeView,
    vscode.commands.registerCommand("ibis.memory.refresh", () => provider.refresh()),
    vscode.commands.registerCommand("ibis.memory.search", async () => {
      const query = await vscode.window.showInputBox({
        title: "Ibis Assistant: Search Memory",
        prompt: "Semantic query over typed memory",
        placeHolder: "e.g. why we vendor ide-host at package time",
      });
      if (!query || !query.trim()) {
        return;
      }
      await provider.search(query.trim());
    }),
    vscode.commands.registerCommand(
      "ibis.memory.openDetail",
      (entry) => openMemoryDetail(api, host, entry),
    ),
  );
  registerMemoryDetailProvider(context);
}

module.exports = { registerMemoryTree, MEMORY_KINDS };