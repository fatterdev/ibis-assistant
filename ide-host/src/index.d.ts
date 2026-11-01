import type { ChildProcess } from "node:child_process";

export const DEFAULT_MCP_URL: string;

export type Backend =
  | { mode: "spawn"; command: string; args: string[]; env: NodeJS.ProcessEnv }
  | { mode: "http"; url: string };

export function resolveBackend(env?: NodeJS.ProcessEnv): Backend;
export function spawnStdioChild(backend?: Backend): ChildProcess;
export function runStdioPassthrough(backend?: Backend): void | Promise<void>;
export function runHttpStdioBridge(url: string): Promise<void>;
export function takeStdioMessages(buf: string | Buffer): { messages: string[]; rest: Buffer };
export function decodeHttpMcpResponse(contentType: string, text: string): string[];
export function callMcpTool(
  backend: Backend,
  toolName: string,
  args?: Record<string, unknown>,
  opts?: { keepAliveMs?: number; timeoutMs?: number },
): Promise<string>;
export const DEFAULT_TOOL_TIMEOUT_MS: number;
export function disposeMcpSessions(): void;
export function extractSettingsURL(text: string): string | null;
export function warnDualPluginChildren(): void;
