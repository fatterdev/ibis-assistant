Three modes (`ibis.runtime.mode`):

- **auto** (default) — attach to the always-on personal daemon (`http://127.0.0.1:3030/mcp`). When it is offline the status bar shows **Ibis daemon offline**; run **Ibis Assistant: Start Daemon** (or `ibis start` in a terminal) to launch it detached.
- **spawn** — legacy: each window starts its own `ibis -mode stdio -runtime-mode plugin` child with a plugin data dir.
- **attach** — pin a custom endpoint and share one personal runtime between Cursor and VS Code.

[Pick the runtime mode](command:ibis.openExtensionSettings)