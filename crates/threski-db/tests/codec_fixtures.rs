//! Per-table codec fixtures.
//!
//! Golden JSON shapes (captured raw-response style) are decoded through the
//! value codec — `serde_json::Value → Value → typed row` — never by hand-digging
//! the tagged JSON view. Fixtures cover the quirks catalog: Q1 numeric coercion,
//! Q2 absent/NONE option fields, Q3 record-link shapes, Q5 numeric issue IDs,
//! Q12 embeddings. Datetime (Q6) values are constructed programmatically since
//! plain JSON cannot carry the driver's datetime kind.

use chrono::{TimeZone, Utc};
use threski_db::tables::*;
use threski_db::value::{decode_row, Embedding, SurrealValue, F64, I64};
use std::str::FromStr;
use surrealdb::types::{Datetime, Number, Object, RecordId, RecordIdKey, ToSql, Value};

/// Converts a fixture's plain JSON into the `Value` tree the WebSocket driver
/// would hand to the codec. JSON numbers become typed numbers. Test-harness-only —
/// production code never converts JSON by hand (the tagged JSON view is never
/// decoded manually).
///
/// `datetimes` marks fields the writers persisted as real datetimes (typed DDL
/// fields like `memory.created_at`): the driver hands those to the codec as the
/// datetime kind, which plain JSON cannot express. Date-ish strings that writers
/// stored verbatim in schemaless tables (commit.date, error_type.created_at, …)
/// stay strings and decode into `String` fields.
fn json_to_value(v: &serde_json::Value, datetimes: bool) -> Value {
    match v {
        serde_json::Value::Null => Value::None,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(Number::Int(i))
            } else {
                Value::Number(Number::Float(n.as_f64().unwrap_or_default()))
            }
        }
        serde_json::Value::String(s) => {
            if datetimes && chrono::DateTime::parse_from_rfc3339(s).is_ok() {
                Datetime::from_str(s)
                    .map(Value::Datetime)
                    .unwrap_or_else(|_| Value::String(s.clone()))
            } else {
                Value::String(s.clone())
            }
        }
        serde_json::Value::Array(a) => Value::from_t(
            a.iter()
                .map(|v| json_to_value(v, datetimes))
                .collect::<Vec<Value>>(),
        ),
        serde_json::Value::Object(o) => {
            // Legacy record-link map shape {"tb"/"table", "id"} (Q3): the JSON
            // drivers of the original era emitted this; the WS driver emits record
            // values instead. The harness reconstructs the driver's value.
            if let Some(record) = record_link_from_json(o) {
                return record;
            }
            let mut obj = Object::new();
            for (k, v) in o {
                obj.insert(k.clone(), json_to_value(v, datetimes));
            }
            Value::Object(obj)
        }
    }
}

fn record_link_from_json(o: &serde_json::Map<String, serde_json::Value>) -> Option<Value> {
    let link_keys = ["id", "ID"];
    let id = link_keys
        .iter()
        .find_map(|k| o.get(*k).and_then(|v| v.as_str()))?;
    let table = ["tb", "table", "Table"]
        .iter()
        .find_map(|k| o.get(*k).and_then(|v| v.as_str()))?;
    let record = if id.contains(':') {
        RecordId::parse_simple(id).ok()?
    } else {
        RecordId::new(table, id)
    };
    Some(Value::RecordId(record))
}

fn decode_json<T: SurrealValue>(json: &str) -> T {
    let parsed: serde_json::Value = serde_json::from_str(json).expect("fixture json");
    decode_row::<T>(json_to_value(&parsed, false), "fixture").expect("decode fixture")
}

/// Decodes a fixture whose row carries real (driver-kind) datetime fields.
fn decode_json_dt<T: SurrealValue>(json: &str) -> T {
    let parsed: serde_json::Value = serde_json::from_str(json).expect("fixture json");
    decode_row::<T>(json_to_value(&parsed, true), "fixture").expect("decode fixture")
}

fn rid(s: &str) -> String {
    let record = RecordId::parse_simple(s).expect("record id");
    threski_db::ids::format_record(&record)
}

