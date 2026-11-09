//! Live observations estate (C8 steps 1–4): the deterministic reconcile over
//! the live SurrealDB sidecar — pattern creation, idempotency, evidence
//! change, evidence refs, scope isolation, the ask channel and the read-only
//! end-to-end guards.
//!
//! Sidecar: `IBIS_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`,
//! root/root). Every test creates its own `c8_*` scratch database, runs the
//! SCHEMAFULL DDL and drops it at the end; when the server is unreachable the
//! test skips gracefully (same pattern as `live_memory_estate.rs`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use ibis_ai::{AIProvider, AiError};
use ibis_db::executor::{SurrealDbExecutor, Vars};
use ibis_db::schema;
use ibis_db::value::Value;
use ibis_db::Executor;
use ibis_search::memory::add::AddMemoryParams;
use ibis_search::memory::observations::{
    observation_id, signature_hash, ReconcileMode, FAMILY_FIX_ROOT_CAUSE, OBSERVATION_AGENT,
};
use ibis_search::memory::{MemoryListParams, MemoryUpdateParams};
use ibis_search::{ActorContext, SearchService};

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";
const NS: &str = "ibis_db_tests";

const PROJECT: &str = "obs_live";
const ROOT_CAUSE: &str = "empty discovery root";
const TITLE_A: &str = "Empty discovery root crashes setup";
const CONTENT_A: &str = "The wizard crashes when discovery_root is empty.";
const TITLE_B: &str = "Discovery root guard missing";
const CONTENT_B: &str = "Setup aborts when discovery_root is empty.";

// ---------------------------------------------------------------------------
// scratch-DB lifecycle (mirrors live_memory_estate.rs)
// ---------------------------------------------------------------------------

/// Process-wide gate for the live suite. Every test owns a private scratch
/// database, but concurrent schema DDL + reconcile transactions still hit
/// SurrealDB `Transaction conflict: Write conflict` at the server level, so
/// the default (parallel) run serializes the whole suite here.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("c8_{}", uuid::Uuid::new_v4().simple());
    let exec = match SurrealDbExecutor::connect(&url, NS, &db_name, "root", "root").await {
        Ok(exec) => exec,
        Err(err) => {
            if std::env::var("IBIS_LIVE_REQUIRED").as_deref() == Ok("1") {
                panic!("IBIS_LIVE_REQUIRED=1 but SurrealDB is unreachable: {err}");
            }
            eprintln!("live: SurrealDB not reachable ({err}), skipping");
            return None;
        }
    };
    Some((Arc::new(exec) as Arc<dyn Executor>, db_name))
}

async fn drop_scratch(exec: &dyn Executor, db_name: &str) {
    let _ = exec
        .execute(&format!("REMOVE DATABASE IF EXISTS {db_name};"))
        .await;
}

async fn init_ddl(exec: &dyn Executor) {
    schema::init_schema(exec).await.expect("init schema");
}

/// Deterministic 768-dim embedding derived from the text bytes (the AI lane
/// itself is a counting fake; the real model is not wired in tests).
fn c8_vec(text: &str) -> Vec<f32> {
    let bytes = text.as_bytes();
    let mut v = vec![0f32; 768];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = f32::from(bytes[i % bytes.len()]) / 255.0 - 0.5;
    }
    v
}

#[derive(Default)]
struct C8Ai {
    embed_calls: AtomicUsize,
    generate_calls: AtomicUsize,
}

#[async_trait]
impl AIProvider for C8Ai {
    fn name(&self) -> &str {
        "c8-live-embedder"
    }

    async fn is_functional(&self) -> bool {
        true
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        self.embed_calls.fetch_add(1, Ordering::SeqCst);
        Ok(c8_vec(text))
    }

    async fn generate_content(
        &self,
        _contents: &[ibis_ai::Content],
        _config: ibis_ai::types::GenerationConfig,
    ) -> Result<ibis_ai::types::Candidate, AiError> {
        // Steps 1–4 never call the LLM; a call here is a bug the counters
        // below surface.
        self.generate_calls.fetch_add(1, Ordering::SeqCst);
        Err(AiError::unsupported("c8-live", "generate_content"))
    }
}

