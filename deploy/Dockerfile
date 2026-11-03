# syntax=docker/dockerfile:1
#
# Ibis Assistant team server image (F4 §B.2) — docs/team_server.md is the runbook.
#
# Build from the repository root:
#   docker build -f deploy/Dockerfile -t ibis-team:0.2.0 .
#
# Pinned components (bump them together with the runtime release):
#   - builder toolchain : rust:1-bookworm
#   - SurrealDB sidecar : v3.3.5 (release tarball; the CI retrieval-eval job
#     pins the same version)
#   - ast-grep          : 0.51.1
# The sidecars ship beside the ibis binary and DB_AUTO_UPDATE=false keeps the
# runtime from probing GitHub at boot: the image is the only version surface.
# GHCR publishing is deferred (master work queue §3.6, publish gate).

ARG RUST_IMAGE=rust:1-bookworm
ARG SURREAL_VERSION=3.3.5
ARG SG_VERSION=0.51.1

# ---------------------------------------------------------------------------
# Build stage.
#
# Layer 1 holds only the workspace manifests with stub sources: cargo resolves
# and builds the whole dependency tree once, and that cache survives any
# source edit. Layer 2 replaces the stubs with the real crates and rebuilds
# just the workspace (path-dependency) crates.
# ---------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS build
WORKDIR /build
ARG IBIS_COMMIT=none
ENV IBIS_COMMIT=${IBIS_COMMIT}

COPY Cargo.toml Cargo.lock ./
COPY crates/ibis-core/Cargo.toml            crates/ibis-core/Cargo.toml
COPY crates/ibis-logger/Cargo.toml          crates/ibis-logger/Cargo.toml
COPY crates/ibis-git/Cargo.toml             crates/ibis-git/Cargo.toml
COPY crates/ibis-db/Cargo.toml              crates/ibis-db/Cargo.toml
COPY crates/ibis-ai/Cargo.toml              crates/ibis-ai/Cargo.toml
COPY crates/ibis-ingest-git/Cargo.toml      crates/ibis-ingest-git/Cargo.toml
COPY crates/ibis-ingest-code/Cargo.toml     crates/ibis-ingest-code/Cargo.toml
COPY crates/ibis-ingest-logs/Cargo.toml     crates/ibis-ingest-logs/Cargo.toml
COPY crates/ibis-ingest-dynamic/Cargo.toml  crates/ibis-ingest-dynamic/Cargo.toml
COPY crates/ibis-ingest-redmine/Cargo.toml  crates/ibis-ingest-redmine/Cargo.toml
COPY crates/ibis-search/Cargo.toml          crates/ibis-search/Cargo.toml
COPY crates/ibis-ticketing/Cargo.toml       crates/ibis-ticketing/Cargo.toml
COPY crates/ibis-repopr/Cargo.toml          crates/ibis-repopr/Cargo.toml
COPY crates/ibis-optimization/Cargo.toml    crates/ibis-optimization/Cargo.toml
COPY crates/ibis-settings/Cargo.toml        crates/ibis-settings/Cargo.toml
COPY crates/ibis-wizard/Cargo.toml          crates/ibis-wizard/Cargo.toml
COPY crates/ibis-cli/Cargo.toml             crates/ibis-cli/Cargo.toml
COPY crates/ibis-httpserver/Cargo.toml      crates/ibis-httpserver/Cargo.toml
COPY crates/ibis-server/Cargo.toml          crates/ibis-server/Cargo.toml
COPY crates/mcp-bridge/Cargo.toml           crates/mcp-bridge/Cargo.toml
COPY crates/xtask/Cargo.toml                crates/xtask/Cargo.toml

# Stub targets satisfy the workspace resolution and the path-dependency
# graph; the real sources overwrite them in the next layer.
RUN set -eu; \
    libs="ibis-core ibis-logger ibis-git ibis-db ibis-ai \
          ibis-ingest-git ibis-ingest-code ibis-ingest-logs ibis-ingest-dynamic \
          ibis-ingest-redmine ibis-search ibis-ticketing ibis-repopr \
          ibis-optimization ibis-settings ibis-wizard ibis-cli ibis-httpserver \
          ibis-server"; \
    for m in $libs mcp-bridge xtask; do mkdir -p "crates/$m/src"; done; \
    for m in $libs; do echo "pub fn __stub() {}" > "crates/$m/src/lib.rs"; done; \
    echo "fn main() {}" > crates/ibis-server/src/main.rs; \
    echo "fn main() {}" > crates/mcp-bridge/src/main.rs; \
    echo "fn main() {}" > crates/xtask/src/main.rs; \
    cargo build --release -p ibis-server

