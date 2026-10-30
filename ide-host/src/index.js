/**
 * Resolve how the IDE should reach Ibis MCP.
 *
 * Default: spawn an isolated `ibis` stdio child (RUNTIME_MODE=plugin).
 * Shared runtime: set IBIS_MCP_URL (e.g. http://127.0.0.1:3030/mcp) to attach to
 * a personal HTTP daemon instead of spawning. Never silently share one SurrealDB
 * across two plugin stdio children.
 *
 * Stdio contract (this repo): newline-delimited JSON-RPC. Content-Length framing
 * is accepted as an optional ingress format; egress to the IDE is always NDJSON.
 */

import { spawn } from "node:child_process";
import process from "node:process";

export const DEFAULT_MCP_URL = "http://127.0.0.1:3030/mcp";

const CRLFCRLF = Buffer.from("\r\n\r\n");

/**
 * @typedef {{ mode: "spawn"; command: string; args: string[]; env: NodeJS.ProcessEnv }
 *   | { mode: "http"; url: string }} Backend
 */

/**
 * @param {NodeJS.ProcessEnv} [env]
 * @returns {Backend}
 */
export function resolveBackend(env = process.env) {
  const url = (env.IBIS_MCP_URL || "").trim();
  if (url) {
    return { mode: "http", url };
  }
  return {
    mode: "spawn",
    command: env.IBIS_BIN || "ibis",
    args: ["-mode", "stdio", "-runtime-mode", "plugin"],
    env: {
      ...env,
      RUNTIME_MODE: "plugin",
    },
  };
}

/**
 * Spawn the Go MCP stdio child. Logs go to stderr only.
 * @param {Backend} [backend]
 * @returns {import("node:child_process").ChildProcess}
 */
export function spawnStdioChild(backend = resolveBackend()) {
  if (backend.mode !== "spawn") {
    throw new Error("spawnStdioChild requires mode=spawn; use IBIS_MCP_URL unset");
  }
  const child = spawn(backend.command, backend.args, {
    env: backend.env,
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true,
  });
  child.stderr?.on("data", (chunk) => {
    process.stderr.write(chunk);
  });
  return child;
}

/**
 * Pipe this process stdio to a spawned Go child (passthrough MCP).
 * @param {Backend} [backend]
 */
export function runStdioPassthrough(backend = resolveBackend()) {
  if (backend.mode === "http") {
    return runHttpStdioBridge(backend.url);
  }
  const child = spawnStdioChild(backend);
  process.stdin.pipe(child.stdin);
  child.stdout.pipe(process.stdout);
  child.on("error", (err) => {
    process.stderr.write(`[ibis-mcp-host] spawn failed: ${err.message}\n`);
    process.exit(1);
  });
  child.on("exit", (code, signal) => {
    if (signal) process.exit(1);
    process.exit(code ?? 0);
  });
}

/**
 * @param {Buffer} buf
 * @param {Buffer} needle
 */
function indexOfBuffer(buf, needle) {
  return buf.indexOf(needle);
}

/**
 * Extract complete JSON-RPC message bodies from a stdio buffer.
 * Content-Length uses UTF-8 byte counts (not JS string length).
 * Prefers Content-Length frames when the buffer starts with that header;
 * otherwise parses newline-delimited JSON (repository contract).
 * Incomplete remainder is always returned as a Buffer so a chunk that ends
 * mid multi-byte code point is not corrupted by a premature UTF-8 decode.
 * @param {string | Buffer} buf
 * @returns {{ messages: string[]; rest: Buffer }}
 */
export function takeStdioMessages(buf) {
  let bytes = Buffer.isBuffer(buf) ? Buffer.from(buf) : Buffer.from(buf, "utf8");
  const messages = [];

  while (bytes.length > 0) {
    // ASCII-only probe for the Content-Length header prefix (safe on partial UTF-8).
    const probe = bytes.subarray(0, Math.min(bytes.length, 64)).toString("latin1");
    if (/^Content-Length:/i.test(probe)) {
      const headerEnd = indexOfBuffer(bytes, CRLFCRLF);
      if (headerEnd === -1) {
        break;
      }
      const header = bytes.subarray(0, headerEnd).toString("utf8");
      const match = /Content-Length:\s*(\d+)/i.exec(header);
      if (!match) {
        const nl = bytes.indexOf(0x0a);
        if (nl === -1) break;
        const line = bytes.subarray(0, nl).toString("utf8").replace(/\r$/, "").trim();
        bytes = bytes.subarray(nl + 1);
        if (line) messages.push(line);
        continue;
      }
      const len = parseInt(match[1], 10);
      const bodyStart = headerEnd + 4;
      if (bytes.length < bodyStart + len) {
        break;
      }
      messages.push(bytes.subarray(bodyStart, bodyStart + len).toString("utf8"));
      bytes = bytes.subarray(bodyStart + len);
      continue;
    }

    const nl = bytes.indexOf(0x0a);
    if (nl === -1) {
      break;
    }
    const line = bytes.subarray(0, nl).toString("utf8").replace(/\r$/, "").trim();
    bytes = bytes.subarray(nl + 1);
    if (line) messages.push(line);
  }

  return { messages, rest: bytes };
}

