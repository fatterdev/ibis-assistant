import assert from "node:assert/strict";
import { takeStdioMessages, decodeHttpMcpResponse, extractSettingsURL } from "./index.js";

function assertRestEmpty(rest) {
  assert.ok(Buffer.isBuffer(rest));
  assert.equal(rest.length, 0);
}

function assertRestEquals(rest, expected) {
  assert.ok(Buffer.isBuffer(rest));
  assert.equal(rest.toString("utf8"), expected);
}

// Newline-delimited initialize must parse without waiting for \r\n\r\n
{
  const init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}';
  const { messages, rest } = takeStdioMessages(`${init}\n`);
  assert.equal(messages.length, 1);
  assert.equal(messages[0], init);
  assertRestEmpty(rest);
}

// Partial NDJSON waits for newline; remainder stays Buffer
{
  const { messages, rest } = takeStdioMessages('{"jsonrpc":"2.0"');
  assert.equal(messages.length, 0);
  assertRestEquals(rest, '{"jsonrpc":"2.0"');
}

// Content-Length framing still works
{
  const body = '{"jsonrpc":"2.0","id":2,"result":{}}';
  const framed = `Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`;
  const { messages, rest } = takeStdioMessages(framed);
  assert.equal(messages.length, 1);
  assert.equal(messages[0], body);
  assertRestEmpty(rest);
}

// Content-Length is UTF-8 bytes, not JS string length (non-ASCII body)
{
  const body = '{"jsonrpc":"2.0","id":3,"result":{"msg":"café"}}';
  const byteLen = Buffer.byteLength(body, "utf8");
  assert.ok(byteLen > body.length); // multi-byte é
  const framed = `Content-Length: ${byteLen}\r\n\r\n${body}`;
  const { messages, rest } = takeStdioMessages(framed);
  assert.equal(messages.length, 1);
  assert.equal(messages[0], body);
  assertRestEmpty(rest);
}

// Streaming split inside multi-byte UTF-8 (é = C3 A9): keep raw bytes across chunks
{
  const body = '{"jsonrpc":"2.0","id":4,"result":{"msg":"café"}}';
  const framed = Buffer.from(
    `Content-Length: ${Buffer.byteLength(body, "utf8")}\r\n\r\n${body}`,
    "utf8",
  );
  const cafeOffset = framed.indexOf(Buffer.from("café", "utf8"));
  assert.ok(cafeOffset > 0);
  // Split after the first byte of é (0xC3), before 0xA9
  const splitAt = cafeOffset + "caf".length + 1;
  const first = framed.subarray(0, splitAt);
  const second = framed.subarray(splitAt);

  const partial = takeStdioMessages(first);
  assert.equal(partial.messages.length, 0);
  assert.ok(Buffer.isBuffer(partial.rest));
  assert.ok(partial.rest.length > 0);
  // Must preserve the leading 0xC3 of é, not replace with U+FFFD
  assert.equal(partial.rest[partial.rest.length - 1], 0xc3);

  const combined = takeStdioMessages(Buffer.concat([partial.rest, second]));
  assert.equal(combined.messages.length, 1);
  assert.equal(combined.messages[0], body);
  assertRestEmpty(combined.rest);
}

// SSE response yields each data: JSON payload
{
  const sse =
    'event: message\ndata: {"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n\n' +
    'data: {"jsonrpc":"2.0","method":"notifications/message"}\n\n';
  const msgs = decodeHttpMcpResponse("text/event-stream", sse);
  assert.equal(msgs.length, 2);
  assert.ok(msgs[0].includes('"id":1'));
}

// application/json is one NDJSON-ready message
{
  const msgs = decodeHttpMcpResponse("application/json", '{"jsonrpc":"2.0","id":1,"result":{}}');
  assert.equal(msgs.length, 1);
}

{
  const url = extractSettingsURL("Open Settings UI: http://127.0.0.1:54321/settings/?token=abc\n(extra)");
  assert.equal(url, "http://127.0.0.1:54321/settings/?token=abc");
}

console.log("ide-host bridge framing ok");
