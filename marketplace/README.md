# ibis-marketplace

Agent-plugin marketplace for **Ibis Assistant** — Agent Plugins 1.0 with the
Claude-Code-compatible marketplace schema. The plugin definition stays in the
[`fatterdev/ibis-assistant`](https://github.com/fatterdev/ibis-assistant)
repository (`plugin/`); this repo only lists it, so there is one source of
truth and no vendored drift.

[![Install plugin in VS Code](https://img.shields.io/badge/VS_Code-Install_Plugin-0098FF)](vscode://chat-plugin/install?source=fatterdev/ibis-marketplace&plugin=ibis "One click installs the ibis agent plugin in VS Code")

## Install the ibis plugin

Deep link (badge above):

```text
vscode://chat-plugin/install?source=fatterdev/ibis-marketplace&plugin=ibis
```

Or from source: **Chat: Install Plugin from Source** → `fatterdev/ibis-assistant`
(folder `plugin/`) for development installs (`chat.pluginLocations`).

## Team recommendations

Add to the **project** repository as `.github/copilot/settings.json` so team
members are offered the plugin:

```json
{
  "extraKnownMarketplaces": {
    "ibis": { "source": { "source": "github", "repo": "fatterdev/ibis-marketplace" } }
  },
  "enabledPlugins": { "ibis@ibis": true }
}
```

## Structure

```text
marketplace.json   # marketplace manifest (plugins[]: ibis -> github fatterdev/ibis-assistant@plugin)
README.md
```

If a client requires the plugin folder to live inside the marketplace repo,
mirror `plugin/` into `plugins/ibis/` at publish time and switch the
`source` entry to `"./plugins/ibis"`.