# Real sources (includes the ibis-settings assets/web directory that
# `include_dir!` embeds) over the stubs; only the workspace crates rebuild.
COPY crates ./crates
RUN cargo build --release -p ibis-server

# ---------------------------------------------------------------------------
# ast-grep sidecar (pinned): the release zip ships `sg` + `ast-grep`; the
# runtime resolves `sg` beside the ibis executable (ast_grep_bin_path).
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS sg
ARG TARGETARCH
ARG SG_VERSION
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl unzip; \
    rm -rf /var/lib/apt/lists/*; \
    case "$TARGETARCH" in \
      amd64) sg_arch=x86_64 ;; \
      arm64) sg_arch=aarch64 ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/sg.zip \
      "https://github.com/ast-grep/ast-grep/releases/download/${SG_VERSION}/app-${sg_arch}-unknown-linux-gnu.zip"; \
    mkdir -p /out; \
    unzip -j /tmp/sg.zip sg ast-grep -d /out; \
    chmod 0755 /out/sg /out/ast-grep; \
    rm /tmp/sg.zip

# ---------------------------------------------------------------------------
# SurrealDB sidecar (pinned): the release tarball is the same source the
# runtime's own installer fetches (`surreal-v<ver>.linux-<arch>.tgz`) and it
# matches the SurrealDB pin of the CI retrieval-eval job. The runtime spawns
# it embedded (127.0.0.1:<db_port> inside the container, never published).
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS surreal
ARG TARGETARCH
ARG SURREAL_VERSION
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl; \
    rm -rf /var/lib/apt/lists/*; \
    case "$TARGETARCH" in \
      amd64) sarch=amd64 ;; \
      arm64) sarch=arm64 ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/surreal.tgz \
      "https://github.com/surrealdb/surrealdb/releases/download/v${SURREAL_VERSION}/surreal-v${SURREAL_VERSION}.linux-${sarch}.tgz"; \
    mkdir -p /out; \
    tar -xzf /tmp/surreal.tgz -C /out; \
    chmod 0755 /out/surreal; \
    rm /tmp/surreal.tgz

# ---------------------------------------------------------------------------
# Runtime: slim debian + the ibis binary + pinned sidecars + rule assets.
#
# git            : git-backed ingest paths shell out to the git CLI (ibis-git)
# curl           : the healthcheck
# ca-certificates: TLS toward Ollama / cloud providers
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build   /build/target/release/ibis /opt/ibis/ibis
COPY --from=surreal /surreal                    /opt/ibis/surreal
COPY --from=sg      /out/sg                     /opt/ibis/sg
COPY --from=sg      /out/ast-grep               /opt/ibis/ast-grep
# ast-grep rule assets: the chunkers resolve `sgconfig.yml` + `rules/` beside
# the executable (same layout as the release distribution).
COPY sgconfig.yml /opt/ibis/sgconfig.yml
COPY rules/       /opt/ibis/rules/

# The pinned sidecars stay pinned: no GitHub update probes at boot.
ENV DB_AUTO_UPDATE=false

# /data is the volume mount point (SurrealDB data file + sidecar ledger);
# /opt/ibis stays writable for surreal.log and the rule-asset sync.
RUN useradd --system --uid 10001 ibis \
 && mkdir -p /data \
 && chown -R ibis:ibis /opt/ibis /data

USER ibis
WORKDIR /opt/ibis
VOLUME /data
EXPOSE 3030

# Liveness: the discovery endpoint answers on the same listener as /mcp.
HEALTHCHECK --interval=30s --timeout=5s --start-period=45s --retries=5 \
  CMD ["curl", "-fsS", "http://127.0.0.1:3030/"]

# Team profile (F4 §B.1): server runtime semantics, .ibis/memory projection
# on. `--bind 0.0.0.0` overrides the loopback default — the container must be
# reachable from outside; the trust boundary stays network reachability (no
# auth: publish the port only on a trusted network). `--consolidate` is the
# F4 §B.5 team recommendation (the daemon owns the loop; drop it from CMD to
# run without consolidation). Absolute --data-dir per the runbook.
ENTRYPOINT ["/opt/ibis/ibis"]
CMD ["serve", "--team", "--bind", "0.0.0.0", "--data-dir", "/data/ibis-team.db", "--consolidate"]