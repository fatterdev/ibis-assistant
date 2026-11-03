"use strict";

/**
 * "Ibis Assistant" activity-bar view: memory kinds -> typed entries via MCP
 * `memory_list` (metadata browse, no embedding), free-text semantic search
 * via `memory_search`, and a read-only detail editor filled from MCP
 * `memory_expand` (fallback: the search summary).
 *
 * Kind browse and search honor `ibis.memory.statusFilter` (F4 §A.2):
 * `current` (default) passes `status: current` to the tools, `all` passes
 * the explicit `all` marker so superseded/archived/stale rows surface — the
 * view title carries an `all` description while the unfiltered browse is
 * active, and the title-bar filter button toggles the setting.
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
const KIND_LIMIT = 100;

/**
 * The memory-tree status filter (F4 §A.2): `current` (default) or `all`.
 * Anything unexpected degrades to `current`.
 * @param {vscode.WorkspaceConfiguration} cfg
 * @returns {"current" | "all"}
 */
function statusFilterOf(cfg) {
  return cfg.get("memory.statusFilter", "current") === "all" ? "all" : "current";
}

/**
 * Writes the status filter to the most specific scope that already set it
 * (user settings otherwise), so a workspace override can never shadow the
 * view-title toggle.
 * @param {vscode.WorkspaceConfiguration} cfg
 * @param {"current" | "all"} value
 */
async function updateStatusFilter(cfg, value) {
  const inspected = cfg.inspect("memory.statusFilter");
  let target = vscode.ConfigurationTarget.Global;
  if (inspected && inspected.workspaceFolderValue !== undefined) {
    target = vscode.ConfigurationTarget.WorkspaceFolder;
  } else if (inspected && inspected.workspaceValue !== undefined) {
    target = vscode.ConfigurationTarget.Workspace;
  }
  await cfg.update("memory.statusFilter", value, target);
}

/**
 * @param {string} query
 * @returns {string}
 */
function searchKey(query) {
  return `search:${query}`;
}

/**
 * Renders the parent-count badge: a plain number, `N+` when the fetch hit
 * the limit cap (more entries exist than loaded).
 * @param {{ n: number, capped: boolean }} count
 * @returns {string}
 */
