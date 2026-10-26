//! ibis-search — knowledge graph + temporal memory core (
//! `internal/search`): hybrid ask, typed memories, git-bound claims, HOLDAt
//! ancestry, as-of reads, knowledge diff, the agentic ReAct loop and the
//! degraded (clone-only) state machine.
//!
//! Behavioral baseline with the sanctioned deltas:
//! - D7 (graph queries `source_file`), F-002 (every
//!   content-bearing query binds variables), claim-commit pointer
//!   notes, at_ref 3-slot claim_file floor, degraded-claim-read signal and
//!   the hot-ref code byte cache over `file_at_commit`.
//!
//! Degraded mode is a first-class state (D8): [`SearchService::db`] is
//! `Option<Arc<dyn Executor>>` and every read path branches exhaustively on
//! it — `None` never errors, it serves the git-native channels.

pub mod agentic;
pub mod commit_meaning;
pub mod error;
pub mod graph;
pub mod memory;
pub mod retrieval_boost;
pub mod rows;
pub mod search;

#[cfg(test)]
pub(crate) mod test_support;

pub use error::{Result, SearchError};
pub use search::{ContextData, SearchResult, SearchService};

/// SurrealDB table/edge names used by this crate (`internal/schema
/// constants; identical set to `ibis_db::schema::names`).
pub mod names {
    pub use ibis_db::schema::names::*;
}

/// The git notes ref for claim pointer notes (the claim-store addendum).
pub use ibis_git::NOTES_REF;

/// The request identity a search call executes under — the 's
/// `context.Value(auth.IbisActorContextKey)` plumbing
/// Carried explicitly instead of through a
/// context map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActorContext {
    /// `X-Ibis-Actor` (trusted only from loopback peers — see `ibis_core::auth`).
    pub actor: Option<String>,
    /// Marks requests whose peer is not loopback: personal-memory
    /// authorization must ignore claimed identity on those requests.
    pub remote_untrusted: bool,
}

impl ActorContext {
    /// Local/stdio anonymous caller (no actor, loopback-trusted).
    pub const LOCAL: ActorContext = ActorContext {
        actor: None,
        remote_untrusted: false,
    };

    /// Context carrying a claimed actor (test helper `memoryActorContext`).
    pub fn with_actor(actor: impl Into<String>) -> Self {
        ActorContext {
            actor: Some(actor.into()),
            remote_untrusted: false,
        }
    }

    /// Remote (non-loopback) HTTP peer — actor claims are untrusted.
    pub fn remote_untrusted() -> Self {
        ActorContext {
            actor: None,
            remote_untrusted: true,
        }
    }

    /// `auth.ResolveActor(ctx, explicit): request actor, then explicit
    /// fallback, then "unknown".
    pub fn resolve_actor(&self, explicit: &str) -> String {
        ibis_core::auth::resolve_actor(self.actor.as_deref(), explicit)
    }

    /// `auth.ResolveTrustedActor(ctx, explicit): identity that may
    /// authorize personal memories; remote peers and bare "unknown" resolve
    /// to "".
    pub fn resolve_trusted_actor(&self, explicit: &str) -> String {
        ibis_core::auth::resolve_trusted_actor(
            self.actor.as_deref(),
            explicit,
            self.remote_untrusted,
        )
    }

    /// `auth.ActorIsRemoteUntrusted(ctx).
    pub fn actor_is_remote_untrusted(&self) -> bool {
        self.remote_untrusted
    }
}
