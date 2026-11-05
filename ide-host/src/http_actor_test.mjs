import assert from "node:assert/strict";
import { createServer } from "node:http";
import { callMcpTool } from "./index.js";

/**
 * F4 §B.4 header-injection test on the http path (ide-host): the actor rides
 * every postJsonRpc request — initialize, notifications/initialized and
 * tools/call — and no header is sent without one.
 *
 * Fake Streamable HTTP MCP server: records the X-Ibis-Actor header of every
 * POST and answers the ide-host handshake shape (JSON body per request;
 * notifications get the Streamable HTTP 202 empty acknowledgement).
 */
const seen = [];
const server = createServer((req, res) => {
  let body = "";
  req.on("data", (chunk) => {
    body += chunk;
  });
  req.on("end", () => {
    seen.push(String(req.headers["x-ibis-actor"] || ""));
    const msg = JSON.parse(body);
    if (msg.id == null) {
      res.writeHead(202);
      res.end();
      return;
    }
    res.writeHead(200, { "Content-Type": "application/json" });
    if (msg.method === "initialize") {
      res.end(
        JSON.stringify({
          jsonrpc: "2.0",
          id: msg.id,
          result: { protocolVersion: "2024-11-05", capabilities: {} },
        }),
      );
      return;
    }
    if (msg.method === "tools/call") {
      res.end(
        JSON.stringify({
          jsonrpc: "2.0",
          id: msg.id,
          result: {
            content: [{ type: "text", text: "pong:" + msg.params.name }],
          },
        }),
      );
      return;
    }
    res.end(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: {} }));
  });
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const url = `http://127.0.0.1:${server.address().port}/mcp`;

try {
  // 1. Actor configured: the header is present on all three requests.
  {
    seen.length = 0;
    const text = await callMcpTool(
      { mode: "http", url, actor: "alice" },
      "memory_list",
      {},
    );
    assert.equal(text, "pong:memory_list");
    assert.equal(seen.length, 3, `three requests expected: ${JSON.stringify(seen)}`);
    assert.deepEqual(seen, ["alice", "alice", "alice"]);
  }

  // 2. No actor: no header at all (anonymous local semantics).
  {
    seen.length = 0;
    await callMcpTool({ mode: "http", url }, "memory_list", {});
    assert.equal(seen.length, 3, `three requests expected: ${JSON.stringify(seen)}`);
    assert.deepEqual(seen, ["", "", ""]);
  }
} finally {
  server.close();
}

console.log("ide-host http actor header ok");