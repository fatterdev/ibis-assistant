# Team server runbook (F4 milestone B)

One shared Ibis runtime for a team: `ibis serve --team` (or the official
compose image) turns the personal daemon into a shareable server. SurrealDB
stays the source of truth, `visibility` stays sharing-first (no row-level
ACL, no auth tokens — the trust boundary is **network reachability**), and
the `.ibis/memory` projection stays regenerable output.

Reference: [F4 design](../superpowers/2026-11-01-memory-view-team-server-design.md)
(§B.1 serve profile, §B.2 compose, §B.3 this runbook, §B.4 actor identity,
§B.5 consolidation ownership, §B.6 sidecar reconciliation).

## 1. Topology and guardrails

- One daemon per team store. Clients attach over Streamable HTTP `/mcp`; they
  never spawn sidecars of their own against the team store.
- The SurrealDB sidecar is spawned by the runtime, bound to `127.0.0.1:8000`
  inside the host/container (`db_url` default `ws://localhost:8000/rpc`) and
  is never published. Only port **3030** is exposed.
- No authentication: anyone who can reach port 3030 can read team memories
  and write with any actor. Publish the port on a trusted network only.
- On startup the runtime reconciles the sidecar ledger
  (`sidecar_ledger.json` beside the data file): stale sidecars whose owner
  ibis is gone are terminated before a new one starts (F4 §B.6).
- The `[TEAM]` startup banner states the four operational facts: bind
  address, actor-header reminder, no-auth boundary, projection on.

## 2. Install path A — docker compose (recommended)

From the repository root (the compose file lives in `deploy/`):

```bash
docker compose -f deploy/docker-compose.yml up -d --build
docker compose -f deploy/docker-compose.yml logs -f ibis
```

What the image does:

- builds `ibis` from source (release profile) on `rust:1-bookworm`;
- ships the **pinned** SurrealDB `v3.3.5` sidecar (same pin as the CI
  retrieval-eval job) and ast-grep `0.51.1` beside the binary, with
  `DB_AUTO_UPDATE=false` — the image is the only version surface, the
  runtime never downloads sidecars at boot;
- copies `sgconfig.yml` + `rules/` beside the executable (ast-grep chunkers
  resolve them there, same layout as the release distribution);
- runs as non-root (`uid 10001`), with `/data` as the volume mount point and
  a healthcheck against the discovery endpoint `GET /`;
- default command: `serve --team --bind 0.0.0.0 --data-dir /data/ibis-team.db
  --consolidate`. The explicit `--bind 0.0.0.0` is required: the container
  must be reachable from outside, while the published host port is the
  actual reachability boundary. `--consolidate` implements the team
  recommendation of §6 — remove it from the image `CMD` (or run a custom
  `command:` in compose) to start the server without consolidation.

Inside a container the kernel job-object semantics do not apply: the compose
`restart: unless-stopped` policy owns crash recovery, and `init: true` reaps
the sidecar on shutdown.

`deploy/docker-compose.yml` documents the optional mounts/env: a team
`config.json` (read-write if the settings UI should persist changes),
`EMBEDDING_URL` pointing at the team Ollama (the container-internal
`127.0.0.1:11434` default is unreachable — AI features degrade to
non-embedding channels otherwise, the server still boots), `LOG_FILE`
(`/data/ibis.log` to persist; docker logs holds stdout), and
`SURREAL_USER`/`SURREAL_PASS` to move off the `root`/`root` defaults.

GHCR publishing is deferred (publish gate, master work queue §3.6): until
then the image is built locally, and `deploy/docker-compose.yml` carries the
commented `image:` reference.

## 3. Install path B — binary + service

Download a release distribution (`ibis` + `mcp-bridge` + `rules/` +
`sgconfig.yml`), or build it: `cargo build --release -p ibis-server`.

Keep the team store **out of personal plugin data**: pass an absolute
`--data-dir` (relative paths resolve against the executable directory, which
is the personal-install layout). The sidecar ledger lives beside the data
file, so the data directory is the unit that travels together.

Linux systemd unit (`/etc/systemd/system/ibis-team.service`):