fn service(exec: &Arc<dyn Executor>, ai: Option<Arc<C8Ai>>) -> SearchService {
    SearchService::new(Some(exec.clone()), ai.map(|ai| ai as Arc<dyn AIProvider>))
}

fn vars_of(pairs: Vec<(&str, Value)>) -> Vars {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

#[allow(clippy::too_many_arguments)]
async fn add_fix(
    svc: &SearchService,
    actor: &ActorContext,
    title: &str,
    content: &str,
    root_cause: &str,
    visibility: &str,
    owner: &str,
    derived_from: Vec<String>,
) -> String {
    let mut params = AddMemoryParams {
        project_name: PROJECT.to_string(),
        kind: "fix".to_string(),
        title: title.to_string(),
        content: content.to_string(),
        visibility: visibility.to_string(),
        owner: owner.to_string(),
        details: Some(serde_json::json!({ "root_cause": root_cause })),
        derived_from,
        embedding: vec![0.1; 768],
        ..Default::default()
    };
    svc.add_memory(actor, &mut params).await.expect("add fix")
}

/// Rewrites a source memory's `details.root_cause` (the reconcile input) the
/// same way a later capture/edit pass would.
async fn set_root_cause(exec: &dyn Executor, id: &str, root_cause: &str) {
    exec.smart_query(
        &format!("UPDATE {id} SET details.root_cause = $root_cause, updated_at = time::now();"),
        vars_of(vec![("root_cause", Value::from_t(root_cause))]),
    )
    .await
    .expect("update root cause");
}

async fn count_rows(exec: &dyn Executor, from_where: &str) -> f64 {
    let sql = format!("SELECT count() AS count FROM {from_where} GROUP ALL;");
    let outcome = exec.execute(&sql).await.expect("count query");
    let values = ibis_search::rows::normalize_rows(&outcome).expect("count rows");
    ibis_search::rows::object_rows(&values)
        .first()
        .map(|row| ibis_search::rows::numeric_float(ibis_search::rows::field(row, "count")))
        .unwrap_or(0.0)
}

/// One scalar projection from an observation row (`details.x`, `status`, …).
async fn obs_scalar(exec: &dyn Executor, id: &str, projection: &str) -> String {
    let sql = format!("SELECT {projection} AS value FROM {id};");
    let outcome = exec.execute(&sql).await.expect("select observation field");
    let values = ibis_search::rows::normalize_rows(&outcome).expect("observation rows");
    ibis_search::rows::object_rows(&values)
        .first()
        .map(|row| ibis_search::rows::scalar_string(ibis_search::rows::field(row, "value")))
        .unwrap_or_default()
}

async fn obs_strings(exec: &dyn Executor, id: &str, projection: &str) -> Vec<String> {
    let sql = format!("SELECT {projection} AS value FROM {id};");
    let outcome = exec.execute(&sql).await.expect("select observation list");
    let values = ibis_search::rows::normalize_rows(&outcome).expect("observation rows");
    ibis_search::rows::object_rows(&values)
        .first()
        .map(|row| ibis_search::rows::string_slice(ibis_search::rows::field(row, "value")))
        .unwrap_or_default()
}

fn team_observation_id() -> String {
    observation_id(&signature_hash(
        PROJECT,
        "team",
        FAMILY_FIX_ROOT_CAUSE,
        ROOT_CAUSE,
    ))
}

/// `memory:⟨mem_x⟩` (engine-built ids) and `memory:mem_x` (decoded record
/// ids) compare equal.
fn plain(id: &str) -> String {
    id.replace(['⟨', '⟩'], "")
}

fn sorted_plain(ids: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = ids.iter().map(|id| plain(id)).collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// C8 §Test "Integration live"
// ---------------------------------------------------------------------------

/// Two fixes with the same root cause yield ONE observation: one
/// `kind=observation` / `store=derived` row, two member `derived_from` edges
/// and one `has_memory` edge; the row is browsable with
/// `memory_list(kind=observation, store=derived)` and `memory_expand`.
#[tokio::test]
async fn two_fixes_same_root_cause_yield_one_observation() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));

    let id_a = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_A,
        CONTENT_A,
        ROOT_CAUSE,
        "team",
        "",
        Vec::new(),
    )
    .await;
    let id_b = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_B,
        CONTENT_B,
        ROOT_CAUSE,
        "team",
        "",
        Vec::new(),
    )
    .await;

    let report = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("full reconcile");
    assert_eq!(report.components, 1);
    assert_eq!(report.create, 1);
    let obs_id = team_observation_id();

    // Exactly one derived observation row.
    assert_eq!(
        count_rows(
            exec.as_ref(),
            "memory WHERE kind = 'observation' AND store = 'derived'"
        )
        .await,
        1.0
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "kind").await,
        "observation"
    );
    assert_eq!(obs_scalar(exec.as_ref(), &obs_id, "store").await, "derived");
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "status").await,
        "current"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "confidence").await,
        "0.75"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "owner").await,
        OBSERVATION_AGENT
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "policy_applied").await,
        "observation.v1"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "details.rubric").await,
        "observation.v1"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "details.family").await,
        FAMILY_FIX_ROOT_CAUSE
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "details.signature_hash").await,
        signature_hash(PROJECT, "team", FAMILY_FIX_ROOT_CAUSE, ROOT_CAUSE)
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "details.member_count").await,
        "2"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "details.generated_by").await,
        "fallback"
    );
    assert_eq!(
        obs_strings(exec.as_ref(), &obs_id, "tags").await,
        vec!["fix_root_cause".to_string(), "observation".to_string()]
    );
    let members = obs_strings(exec.as_ref(), &obs_id, "details.members").await;
    assert_eq!(members, sorted_plain(&[&id_a, &id_b]));

    // Edges: 2 members via derived_from, one has_memory.
    assert_eq!(
        count_rows(exec.as_ref(), &format!("derived_from WHERE in = {obs_id}")).await,
        2.0
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("has_memory WHERE in = repo:⟨obs_live⟩ AND out = {obs_id}")
        )
        .await,
        1.0
    );

    // Browse surface: memory_list(kind=observation, store=derived).
    let listed = svc
        .list_memories(
            &ActorContext::LOCAL,
            MemoryListParams {
                kinds: vec!["observation".to_string()],
                store: "derived".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("browse observations");
    assert_eq!(listed.len(), 1);
    assert_eq!(plain(&listed[0].id), plain(&obs_id));
    assert_eq!(listed[0].store, "derived");
    assert_eq!(listed[0].kind, "observation");

    // memory_expand returns the member graph.
    let expansion = svc
        .memory_expand(&ActorContext::LOCAL, &obs_id, 1, "")
        .await
        .expect("expand observation");
    assert!(expansion.derived_from.contains(&plain(&id_a)));
    assert!(expansion.derived_from.contains(&plain(&id_b)));

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// A second pass over unchanged evidence is a strict no-op: same id, same
/// `evidence_hash`, unchanged `updated_at`, zero LLM calls and zero
/// re-embeddings; the `has_memory` edge stays single after three passes.
#[tokio::test]
async fn second_pass_is_idempotent() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));
    for (title, content) in [(TITLE_A, CONTENT_A), (TITLE_B, CONTENT_B)] {
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            title,
            content,
            ROOT_CAUSE,
            "team",
            "",
            Vec::new(),
        )
        .await;
    }
    let obs_id = team_observation_id();

    svc.reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("first pass");
    let updated_before = obs_scalar(exec.as_ref(), &obs_id, "updated_at").await;
    let evidence_before = obs_scalar(exec.as_ref(), &obs_id, "details.evidence_hash").await;
    let embeds_after_first = ai.embed_calls.load(Ordering::SeqCst);
    assert_eq!(embeds_after_first, 1, "one embedding at synthesis");

    let report = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("second pass");
    assert_eq!(report.create, 0);
    assert_eq!(report.refresh, 0);
    assert_eq!(report.skipped, 1, "unchanged fallback row is a no-op");
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "details.evidence_hash").await,
        evidence_before
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "updated_at").await,
        updated_before,
        "idempotent pass must not touch updated_at"
    );
    assert_eq!(
        ai.embed_calls.load(Ordering::SeqCst),
        embeds_after_first,
        "no re-embedding when evidence is unchanged"
    );
    assert_eq!(
        ai.generate_calls.load(Ordering::SeqCst),
        0,
        "zero LLM calls"
    );

    // Third pass: the upsert stays a single has_memory edge.
    svc.reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("third pass");
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("has_memory WHERE in = repo:⟨obs_live⟩ AND out = {obs_id}")
        )
        .await,
        1.0,
        "upsert does not duplicate has_memory"
    );

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// Changing a member's evidence regenerates the SAME row (deterministic id),
/// keeps it `current` and creates no `supersedes` edges.
#[tokio::test]
async fn evidence_change_regenerates_same_row() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));
    let id_a = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_A,
        CONTENT_A,
        ROOT_CAUSE,
        "team",
        "",
        Vec::new(),
    )
    .await;
    add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_B,
        CONTENT_B,
        ROOT_CAUSE,
        "team",
        "",
        Vec::new(),
    )
    .await;
    let obs_id = team_observation_id();

    svc.reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("first pass");
    let evidence_before = obs_scalar(exec.as_ref(), &obs_id, "details.evidence_hash").await;

    exec.smart_query(
        &format!("UPDATE {id_a} SET content = $content, updated_at = time::now();"),
        vars_of(vec![(
            "content",
            Value::from_t(
                "The wizard now reports an actionable error for an empty discovery_root.",
            ),
        )]),
    )
    .await
    .expect("edit the source memory");

    let report = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("refresh pass");
    assert_eq!(report.create, 0);
    assert_eq!(report.refresh, 1);
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "status").await,
        "current"
    );
    let evidence_after = obs_scalar(exec.as_ref(), &obs_id, "details.evidence_hash").await;
    assert_ne!(evidence_after, evidence_before);
    // Same identity, no supersede/split churn for a plain evidence refresh.
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("supersedes WHERE in = {obs_id} OR out = {obs_id}")
        )
        .await,
        0.0
    );

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// Evidence refs from the graph (source_file) enter the observation as
/// `derived_from` edges and `details.evidence_refs` — the schema union
/// extension in action.
#[tokio::test]
async fn source_file_evidence_refs_join_the_component() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));

    let sf_id = ibis_db::ids::source_file_id("repo:⟨obs_live⟩", "src/wizard/discovery.rs");
    exec.execute(&format!(
        "CREATE {sf_id} SET rel_path = 'src/wizard/discovery.rs', path = 'src/wizard/discovery.rs', hash = 'hash-c8';"
    ))
    .await
    .expect("seed source_file");

    let id_a = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_A,
        CONTENT_A,
        ROOT_CAUSE,
        "team",
        "",
        vec![sf_id.clone()],
    )
    .await;
    let id_b = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_B,
        CONTENT_B,
        ROOT_CAUSE,
        "team",
        "",
        Vec::new(),
    )
    .await;
    let obs_id = team_observation_id();

    svc.reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("full reconcile");

    // The component carries the file signature too.
    let signatures = obs_strings(exec.as_ref(), &obs_id, "details.signatures").await;
    assert!(
        signatures
            .iter()
            .any(|signature| signature == "file|src/wizard/discovery.rs"),
        "{signatures:?}"
    );
    // Evidence refs: the source_file record (SDK rendering).
    let refs = obs_strings(exec.as_ref(), &obs_id, "details.evidence_refs").await;
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert!(refs[0].starts_with("source_file:"), "{refs:?}");
    assert!(refs[0].contains("src/wizard/discovery.rs"), "{refs:?}");
    // Edges: 2 members + 1 evidence ref.
    assert_eq!(
        count_rows(exec.as_ref(), &format!("derived_from WHERE in = {obs_id}")).await,
        3.0
    );
    let members = obs_strings(exec.as_ref(), &obs_id, "details.members").await;
    assert_eq!(members, sorted_plain(&[&id_a, &id_b]));

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// Team and personal scopes never mix: distinct observations, distinct
/// owners/visibility, disjoint members.
#[tokio::test]
async fn personal_and_team_scopes_never_mix() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));

    let team_a = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_A,
        CONTENT_A,
        ROOT_CAUSE,
        "team",
        "",
        Vec::new(),
    )
    .await;
    let team_b = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_B,
        CONTENT_B,
        ROOT_CAUSE,
        "team",
        "",
        Vec::new(),
    )
    .await;
    let alice = ActorContext::with_actor("alice");
    let personal_a = add_fix(
        &svc,
        &alice,
        TITLE_A,
        CONTENT_A,
        ROOT_CAUSE,
        "personal",
        "alice",
        Vec::new(),
    )
    .await;
    let personal_b = add_fix(
        &svc,
        &alice,
        TITLE_B,
        CONTENT_B,
        ROOT_CAUSE,
        "personal",
        "alice",
        Vec::new(),
    )
    .await;

    svc.reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("team reconcile");
    svc.reconcile_observations(PROJECT, "personal:alice", ReconcileMode::Full)
        .await
        .expect("personal reconcile");

    let team_obs = team_observation_id();
    let personal_obs = observation_id(&signature_hash(
        PROJECT,
        "personal:alice",
        FAMILY_FIX_ROOT_CAUSE,
        ROOT_CAUSE,
    ));
    assert_ne!(
        team_obs, personal_obs,
        "scope partitions the signature hash"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            "memory WHERE kind = 'observation' AND store = 'derived'"
        )
        .await,
        2.0
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &team_obs, "visibility").await,
        "team"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &team_obs, "owner").await,
        OBSERVATION_AGENT
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &personal_obs, "visibility").await,
        "personal"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &personal_obs, "owner").await,
        "alice"
    );
    assert_eq!(
        obs_strings(exec.as_ref(), &team_obs, "details.members").await,
        sorted_plain(&[&team_a, &team_b])
    );
    assert_eq!(
        obs_strings(exec.as_ref(), &personal_obs, "details.members").await,
        sorted_plain(&[&personal_a, &personal_b])
    );

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// The ask channel sees the observation (`kind=observation`, memory source).
#[tokio::test]
async fn ask_project_sees_the_observation() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));
    for (title, content) in [(TITLE_A, CONTENT_A), (TITLE_B, CONTENT_B)] {
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            title,
            content,
            ROOT_CAUSE,
            "team",
            "",
            Vec::new(),
        )
        .await;
    }
    svc.reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("full reconcile");
    let obs_id = team_observation_id();

    let results = svc
        .ask_project(&ActorContext::LOCAL, "empty discovery root crash")
        .await
        .expect("ask_project");
    let hit = results.iter().find(|r| plain(&r.id) == plain(&obs_id));
    assert!(
        hit.is_some_and(|hit| hit.kind == "observation"),
        "the memory channel must return kind=observation: {results:?}"
    );

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// End-to-end read-only guards: update/delete/share/supersede refuse, and
/// the row stays `current`.
#[tokio::test]
async fn read_only_end_to_end() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));
    for (title, content) in [(TITLE_A, CONTENT_A), (TITLE_B, CONTENT_B)] {
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            title,
            content,
            ROOT_CAUSE,
            "team",
            "",
            Vec::new(),
        )
        .await;
    }
    svc.reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("full reconcile");
    let obs_id = team_observation_id();

    let err = svc
        .update_memory(
            &ActorContext::LOCAL,
            MemoryUpdateParams {
                id: obs_id.clone(),
                content: Some("manual edit".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("update must refuse");
    assert!(
        err.to_string().contains("derived from its evidence"),
        "{err}"
    );

    let err = svc
        .delete_memory(&ActorContext::LOCAL, &obs_id, "")
        .await
        .expect_err("soft delete must refuse");
    assert!(err.to_string().contains("cannot be archived"), "{err}");

    let err = svc
        .share_memory(&ActorContext::LOCAL, &obs_id, "")
        .await
        .expect_err("share must refuse");
    assert!(err.to_string().contains("cannot be shared"), "{err}");

    let mut params = AddMemoryParams {
        project_name: PROJECT.to_string(),
        kind: "fix".to_string(),
        content: "replacement".to_string(),
        supersedes: obs_id.clone(),
        ..Default::default()
    };
    let err = svc
        .add_memory(&ActorContext::LOCAL, &mut params)
        .await
        .expect_err("supersede must refuse");
    assert!(
        err.to_string()
            .contains("only the observation engine may supersede"),
        "{err}"
    );

    // The row is untouched.
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "status").await,
        "current"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            "memory WHERE kind = 'observation' AND store = 'derived' AND status = 'current'"
        )
        .await,
        1.0
    );

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// `file_chunk.symbol_name` whose tokens all appear in the memory text joins
/// the component as a `symbol|path#symbol` signature; the `source_file` is an
/// evidence ref (`derived_from` edge) of the observation.
#[tokio::test]
async fn symbol_family_is_extracted() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));

    let path = "src/wizard/discovery.rs";
    let sf_id = ibis_db::ids::source_file_id("repo:⟨obs_live⟩", path);
    exec.execute(&format!(
        "CREATE {sf_id} SET rel_path = '{path}', path = '{path}', hash = 'hash-c8-symbol';"
    ))
    .await
    .expect("seed source_file");
    let chunk_id = ibis_db::ids::file_chunk_id(&sf_id, 10);
    exec.execute(&format!(
        "CREATE {chunk_id} SET file = {sf_id}, symbol_name = 'DiscoveryRoot', start_line = 10, end_line = 42;"
    ))
    .await
    .expect("seed file_chunk");

    let id_a = add_fix(
        &svc,
        &ActorContext::LOCAL,
        "DiscoveryRoot crashes setup",
        "The wizard crashes when DiscoveryRoot is empty.",
        ROOT_CAUSE,
        "team",
        "",
        vec![sf_id.clone()],
    )
    .await;
    let id_b = add_fix(
        &svc,
        &ActorContext::LOCAL,
        "DiscoveryRoot guard missing",
        "Setup aborts when DiscoveryRoot is empty.",
        ROOT_CAUSE,
        "team",
        "",
        vec![sf_id.clone()],
    )
    .await;
    let obs_id = team_observation_id();

    let report = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("full reconcile");
    assert_eq!(report.components, 1);
    assert_eq!(report.create, 1);

    let signatures = obs_strings(exec.as_ref(), &obs_id, "details.signatures").await;
    let expected = format!("symbol|{path}#discoveryroot");
    assert!(
        signatures.iter().any(|signature| signature == &expected),
        "missing {expected}: {signatures:?}"
    );
    assert!(
        signatures
            .iter()
            .any(|signature| signature == &format!("file|{path}")),
        "{signatures:?}"
    );

    // The source_file is both an evidence ref and a derived_from edge.
    let refs = obs_strings(exec.as_ref(), &obs_id, "details.evidence_refs").await;
    assert!(
        refs.iter()
            .any(|r| r.starts_with("source_file:") && r.contains(path)),
        "{refs:?}"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("derived_from WHERE in = {obs_id} AND out = {sf_id}")
        )
        .await,
        1.0,
        "the observation must derive from the source_file"
    );
    assert_eq!(
        count_rows(exec.as_ref(), &format!("derived_from WHERE in = {obs_id}")).await,
        3.0,
        "2 members + 1 source_file evidence ref"
    );
    let members = obs_strings(exec.as_ref(), &obs_id, "details.members").await;
    assert_eq!(members, sorted_plain(&[&id_a, &id_b]));

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// `commit -> implements -> issue` behind the memories yields the canonical
/// `issue|<provider>#<key>` signature on the observation.
#[tokio::test]
async fn issue_family_is_extracted() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));

    let commit_id = ibis_db::ids::format_record_id("commit", "c8e15issue");
    exec.execute(&format!(
        "CREATE {commit_id} SET hash = 'c8e15issue', date = time::now(), message = 'wire the issue anchor';"
    ))
    .await
    .expect("seed commit");
    // Real tracker keys are hyphenated (`IBIS-42`): the evidence edge must
    // render the record id safely, not as `issue:IBIS - 42`.
    let issue_id = ibis_db::ids::format_record_id("issue", "IBIS-42");
    exec.execute(&format!(
        "CREATE {issue_id} SET provider = 'github', external_key = 'IBIS-42', subject = 'observation review gap';"
    ))
    .await
    .expect("seed issue");
    exec.execute(&format!("RELATE {commit_id}->implements->{issue_id};"))
        .await
        .expect("seed implements edge");

    let id_a = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_A,
        CONTENT_A,
        ROOT_CAUSE,
        "team",
        "",
        vec![commit_id.clone()],
    )
    .await;
    let id_b = add_fix(
        &svc,
        &ActorContext::LOCAL,
        TITLE_B,
        CONTENT_B,
        ROOT_CAUSE,
        "team",
        "",
        vec![commit_id.clone()],
    )
    .await;
    let obs_id = team_observation_id();

    let report = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("full reconcile");
    assert_eq!(report.components, 1);
    assert_eq!(report.create, 1);

    let signatures = obs_strings(exec.as_ref(), &obs_id, "details.signatures").await;
    assert!(
        signatures
            .iter()
            .any(|signature| signature == "issue|github#IBIS-42"),
        "{signatures:?}"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("derived_from WHERE in = {obs_id} AND out = {commit_id}")
        )
        .await,
        1.0,
        "the commit is an evidence ref"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("derived_from WHERE in = {obs_id} AND out = {issue_id}")
        )
        .await,
        1.0,
        "the issue is an evidence ref"
    );
    let members = obs_strings(exec.as_ref(), &obs_id, "details.members").await;
    assert_eq!(members, sorted_plain(&[&id_a, &id_b]));

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// Split: four fixes share `combined` (one observation); moving two to
/// `alpha` and two to `beta` removes the old anchor, so the two new rows
/// supersede the old one (one incoming edge each).
#[tokio::test]
async fn split_supersedes_old_observation() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));

    let ids = [
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            "Combined A",
            "First combined member.",
            "combined",
            "team",
            "",
            Vec::new(),
        )
        .await,
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            "Combined B",
            "Second combined member.",
            "combined",
            "team",
            "",
            Vec::new(),
        )
        .await,
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            "Combined C",
            "Third combined member.",
            "combined",
            "team",
            "",
            Vec::new(),
        )
        .await,
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            "Combined D",
            "Fourth combined member.",
            "combined",
            "team",
            "",
            Vec::new(),
        )
        .await,
    ];
    let old_id = observation_id(&signature_hash(
        PROJECT,
        "team",
        FAMILY_FIX_ROOT_CAUSE,
        "combined",
    ));

    let first = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("first reconcile");
    assert_eq!(first.components, 1);
    assert_eq!(first.create, 1);
    assert_eq!(
        obs_scalar(exec.as_ref(), &old_id, "status").await,
        "current"
    );

    set_root_cause(exec.as_ref(), &ids[0], "alpha").await;
    set_root_cause(exec.as_ref(), &ids[1], "alpha").await;
    set_root_cause(exec.as_ref(), &ids[2], "beta").await;
    set_root_cause(exec.as_ref(), &ids[3], "beta").await;

    let report = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("split reconcile");
    assert_eq!(report.components, 2);
    assert_eq!(report.create, 2);
    assert_eq!(report.supersede, 2, "one incoming edge per new component");

    let alpha_id = observation_id(&signature_hash(
        PROJECT,
        "team",
        FAMILY_FIX_ROOT_CAUSE,
        "alpha",
    ));
    let beta_id = observation_id(&signature_hash(
        PROJECT,
        "team",
        FAMILY_FIX_ROOT_CAUSE,
        "beta",
    ));
    assert_eq!(
        obs_scalar(exec.as_ref(), &old_id, "status").await,
        "superseded"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &alpha_id, "status").await,
        "current"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &beta_id, "status").await,
        "current"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            "memory WHERE kind = 'observation' AND store = 'derived' AND status = 'current'"
        )
        .await,
        2.0
    );
    assert_eq!(
        count_rows(exec.as_ref(), &format!("supersedes WHERE out = {old_id}")).await,
        2.0,
        "both new observations supersede the old row"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("supersedes WHERE in = {alpha_id} AND out = {old_id}")
        )
        .await,
        1.0
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("supersedes WHERE in = {beta_id} AND out = {old_id}")
        )
        .await,
        1.0
    );

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// Merge: `alpha` and `beta` observations both collapse into a third
/// `merged` root cause; the new row supersedes both old rows.
#[tokio::test]
async fn merge_supersedes_both_old_observations() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));

    let ids = [
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            "Alpha A",
            "First alpha member.",
            "alpha",
            "team",
            "",
            Vec::new(),
        )
        .await,
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            "Alpha B",
            "Second alpha member.",
            "alpha",
            "team",
            "",
            Vec::new(),
        )
        .await,
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            "Beta A",
            "First beta member.",
            "beta",
            "team",
            "",
            Vec::new(),
        )
        .await,
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            "Beta B",
            "Second beta member.",
            "beta",
            "team",
            "",
            Vec::new(),
        )
        .await,
    ];
    let alpha_id = observation_id(&signature_hash(
        PROJECT,
        "team",
        FAMILY_FIX_ROOT_CAUSE,
        "alpha",
    ));
    let beta_id = observation_id(&signature_hash(
        PROJECT,
        "team",
        FAMILY_FIX_ROOT_CAUSE,
        "beta",
    ));

    let first = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("first reconcile");
    assert_eq!(first.components, 2);
    assert_eq!(first.create, 2);
    assert_eq!(
        count_rows(
            exec.as_ref(),
            "memory WHERE kind = 'observation' AND store = 'derived' AND status = 'current'"
        )
        .await,
        2.0
    );

    for id in &ids {
        set_root_cause(exec.as_ref(), id, "merged").await;
    }

    let report = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("merge reconcile");
    assert_eq!(report.components, 1);
    assert_eq!(report.create, 1);
    assert_eq!(report.supersede, 2, "the new row supersedes both olds");

    let merged_id = observation_id(&signature_hash(
        PROJECT,
        "team",
        FAMILY_FIX_ROOT_CAUSE,
        "merged",
    ));
    assert_eq!(
        obs_scalar(exec.as_ref(), &merged_id, "status").await,
        "current"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &alpha_id, "status").await,
        "superseded"
    );
    assert_eq!(
        obs_scalar(exec.as_ref(), &beta_id, "status").await,
        "superseded"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            "memory WHERE kind = 'observation' AND store = 'derived' AND status = 'current'"
        )
        .await,
        1.0
    );
    assert_eq!(
        count_rows(exec.as_ref(), &format!("supersedes WHERE in = {merged_id}")).await,
        2.0,
        "the merged observation supersedes both old rows"
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("supersedes WHERE in = {merged_id} AND out = {alpha_id}")
        )
        .await,
        1.0
    );
    assert_eq!(
        count_rows(
            exec.as_ref(),
            &format!("supersedes WHERE in = {merged_id} AND out = {beta_id}")
        )
        .await,
        1.0
    );
    let members = obs_strings(exec.as_ref(), &merged_id, "details.members").await;
    assert_eq!(members, sorted_plain(&[&ids[0], &ids[1], &ids[2], &ids[3]]));

    drop_scratch(exec.as_ref(), &db_name).await;
}

