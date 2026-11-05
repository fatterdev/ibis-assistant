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
 *
 * The Workstreams view (F4 §A.3) is the second tree in the same
 * activity-bar container: roots from `memory_workstreams` (name +
 * current-member count), children from `memory_list({workstream})` under
 * the same status filter, same detail editor and error degradation.
 *
 * Entry nodes carry the inline curation menu (F4 §A.4): Share / Archive /
 * Supersede / Update content through the mutating MCP tools, with the
 * owner-based Update guard and runtime governance surfaced as toasts.
 *
 * Both views refresh on focus (F4 §A.4/A.5): kind badges and expanded
 * parents re-derive from `memory_list` (embedding-free); search roots are
 * never auto-refreshed (embedding cost) — rerunning Search re-executes.
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
 * Actor identity for the Update-content guard (F4 §A.4) and the attach-mode
 * `X-Ibis-Actor` header (F4 §B.4, live since A5): the explicit
 * `ibis.identity.actor` setting or the `IBIS_ACTOR` environment variable.
 * Rows owned by the anonymous local actor (`"unknown"`) stay editable; named
 * rows are editable only by their owner.
 * @param {vscode.WorkspaceConfiguration} cfg
 * @returns {string}
 */
function configuredActor(cfg) {
  const setting = String(cfg.get("identity.actor", "")).trim();
  return setting || String(process.env.IBIS_ACTOR || "").trim();
}

/**
 * The Update-content guard (F4 §A.4): the menu item is offered only on rows
 * the caller owns. Anonymous local team rows (`owner: "unknown"`) and rows
 * matching the configured actor qualify; empty owners are git-claim rows
 * (files carry no owner — the row is the owner registry) and never qualify.
 * @param {Record<string, any>} memory
 * @param {vscode.WorkspaceConfiguration} cfg
 * @returns {boolean}
 */
function ownedByMe(memory, cfg) {
  const owner = String(memory.owner || "").trim();
  if (owner === "unknown") {
    return true;
  }
  if (!owner) {
    return false;
  }
  const actor = configuredActor(cfg);
  return Boolean(actor) && owner === actor;
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
    throw new Error(`unexpected memory list output: ${String(text).slice(0, 120)}`);
  }
  if (parsed && Array.isArray(parsed.results)) {
    parsed = parsed.results;
  }
  if (!Array.isArray(parsed)) {
    throw new Error("unexpected memory list payload (expected an array)");
  }
  return parsed;
}