```ini
[Unit]
Description=Ibis Assistant team server
After=network-online.target

[Service]
User=ibis
ExecStart=/opt/ibis/ibis serve --team --bind 192.168.1.10 --data-dir /var/lib/ibis-team/ibis-team.db --consolidate
Restart=on-failure
# Sidecar reconciliation (F4 §B.6) cleans stale SurrealDB instances on restart.

[Install]
WantedBy=multi-user.target
```

Windows: `ibis install` registers the personal daemon service without the
team flags — for the team profile use NSSM with the full command line:

```bat
nssm install IbisTeam "C:\tools\ibis\ibis.exe" "serve --team --bind 192.168.1.10 --data-dir C:\ibis-team\ibis-team.db --consolidate"
nssm start IbisTeam
```

Without a service manager, `ibis start` / `ibis stop` run/stop a background
daemon (D10 PID-file lifecycle) with the same flags appended.

### Flags

| Flag | Meaning |
|---|---|
| `--team` | team profile (see below) |
| `--bind <addr>` | HTTP listener address; empty → personal/plugin `127.0.0.1`, server `0.0.0.0`. **LAN exposure is an explicit `--bind <lan-address>`** |
| `--data-dir <path>` | SurrealDB data file (surrealkv); use absolute paths on team hosts |
| `--consolidate` | force the daemon-owned consolidation loop on (overrides `ai.consolidation.enabled: false`) |
| `--port <n>` | HTTP port (default 3030; `config.json` `port` also drives CLI clients) |
| `-scan false` | skip repository discovery at boot (repos come from a mounted `config.json` instead) |
| `--config <path>` | explicit `config.json` |

The `--team` profile only fills **unset** knobs: runtime mode → `server`
(personal claims `db_only`), bind → `127.0.0.1`, `.ibis/memory` projection
on (a config `memory.projection_enabled: false` survives). Explicit flags
and config choices win over the profile.

### Environment variables (subset)

| Env | Meaning |
|---|---|
| `BIND_ADDRESS` / `PORT` | same as `--bind` / `--port` |
| `LOG_LEVEL`, `LOG_FILE` | logging (absolute `LOG_FILE` to persist beside the data) |
| `DB_AUTO_UPDATE` | the image sets `false` (pinned sidecars); bare installs default `true` |
| `SURREAL_URL` / `SURREAL_USER` / `SURREAL_PASS` | embedded DB connection/credentials |
| `EMBEDDING_URL` / `EMBEDDING_MODEL` / `EMBEDDING_PROVIDER` | AI embedding endpoint (Ollama host on the LAN) |
| `GIT_TOKEN` | credential fallback for git-backed ingest |
| `DISCOVERY_ROOT`, `LOGS_ROOT` | discovery/ingest roots |
| `IBIS_CLAIMS_PERSONAL_MODE` | personal-claims policy override: `db_only` / `stub` / `in_repo` (the VSIX knob `ibis.claims.personalMode` forwards here for spawned children and Start Daemon) |

## 4. Actor identity (F4 §B.4)

Attribution on shared rows uses one of, in order: the `X-Ibis-Actor` header,
the tool's `actor` argument, `"unknown"`.

- **`X-Ibis-Actor` is trusted only from loopback peers** (X-Forwarded-For is
  ignored so proxies cannot mint an actor). A LAN team server therefore
  attributes remote clients through the tool `actor` argument; requests
  without one land as `unknown`.
- Remote (non-loopback) peers are always marked remote-untrusted:
  **personal-memory authorization stays team-only** for them — they never
  match other people's personal rows, regardless of claimed identity.
- Credential headers (`X-Redmine-API-Key`, `X-Jira-*`, `X-Azure-DevOps-*`,
  `X-Git-Token`) are honored from any peer — one more reason to keep the
  port inside a trusted network.
- Client-side automation of the header (`ibis.identity.actor` setting,
  CLI `IBIS_ACTOR`) lands with the F4-A sub-phases; until then pass the
  `actor` argument on memory writes from remote clients (the curation tools
  already accept it).

## 5. Client onboarding

