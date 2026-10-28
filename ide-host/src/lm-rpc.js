/**
 * Loopback HTTP RPC for Go CursorLMProvider.
 * Start when the IDE registers a vscode.lm / LM API handler via setGenerateContentHandler.
 */

import http from "node:http";
import { generateContent, isLmBridgeAvailable, setGenerateContentHandler } from "./lm-bridge.js";

const DEFAULT_PORT = 39201;

/**
 * @param {{ port?: number }} [opts]
 * @returns {Promise<http.Server>}
 */
export function startLmRpcServer(opts = {}) {
  const port = opts.port ?? Number(process.env.IBIS_LM_BRIDGE_PORT || DEFAULT_PORT);
  const server = http.createServer(async (req, res) => {
    if (req.method === "GET" && req.url === "/health") {
      const ok = isLmBridgeAvailable();
      res.writeHead(ok ? 200 : 503, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ ok, bridge: ok }));
      return;
    }
    if (req.method === "POST" && req.url === "/v1/generate") {
      let raw = "";
      for await (const chunk of req) raw += chunk;
      try {
        const body = JSON.parse(raw || "{}");
        const candidate = await generateContent(body.contents || [], body.config || {});
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ candidate }));
      } catch (err) {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ error: String(err && err.message ? err.message : err) }));
      }
      return;
    }
    res.writeHead(404);
    res.end();
  });
  return new Promise((resolve, reject) => {
    server.listen(port, "127.0.0.1", () => resolve(server));
    server.on("error", reject);
  });
}

/**
 * Wire vscode.lm / VS Code Language Model API into the bridge handler.
 * @param {any} vscode
 * @param {{ family?: string }} [options] Optional model family filter (empty = first available)
 * @returns {Promise<boolean>}
 */
export async function attachVscodeLm(vscode, options = {}) {
  const lm = vscode && vscode.lm;
  if (!lm || typeof lm.selectChatModels !== "function") {
    return false;
  }
  const family = (options.family || "").trim();
  setGenerateContentHandler(async (contents, config) => {
    const models = await lm.selectChatModels(family ? { family } : {});
    if (!models || models.length === 0) {
      throw new Error("no IDE language models available");
    }
    const model = models[0];
    const messages = (contents || []).map((c) => {
      const text = (c.parts || []).map((p) => p.text || "").join("");
      const role = c.role === "model" || c.role === "assistant" ? "assistant" : "user";
      return new vscode.LanguageModelChatMessage(
        role === "assistant"
          ? vscode.LanguageModelChatMessageRole.Assistant
          : vscode.LanguageModelChatMessageRole.User,
        text,
      );
    });
    const tokenSource = new vscode.CancellationTokenSource();
    const response = await model.sendRequest(messages, {
      justification: "Ibis Assistant reasoning via IDE-provided models",
    }, tokenSource.token);
    let out = "";
    for await (const chunk of response.text) {
      out += chunk;
    }
    return {
      content: { role: "model", parts: [{ text: out }] },
      finishReason: "stop",
    };
  });
  return true;
}

export { DEFAULT_PORT as LM_BRIDGE_DEFAULT_PORT };
