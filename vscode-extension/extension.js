const vscode = require("vscode");

/**
 * Load ESM ide-host via its package exports (works from monorepo and packaged VSIX
 * under node_modules/@ibis-assistant/ide-host).
 */
async function loadIdeHost() {
  const host = await import("@ibis-assistant/ide-host");
  const lmRpc = await import("@ibis-assistant/ide-host/lm-rpc");
  return { ...host, ...lmRpc };
}

/** @type {import("http").Server | undefined} */
let lmServer;

/**
 * @param {vscode.ExtensionContext} context
 */
async function activate(context) {
  const host = await loadIdeHost();
  const backend = host.resolveBackend(process.env);
  if (backend.mode === "spawn") {
    host.warnDualPluginChildren();
  }

  try {
    const attached = await host.attachVscodeLm(vscode);
    if (attached) {
      lmServer = await host.startLmRpcServer({});
      process.env.IBIS_LM_BRIDGE_URL =
        process.env.IBIS_LM_BRIDGE_URL || `http://127.0.0.1:${host.LM_BRIDGE_DEFAULT_PORT}`;
    }
  } catch (err) {
    console.warn("[ibis-assistant] LM bridge not started:", err);
  }

  registerMcpProvider(context, backend);

  context.subscriptions.push(
    vscode.commands.registerCommand("ibis.openSettings", async () => {
      try {
        const url = await resolveSettingsURL(host, backend);
        await vscode.env.openExternal(vscode.Uri.parse(url));
      } catch (err) {
        vscode.window.showErrorMessage(
          `Ibis Settings unavailable: ${err && err.message ? err.message : err}`,
        );
      }
    }),
    vscode.commands.registerCommand("ibis.status", async () => {
      const shared = !!process.env.IBIS_MCP_URL;
      const msg = [
        `Backend: ${backend.mode}${backend.mode === "http" ? " " + backend.url : " (stdio spawn)"}`,
        `Shared runtime (IBIS_MCP_URL): ${shared ? "yes" : "no (isolated child)"}`,
        `IDE LM bridge: ${lmServer ? "listening" : "unavailable"}`,
        "Memory: call load_session_context once per session; capture only genuinely new, reusable observations.",
        "Call MCP ticket_get_capabilities / settings_get for full readiness.",
      ].join("\n");
      vscode.window.showInformationMessage(msg);
    }),
    vscode.commands.registerCommand("ibis.remember", async () => {
      try {
        await rememberMemory(host, backend);
      } catch (err) {
        vscode.window.showErrorMessage(
          `Ibis Remember failed: ${err && err.message ? err.message : err}`,
        );
      }
    }),
    vscode.commands.registerCommand("ibis.setWorkingState", async () => {
      try {
        await setWorkingState(host, backend);
      } catch (err) {
        vscode.window.showErrorMessage(
          `Ibis Set working state failed: ${err && err.message ? err.message : err}`,
        );
      }
    }),
  );
}

/**
 * Register the contributed MCP server-definition provider when the host API exists.
 * @param {vscode.ExtensionContext} context
 * @param {{ mode: string, url?: string, command?: string, args?: string[], env?: NodeJS.ProcessEnv }} backend
 */
function registerMcpProvider(context, backend) {
  const lm = vscode.lm;
  if (!lm || typeof lm.registerMcpServerDefinitionProvider !== "function") {
    return;
  }

  const provider = {
    provideMcpServerDefinitions: async () => {
      const defs = [];
      if (backend.mode === "http" && backend.url) {
        if (typeof vscode.McpHttpServerDefinition === "function") {
          defs.push(new vscode.McpHttpServerDefinition("Ibis Assistant", vscode.Uri.parse(backend.url)));
        } else if (typeof vscode.McpServerDefinition === "function") {
          defs.push({ label: "Ibis Assistant", uri: vscode.Uri.parse(backend.url) });
        }
        return defs;
      }
      if (typeof vscode.McpStdioServerDefinition === "function") {
        defs.push(
          new vscode.McpStdioServerDefinition(
            "Ibis Assistant",
            backend.command || "ibis-assistant",
            backend.args || ["-mode", "stdio", "-runtime-mode", "plugin"],
            { ...(backend.env || {}), RUNTIME_MODE: "plugin" },
          ),
        );
      }
      return defs;
    },
  };

  try {
    context.subscriptions.push(lm.registerMcpServerDefinitionProvider("ibis-assistant", provider));
  } catch (err) {
    console.warn("[ibis-assistant] MCP server definition provider not registered:", err);
  }
}