#[test]
fn memory_full_row_with_provenance() {
    let row: MemoryRow = decode_json_dt(
        r#"{
            "id": "memory:ws_abc123",
            "kind": "decision",
            "title": "Use hash-suffixed file ids",
            "content": "Source files are keyed <repo>_<relpath>_<sha256>.",
            "status": "current",
            "confidence": 0.7,
            "visibility": "team",
            "owner": "alice",
            "tags": ["ingest", "code"],
            "branch": "main",
            "project": {"tb": "repo", "id": "threski"},
            "valid_from": "2026-10-25T00:00:00Z",
            "created_at": "2026-10-25T00:00:00Z",
            "updated_at": "2026-10-25T01:00:00Z",
            "store": "git",
            "policy_applied": "require_clean",
            "source_hash": "sha256:abc",
            "score": 0.925,
            "introduced_in_oid": "0123456789abcdef",
            "provenance": {
                "agent": "cursor",
                "session": "session-1",
                "tool": "memory_add",
                "source_path": "docs/notes.md",
                "introduced_in_commit": "commit:0123456789abcdef",
                "dirty": false,
                "promote_on_commit": true
            },
            "embedding": [0.1, -0.2, 3.5]
        }"#,
    );
    assert_eq!(rid("memory:ws_abc123"), "memory:⟨ws_abc123⟩");
    assert_eq!(row.kind, MemoryKind::Decision);
    assert_eq!(row.status, MemoryStatus::Current);
    assert_eq!(row.visibility, Visibility::Team);
    assert_eq!(row.store, Some(Store::Git));
    // Q1: JSON 0.7 coerces through F64 (Decimal/Float/Int all accepted).
    assert_eq!(row.confidence, Some(F64(0.7)));
    assert_eq!(row.score, Some(F64(0.925)));
    assert_eq!(row.owner.as_deref(), Some("alice"));
    assert_eq!(row.tags.as_ref().map(|t| t.len()), Some(2));
    // Q3: object-shaped link {"tb","id"} decodes into RecordId.
    let project = row.project.expect("project link");
    assert_eq!(project.table.as_str(), "repo");
    assert_eq!(project.key, RecordIdKey::String("threski".to_string()));
    assert_eq!(row.introduced_in_oid.as_deref(), Some("0123456789abcdef"));
    // Q2 partial provenance: absent members decode as None.
    let prov = row.provenance.expect("provenance");
    assert_eq!(prov.agent.as_deref(), Some("cursor"));
    assert_eq!(prov.session.as_deref(), Some("session-1"));
    assert_eq!(prov.tool.as_deref(), Some("memory_add"));
    assert_eq!(prov.imported_from, None);
    assert_eq!(prov.source_hash, None);
    assert_eq!(prov.source_path.as_deref(), Some("docs/notes.md"));
    assert_eq!(
        prov.introduced_in_commit.map(|r| r.to_sql()),
        Some("commit:0123456789abcdef".to_string())
    );
    assert_eq!(prov.invalidated_at_commit, None);
    assert_eq!(prov.dirty, Some(false));
    assert_eq!(prov.worktree_hash, None);
    assert_eq!(prov.promote_on_commit, Some(true));
    assert_eq!(prov.orphan_oid, None);
    // Q12 embedding.
    assert_eq!(row.embedding, Some(Embedding(vec![0.1f32, -0.2, 3.5])));
}

#[test]
fn memory_row_without_provenance_and_with_none_fields() {
    // Legacy row shape (migration 3 folded away): no provenance object at all,
    // several option fields absent entirely (Q2 absent-key side).
    let row: MemoryRow = decode_json_dt(
        r#"{
            "id": "memory:ws_plain",
            "kind": "fact",
            "content": "plain fact",
            "status": "current",
            "visibility": "team",
            "created_at": "2026-10-25T00:00:00Z",
            "updated_at": "2026-10-25T00:00:00Z"
        }"#,
    );
    assert_eq!(row.kind, MemoryKind::Fact);
    assert_eq!(row.content, "plain fact");
    assert_eq!(row.title, None);
    assert_eq!(row.confidence, None);
    assert_eq!(row.tags, None);
    assert_eq!(row.project, None);
    assert_eq!(row.valid_from, None);
    assert_eq!(row.valid_until, None);
    assert_eq!(row.store, None);
    assert_eq!(row.provenance, None);
    assert_eq!(row.embedding, None);
    assert_eq!(row.source_hash, None);
    assert_eq!(row.score, None);
}

