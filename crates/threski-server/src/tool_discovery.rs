//! F0.6 dynamic tool discovery — the session-peer broadcast set for
//! `notifications/tools/list_changed` plus the flip detector.
//!
//! The `ticket_*` group is announced only when a ticketing provider is
//! configured (`AppState::ticketing_configured`). When a config change
//! (`settings_apply` tool, settings HTTP API) hot-reloads the ticketing
//! service and that gate flips, [`ToolDiscovery::publish_ticketing_visibility`]
//! sends `notifications/tools/list_changed` to every registered session peer
//! so clients re-run `tools/list` against the new surface.
//!
//! Peer registration happens on the session `initialized` notification (the
//! initialize lifecycle stdio and Streamable-HTTP clients use). The
//! 2026-07-28 `server/discover` lifecycle has no session notification — those
//! clients rely on the `ttl_ms: 0` cache hint instead and simply re-list.
//! Dead peers (closed sessions) are pruned on first failed send.
//!
//! Concurrency note: peers are keyed by the shared peer-info allocation
//! address (`crate::resources::peer_connection_id`) — the same identity the
//! F0.4 resource subscriptions use, so re-registrations replace instead of
//! duplicating.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rmcp::service::Peer;
use rmcp::RoleServer;

/// Registered session peers + the ticketing visibility the tool list last
/// carried.
pub struct ToolDiscovery {
    peers: Mutex<HashMap<u64, Peer<RoleServer>>>,
    published: AtomicBool,
}

impl ToolDiscovery {
    /// Starts tracking `published` (the visibility at assembly time).
    pub fn new(published: bool) -> Self {
        ToolDiscovery {
            peers: Mutex::new(HashMap::new()),
            published: AtomicBool::new(published),
        }
    }

    /// Registers the session peer (replaces any earlier registration from
    /// the same connection).
    pub fn register(&self, peer: &Peer<RoleServer>) {
        let conn = crate::resources::peer_connection_id(peer);
        self.peers.lock().unwrap().insert(conn, peer.clone());
    }

    /// Registered peer count (diagnostics/protocol-gate surface).
    pub fn peer_count(&self) -> usize {
        self.peers.lock().unwrap().len()
    }

    /// The published ticketing visibility.
    pub fn published_visibility(&self) -> bool {
        self.published.load(Ordering::SeqCst)
    }

    /// Fires `notifications/tools/list_changed` on every registered peer
    /// when `visible` differs from the published state, recording it as the
    /// new state. Returns whether a change was announced (no-op — and no
    /// notification — when the visibility did not flip). Send failures prune
    /// the dead peer and never fail the caller.
    pub async fn publish_ticketing_visibility(&self, visible: bool) -> bool {
        if self.published.load(Ordering::SeqCst) == visible {
            return false;
        }
        self.published.store(visible, Ordering::SeqCst);
        let peers: Vec<Peer<RoleServer>> = self.peers.lock().unwrap().values().cloned().collect();
        for peer in peers {
            if peer.notify_tool_list_changed().await.is_err() {
                let conn = crate::resources::peer_connection_id(&peer);
                self.peers.lock().unwrap().remove(&conn);
                tracing::debug!("tools/list_changed delivery failed; peer pruned");
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_flip_no_announcement() {
        let discovery = ToolDiscovery::new(true);
        assert!(discovery.published_visibility());
        // Same visibility → no change announced.
        assert!(!tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(discovery.publish_ticketing_visibility(true)));
        assert!(discovery.published_visibility());
    }

    #[test]
    fn flip_without_peers_announces_and_records() {
        let discovery = ToolDiscovery::new(false);
        assert!(tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(discovery.publish_ticketing_visibility(true)));
        assert!(discovery.published_visibility());
        assert_eq!(discovery.peer_count(), 0);
        // Second publish back to false flips again.
        assert!(tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(discovery.publish_ticketing_visibility(false)));
        assert!(!discovery.published_visibility());
    }
}