/**
 * Decode an HTTP MCP response body into JSON-RPC message strings.
 * Handles application/json and text/event-stream (SSE data: lines).
 * @param {string} contentType
 * @param {string} text
 * @returns {string[]}
 */
export function decodeHttpMcpResponse(contentType, text) {
  const ct = (contentType || "").toLowerCase();
  const trimmed = (text || "").trim();
  if (!trimmed) return [];

  if (ct.includes("text/event-stream")) {
    const out = [];
    for (const block of trimmed.split(/\r?\n\r?\n/)) {
      for (const line of block.split(/\r?\n/)) {
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (!data || data === "[DONE]") continue;
        out.push(data);
      }
    }
    return out;
  }

  // Single JSON object/array, or already NDJSON.
  if (trimmed.includes("\n")) {
    return trimmed
      .split(/\r?\n/)
      .map((l) => l.trim())
      .filter(Boolean);
  }
  return [trimmed];
}

/**
 * Write one JSON-RPC message as newline-delimited stdout (repo stdio contract).
 * @param {string} message
 */
function writeNdjson(message) {
  const line = message.endsWith("\n") ? message : `${message}\n`;
  process.stdout.write(line);
}

/**
 * Minimal Streamable HTTP MCP bridge: each stdin JSON-RPC message is POSTed
 * to url; decoded response messages are written as NDJSON on stdout.
 * Persists Mcp-Session-Id from initialize for subsequent requests.
 * @param {string} url
 */
export async function runHttpStdioBridge(url) {
  process.stderr.write(
    `[ibis-mcp-host] attaching to shared runtime ${url} (IBIS_MCP_URL); not spawning a plugin child\n`,
  );

  /** @type {string | null} */
  let sessionId = null;
  let buf = Buffer.alloc(0);
  let chain = Promise.resolve();
  process.stdin.on("data", (chunk) => {
    buf = Buffer.concat([buf, Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)]);
    chain = chain.then(() => drainBuffer());
  });
  process.stdin.on("end", () => {
    chain.finally(() => process.exit(0));
  });

  async function drainBuffer() {
    const { messages, rest } = takeStdioMessages(buf);
    buf = rest;
    for (const msg of messages) {
      await forward(msg);
    }
  }

  async function forward(body) {
    try {
      /** @type {Record<string, string>} */
      const headers = {
        "Content-Type": "application/json",
        Accept: "application/json, text/event-stream",
      };
      if (sessionId) {
        headers["Mcp-Session-Id"] = sessionId;
      }
      const res = await fetch(url, {
        method: "POST",
        headers,
        body,
      });
      const sid = res.headers.get("mcp-session-id");
      if (sid) {
        sessionId = sid;
      }
      const text = await res.text();
      if (!res.ok) {
        process.stderr.write(`[ibis-mcp-host] HTTP ${res.status}: ${text.slice(0, 200)}\n`);
      }
      const ct = res.headers.get("content-type") || "";
      for (const msg of decodeHttpMcpResponse(ct, text)) {
        writeNdjson(msg);
      }
    } catch (err) {
      process.stderr.write(`[ibis-mcp-host] bridge error: ${err}\n`);
    }
  }
}

/**
 * Call an MCP tool via Streamable HTTP or a one-shot stdio child.
 * Returns concatenated text content from the tool result.
 * @param {Backend} backend
 * @param {string} toolName
 * @param {Record<string, unknown>} [args]
 * @param {{ keepAliveMs?: number }} [opts]
 * @returns {Promise<string>}
 */
export async function callMcpTool(backend, toolName, args = {}, opts = {}) {
  if (backend.mode === "http") {
    return callMcpToolHttp(backend.url, toolName, args);
  }
  return callMcpToolStdio(backend, toolName, args, opts);
}

/**
 * @param {string} url
 * @param {string} toolName
 * @param {Record<string, unknown>} args
 */
async function callMcpToolHttp(url, toolName, args) {
  /** @type {{ id: string | null }} */
  const session = { id: null };
  const init = await postJsonRpc(url, {
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "ibis-ide-host", version: "1.0.0" },
    },
  }, session);
  if (init.error) {
    throw new Error(init.error.message || JSON.stringify(init.error));
  }
  await postJsonRpc(url, {
    jsonrpc: "2.0",
    method: "notifications/initialized",
    params: {},
  }, session);
  const result = await postJsonRpc(url, {
    jsonrpc: "2.0",
    id: 2,
    method: "tools/call",
    params: { name: toolName, arguments: args },
  }, session);
  if (result.error) {
    throw new Error(result.error.message || JSON.stringify(result.error));
  }
  return textFromToolResult(result.result);
}

/**
 * @param {Backend & { mode: "spawn" }} backend
 * @param {string} toolName
 * @param {Record<string, unknown>} args
 * @param {{ keepAliveMs?: number }} [opts]
 */
