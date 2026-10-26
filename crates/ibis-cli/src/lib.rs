//! `ibis-cli` — CLI client library of a running Ibis Assistant server.
//!
//! The binary delegates every non-flag, non-server subcommand to the server
//! CLI; this crate exposes the same surface as a **library** so the
//! `ibis-server` binary can dispatch into it:
//!
//! ```ignore
//! // binary dispatch:
//! match ibis_cli::execute_cli(std::env::args().skip(1), |payload| {
//!     if let Err(error) = tokio::runtime::Runtime::new()
//!         .expect("cli runtime")
//!         .block_on(ibis_cli::call_server_tool(payload))
//!     {
//!         eprintln!("{error}");
//!         std::process::exit(1);
//!     }
//! }) {
//!     Ok(()) => {}
//!     Err(error) => {
//!         eprintln!("{error}");
//!         std::process::exit(1);
//!     }
//! }
//! ```
//!
//! Layering:
//!
//! - [`client`]: clap command tree (`ask`/`ingest`/`ticket`/`pr`/`logs`/
//!   `credentials`/`memory`/`outcome`/`optimize`) building [`ToolCallPayload`]s
//!   — one MCP tool invocation per subcommand.
//! - [`mcp`]: endpoint resolution (config.json `port`, default 3030) and the
//!   Streamable HTTP MCP glue: rmcp client, progress-token meta, progress
//!   notifications on stderr, JSON pretty-printing of tool output.
//!
//! # Deviations from the original CLI
//!
//! - Usage/validation errors surface as [`CliError`] instead of printing to
//!   stdout + `os.Exit(1)` — the binary owns the stream and exit code (the
//!   original printed those to **stdout**; the library caller prints to stderr).
//! - Flag parse failures map to clap's native handling (stderr, exit code 2).
//! - Trailing flags after positionals now parse (`ask "q" --branch main`).
//! - `credentials add --auth-type ssh` requires `--ssh-key` instead of a
//!   dummy `--token` (the server rejects ssh auth without `ssh_private_key`).
//!
//! The MCP client drives **Streamable HTTP only**; the stdio binding is a
//! server-side mode, not a CLI transport.

pub mod client;
pub mod mcp;

pub use client::{execute_cli, Cli, Command};
pub use mcp::{call_mcp_tool, call_server_tool, default_mcp_endpoint};

/// Error surfaced by CLI dispatch, carrying the original CLI's message text.
///
/// the `ExecuteCLI` ended in `os.Exit(1)` on every failure path; this crate
/// returns typed errors so the wave-4 binary owns `process::exit`:
/// print the message (to stderr, unlike the stdout for usage errors) and
/// exit 1 for both variants.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Subcommand usage/validation failure (`fmt.Println` + `os.Exit(1)`,
    /// e.g. ).
    #[error("{0}")]
    Usage(String),
    /// MCP transport/protocol failure (stderr + `os.Exit(1)`).
    #[error("{0}")]
    Transport(String),
}

/// The MCP `tools/call` body built by CLI subcommands.
///
/// `arguments` is a JSON object map like the `map[string]interface{}`; keys
/// serialize sorted (the `encoding/json` also sorts map keys).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCallPayload {
    /// MCP tool name (`ask_project`, `memory_add`, ...).
    pub name: String,
    /// Tool arguments; only keys would send are present.
    #[serde(default)]
    pub arguments: serde_json::Map<String, serde_json::Value>,
}

impl ToolCallPayload {
    /// Build a payload with the given name and pre-built argument map.
    pub fn new(
        name: impl Into<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }
}
