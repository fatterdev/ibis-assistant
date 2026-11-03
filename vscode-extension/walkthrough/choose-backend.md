Two modes:

- **spawn** (default) — each window starts its own `ibis -mode stdio -runtime-mode plugin` child with a plugin data dir.
- **attach** — share one personal runtime (`ibis run` on `http://127.0.0.1:3030/mcp`) between Cursor and VS Code.

[Pick the runtime mode](command:ibis.openExtensionSettings)