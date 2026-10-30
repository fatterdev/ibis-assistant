//! F0.4 MCP resources — memory, rules and tickets as first-class resources:
//! the URI templates `ibis://memory/{kind}/{id}`, `ibis://rules/{name}`
//! and `ibis://ticket/{id}`, the read dispatch backing
//! `resources/read`, and the live-update registry that turns memory
//! mutations into `notifications/resources/updated` on the transport's
//! notification lane.
//!
//! Two subscription mechanisms are served, keyed by the negotiated protocol
//! revision (rmcp 3.7 dispatches per client):
//! - legacy `resources/subscribe` / `resources/unsubscribe` (≤ 2025-11-25):
//!   the requesting connection's peer handle is registered per URI;
//! - `subscriptions/listen` (≥ 2026-07-28): one [`SubscriptionSink`] per
//!   accepted listen request is registered for the URIs in its accepted
//!   filter, and removed when the listen request is cancelled.
//!
//! The registry is connection-aware: a re-subscribe from the same connection
//! replaces its earlier registration for the same URI, and fires are exact
//! URI matches (the MCP subscribe model is per concrete resource URI, not
//! per template).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt};
use rmcp::model::{ErrorData, ResourceTemplate, ResourceUpdatedNotificationParam};
use rmcp::service::SubscriptionSink;
use rmcp::Peer;
use rmcp::RoleServer;

use crate::state::AppState;
use ibis_search::ActorContext;

/// The memory kinds addressable via `ibis://memory/{kind}/{id}` — the
/// full typed-memory set (working_state/observation/reasoning included;
/// the F0.3 prompt completions use the narrower durable subset).
pub const MEMORY_URI_KINDS: &[&str] = &[
    "decision",
    "fix",
    "constraint",
    "rule",
    "gotcha",
    "design",
    "fact",
    "working_state",
    "observation",
    "reasoning",
];

/// URI template for one typed memory.
pub const MEMORY_TEMPLATE: &str = "ibis://memory/{kind}/{id}";
/// URI template for one current team rule (by title or record id).
pub const RULES_TEMPLATE: &str = "ibis://rules/{name}";
/// URI template for one ticket/work item.
pub const TICKET_TEMPLATE: &str = "ibis://ticket/{id}";

/// Builds the resource URI for a memory (`ibis://memory/{kind}/{id}`).
pub fn memory_uri(kind: &str, id: &str) -> String {
    format!("ibis://memory/{kind}/{id}")
}

/// Builds the resource URI for a rule (`ibis://rules/{name}`).
pub fn rule_uri(name: &str) -> String {
    format!("ibis://rules/{name}")
}

/// Builds the resource URI for a ticket (`ibis://ticket/{id}`).
pub fn ticket_uri(id: &str) -> String {
    format!("ibis://ticket/{id}")
}

/// One parsed `ibis://…` resource URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceUri {
    /// `ibis://memory/{kind}/{id}`
    Memory { kind: String, id: String },
    /// `ibis://rules/{name}`
    Rule { name: String },
    /// `ibis://ticket/{id}`
    Ticket { id: String },
}

/// Parses a concrete resource URI against the three templates; `None` when
/// the URI is not one of them (the guide resource and foreign schemes stay
/// with `resources/read`'s static arm).
pub fn parse_uri(uri: &str) -> Option<ResourceUri> {
    let (scheme, rest) = uri.split_once("://")?;
    if scheme != "ibis" {
        return None;
    }
    if let Some(rest) = rest.strip_prefix("memory/") {
        let (kind, id) = rest.split_once('/')?;
        let kind = kind.trim().to_ascii_lowercase();
        let id = id.trim();
        if id.is_empty() || !MEMORY_URI_KINDS.contains(&kind.as_str()) || id.contains('/') {
            return None;
        }
        return Some(ResourceUri::Memory {
            kind,
            id: id.to_string(),
        });
    }
    if let Some(name) = rest.strip_prefix("rules/") {
        let name = name.trim();
        if name.is_empty() || name.contains('/') {
            return None;
        }
        return Some(ResourceUri::Rule {
            name: name.to_string(),
        });
    }
    if let Some(id) = rest.strip_prefix("ticket/") {
        let id = id.trim();
        if id.is_empty() || id.contains('/') {
            return None;
        }
        return Some(ResourceUri::Ticket { id: id.to_string() });
    }
    None
}

