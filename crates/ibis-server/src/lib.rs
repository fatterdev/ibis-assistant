//! ibis-server library surface — the `ibis-assistant` binary's modules,
//! exposed so the contract-gate integration tests (`tests/tool_contract_test.rs`,
//! `tests/ticket_args_test.rs`) can drive the D3 registry and the arg
//! helpers (the / were
//! `package main` unit tests; the Rust port reaches the same code through
//! the library target).
//!
//! The daemon/service lane appends its modules here (`pub mod service;` /
//! `pub mod daemon_*;`) — see the `dispatch_service_command` seam in the
//! binary.

pub mod args;
pub mod assembly;
pub mod collision;
pub mod mcp_server;
pub mod progress;
pub mod registry;
pub mod state;
pub mod tools;
pub mod tools_memory;
pub mod tools_ticket;

// --- Lane N-service append point: PID-file daemon + Windows SCM program
// --- (D10), with the F-011 one-pass elevation for install/uninstall.
pub mod daemon;
pub mod service;