/** Parses a mutation tool result (`{id, status}` JSON or raw text). */
function parseToolResult(text) {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
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

/**
 * Typed entry row shared by both views (kind browse, search, workstream
 * children). The curation payload rides on the TreeItem (`ibisMemory`) so
 * `view/item/context` menu commands receive it with the item; the
 * `contextValue` variant carries the Update-content guard (F4 §A.4): only
 * owner-matching rows become `ibisMemoryEntryUpdatable`.
 * @param {Record<string, any>} memory
 * @param {string} [kindIcon]
 * @param {vscode.WorkspaceConfiguration} [cfg]
 */
function entryNode(memory, kindIcon, cfg) {
  const kindMeta = MEMORY_KINDS.find((k) => k.kind === memory.kind);
  const description = [
    memory.store === "git" ? "git" : "",
    memory.status && memory.status !== "current" ? memory.status : "",
    memory.visibility === "personal" ? "personal" : "",
    Array.isArray(memory.tags) && memory.tags.length ? memory.tags.slice(0, 3).join(", ") : "",
  ]
    .filter(Boolean)
    .join(" · ");
  const tooltip = memoryTooltip(memory);
  if (memory.store === "git") {
    // F4 §A.4: git-owned claims keep every curation action disabled — the
    // file in .ibis/claims/ is the source of truth and the runtime rejects
    // the mutating calls anyway.
    tooltip.appendMarkdown(
      "\n\n_Repo is source of truth — supersede via `.ibis/claims/` commit._",
    );
  }
  const node = new MemoryNode(
    memory.title || memory.id,
    {
      icon: kindMeta ? kindMeta.icon : kindIcon || "bookmark",
      contextValue: memory.store === "git"
        ? "ibisMemoryEntryGit"
        : cfg && ownedByMe(memory, cfg)
          ? "ibisMemoryEntryUpdatable"
          : "ibisMemoryEntry",
      description,
      tooltip,
      command: {
        command: "ibis.memory.openDetail",
        title: "Open memory detail",
        arguments: [memory],
      },
    },
    { type: "entry", memory },
  );
  node.item.ibisMemory = memory;
  return node;
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
    /** Identity-stable kind roots: element-scoped fires (focus refresh,
     * count announcements) must hit the exact instances the tree holds. */
    this.kindNodes = new Map();
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
   * Builds (once) and re-renders a kind root; stable identity keeps the
   * element-scoped fires convergent across refresh cycles.
   * @param {{ kind: string, icon: string }} kindMeta
   * @returns {MemoryNode}
   */
  kindNode({ kind, icon }) {
    let node = this.kindNodes.get(kind);
    if (!node) {
      node = new MemoryNode(
        kind,
        {
          collapsible: vscode.TreeItemCollapsibleState.Collapsed,
          icon,
          contextValue: "ibisMemoryKind",
        },
        { type: "kind", kind, icon },
      );
      this.kindNodes.set(kind, node);
    }
    const count = this.counts.get(kind);
    node.item.description = count ? countLabel(count) : undefined;
    return node;
  }

  /**
   * @param {MemoryNode | undefined} element
   * @returns {Promise<MemoryNode[]>}
   */
  async getChildren(element) {
    if (!element) {
      const roots = MEMORY_KINDS.map((k) => this.kindNode(k));
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
        return entries.map((memory) => entryNode(memory, icon, this.api.getConfig()));
      } catch (err) {
        const message = err && err.message ? err.message : String(err);
        this.api.logLine(`memory tree fetch failed: ${message}`);
        const short = message
          .replace(/^unexpected memory list output:\s*/, "")
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
   * Focus refresh (F4 §A.4/A.5): kind badges are refetched and every kind
   * root re-fires — expanded parents re-derive their children from
   * `memory_list` (embedding-free, cheap), collapsed ones just re-render.
   * Search roots are never fired: the embedding cost is paid only by an
   * explicit re-run of the search command.
   */
  async refreshOnFocus() {
    const t0 = Date.now();
    try {
      await this.refreshCounts();
    } catch {
      /* a failed badge fetch keeps its last known count */
    }
    for (const node of this.kindNodes.values()) {
      const count = this.counts.get(node.data.kind);
      node.item.description = count ? countLabel(count) : undefined;
      this._onDidChangeTreeData.fire(node);
    }
    this.api.logLine(`memory tree: focus refresh in ${Date.now() - t0}ms`);
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
   * @param {MemoryNode} element
   * @returns {vscode.TreeItem}
   */
  getTreeItem(element) {
    return element.item;
  }
}

/**
 * Refresh-on-focus subscription (F4 §A.4/A.5), guarded: portable VS Code
 * builds may lack `window.onDidChangeTreeViewVisibility` — a hard call aborts
 * activation mid-`registerMemoryTree`, before the memory detail provider
 * registers, and every detail open then fails "Unable to resolve resource"
 * (seen on build a717861c07, 05/11).
 * @param {vscode.ExtensionContext} context
 * @param {(event: { viewId: string, visible: boolean }) => void} handler
 */
function subscribeTreeViewVisibility(context, handler) {
  if (typeof vscode.window.onDidChangeTreeViewVisibility !== "function") {
    return;
  }
  context.subscriptions.push(vscode.window.onDidChangeTreeViewVisibility(handler));
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
  // The detail provider must register before any optional-API call in this
  // function: a throw above it would leave every detail open unresolvable.
  registerMemoryDetailProvider(context);
  // Refresh on focus (F4 §A.4/A.5): re-deriving kind badges and expanded
  // parents costs metadata reads only; search roots are never re-run.
  subscribeTreeViewVisibility(context, (event) => {
    if (event && event.viewId === "ibis.memoryExplorer" && event.visible) {
      provider.refreshOnFocus().catch(() => {});
    }
  });
  // Preload the kind counts at activation (owner decision 03/11): the
  // badges are visible on a virgin tree before any expansion, so collapsed
  // and virgin kinds behave the same way. applyStatusFilter also restores
  // the persisted `all` badge (F4 §A.2).
  applyStatusFilter().catch(() => {});
}

/**
 * Workstreams view (F4 §A.3): roots from the read-only `memory_workstreams`
 * tool (`{id, name, status, current_count}`, most-recently-touched first),
 * children from `memory_list({workstream})` under the same status filter as
 * the kind browse. Same entry rows, detail editor, and error-node
 * degradation as the Memory tree.
 */
class WorkstreamTreeDataProvider {
  /**
   * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
   *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
   *           logLine: (message: string) => void }} api
   * @param {Record<string, any>} host
   */
  constructor(api, host) {
    this.api = api;
    this.host = host;
    this._onDidChangeTreeData = new vscode.EventEmitter();
    this.onDidChangeTreeData = this._onDidChangeTreeData.event;
  }

  refresh() {
    this._onDidChangeTreeData.fire();
  }

  /**
   * @returns {"current" | "all"}
   */
  statusFilter() {
    return statusFilterOf(this.api.getConfig());
  }

  /**
   * @param {MemoryNode | undefined} element
   * @returns {Promise<MemoryNode[]>}
   */
  async getChildren(element) {
    if (!element) {
      try {
        const workstreams = await this.fetchWorkstreams();
        if (workstreams.length === 0) {
          return [
            new MemoryNode("no workstreams yet", {
              icon: "circle-slash",
              description: "workstream rows appear as memories attach to them",
            }),
          ];
        }
        return workstreams.map((ws) => {
          const node = new MemoryNode(
            ws.name || ws.id,
            {
              collapsible: vscode.TreeItemCollapsibleState.Collapsed,
              icon: "project",
              contextValue: "ibisWorkstream",
              description: `${ws.current_count} current`,
              tooltip: workstreamTooltip(ws),
            },
            { type: "workstream", ws },
          );
          node.item.ibisWorkstream = ws;
          return node;
        });
      } catch (err) {
        return [this.errorNode("workstream browse failed", err)];
      }
    }

    const { ws } = element.data;
    try {
      const entries = await this.fetchEntries(ws);
      if (entries.length === 0) {
        return [
          new MemoryNode("no memories found", {
            icon: "circle-slash",
            description: `no memories in "${ws.name || ws.id}"`,
          }),
        ];
      }
      return entries.map((memory) => entryNode(memory, undefined, this.api.getConfig()));
    } catch (err) {
      return [this.errorNode(`workstream "${ws.name || ws.id}" fetch failed`, err)];
    }
  }

  /**
   * @returns {Promise<Array<Record<string, any>>>}
   */
  async fetchWorkstreams() {
    const t0 = Date.now();
    const backend = await this.api.resolveBackend(this.host, this.api.getConfig());
    const text = await this.host.callMcpTool(backend, "memory_workstreams", {});
    const rows = parseMemoryArray(text);
    this.api.logLine(
      `workstream tree: memory_workstreams in ${Date.now() - t0}ms, ${rows.length} rows`,
    );
    return rows;
  }

  /**
   * Children honor the shared status filter (F4 §A.2): the explicit workstream
   * record id skips the name lookup server-side.
   * @param {Record<string, any>} ws
   * @returns {Promise<Array<Record<string, any>>>}
   */
  async fetchEntries(ws) {
    const cfg = this.api.getConfig();
    const status = statusFilterOf(cfg);
    const t0 = Date.now();
    const backend = await this.api.resolveBackend(this.host, cfg);
    const text = await this.host.callMcpTool(backend, "memory_list", {
      workstream: ws.id || ws.name,
      limit: KIND_LIMIT,
      status,
    });
    const entries = parseMemoryArray(text);
    this.api.logLine(
      `workstream tree: workstream=${ws.name || ws.id} status=${status} via memory_list in ${
        Date.now() - t0
      }ms, ${entries.length} entries`,
    );
    return entries;
  }

  /**
   * Same degradation pattern as the Memory tree: a short `error:` row, full
   * message in the output channel.
   * @param {string} label
   * @param {unknown} err
   * @returns {MemoryNode}
   */
  errorNode(label, err) {
    const message = err && err.message ? err.message : String(err);
    this.api.logLine(`${label}: ${message}`);
    const short = message
      .replace(/^unexpected memory list output:\s*/, "")
      .replace(/\s+/g, " ")
      .slice(0, 140);
    return new MemoryNode(`error: ${short}`, { icon: "error", description: label });
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
 * @param {Record<string, any>} ws
 * @returns {vscode.MarkdownString}
 */
function workstreamTooltip(ws) {
  const tooltip = new vscode.MarkdownString();
  tooltip.isTrusted = false;
  tooltip.appendMarkdown(
    [
      `**${ws.name || ws.id}**`,
      "",
      `${ws.current_count} current member(s)`,
      `_${ws.status ? `status: ${ws.status}` : "workstream"}_`,
    ].join("\n"),
  );
  return tooltip;
}

/**
 * Wire the Workstreams view, its refresh command, and its focus/filter
 * reactions (F4 §A.2/A.5).
 * @param {vscode.ExtensionContext} context
 * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
 *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
 *           logLine: (message: string) => void }} api
 * @param {Record<string, any>} host
 */
function registerWorkstreamTree(context, api, host) {
  const provider = new WorkstreamTreeDataProvider(api, host);
  const treeView = vscode.window.createTreeView("ibis.workstreamExplorer", {
    treeDataProvider: provider,
    showCollapseAll: true,
  });
  /**
   * The Workstreams children honor the shared status filter (F4 §A.2): the
   * view title mirrors the `all` badge of the Memory view.
   */
  function applyStatusFilter() {
    const filter = provider.statusFilter();
    treeView.description = filter === "all" ? "all" : undefined;
    provider.refresh();
    api.logLine(`workstream tree: status filter ${filter}`);
  }
  context.subscriptions.push(
    treeView,
    vscode.commands.registerCommand("ibis.workstream.refresh", async () => {
      await vscode.window.withProgress(
        { location: { viewId: "ibis.workstreamExplorer" }, title: "Ibis: refreshing workstreams…" },
        async () => {
          api.logLine("workstream tree: refresh requested");
          provider.refresh();
          await new Promise((resolve) => setTimeout(resolve, 400));
        },
      );
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("ibis.memory.statusFilter")) {
        applyStatusFilter();
      }
    }),
  );
  // Refresh on focus (F4 §A.5): the whole browse is `memory_workstreams` +
  // `memory_list` metadata — no embedding on this path. Guarded: see
  // `subscribeTreeViewVisibility` (portable builds lack the API).
  subscribeTreeViewVisibility(context, (event) => {
    if (event && event.viewId === "ibis.workstreamExplorer" && event.visible) {
      api.logLine("workstream tree: focus refresh");
      provider.refresh();
    }
  });
  treeView.description = provider.statusFilter() === "all" ? "all" : undefined;
}

/**
 * Inline curation menu (F4 §A.4): context-menu actions on entry rows of both
 * views. Every action toasts the compact result and logs the full tool
 * output; governance errors surface verbatim (the runtime is the authority).
 *
 * Git-owned claims (`store: "git"`, ownerless rows) are rejected by the
 * runtime on mutating calls ("repo is SoT") — `memory_list` does not project
 * the store marker, so the menu relies on the runtime message instead of a
 * pre-disabled item (recorded residual in the F4-A4 queue entry).
 * @param {vscode.ExtensionContext} context
 * @param {{ getConfig: () => vscode.WorkspaceConfiguration,
 *           resolveBackend: (host: any, cfg: vscode.WorkspaceConfiguration) => Promise<any>,
 *           logLine: (message: string) => void }} api
 * @param {Record<string, any>} host
 */
function registerCurationMenu(context, api, host) {
  /**
   * @param {Record<string, any>} memory
   * @param {string} tool
   * @param {Record<string, unknown>} args
   */
  async function callTool(memory, tool, args) {
    const cfg = api.getConfig();
    const t0 = Date.now();
    const backend = await api.resolveBackend(host, cfg);
    const text = await host.callMcpTool(backend, tool, args);
    api.logLine(`memory curation: ${tool} ${memory.id} in ${Date.now() - t0}ms: ${text}`);
    return text;
  }

  function shortError(err) {
    return String(err && err.message ? err.message : err)
      .replace(/\s+/g, " ")
      .slice(0, 200);
  }

  async function refreshTrees() {
    await Promise.resolve(vscode.commands.executeCommand("ibis.memory.refresh")).catch(() => {});
    await Promise.resolve(vscode.commands.executeCommand("ibis.workstream.refresh")).catch(
      () => {},
    );
  }

  /** Share (F4 §A.4): promote to team — low risk, no confirmation. */
  async function shareMemory(memory) {
    await callTool(memory, "memory_share", { id: memory.id, visibility: "team" });
    vscode.window.showInformationMessage(
      `Ibis: shared "${memory.title || memory.id}" to team visibility`,
    );
    await refreshTrees();
  }

  /** Archive: confirm, then `memory_update status=archived`. */
  async function archiveMemory(memory) {
    const title = memory.title || memory.id;
    const pick = await vscode.window.showWarningMessage(
      `Archive "${title}"? It will be marked archived (history and edges retained).`,
      { modal: true },
      "Archive",
    );
    if (pick !== "Archive") {
      return;
    }
    await callTool(memory, "memory_update", { id: memory.id, status: "archived" });
    vscode.window.showInformationMessage(`Ibis: archived "${title}"`);
    await refreshTrees();
  }

  /**
   * Supersede: a new memory (kind/visibility prefilled, title + content via
   * input boxes) replaces this one through the `supersedes` edge — the
   * governance route for rows the caller cannot write.
   */
  async function supersedeMemory(memory) {
    const title = await vscode.window.showInputBox({
      title: "Ibis Assistant: Supersede Memory",
      prompt: `New title (kind ${memory.kind || "fact"}, visibility ${
        memory.visibility || "team"
      } prefilled)`,
      value: memory.title || "",
      ignoreFocusOut: true,
    });
    if (title === undefined) {
      return;
    }
    const content = await vscode.window.showInputBox({
      title: "Ibis Assistant: Supersede Memory",
      prompt: "New full content — the replaced memory is marked superseded",
      placeHolder: "What replaces it?",
      ignoreFocusOut: true,
    });
    if (!content || !content.trim()) {
      return;
    }
    const pick = await vscode.window.showWarningMessage(
      `Supersede "${memory.title || memory.id}" with a new ${
        memory.kind || "fact"
      } memory?`,
      { modal: true },
      "Supersede",
    );
    if (pick !== "Supersede") {
      return;
    }
    const text = await callTool(memory, "memory_add", {
      kind: memory.kind || "fact",
      title: title.trim(),
      content: content.trim(),
      visibility: memory.visibility || "team",
      supersedes: memory.id,
    });
    const created = parseToolResult(text);
    vscode.window.showInformationMessage(
      `Ibis: superseded "${memory.title || memory.id}" with ${created && created.id ? created.id : "new memory"}`,
    );
    await refreshTrees();
  }

  /** Update content: owner-only at the menu; re-embeds on write. */
  async function updateMemoryContent(memory) {
    const title = memory.title || memory.id;
    const content = await vscode.window.showInputBox({
      title: "Ibis Assistant: Update Memory Content",
      prompt: "New full content (the memory is re-embedded)",
      value: String(memory.content || ""),
      ignoreFocusOut: true,
    });
    if (content === undefined || !content.trim()) {
      return;
    }
    const pick = await vscode.window.showWarningMessage(
      `Update the content of "${title}"? The new text replaces it and re-embeds.`,
      { modal: true },
      "Update",
    );
    if (pick !== "Update") {
      return;
    }
    try {
      await callTool(memory, "memory_update", { id: memory.id, content: content.trim() });
      vscode.window.showInformationMessage(`Ibis: updated "${title}"`);
      await refreshTrees();
    } catch (err) {
      const message = String(err && err.message ? err.message : err);
      api.logLine(`memory curation: memory_update ${memory.id} failed: ${message}`);
      const governance =
        /supersede|immutable|repo is SoT|writable only by owner/i.test(message);
      const detail = governance
        ? "The runtime rejected the update — supersede it instead."
        : shortError(err);
      const retry = await vscode.window.showErrorMessage(
        `Ibis update failed: ${detail}`,
        governance ? "Create a superseding memory instead" : undefined,
      );
      if (retry === "Create a superseding memory instead") {
        await supersedeMemory(memory);
      }
    }
  }

  /**
   * Menu wrapper: `view/item/context` hands over the TreeItem of the row
   * (both views share the entry context values) — the curation payload rides
   * on `item.ibisMemory`.
   * @param {(memory: Record<string, any>) => Promise<void>} action
   */
  function contextAction(action) {
    return async (item) => {
      const memory = item && (item.ibisMemory || (item.data && item.data.memory));
      if (!memory || !memory.id) {
        return;
      }
      try {
        await action(memory);
      } catch (err) {
        api.logLine(`memory curation failed: ${shortError(err)}`);
        await vscode.window.showErrorMessage(`Ibis curation failed: ${shortError(err)}`);
      }
    };
  }

  context.subscriptions.push(
    vscode.commands.registerCommand("ibis.memory.share", contextAction(shareMemory)),
    vscode.commands.registerCommand("ibis.memory.archive", contextAction(archiveMemory)),
    vscode.commands.registerCommand("ibis.memory.supersede", contextAction(supersedeMemory)),
    vscode.commands.registerCommand(
      "ibis.memory.updateContent",
      contextAction(updateMemoryContent),
    ),
  );
}

module.exports = {
  registerMemoryTree,
  registerWorkstreamTree,
  registerCurationMenu,
  configuredActor,
  MEMORY_KINDS,
};