/// `InvalidateOnly` reactivation must clear the archival `valid_until`, not
/// only flip `status` back to `current`.
#[tokio::test]
async fn invalidate_only_reactivation_clears_valid_until() {
    let _serial = SERIAL.lock().await;
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let ai = Arc::new(C8Ai::default());
    let svc = service(&exec, Some(ai.clone()));

    for (title, content) in [(TITLE_A, CONTENT_A), (TITLE_B, CONTENT_B)] {
        add_fix(
            &svc,
            &ActorContext::LOCAL,
            title,
            content,
            ROOT_CAUSE,
            "team",
            "",
            Vec::new(),
        )
        .await;
    }
    let obs_id = team_observation_id();
    svc.reconcile_observations(PROJECT, "team", ReconcileMode::Full)
        .await
        .expect("full reconcile");

    exec.execute(&format!(
        "UPDATE {obs_id} SET status = 'archived', valid_until = time::now(), updated_at = time::now();"
    ))
    .await
    .expect("archive the observation");
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "status").await,
        "archived"
    );
    assert!(
        !obs_scalar(exec.as_ref(), &obs_id, "valid_until")
            .await
            .is_empty(),
        "the raw archive stamps valid_until"
    );

    let report = svc
        .reconcile_observations(PROJECT, "team", ReconcileMode::InvalidateOnly)
        .await
        .expect("reactivation reconcile");
    assert_eq!(report.refresh, 1, "the archived row is reactivated");
    assert_eq!(
        obs_scalar(exec.as_ref(), &obs_id, "status").await,
        "current"
    );
    assert!(
        obs_scalar(exec.as_ref(), &obs_id, "valid_until")
            .await
            .is_empty(),
        "reactivation must clear valid_until"
    );

    drop_scratch(exec.as_ref(), &db_name).await;
}
