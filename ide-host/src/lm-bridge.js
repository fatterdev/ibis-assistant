/**
 * Language-model bridge stub (PR4 fills in vscode.lm / VS Code LM API).
 * Hosts may register a generateContent handler; Go calls the bridge over loopback RPC.
 */

/** @typedef {{ role?: string, parts: { text: string }[] }} Content */
/** @typedef {{ temperature?: number, maxOutputTokens?: number, routeHint?: string }} GenerationConfig */
/** @typedef {{ content: Content, finishReason: string }} Candidate */

/** @type {null | ((contents: Content[], config: GenerationConfig) => Promise<Candidate>)} */
let handler = null;

/**
 * @param {(contents: Content[], config: GenerationConfig) => Promise<Candidate>} fn
 */
export function setGenerateContentHandler(fn) {
  handler = fn;
}

export function isLmBridgeAvailable() {
  return typeof handler === "function";
}

/**
 * @param {Content[]} contents
 * @param {GenerationConfig} [config]
 * @returns {Promise<Candidate>}
 */
export async function generateContent(contents, config = {}) {
  if (!handler) {
    const err = new Error("IDE LM bridge unavailable");
    err.code = "THRESKI_LM_UNAVAILABLE";
    throw err;
  }
  return handler(contents, config);
}