#[test]
fn memory_row_kind_status_domains_are_closed() {
    // Out-of-domain strings surface as typed decode errors at the boundary.
    for bad in ["kind_of_the_machine", ""] {
        let json = format!(
            r#"{{ "id": "memory:⟨x⟩", "kind": "{bad}", "content": "c", "status": "current", "visibility": "team",
                 "created_at": "2026-10-25T00:00:00Z", "updated_at": "2026-10-25T00:00:00Z" }}"#
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert!(
            decode_row::<MemoryRow>(json_to_value(&parsed, true), "fixture").is_err(),
            "kind {bad:?} must not decode"
        );
    }
}

#[test]
fn commit_row() {
    let row: CommitRow = decode_json_dt(
        r#"{
            "id": "commit:0123456789abcdef0123456789abcdef01234567",
            "hash": "0123456789abcdef0123456789abcdef01234567",
            "date": "2026-10-24T10:11:12Z",
            "message": "Fix bug #123",
            "repo": "repo:threski",
            "batch_status": "pending"
        }"#,
    );
    assert_eq!(row.hash, "0123456789abcdef0123456789abcdef01234567");
    // Typed datetime (D17 R7): RFC3339 strings decode into Datetime; render
    // back to RFC3339 for the roundtrip assertion.
    assert_eq!(
        row.date.as_ref().map(|d| d
            .into_inner()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        Some("2026-10-24T10:11:12Z".to_string())
    );
    assert_eq!(row.message.as_deref(), Some("Fix bug #123"));
    assert_eq!(
        row.repo.map(|r| r.table.as_str().to_string()),
        Some("repo".to_string())
    );
    assert_eq!(row.batch_status, Some(BatchStatus::Pending));
    assert_eq!(row.batch_error, None);
}

#[test]
fn source_file_writer_shapes() {
    // Code-ingest shape: full row (updated_at is a real datetime via time::now).
    let row: SourceFileRow = decode_json_dt(
        r#"{
            "id": "source_file:threski_src_main_rs_0123456789abcdef",
            "path": "C:\\repos\\threski\\src\\main.rs",
            "rel_path": "src/main.rs",
            "hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "is_head": true,
            "repo_name": "threski",
            "updated_at": "2026-10-25T02:00:00Z"
        }"#,
    );
    assert_eq!(row.rel_path.as_deref(), Some("src/main.rs"));
    assert_eq!(row.repo_name.as_deref(), Some("threski"));
    assert_eq!(row.is_head, Some(true));

    // Git-ingest shape: path-only rows (older id form).
    let git_row: SourceFileRow = decode_json(
        r#"{
            "id": "source_file:threski_src_main_rs",
            "path": "src/main.rs"
        }"#,
    );
    assert_eq!(git_row.path.as_deref(), Some("src/main.rs"));
    assert_eq!(git_row.hash, None);
    assert_eq!(git_row.rel_path, None);
    assert_eq!(git_row.repo_name, None);
}

#[test]
fn file_chunk_row() {
    let row: FileChunkRow = decode_json(
        r#"{
            "id": "file_chunk:threski_src_main_rs_0123456789abcdef_3",
            "file": "source_file:threski_src_main_rs_0123456789abcdef",
            "hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "content": "fn main() {}",
            "symbol_name": "main",
            "kind": "function",
            "start_line": 10,
            "end_line": 42,
            "embedding": [0.5, 0.25],
            "batch_status": "completed",
            "batch_id": "gemini-batch-1"
        }"#,
    );
    assert_eq!(row.start_line, Some(I64(10)));
    assert_eq!(row.end_line, Some(I64(42)));
    assert_eq!(row.batch_status, Some(BatchStatus::Completed));
    assert_eq!(row.embedding, Some(Embedding(vec![0.5f32, 0.25])));
    assert_eq!(
        row.file.map(|r| threski_db::ids::format_record(&r)),
        Some("source_file:⟨threski_src_main_rs_0123456789abcdef⟩".to_string())
    );
}