async function callMcpToolStdio(backend, toolName, args, { keepAliveMs = 0 } = {}) {
  const child = spawnStdioChild(backend);
  let outBuf = Buffer.alloc(0);
  const replies = new Map();
  /** @type {Array<() => void>} */
  let waiters = [];

  const wake = () => {
    const pending = waiters;
    waiters = [];
    for (const w of pending) w();
  };

  child.stdout.on("data", (chunk) => {
    outBuf = Buffer.concat([outBuf, Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)]);
    const { messages, rest } = takeStdioMessages(outBuf);
    outBuf = rest;
    for (const raw of messages) {
      let msg;
      try {
        msg = JSON.parse(raw);
      } catch {
        continue;
      }
      if (msg.id != null) {
        replies.set(msg.id, msg);
        wake();
      }
    }
  });

  const write = (obj) => {
    child.stdin.write(`${JSON.stringify(obj)}\n`);
  };

  const waitFor = (id, ms = 15000) =>
    new Promise((resolve, reject) => {
      let settled = false;
      const finish = (fn, value) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        child.off("exit", onExit);
        child.off("error", onError);
        waiters = waiters.filter((w) => w !== check);
        fn(value);
      };
      const timer = setTimeout(() => {
        finish(reject, new Error(`timeout waiting for MCP response id=${id}`));
      }, ms);
      const onExit = (code, signal) => {
        finish(
          reject,
          new Error(`child exited before MCP response id=${id} (code=${code} signal=${signal})`),
        );
      };
      const onError = (err) => {
        finish(reject, err);
      };
      const check = () => {
        if (replies.has(id)) {
          finish(resolve, replies.get(id));
        }
      };
      child.once("exit", onExit);
      child.once("error", onError);
      waiters.push(check);
      check();
    });

  try {
    write({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2024-11-05",
        capabilities: {},
        clientInfo: { name: "ibis-ide-host", version: "1.0.0" },
      },
    });
    const init = await waitFor(1);
    if (init.error) {
      throw new Error(init.error.message || JSON.stringify(init.error));
    }
    write({ jsonrpc: "2.0", method: "notifications/initialized", params: {} });
    write({
      jsonrpc: "2.0",
      id: 2,
      method: "tools/call",
      params: { name: toolName, arguments: args },
    });
    const result = await waitFor(2);
    if (result.error) {
      throw new Error(result.error.message || JSON.stringify(result.error));
    }
    return textFromToolResult(result.result);
  } finally {
    if (keepAliveMs > 0) {
      // The one-shot child hosts in-process listeners that outlive this call
      // (e.g. the ephemeral settings UI): keep it alive for keepAliveMs
      // instead of tearing it down right after the response. The runtime's
      // own idle timeout (20 min) stays authoritative; if this host exits
      // first, the closing stdin pipe ends the child by itself.
      const closeChild = () => {
        try {
          child.stdin.end();
        } catch {
          /* ignore */
        }
        child.kill();
      };
      const timer = setTimeout(closeChild, keepAliveMs);
      if (typeof timer.unref === "function") {
        timer.unref();
      }
    } else {
      try {
        child.stdin.end();
      } catch {
        /* ignore */
      }
      child.kill();
    }
  }
}

/**
 * @param {string} url
 * @param {object} payload
 * @param {{ id: string | null }} session
 */
async function postJsonRpc(url, payload, session) {
  /** @type {Record<string, string>} */
  const headers = {
    "Content-Type": "application/json",
    Accept: "application/json, text/event-stream",
  };
  if (session.id) {
    headers["Mcp-Session-Id"] = session.id;
  }
  const res = await fetch(url, {
    method: "POST",
    headers,
    body: JSON.stringify(payload),
  });
  const sid = res.headers.get("mcp-session-id");
  if (sid) {
    session.id = sid;
  }
  const text = await res.text();
  const msgs = decodeHttpMcpResponse(res.headers.get("content-type") || "", text);
  const raw = msgs[0] || text;
  try {
    return JSON.parse(raw);
  } catch {
    throw new Error(`non-JSON MCP response: ${raw.slice(0, 200)}`);
  }
}

/**
 * @param {unknown} result
 */
function textFromToolResult(result) {
  if (!result || typeof result !== "object") return String(result ?? "");
  const content = /** @type {{ content?: Array<{ type?: string, text?: string }> }} */ (result)
    .content;
  if (!Array.isArray(content)) return JSON.stringify(result);
  return content
    .filter((c) => c && c.type === "text" && c.text)
    .map((c) => c.text)
    .join("\n");
}

/**
 * Parse a Settings URL out of settings_open_ui tool text.
 * @param {string} text
 * @returns {string | null}
 */
export function extractSettingsURL(text) {
  const m = /https?:\/\/[^\s)]+/i.exec(text || "");
  return m ? m[0] : null;
}

export function warnDualPluginChildren() {
  process.stderr.write(
    "[ibis-mcp-host] warning: two IDE plugin stdio children must not share one SurrealDB; use IBIS_MCP_URL + personal daemon for a unique shared runtime\n",
  );
}