/// The `resources/templates/list` table (F0.4): one row per template, JSON
/// mime (rows are machine-shaped projections of the same data the tools
/// return).
pub fn resource_templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new(MEMORY_TEMPLATE, "Typed Memory")
            .with_title("Typed Memory")
            .with_description(
                "One typed memory with provenance and supersession lineage (the memory_expand \
                 projection); {kind} is one of decision|fix|constraint|rule|gotcha|design|fact|\
                 working_state|observation|reasoning.",
            )
            .with_mime_type("application/json"),
        ResourceTemplate::new(RULES_TEMPLATE, "Team Rule")
            .with_title("Team Rule")
            .with_description(
                "One current team rule/convention by title (case-insensitive) or memory record id.",
            )
            .with_mime_type("application/json"),
        ResourceTemplate::new(TICKET_TEMPLATE, "Ticket")
            .with_title("Ticket / Work Item")
            .with_description(
                "One ticket or work item from the configured ticketing provider, by ID or \
                 external key.",
            )
            .with_mime_type("application/json"),
    ]
}

/// Read outcome: the serialized body plus its mime type.
pub struct ReadBody {
    pub content: String,
    pub mime_type: &'static str,
}

fn not_found(message: String) -> ErrorData {
    ErrorData::resource_not_found(message, None)
}

/// `resources/read` for a parsed template URI (degraded mode without a DB
/// surfaces the memory lane's "database not connected" error).
pub async fn read_parsed(
    state: &Arc<AppState>,
    actor: &ActorContext,
    ticket_auth: &ibis_ticketing::AuthContext,
    uri: &ResourceUri,
) -> Result<ReadBody, ErrorData> {
    match uri {
        ResourceUri::Memory { kind, id } => read_memory(state, actor, kind, id).await,
        ResourceUri::Rule { name } => read_rule(state, name).await,
        ResourceUri::Ticket { id } => read_ticket(state, ticket_auth, id).await,
    }
}

/// Reads one memory: the full `memory_expand` projection, with the URI kind
/// segment pinned against the record's actual kind (a stale URI is 404, not
/// a silent mismatch).
async fn read_memory(
    state: &Arc<AppState>,
    actor: &ActorContext,
    kind: &str,
    id: &str,
) -> Result<ReadBody, ErrorData> {
    if !ibis_db::ids::is_safe_record_id(id) {
        return Err(not_found(format!("memory {id:?} is not a record id")));
    }
    let service = state.search();
    let expansion = service
        .memory_expand(actor, id, 1, "")
        .await
        .map_err(|e| not_found(format!("memory {id}: {e}")))?;
    if expansion.memory.kind != kind {
        return Err(not_found(format!(
            "memory {id} is kind {}, not {kind}",
            expansion.memory.kind
        )));
    }
    let content = serde_json::to_string_pretty(&expansion)
        .map_err(|e| ErrorData::internal_error(format!("serialize memory: {e}"), None))?;
    Ok(ReadBody {
        content,
        mime_type: "application/json",
    })
}

/// Reads one rule by title (case-insensitive, current rules) or memory
/// record id; returns the same shape `get_rules` rows have.
async fn read_rule(state: &Arc<AppState>, name: &str) -> Result<ReadBody, ErrorData> {
    let service = state.search();
    let memory = if ibis_db::ids::is_safe_record_id(name) {
        service
            .memory_expand(&ActorContext::LOCAL, name, 1, "")
            .await
            .map_err(|e| not_found(format!("rule {name}: {e}")))?
            .memory
    } else {
        let rules = service
            .get_rules("", "", &[], 200)
            .await
            .map_err(|e| not_found(format!("rules lookup failed: {e}")))?;
        rules
            .into_iter()
            .find(|r| r.kind == "rule" && r.title.eq_ignore_ascii_case(name))
            .ok_or_else(|| not_found(format!("no current rule named {name:?}")))?
    };
    if memory.kind != "rule" {
        return Err(not_found(format!(
            "record {name} is kind {}, not rule",
            memory.kind
        )));
    }
    let content = serde_json::to_string_pretty(&memory)
        .map_err(|e| ErrorData::internal_error(format!("serialize rule: {e}"), None))?;
    Ok(ReadBody {
        content,
        mime_type: "application/json",
    })
}

/// Reads one ticket through the configured ticketing providers.
async fn read_ticket(
    state: &Arc<AppState>,
    auth: &ibis_ticketing::AuthContext,
    id: &str,
) -> Result<ReadBody, ErrorData> {
    match state.ticket.get(auth, "", id, "").await {
        Ok(Some(issue)) => {
            let content = serde_json::to_string_pretty(&issue)
                .map_err(|e| ErrorData::internal_error(format!("serialize ticket: {e}"), None))?;
            Ok(ReadBody {
                content,
                mime_type: "application/json",
            })
        }
        Ok(None) => Err(not_found(format!("ticket {id:?} not found"))),
        Err(e) => Err(ErrorData::internal_error(format!("ticketing: {e}"), None)),
    }
}

/// One fire closure: receives the changed resource URI, sends
/// `notifications/resources/updated` on the subscriber's lane.
pub type NotifyFn = Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>;