#[test]
fn commit_chunk_row() {
    let row: CommitChunkRow = decode_json(
        r#"{
            "id": "commit_chunk:0123456789abcdef0123456789abcdef01234567_chunk0",
            "content": "COMMIT MESSAGE:\nFix bug #123",
            "batch_status": "pending",
            "commit_hash": "0123456789abcdef0123456789abcdef01234567",
            "commit_date": "2026-10-24T10:11:12Z"
        }"#,
    );
    assert_eq!(
        row.commit_hash.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(row.commit_date.as_deref(), Some("2026-10-24T10:11:12Z"));
    assert_eq!(row.batch_status, Some(BatchStatus::Pending));
    assert_eq!(row.embedding, None);
}

#[test]
fn error_type_row() {
    let row: ErrorTypeRow = decode_json_dt(
        r#"{
            "id": "error_type:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "category": "panic",
            "stack_trace": "goroutine 1: index out of range",
            "severity": 3,
            "template": "index out of range [N]",
            "created_at": "2026-10-25T03:00:00Z",
            "hidden": false,
            "embedding": [1.0, 2.0]
        }"#,
    );
    assert_eq!(row.severity, Some(I64(3)));
    assert_eq!(row.hidden, Some(false));
    // Typed datetime (D17 R7): RFC3339 decode → instant → RFC3339 render.
    assert_eq!(
        row.created_at.as_ref().map(|d| d
            .into_inner()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        Some("2026-10-25T03:00:00Z".to_string())
    );
}

#[test]
fn log_rows() {
    let log: LogFileRow = decode_json(
        r#"{
            "id": "log_file:⟨server_log⟩",
            "path": "logs/proj/server.log",
            "project": "project:⟨proj⟩",
            "offset": 102400
        }"#,
    );
    assert_eq!(log.offset, Some(I64(102400)));
    assert_eq!(log.path.as_deref(), Some("logs/proj/server.log"));

    let processed: LogProcessedFileRow = decode_json_dt(
        r#"{
            "id": "log_processed_file:0123456789abcdef",
            "path": "logs/proj/server.log",
            "hash": "0123456789abcdef0123456789abcdef01234567",
            "processed_at": "2026-10-25T04:00:00Z"
        }"#,
    );
    assert_eq!(
        processed.hash.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
}

#[test]
fn issue_row_with_numeric_id() {
    // Q5: redmine numeric record ids stay unwrapped.
    let row: IssueRow = decode_json(
        r#"{
            "id": "issue:42",
            "provider": "redmine",
            "external_id": "42",
            "external_key": "42",
            "project_key": "threski",
            "subject": "Fix the thing",
            "description": "Details",
            "status": "open",
            "type": "Bug",
            "updated_on": "2026-10-24T00:00:00Z",
            "url": "https://redmine.example.com/issues/42"
        }"#,
    );
    assert_eq!(rid("issue:42"), "issue:42");
    assert_eq!(row.provider.as_deref(), Some("redmine"));
    assert_eq!(row.issue_type.as_deref(), Some("Bug"));
    assert_eq!(row.subject.as_deref(), Some("Fix the thing"));

    // Non-redmine providers use <provider>_<key> string ids.
    let jira: IssueRow = decode_json(
        r#"{
            "id": "issue:⟨jira_API_9⟩",
            "provider": "jira",
            "external_key": "API-9"
        }"#,
    );
    assert_eq!(jira.external_key.as_deref(), Some("API-9"));
}

