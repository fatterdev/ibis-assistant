---
description: Open Ibis Assistant Settings UI (hybrid AI, cloud keys, local models)
---

# Ibis: Open Settings

Open or describe the Ibis Assistant settings UI.

1. Call MCP tool `settings_open_ui` on the `ibis` server and show the URL to the user.
2. Open that URL in a browser when possible. In plugin mode the URL may include `?token=` for an ephemeral loopback listener — do not strip the token.
3. If that fails, call `settings_get` and summarize probe + recommendations.
4. Prefer a personal daemon at `http://127.0.0.1:3030/settings/` when the user wants a shared runtime across IDEs.