/// Connection identity for a peer: the address of its shared peer-info
/// allocation (one per MCP session; clones share it). Legacy
/// unsubscribe/re-subscribe remove by connection, and same-connection
/// re-subscribes replace rather than duplicate.
pub fn peer_connection_id(peer: &Peer<RoleServer>) -> u64 {
    peer.peer_info()
        .map(|info| Arc::as_ptr(&info) as *const () as usize as u64)
        .unwrap_or(0)
}

struct Subscriber {
    token: u64,
    conn: u64,
    notify: NotifyFn,
}

/// The live-update registry behind `resources/subscribe` and
/// `subscriptions/listen`: per-URI subscriber lists with connection-aware
/// replacement, plus exact-URI fires for `notifications/resources/updated`.
#[derive(Default)]
pub struct ResourceSubscriptions {
    next_token: AtomicU64,
    inner: Mutex<HashMap<String, Vec<Subscriber>>>,
}

impl ResourceSubscriptions {
    fn new_token(&self) -> u64 {
        self.next_token.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers one subscriber for `uri` (replacing any earlier
    /// registration from the same connection) and returns its token.
    pub fn subscribe(&self, uri: &str, conn: u64, notify: NotifyFn) -> u64 {
        let token = self.new_token();
        let mut inner = self.inner.lock().unwrap();
        let list = inner.entry(uri.to_string()).or_default();
        list.retain(|s| s.conn != conn);
        list.push(Subscriber {
            token,
            conn,
            notify,
        });
        token
    }

    /// Removes every registration `conn` holds for `uri`
    /// (`resources/unsubscribe`).
    pub fn unsubscribe(&self, uri: &str, conn: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(list) = inner.get_mut(uri) {
            list.retain(|s| s.conn != conn);
            if list.is_empty() {
                inner.remove(uri);
            }
        }
    }

    /// Removes one `subscriptions/listen` registration by token (run when
    /// the listen request is cancelled or completes).
    pub fn unsubscribe_token(&self, token: u64) {
        let mut inner = self.inner.lock().unwrap();
        let mut dead_uris = Vec::new();
        for (uri, list) in inner.iter_mut() {
            list.retain(|s| s.token != token);
            if list.is_empty() {
                dead_uris.push(uri.clone());
            }
        }
        for uri in dead_uris {
            inner.remove(&uri);
        }
    }

    /// Subscriber rows across all URIs (diagnostics/protocol-gate surface).
    pub fn subscriber_count(&self) -> usize {
        self.inner.lock().unwrap().values().map(Vec::len).sum()
    }

    /// Fires `notifications/resources/updated` to every subscriber of
    /// `uri`. Send failures (a closed connection that never unsubscribed)
    /// are logged and dropped; the caller's tool result is unaffected.
    pub async fn fire(&self, uri: &str) {
        let subscribers: Vec<NotifyFn> = {
            let inner = self.inner.lock().unwrap();
            inner
                .get(uri)
                .map(|list| list.iter().map(|s| s.notify.clone()).collect())
                .unwrap_or_default()
        };
        if subscribers.is_empty() {
            return;
        }
        let uri = uri.to_string();
        let fires: Vec<BoxFuture<'static, ()>> = subscribers
            .into_iter()
            .map(|notify| notify(uri.clone()))
            .collect();
        let _ = futures::future::join_all(fires).await;
    }
}

/// Notification closure for a legacy `resources/subscribe` peer.
pub fn peer_notify(peer: Peer<RoleServer>) -> NotifyFn {
    Arc::new(move |uri: String| {
        let peer = peer.clone();
        async move {
            if let Err(e) = peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(uri))
                .await
            {
                tracing::debug!("resources/updated delivery failed: {e}");
            }
        }
        .boxed()
    })
}

/// Notification closure for a `subscriptions/listen` sink (the sink
/// enforces the accepted URI filter itself).
pub fn sink_notify(sink: SubscriptionSink) -> NotifyFn {
    Arc::new(move |uri: String| {
        let sink = sink.clone();
        async move {
            if let Err(e) = sink.notify_resource_updated(uri).await {
                tracing::debug!("resources/updated delivery failed: {e}");
            }
        }
        .boxed()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_build_and_parse_round_trip() {
        let memory = memory_uri("decision", "memory:abc123");
        assert_eq!(
            parse_uri(&memory),
            Some(ResourceUri::Memory {
                kind: "decision".into(),
                id: "memory:abc123".into()
            })
        );
        let rule = rule_uri("No Deps On Node Builtins");
        assert_eq!(
            parse_uri(&rule),
            Some(ResourceUri::Rule {
                name: "No Deps On Node Builtins".into()
            })
        );
        let ticket = ticket_uri("PROJ-123");
        assert_eq!(
            parse_uri(&ticket),
            Some(ResourceUri::Ticket {
                id: "PROJ-123".into()
            })
        );
        // Kind segment is case-normalized.
        assert_eq!(
            parse_uri("ibis://memory/Decision/memory:abc123"),
            parse_uri("ibis://memory/decision/memory:abc123")
        );
    }

    #[test]
    fn uri_parse_rejects_malformed() {
        for uri in [
            "ibis://guide",
            "ibis://memory/decision",           // missing id
            "ibis://memory/decision/",          // empty id
            "ibis://memory/unknown/memory:abc", // unknown kind
            "ibis://memory/decision/a/b",       // id with slash
            "ibis://rules/",                    // empty name
            "ibis://rules/a/b",                 // name with slash
            "ibis://ticket/",                   // empty id
            "ibis://ticket/a/b",                // id with slash
            "threski://memory/decision/x",         // foreign scheme (the retired one)
            "https://example.com",                 // foreign scheme
            "ibis://other/x",                   // unknown surface
        ] {
            assert!(parse_uri(uri).is_none(), "{uri} must not parse");
        }
    }

    #[test]
    fn templates_table_has_the_three_f04_rows() {
        let templates = resource_templates();
        assert_eq!(templates.len(), 3);
        let mut uris: Vec<&str> = templates.iter().map(|t| t.uri_template.as_str()).collect();
        uris.sort_unstable();
        assert_eq!(
            uris,
            [MEMORY_TEMPLATE, RULES_TEMPLATE, TICKET_TEMPLATE].map(|s| s as &str)
        );
        for t in &templates {
            assert_eq!(t.mime_type.as_deref(), Some("application/json"));
            assert!(t.title.is_some(), "{} title", t.uri_template);
            assert!(
                t.description
                    .as_deref()
                    .map(|d| d.len() > 20)
                    .unwrap_or(false),
                "{} description",
                t.uri_template
            );
        }
    }

    #[tokio::test]
    async fn registry_subscribe_fire_and_removal() {
        let registry = ResourceSubscriptions::default();
        let fired = Arc::new(Mutex::new(Vec::<String>::new()));
        let fired_for_notify = fired.clone();
        let notify: NotifyFn = Arc::new(move |uri: String| {
            let fired = fired_for_notify.clone();
            async move {
                fired.lock().unwrap().push(uri);
            }
            .boxed()
        });
        let token = registry.subscribe("ibis://memory/decision/m1", 7, notify);
        assert_eq!(registry.subscriber_count(), 1);

        registry.fire("ibis://memory/gotcha/other").await; // no match
        assert!(fired.lock().unwrap().is_empty());
        registry.fire("ibis://memory/decision/m1").await;
        assert_eq!(
            fired.lock().unwrap().as_slice(),
            ["ibis://memory/decision/m1"]
        );

        // Same-connection re-subscribe replaces; old token is inert.
        let fired2 = Arc::new(Mutex::new(Vec::<String>::new()));
        let fired2_clone = fired2.clone();
        let notify2: NotifyFn = Arc::new(move |uri: String| {
            let fired = fired2_clone.clone();
            async move {
                fired.lock().unwrap().push(uri);
            }
            .boxed()
        });
        registry.subscribe("ibis://memory/decision/m1", 7, notify2);
        assert_eq!(registry.subscriber_count(), 1);
        registry.unsubscribe_token(token); // stale token is a no-op
        registry.fire("ibis://memory/decision/m1").await;
        assert_eq!(
            fired2.lock().unwrap().as_slice(),
            ["ibis://memory/decision/m1"]
        );
        assert!(
            fired.lock().unwrap().len() == 1,
            "replaced subscriber not fired twice"
        );

        // Legacy-style unsubscribe by (uri, conn) removes the registration.
        registry.unsubscribe("ibis://memory/decision/m1", 7);
        assert_eq!(registry.subscriber_count(), 0);
        registry.fire("ibis://memory/decision/m1").await;
        assert_eq!(
            fired2.lock().unwrap().len(),
            1,
            "no delivery after unsubscribe"
        );
    }

    #[tokio::test]
    async fn registry_listen_tokens_remove_their_rows() {
        let registry = ResourceSubscriptions::default();
        let hits = Arc::new(Mutex::new(Vec::<String>::new()));
        let notify: NotifyFn = {
            let hits = hits.clone();
            Arc::new(move |uri: String| {
                let hits = hits.clone();
                async move {
                    hits.lock().unwrap().push(uri);
                }
                .boxed()
            })
        };
        let token = registry.subscribe("ibis://ticket/PROJ-1", 0, notify);
        registry.unsubscribe_token(token);
        assert_eq!(registry.subscriber_count(), 0);
        registry.fire("ibis://ticket/PROJ-1").await;
        assert!(hits.lock().unwrap().is_empty());
    }
}