- **VS Code (VSIX)** — set `ibis.runtime.mode: attach` and
  `ibis.runtime.mcpUrl: http://<server>:3030/mcp` (equivalently the
  `IBIS_MCP_URL` environment variable while the mode setting is untouched).
  F4-A1 flips the default to attach against the personal daemon; the team
  URL keeps working unchanged.
- **Cursor** — `mcp.json` http variant (also printed by the discovery
  document on `GET /`):

  ```json
  { "mcpServers": { "ibis": { "url": "http://<server>:3030/mcp" } } }
  ```

- **Claude Desktop** — the bundled `mcp-bridge` binary (stdio → Streamable
  HTTP): `mcp-bridge -url http://<server>:3030/mcp`.
- **Plugin (stdio default)** — the plugin keeps its stdio child
  (`ibis -mode stdio -runtime-mode plugin`, works daemon-less). On team
  setups switch `plugin/mcp.json` to the http variant above so the plugin
  attaches to the team runtime instead of spawning its own.
- **CLI client** (`ibis memory …`) — resolves `127.0.0.1:<port>` from a
  local `config.json`: it works on the server host itself; pointing the CLI
  at a remote team server is a known gap, not a supported path.

## 6. Consolidation runs in the daemon only (F4 §B.5)

- The consolidation loop is a daemon-side, **opt-in** capability:
  `--consolidate` (wins over config) or `ai.consolidation.enabled: true` in
  `config.json`; `ai.consolidation.interval_secs` tunes the cadence (60s
  floor, default 1h). Client surfaces (stdio/plugin/IDE-host children) never
  start it.
- Recommended **ON** for the team server (both install paths default to
  `--consolidate`): with a single runtime the duplicate-supersede risk
  disappears, and the backlog drains at boot plus once per cadence — one
  `consolidate_loop` pass per project that still holds `current`
  observations, discovered store-side (team clients can capture for repos
  unknown to the server config), strictly sequential.
- The weekly `memory_consolidate` automation
  (`plugin/automations/weekly-consolidate.automation.md`) stays the explicit
  human trigger for reviewed curation; the daemon loop complements it, it
  does not replace the review step.

## 7. Data lifecycle

- **What lives where**: the data directory holds the surrealkv data file
  (`ibis-team.db`) + `sidecar_ledger.json`; inside the container both are on
  the `ibis-team-data` volume mounted at `/data`.
- **Backup** — stop the server first (`docker compose down`, `systemctl
  stop ibis-team`, `nssm stop IbisTeam`): the graceful stop closes the DB,
  waits, terminates the sidecar and unregisters the ledger entry. Then copy
  the data directory (file + ledger).
- **Restore** — stop, replace the files, start. The ledger reconciles stale
  sidecars on the next boot.
- **Upgrade** — rebuild/pull the image, `docker compose up -d --build`; the
  volume persists. Version pins (rust base, SurrealDB, ast-grep) bump
  together with the runtime release; SurrealDB major bumps deserve a backup
  first (data-file format).

## 8. Verification checklist (gate R7 manual scenario)

1. `docker compose logs ibis` (or `journalctl -u ibis-team`) shows the four
   `[TEAM]` banner lines and `Starting Streamable HTTP at <bind>:3030`.
2. `curl http://<server>:3030/` returns the discovery document
   (`"status": "online"`); the image healthcheck uses the same endpoint.
3. Two real clients attach (VSIX attach mode + plugin http variant); a
   `memory_add` from each shows the intended attribution on the shared rows
   (§4 semantics).
4. Exactly one consolidation loop runs (one daemon) — no duplicate passes
   from client surfaces.
5. Restart the daemon: the ledger reconciliation cleans stale sidecars and
   the store survives (data file + ledger on the volume/data dir).

## 9. Security notes

- No auth by design: do not expose port 3030 beyond the trusted network
  (no direct internet exposure, no untrusted shared hosts).
- The embedded SurrealDB binds loopback inside the host/container — the
  published surface is only the HTTP listener.
- Keep host firewalls/DNS names consistent with the `--bind` address; the
  banner prints the effective bind so drift is visible at boot.