/**
 * Prompt for a durable decision/fix/rule and store it via memory_add.
 * @param {Awaited<ReturnType<typeof loadIdeHost>>} host
 * @param {{ mode: string, url?: string, command?: string, args?: string[], env?: NodeJS.ProcessEnv }} backend
 */
async function rememberMemory(host, backend) {
  const kindPick = await vscode.window.showQuickPick(
    [
      { label: "decision", description: "A choice the team should keep" },
      { label: "fix", description: "What resolved a failure" },
      { label: "rule", description: "A durable constraint or convention" },
    ],
    { placeHolder: "Memory kind", title: "Ibis: Remember" },
  );
  if (!kindPick) {
    return;
  }
  const title = await vscode.window.showInputBox({
    title: "Ibis: Remember",
    prompt: "Short title",
    placeHolder: "e.g. Prefer Streamable HTTP for MCP",
  });
  if (title === undefined) {
    return;
  }
  const content = await vscode.window.showInputBox({
    title: "Ibis: Remember",
    prompt: "Full memory content",
    placeHolder: "What should future agents and teammates reuse?",
    ignoreFocusOut: true,
  });
  if (!content || !content.trim()) {
    return;
  }
  const text = await host.callMcpTool(backend, "memory_add", {
    kind: kindPick.label,
    title: title.trim(),
    content: content.trim(),
    visibility: "team",
  });
  vscode.window.showInformationMessage(`Ibis Remembered (${kindPick.label}): ${text}`);
}

/**
 * Prompt for current-task text and upsert via set_working_state.
 * @param {Awaited<ReturnType<typeof loadIdeHost>>} host
 * @param {{ mode: string, url?: string, command?: string, args?: string[], env?: NodeJS.ProcessEnv }} backend
 */
async function setWorkingState(host, backend) {
  const content = await vscode.window.showInputBox({
    title: "Ibis: Set working state",
    prompt: "What are you working on right now?",
    placeHolder: "Current task, open files, next steps",
    ignoreFocusOut: true,
  });
  if (!content || !content.trim()) {
    return;
  }
  /** @type {Record<string, string>} */
  const args = { content: content.trim() };
  const folder = vscode.workspace.workspaceFolders && vscode.workspace.workspaceFolders[0];
  if (folder) {
    args.project_name = folder.name;
  }
  const text = await host.callMcpTool(backend, "set_working_state", args);
  vscode.window.showInformationMessage(`Ibis working state set: ${text}`);
}

/**
 * Prefer IBIS_SETTINGS_URL / personal probe, else MCP settings_open_ui.
 * @param {Awaited<ReturnType<typeof loadIdeHost>>} host
 * @param {{ mode: string, url?: string, command?: string, args?: string[], env?: NodeJS.ProcessEnv }} backend
 */
async function resolveSettingsURL(host, backend) {
  if (process.env.IBIS_SETTINGS_URL) {
    return process.env.IBIS_SETTINGS_URL;
  }
  for (const base of ["http://127.0.0.1:3030", "http://127.0.0.1:3031"]) {
    try {
      const res = await fetch(`${base}/settings/`, { signal: AbortSignal.timeout(800) });
      if (res.ok) {
        return `${base}/settings/`;
      }
    } catch {
      /* probe next */
    }
  }
  const text = await host.callMcpTool(backend, "settings_open_ui", {});
  const url = host.extractSettingsURL(text);
  if (!url) {
    throw new Error("settings_open_ui did not return a URL");
  }
  return url;
}

/**
 * @param {{ mode: string, url?: string, command?: string, args?: string[] }} backend
 */
function sharedRuntimeMcpConfig(backend) {
  if (backend.mode === "http") {
    return { ibisAssistant: { url: backend.url } };
  }
  return {
    ibisAssistant: {
      command: backend.command || "ibis-assistant",
      args: backend.args || ["-mode", "stdio", "-runtime-mode", "plugin"],
      env: { RUNTIME_MODE: "plugin" },
    },
  };
}

function deactivate() {
  if (lmServer) {
    lmServer.close();
    lmServer = undefined;
  }
}

module.exports = { activate, deactivate, sharedRuntimeMcpConfig, resolveSettingsURL };