#[test]
fn git_credential_key_usage_workstream_rows() {
    let cred: GitCredentialRow = decode_json(
        r#"{
            "id": "git_credential:github_com",
            "target": "github.com",
            "provider": "github",
            "auth_type": "token",
            "token": "ghp_secret"
        }"#,
    );
    assert_eq!(cred.auth_type.as_deref(), Some("token"));
    assert_eq!(cred.token.as_deref(), Some("ghp_secret"));
    assert_eq!(cred.username, None);
    assert_eq!(cred.ssh_private_key, None);

    // Q1: `requests` counts arrive as JSON numbers of any numeric shape.
    let usage: KeyUsageRow = decode_json(
        r#"{
            "id": "key_usage:01234567_20261025",
            "requests": 12,
            "owner": "gemini-primary"
        }"#,
    );
    assert_eq!(usage.requests, Some(I64(12)));
    assert_eq!(usage.owner.as_deref(), Some("gemini-primary"));
    assert_eq!(usage.date, None);

    let ws: WorkstreamRow = decode_json(
        r#"{ "id": "workstream:⟨auth_refactor⟩", "name": "auth refactor", "status": "active" }"#,
    );
    assert_eq!(ws.status.as_deref(), Some("active"));
}

#[test]
fn migration_row() {
    let row: MigrationRow = decode_json_dt(
        r#"{
            "id": "migration:current",
            "version": 2,
            "applied_at": "2026-10-25T05:00:00Z"
        }"#,
    );
    assert_eq!(row.version, Some(I64(2)));
}

#[test]
fn q2_explicit_none_decodes_as_absent() {
    // A writer that explicitly sets a field to NONE yields the same decode
    // result as an absent key: both become None (Q2).
    let parsed: serde_json::Value = serde_json::from_str(
        r#"{
            "id": "memory:⟨m1⟩",
            "kind": "fact",
            "content": "c",
            "status": "current",
            "visibility": "team",
            "created_at": "2026-10-25T00:00:00Z",
            "updated_at": "2026-10-25T00:00:00Z"
        }"#,
    )
    .expect("json");
    let mut value = json_to_value(&parsed, true);
    if let Value::Object(ref mut map) = value {
        map.insert("owner".to_string(), Value::None);
        map.insert("tags".to_string(), Value::None);
    }
    let row = decode_row::<MemoryRow>(value, "fixture").expect("decode");
    assert_eq!(row.owner, None);
    assert_eq!(row.tags, None);
}

#[test]
fn q6_datetime_kind_decodes() {
    // Plain JSON cannot carry the driver's datetime kind; build the value the
    // WS driver produces and decode it (Q6).
    #[derive(Debug, SurrealValue)]
    struct SeenRow {
        seen: Option<Datetime>,
    }
    let when = Utc.with_ymd_and_hms(2026, 10, 25, 6, 0, 0).unwrap();
    let mut obj = surrealdb::types::Object::new();
    obj.insert("seen", Datetime::from(when));
    let row: SeenRow = decode_row(Value::Object(obj), "fixture").expect("decode");
    assert_eq!(row.seen.map(|d| d.to_rfc3339()), Some(when.to_rfc3339()));
}

#[test]
fn q3_record_link_string_shapes() {
    // Record links arrive as strings ("table:id") or {"tb","id"} objects
    // depending on the legacy shape (Q3); both decode into RecordId.
    let when = Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap();
    let mut obj = Object::new();
    obj.insert("id", RecordId::new("memory", "m1"));
    obj.insert("kind", "fact");
    obj.insert("content", "c");
    obj.insert("status", "current");
    obj.insert("visibility", "team");
    obj.insert("created_at", Datetime::from(when));
    obj.insert("updated_at", Datetime::from(when));
    obj.insert("project", "repo:threski");
    let row = decode_row::<MemoryRow>(Value::Object(obj), "fixture").expect("decode");
    let project = row.project.expect("string link decodes");
    assert_eq!(project.table.as_str(), "repo");
    assert_eq!(project.key, RecordIdKey::String("threski".to_string()));
    // The row's own id decodes to the project-canonical form.
    assert_eq!(threski_db::ids::format_record(&row.id), "memory:⟨m1⟩");
}