function countLabel({ n, capped }) {
  return capped ? `${n}+` : String(n);
}

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
    /** Loaded-entry count per tree parent (kind or search): { n, capped }. */
    this.counts = new Map();
    /** Last count label rendered per parent — the convergent fire guard. */
    this.renderedCounts = new Map();
    this._onDidChangeTreeData = new vscode.EventEmitter();
    this.onDidChangeTreeData = this._onDidChangeTreeData.event;
  }

  refresh() {
    this._onDidChangeTreeData.fire();
  }

  /**
   * Re-renders one parent row only when its count label actually changed.
   * Firing unconditionally from `getChildren` would loop: an element-scoped
   * fire re-requests the children of an expanded element, whose fetch fires
   * again. Firing on label change converges (next fetch renders the same
   * label → no fire).
   * @param {MemoryNode} element
   * @param {string} key
   */
  announceCount(element, key) {
    const count = this.counts.get(key);
    const label = count ? countLabel(count) : "";
    if (this.renderedCounts.get(key) === label) {
      element.item.description = label || undefined;
      return;
    }
    this.renderedCounts.set(key, label);
    element.item.description = label || undefined;
    this._onDidChangeTreeData.fire(element);
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
      const roots = MEMORY_KINDS.map((k) => {
        const count = this.counts.get(k.kind);
        return new MemoryNode(
          k.kind,
          {
            collapsible: vscode.TreeItemCollapsibleState.Collapsed,
            icon: k.icon,
            contextValue: "ibisMemoryKind",
            description: count ? countLabel(count) : undefined,
          },
          { type: "kind", kind: k.kind, icon: k.icon },
        );
      });
      if (this.searchQuery) {
        const count = this.counts.get(searchKey(this.searchQuery));
        roots.push(
          new MemoryNode(
            `search: "${this.searchQuery}"`,
            {
              collapsible: vscode.TreeItemCollapsibleState.Collapsed,
              icon: "search",
              contextValue: "ibisMemorySearch",
              description: count ? countLabel(count) : "search results",
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
        // The tree parent carries a visible count of the loaded entries
        // (owner request 03/11): re-render the row only when the label
        // changed (see announceCount — no unconditional fire here, it loops).
        const key = type === "kind" ? kind : searchKey(query);
        const capped = entries.length >= (type === "kind" ? KIND_LIMIT : SEARCH_LIMIT);
        this.counts.set(key, { n: entries.length, capped });
        this.announceCount(element, key);
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
        const short = message
          .replace(/^unexpected memory_search output:\s*/, "")
          .replace(/\s+/g, " ")
          .slice(0, 140);
        return [new MemoryNode(`error: ${short}`, { icon: "error" })];
      }
    }
    return [];
  }

  /**
   * Counts for every kind (collapsed parents included — owner decision
   * 03/11): the root badges stay truthful after a refresh even when the
   * kind is not expanded (expanded kinds re-fetch through the fire anyway).
   * A failed kind fetch keeps its last known count.
   */
  async refreshCounts() {
    const results = await Promise.all(
      MEMORY_KINDS.map(async ({ kind }) => {
        try {
          return { kind, entries: await this.fetchEntries("", kind) };
        } catch {
          return null;
        }
      }),
    );
    for (const result of results) {
      if (!result) {
        continue;
      }
      this.counts.set(result.kind, {
        n: result.entries.length,
        capped: result.entries.length >= KIND_LIMIT,
      });
    }
  }

  /**
   * @returns {"current" | "all"}
   */
  statusFilter() {
    return statusFilterOf(this.api.getConfig());
  }

  /**
   * Kind browse uses `memory_list` (metadata, no embedding — instant);
   * free-text search keeps `memory_search` (semantic, one call). Both honor
   * the status filter: `current` passes the explicit status, `all` passes
   * the server-side `all` marker (F4 §A.2).
   * @param {string} query
   * @param {string} kind
   * @returns {Promise<Array<Record<string, any>>>}
   */
  async fetchEntries(query, kind) {
    const cfg = this.api.getConfig();
    const status = statusFilterOf(cfg);
    const t0 = Date.now();
    const backend = await this.api.resolveBackend(this.host, cfg);
    const text = kind
      ? await this.host.callMcpTool(backend, "memory_list", { kind, limit: KIND_LIMIT, status })
      : await this.host.callMcpTool(backend, "memory_search", { query, limit: SEARCH_LIMIT, status });
    const entries = parseMemoryArray(text);
    this.api.logLine(
      kind
        ? `memory tree: kind=${kind} status=${status} via memory_list in ${Date.now() - t0}ms, ${entries.length} entries`
        : `memory tree: search "${query}" status=${status} via memory_search in ${Date.now() - t0}ms, ${entries.length} entries`,
    );
    return entries;
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
    const t0 = Date.now();
    const text = await host.callMcpTool(backend, "memory_expand", { id: entry.id });
    const expanded = JSON.parse(text);
    api.logLine(
      `memory detail: memory_expand ${entry.id} in ${Date.now() - t0}ms (chain=${
        Array.isArray(expanded && expanded.supersedes_chain) ? expanded.supersedes_chain.length : 0
      })`,
    );
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
  /**
   * Re-derives the whole tree from `ibis.memory.statusFilter` (F4 §A.2): the
   * view title carries an `all` description while the unfiltered browse is
   * active, and counts are refetched under the new filter (stale counts
   * from the other filter would mislabel the root badges).
   * @returns {Promise<"current" | "all">}
   */
  async function applyStatusFilter() {
    const filter = provider.statusFilter();
    provider.counts.clear();
    provider.renderedCounts.clear();
    treeView.description = filter === "all" ? "all" : undefined;
    await provider.refreshCounts().catch(() => {});
    provider.refresh();
    api.logLine(`memory tree: status filter ${filter}`);
    return filter;
  }
  context.subscriptions.push(
    treeView,
    vscode.commands.registerCommand("ibis.memory.toggleStatusFilter", async () => {
      const next = provider.statusFilter() === "all" ? "current" : "all";
      await updateStatusFilter(api.getConfig(), next);
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("ibis.memory.statusFilter")) {
        applyStatusFilter().catch(() => {});
      }
    }),
    vscode.commands.registerCommand("ibis.memory.refresh", async () => {
      // Visible feedback (owner request 03/11): a spinner in the view title
      // while the re-fetch cycle runs — the fire() → getChildren work happens
      // inside VS Code's render, so the wait is a bounded grace period.
      await vscode.window.withProgress(
        { location: { viewId: "ibis.memoryExplorer" }, title: "Ibis: refreshing memory…" },
        async () => {
          const t0 = Date.now();
          api.logLine("memory tree: refresh requested");
          await provider.refreshCounts();
          provider.refresh();
          await new Promise((resolve) => setTimeout(resolve, 400));
          api.logLine(`memory tree: refresh cycle complete in ${Date.now() - t0}ms`);
        },
      );
    }),
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
  // Preload the kind counts at activation (owner decision 03/11): the
  // badges are visible on a virgin tree before any expansion, so collapsed
  // and virgin kinds behave the same way. applyStatusFilter also restores
  // the persisted `all` badge (F4 §A.2).
  applyStatusFilter().catch(() => {});
}

module.exports = { registerMemoryTree, MEMORY_KINDS };