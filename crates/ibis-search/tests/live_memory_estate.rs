//! Live memory estate (row H-A03): the REAL memory stack over the
//! live SurrealDB sidecar + real git fixtures — claim store reconciliation,
//! ask at-ref, as-of claims, knowledge diff, hot-ref cache, ancestry TTL and
//! the per-table serde roundtrip gate.
//!
//! Sidecar: `IBIS_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`,
//! root/root). Every test creates its own `h03_*` scratch database, runs the
//! SCHEMAFULL DDL and drops it at the end; when the server is unreachable the
//! test skips gracefully (same pattern as `ibis-db/tests/live_surreal.rs`).
//!
//! Intents pinned live: memory claim store, ask-ref retrieval, as-of world
//! state, ancestry proof, knowledge diff, hot-ref cache, claim notes and
//! claim tree (pointer notes + tree unions).
//! (pointer notes + tree unions).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tempfile::TempDir;

use ibis_ai::{AIProvider, AiError};
use ibis_core::claims::{self, Claim};
use ibis_db::executor::{SurrealDbExecutor, Vars};
use ibis_db::schema;
use ibis_db::tables::{MemoryKind, MemoryRow, MemoryStatus, Store, Visibility};
use ibis_db::value::{Embedding, RecordIdKey, Value};
use ibis_db::Executor;
use ibis_git::{
    ancestor_set_for, AncestorCache, AncestorSet, GitAnchorRunner, GitClaimWriter, GitCommitter,
    GitNotesRunner, GitNotesSource, GitRunner, NOTES_REF,
};
use ibis_search::memory::add::AddMemoryParams;
use ibis_search::memory::ancestry::invalidate_ancestors;
use ibis_search::memory::claim_store::{
    resolve_personal_claims_policy, PERSONAL_CLAIMS_DB_ONLY, PERSONAL_CLAIMS_IN_REPO,
    PERSONAL_CLAIMS_STUB,
};
use ibis_search::memory::kdiff::DiffMode;
use ibis_search::rows;
use ibis_search::{ActorContext, SearchService};

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";
const NS: &str = "ibis_db_tests";

// ---------------------------------------------------------------------------
// scratch-DB lifecycle: scratch SurrealDB → InitSchema → write → raw
// SELECT → decode → compare → drop)
// ---------------------------------------------------------------------------

/// Connects to the sidecar on a fresh `h03_*` scratch database. Returns
/// `None` when unreachable (test skips).
async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("h03_{}", uuid::Uuid::new_v4().simple());
    let exec = match SurrealDbExecutor::connect(&url, NS, &db_name, "root", "root").await {
        Ok(exec) => exec,
        Err(err) => {
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

/// Full SCHEMAFULL DDL on the scratch DB (idempotent; re-run in the serde
/// roundtrip test to prove re-init safety).
async fn init_ddl(exec: &dyn Executor) {
    schema::init_schema(exec).await.expect("init schema");
}

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn str_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn run_git(dir: &str, args: &[&str]) -> String {
    let out = GitRunner::anonymous()
        .run(dir, args)
        .await
        .unwrap_or_else(|e| panic!("git {args:?} in {dir} failed: {e}"));
    String::from_utf8_lossy(&out).into_owned()
}

/// Real git repo in a tempdir (same fixture shape as
/// `ibis-git/tests/git_integration.rs`).
struct Repo {
    _dir: TempDir,
    path: String,
}

impl Repo {
    fn s(&self) -> &str {
        &self.path
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    async fn run(&self, args: &[&str]) -> String {
        run_git(&self.path, args).await
    }

    async fn head(&self) -> String {
        self.run(&["rev-parse", "HEAD"]).await.trim().to_string()
    }

    async fn branch(&self) -> String {
        self.run(&["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .trim()
            .to_string()
    }

    /// Writes bytes (binary-safe), stages and commits; returns the new HEAD.
    async fn commit_file(&self, rel: &str, content: &[u8], msg: &str) -> String {
        let abs = self.root().join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&abs, content).unwrap();
        self.run(&["add", rel]).await;
        self.run(&["commit", "-q", "-m", msg]).await;
        self.head().await
    }

    /// Commits claim files through the production path-scoped committer.
    async fn commit_claim(
        &self,
        writer: &GitCommitter,
        message: &str,
        writes: &HashMap<String, Vec<u8>>,
        deletes: &[String],
    ) -> String {
        writer
            .commit_claim(self.s(), message, writes, deletes)
            .await
            .expect("commit claim")
    }
}

async fn init_repo(name: &str) -> Repo {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    let repo_path = str_of(&path);
    run_git(&repo_path, &["init", "-q"]).await;
    run_git(&repo_path, &["config", "user.name", "H03 Test"]).await;
    run_git(&repo_path, &["config", "user.email", "h03@test"]).await;
    run_git(&repo_path, &["config", "core.autocrlf", "false"]).await;
    Repo {
        _dir: dir,
        path: repo_path,
    }
}

/// Deterministic 768-dim embedding derived from the text bytes.
fn h03_vec(text: &str) -> Vec<f32> {
    let bytes = text.as_bytes();
    let mut v = vec![0f32; 768];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = f32::from(bytes[i % bytes.len()]) / 255.0 - 0.5;
    }
    v
}

fn emb(text: &str) -> Value {
    Value::from_t(Embedding(h03_vec(text)))
}

/// Live-estate embedding provider: the AI lane itself is not live (no Ollama
/// sidecar), matching the original estate where `MockAI` backs the same scenarios.
struct H03Ai;

#[async_trait]
impl AIProvider for H03Ai {
    fn name(&self) -> &str {
        "h03-live-embedder"
    }

    async fn is_functional(&self) -> bool {
        true
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        Ok(h03_vec(text))
    }
}

fn vars_of(pairs: Vec<(&str, Value)>) -> Vars {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// The service under test wired to REAL git sources (no fakes).
fn service(exec: &Arc<dyn Executor>, ai: bool) -> (SearchService, GitCommitter) {
    let mut svc = SearchService::new(
        Some(exec.clone()),
        if ai {
            Some(Arc::new(H03Ai) as Arc<dyn AIProvider>)
        } else {
            None
        },
    );
    let runner = GitRunner::anonymous();
    svc.git_anchors = Some(Arc::new(GitAnchorRunner::new(runner.clone())));
    let committer = GitCommitter::new(runner.clone());
    svc.claim_writer = Some(Arc::new(committer.clone()));
    svc.notes = Some(Arc::new(GitNotesRunner::new(runner)));
    (svc, committer)
}

/// Seeds one typed memory row with bound values (record link inlined — the
/// only identifier interpolation, per F-002). Returns the record id.
async fn seed_memory(exec: &dyn Executor, key: &str, project: &str, fields: Vec<(&str, Value)>) {
    let mut sets: Vec<String> = Vec::with_capacity(fields.len() + 1);
    let mut vars = Vars::new();
    for (i, (name, value)) in fields.iter().enumerate() {
        let var = format!("f{i}");
        sets.push(format!("{name} = ${var}"));
        vars.insert(var, value.clone());
    }
    if !project.is_empty() {
        sets.push(format!("project = repo:⟨{}⟩", project));
    }
    let sql = format!("CREATE memory:⟨{key}⟩ SET {};", sets.join(", "));
    exec.smart_query(&sql, vars).await.expect("seed memory row");
}

/// Decodes one memory row through the typed row struct decode).
async fn memory_row(exec: &dyn Executor, id: &str) -> MemoryRow {
    let sql = format!("SELECT * FROM {id};");
    let outcome = exec.execute(&sql).await.expect("select memory");
    let mut list = outcome.rows::<MemoryRow>(&sql).expect("decode memory");
    assert_eq!(list.len(), 1, "expected exactly one row for {id}");
    list.remove(0)
}

/// `Some(content)` when a `file_at_commit` row exists, `None` otherwise.
async fn cache_row_content(exec: &dyn Executor, oid: &str, path: &str) -> Option<String> {
    let record_id = format!("file_at_commit:⟨{oid}:{path}⟩");
    let sql = format!("SELECT content FROM {record_id};");
    let outcome = exec.execute(&sql).await.expect("select cache row");
    let rows_values = rows::normalize_rows(&outcome).expect("normalize cache rows");
    let obj = rows::object_rows(&rows_values);
    obj.first()
        .map(|row| rows::scalar_string(rows::field(row, "content")))
}

/// Raw `SELECT VALUE id` over the ask at_ref result ids.
fn result_ids(results: &[ibis_search::SearchResult]) -> Vec<String> {
    results.iter().map(|r| r.id.clone()).collect()
}

fn claim_bytes(id: &str, kind: &str, title: &str, visibility: &str, body: &str) -> Vec<u8> {
    claims::render(&Claim {
        schema: claims::SCHEMA_VERSION,
        id: id.to_string(),
        kind: kind.to_string(),
        title: title.to_string(),
        visibility: visibility.to_string(),
        related_paths: Vec::new(),
        supersedes: String::new(),
        created: Some(chrono::Utc::now()),
        redacted: String::new(),
        body: body.to_string(),
    })
    .expect("render claim")
}

fn one_write(path: &str, data: Vec<u8>) -> HashMap<String, Vec<u8>> {
    let mut writes = HashMap::new();
    writes.insert(path.to_string(), data);
    writes
}

// ---------------------------------------------------------------------------
// — per-table serde roundtrip (the CI gate): write memory rows with
// provenance absent and partial → raw SELECT → decode → field-by-field assert
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serde_roundtrip_provenance_shapes_and_typed_decode_errors() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    // Re-init safety (LB-07: `IF NOT EXISTS` everywhere).
    schema::init_schema(exec.as_ref())
        .await
        .expect("re-init schema");

    // Provenance ABSENT: server-side defaults from the typed fields.
    exec.smart_query(
        "CREATE memory:⟨h03_prov_absent⟩ SET content = $content;",
        vars_of(vec![("content", Value::from_t("prov absent"))]),
    )
    .await
    .expect("create row");
    let row = memory_row(exec.as_ref(), "memory:⟨h03_prov_absent⟩").await;
    assert_eq!(row.kind, MemoryKind::Fact);
    assert_eq!(row.status, MemoryStatus::Current);
    assert_eq!(row.visibility, Visibility::Team);
    assert_eq!(row.confidence, Some(ibis_db::value::F64(0.7)));
    assert_eq!(row.tags, Some(Vec::new()));
    assert_eq!(row.provenance, None);
    assert_eq!(row.store, None);
    assert_eq!(row.introduced_in_oid, None);
    assert!(row.created_at.into_inner() > chrono::Utc::now() - chrono::Duration::minutes(1));

    // Provenance PARTIAL (production writer shape: `UPDATE … SET provenance.x`).
    exec.smart_query(
        "UPDATE memory:⟨h03_prov_absent⟩ SET provenance.agent = $agent, provenance.tool = $tool;",
        vars_of(vec![
            ("agent", Value::from_t("h03-agent")),
            ("tool", Value::from_t("memory_add")),
        ]),
    )
    .await
    .expect("partial provenance");
    let row = memory_row(exec.as_ref(), "memory:⟨h03_prov_absent⟩").await;
    let prov = row.provenance.as_ref().expect("provenance object");
    assert_eq!(prov.agent.as_deref(), Some("h03-agent"));
    assert_eq!(prov.tool.as_deref(), Some("memory_add"));
    assert_eq!(prov.session, None);
    assert_eq!(prov.introduced_in_commit, None);

    // Git-bound provenance: denormalized scalars + record links + dirty flags.
    let oid = "a".repeat(40);
    exec.smart_query(
        &format!(
            "UPDATE memory:⟨h03_prov_absent⟩ SET introduced_in_oid = $oid, \
             provenance.introduced_in_commit = commit:⟨{oid}⟩, provenance.dirty = true, \
             provenance.worktree_hash = $wh, provenance.promote_on_commit = true;"
        ),
        vars_of(vec![
            ("oid", Value::from_t(oid.as_str())),
            ("wh", Value::from_t("worktree-hash")),
        ]),
    )
    .await
    .expect("git-bound provenance");
    let row = memory_row(exec.as_ref(), "memory:⟨h03_prov_absent⟩").await;
    assert_eq!(row.introduced_in_oid.as_deref(), Some(oid.as_str()));
    let prov = row.provenance.as_ref().expect("provenance");
    let link = prov.introduced_in_commit.as_ref().expect("commit link");
    assert_eq!(link.table.as_str(), "commit");
    assert_eq!(link.key, RecordIdKey::String(oid.clone()));
    assert_eq!(prov.dirty, Some(true));
    assert_eq!(prov.worktree_hash.as_deref(), Some("worktree-hash"));
    assert_eq!(prov.promote_on_commit, Some(true));

    // Temporal datetimes, tags, branch, embedding and a bound confidence.
    exec.smart_query(
        "CREATE memory:⟨h03_prov_full⟩ SET content = $content, title = $title, \
         valid_from = type::datetime($vf), valid_until = type::datetime($vu), \
         tags = $tags, branch = $branch, confidence = $conf, embedding = $embedding;",
        vars_of(vec![
            ("content", Value::from_t("full row")),
            ("title", Value::from_t("Full row")),
            ("vf", Value::from_t("2026-01-02T03:04:05Z")),
            ("vu", Value::from_t("2027-01-02T03:04:05Z")),
            (
                "tags",
                Value::from_t(vec![Value::from_t("alpha"), Value::from_t("beta")]),
            ),
            ("branch", Value::from_t("main")),
            ("conf", Value::from_t(0.9f64)),
            ("embedding", emb("full row")),
        ]),
    )
    .await
    .expect("create full row");
    let row = memory_row(exec.as_ref(), "memory:⟨h03_prov_full⟩").await;
    let vf = row.valid_from.expect("valid_from");
    assert_eq!(
        vf.into_inner(),
        chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    );
    let vu = row.valid_until.expect("valid_until");
    assert_eq!(
        vu.into_inner(),
        chrono::DateTime::parse_from_rfc3339("2027-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    );
    assert_eq!(
        row.tags,
        Some(vec!["alpha".to_string(), "beta".to_string()])
    );
    assert_eq!(row.branch.as_deref(), Some("main"));
    assert_eq!(row.confidence, Some(ibis_db::value::F64(0.9)));
    assert_eq!(row.embedding.as_ref().map(|e| e.0.len()), Some(768));

    // No-unwrap rule: decoding failures surface typed errors with
    // the SQL excerpt — a schemaless-ish file_at_commit row is not a MemoryRow.
    exec.execute(
        "CREATE file_at_commit:⟨h03_decode_err⟩ SET oid = 'x', path = 'p', content = 'c';",
    )
    .await
    .expect("create cache row");
    let sql = "SELECT * FROM file_at_commit:⟨h03_decode_err⟩;";
    let outcome = exec.execute(sql).await.expect("select cache row");
    let err = outcome
        .rows::<MemoryRow>(sql)
        .expect_err("file_at_commit row must not decode as MemoryRow");
    assert!(
        err.to_string().contains("SELECT * FROM file_at_commit"),
        "decode error must carry the SQL excerpt: {err}"
    );

    drop_scratch(exec.as_ref(), &db_name).await;
}

// ---------------------------------------------------------------------------
// claim store: add → claim commit → row + pointer note → reconcile → withdraw
// ( + note hook)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_store_add_commit_reconcile_and_withdraw() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let repo = init_repo("h03_claimstore").await;
    repo.commit_file("src/base.txt", b"base\n", "base").await;
    // Unrelated dirty file that must never be swept into the claim commit.
    std::fs::write(repo.root().join("dirty.txt"), "user change").unwrap();
    let (svc, committer) = service(&exec, false);

    // Add a git-owned team decision: claim file lands BEFORE the Surreal row.
    let id = svc
        .add_memory(
            &ActorContext::LOCAL,
            &mut AddMemoryParams {
                project_name: "h03_claims".to_string(),
                kind: claims::KIND_DECISION.to_string(),
                title: "Bind claims to commits".to_string(),
                content: "Every decision must carry a commit anchor.".to_string(),
                visibility: claims::VISIBILITY_TEAM.to_string(),
                workspace_root: repo.s().to_string(),
                embedding: vec![0.1; 768],
                ..Default::default()
            },
        )
        .await
        .expect("add git-owned claim");
    let key = id
        .strip_prefix("memory:⟨")
        .unwrap()
        .strip_suffix("⟩")
        .unwrap()
        .to_string();
    let claim_path = format!(".ibis/claims/{key}.md");

    let head = repo.head().await;
    assert_eq!(head.len(), 40, "claim commit created");
    // Path-scoped commit: exactly the claim file; the dirty file stays dirty.
    let names = repo
        .run(&["show", "--name-only", "--format=", "HEAD"])
        .await;
    assert_eq!(
        names.trim(),
        claim_path,
        "commit contains only the claim file"
    );
    let status = repo.run(&["status", "--porcelain"]).await;
    assert!(status.contains("dirty.txt"), "unrelated dirty file lost");
    // Claim file on disk with the rendered frontmatter + body.
    let file = std::fs::read(repo.root().join(&claim_path)).unwrap();
    let text = String::from_utf8_lossy(&file).into_owned();
    for want in [
        "kind: decision",
        "title: Bind claims to commits",
        "commit anchor.",
    ] {
        assert!(text.contains(want), "claim file missing {want:?}:\n{text}");
    }
    // Pointer note on the claims commit (PR #34 hook, best-effort by design).
    let notes = GitNotesRunner::new(GitRunner::anonymous())
        .notes_list(repo.s(), NOTES_REF)
        .await
        .expect("notes list");
    let note = notes.get(&head).expect("pointer note on claims commit");
    assert!(note.contains(&format!("claim={key}")), "note body: {note}");

    // DB row: git-owned, anchored to the claims commit (I from file history).
    let row = memory_row(exec.as_ref(), &id).await;
    assert_eq!(row.store, Some(Store::Git));
    assert_eq!(row.kind, MemoryKind::Decision);
    assert_eq!(row.visibility, Visibility::Team);
    assert_eq!(row.status, MemoryStatus::Current);
    assert_eq!(row.introduced_in_oid.as_deref(), Some(head.as_str()));
    let prov = row.provenance.as_ref().expect("provenance");
    let link = prov.introduced_in_commit.as_ref().expect("commit link");
    assert_eq!(link.key, RecordIdKey::String(head.clone()));
    assert_eq!(row.content, "Every decision must carry a commit anchor.");

    // Reconcile: the present file upserts I/X denormalized from history
    // ( TestIndexClaimStoreReconcilesExistingRow — no re-create, one UPDATE).
    let n = svc
        .index_claim_store(repo.s(), "h03_claims")
        .await
        .expect("index claim store");
    assert_eq!(n, 1, "reconciled existing claim");
    assert_eq!(repo.head().await, head, "reconcile must not commit");
    let row = memory_row(exec.as_ref(), &id).await;
    assert_eq!(row.store, Some(Store::Git));

    // Hand-written claim files are legitimate writes (repo is SoT): the valid
    // one gets a minimal row, the malformed one is skipped, never fatal
    // ( TestIndexClaimStoreCreatesRowsFromTree / SkipsMalformed).
    let manual = claim_bytes(
        "dec_h03_manual",
        claims::KIND_DECISION,
        "Hand written claim",
        claims::VISIBILITY_TEAM,
        "Manual body.",
    );
    let broken: Vec<u8> = b"not frontmatter at all".to_vec();
    let mut writes = one_write(".ibis/claims/dec_h03_manual.md", manual);
    writes.insert(".ibis/claims/h03_broken.md".to_string(), broken);
    let manual_commit = repo
        .commit_claim(
            &committer,
            "chore(claims): hand write dec_h03_manual",
            &writes,
            &[],
        )
        .await;
    let n = svc
        .index_claim_store(repo.s(), "h03_claims")
        .await
        .expect("index after hand write");
    assert_eq!(n, 2, "manual create + mem_<hex> reconcile");
    let manual_row = memory_row(exec.as_ref(), "memory:⟨dec_h03_manual⟩").await;
    assert_eq!(manual_row.store, Some(Store::Git));
    assert_eq!(manual_row.kind, MemoryKind::Decision);
    assert!(
        manual_row.content.contains("Manual body."),
        "manual row content: {:?}",
        manual_row.content
    );
    assert_eq!(
        manual_row.introduced_in_oid.as_deref(),
        Some(manual_commit.as_str()),
        "I(m) = oldest commit touching the file"
    );
    assert!(
        memory_row_absent(exec.as_ref(), "memory:⟨h03_broken⟩").await,
        "malformed file must be skipped"
    );

    // Withdrawal: the claim file vanishes → row archived with X from history
    // ( TestIndexClaimStoreWithdrawsRemovedClaims).
    repo.commit_claim(
        &committer,
        &format!("chore(claims): withdraw {key}"),
        &HashMap::new(),
        std::slice::from_ref(&claim_path),
    )
    .await;
    let withdraw_commit = repo.head().await;
    let n = svc
        .index_claim_store(repo.s(), "h03_claims")
        .await
        .expect("index after withdrawal");
    assert_eq!(n, 2, "withdrawal + manual reconcile");
    let row = memory_row(exec.as_ref(), &id).await;
    assert_eq!(row.status, MemoryStatus::Archived);
    assert_eq!(
        row.invalidated_at_oid.as_deref(),
        Some(withdraw_commit.as_str()),
        "X(m) = newest commit touching the file"
    );
    let prov = row
        .provenance
        .as_ref()
        .expect("provenance after withdrawal");
    assert!(prov.invalidated_at_commit.is_some());
    // Idempotent: a second pass withdraws nothing new.
    let n = svc
        .index_claim_store(repo.s(), "h03_claims")
        .await
        .expect("index idempotence");
    assert_eq!(n, 1, "only the manual reconcile remains");
    let manual_row = memory_row(exec.as_ref(), "memory:⟨dec_h03_manual⟩").await;
    assert_eq!(manual_row.status, MemoryStatus::Current);

    // Governance: git-owned rows are immutable via the DB (repo is SoT).
    let err = svc
        .update_memory(
            &ActorContext::LOCAL,
            ibis_search::memory::MemoryUpdateParams {
                id: id.clone(),
                content: Some("Tampered".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("git-owned claim must refuse content changes");
    assert!(err.to_string().contains("immutable"), "{err}");
    let err = svc
        .delete_memory(&ActorContext::LOCAL, &id, "")
        .await
        .expect_err("git-owned claim must refuse DB deletion");
    assert!(err.to_string().contains("withdrawn"), "{err}");

    drop_scratch(exec.as_ref(), &db_name).await;
}

async fn memory_row_absent(exec: &dyn Executor, id: &str) -> bool {
    let sql = format!("SELECT * FROM {id};");
    let outcome = exec.execute(&sql).await.expect("select absent row");
    outcome.rows::<MemoryRow>(&sql).expect("decode").is_empty()
}

// ---------------------------------------------------------------------------
// claim store decision matrix: kind × visibility × personal-claims policy
// ( TestClaimStoreDecisionPolicyLevels + personal stub/in_repo/db_only adds)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_store_decision_matrix_and_personal_policies() {
    // Pure policy resolution ( ResolvePersonalClaimsPolicy).
    assert_eq!(
        resolve_personal_claims_policy("", "server"),
        PERSONAL_CLAIMS_DB_ONLY
    );
    assert_eq!(
        resolve_personal_claims_policy("", "personal"),
        PERSONAL_CLAIMS_STUB
    );
    assert_eq!(
        resolve_personal_claims_policy("in_repo", "server"),
        PERSONAL_CLAIMS_IN_REPO
    );

    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let repo = init_repo("h03_matrix").await;
    let base = repo.commit_file("src/base.txt", b"base\n", "base").await;
    let (svc, _committer) = service(&exec, false);

    // Non-eligible kind stays Surreal-only; a clean file-anchored source binds
    // HEAD via the worktree anchor ( TestAddMemorySurrealOnlyForNonEligibleKind
    // + memory_gitbound binding).
    let id = svc
        .add_memory(
            &ActorContext::LOCAL,
            &mut AddMemoryParams {
                project_name: "h03_matrix".to_string(),
                kind: "fact".to_string(),
                content: "Plain fact anchored to a file".to_string(),
                workspace_root: repo.s().to_string(),
                provenance_source_path: "src/base.txt".to_string(),
                embedding: vec![0.1; 768],
                ..Default::default()
            },
        )
        .await
        .expect("add fact");
    let row = memory_row(exec.as_ref(), &id).await;
    assert_eq!(row.store, Some(Store::Surreal));
    assert_eq!(row.introduced_in_oid.as_deref(), Some(base.as_str()));
    assert_eq!(repo.head().await, base, "no claim commit for fact kind");
    assert!(!repo.root().join(".ibis/claims").exists());

    // require_clean refuses on a dirty tree before any DB write or commit
    // ( TestAddMemoryRequireCleanRefusesDirtyClaimCommit).
    std::fs::write(repo.root().join("dirty.txt"), "x").unwrap();
    let err = svc
        .add_memory(
            &ActorContext::LOCAL,
            &mut AddMemoryParams {
                project_name: "h03_matrix".to_string(),
                kind: claims::KIND_DECISION.to_string(),
                content: "Decision on dirty tree".to_string(),
                workspace_root: repo.s().to_string(),
                anchor_mode: "require_clean".to_string(),
                embedding: vec![0.1; 768],
                ..Default::default()
            },
        )
        .await
        .expect_err("dirty tree must refuse the claim commit");
    assert!(err.to_string().contains("dirty"), "{err}");
    assert_eq!(repo.head().await, base, "no commit on refusal");

    // Personal + stub policy: stub file committed (no body leak), full body
    // kept in the DB row with the policy marker.
    let svc = svc_with_policy(svc, PERSONAL_CLAIMS_STUB);
    let head_before = repo.head().await;
    let stub_id = svc
        .add_memory(
            &ActorContext::with_actor("alice"),
            &mut AddMemoryParams {
                project_name: "h03_matrix".to_string(),
                kind: claims::KIND_DECISION.to_string(),
                title: "Private decision".to_string(),
                content: "Secret reasoning that must not leak.".to_string(),
                visibility: claims::VISIBILITY_PERSONAL.to_string(),
                workspace_root: repo.s().to_string(),
                embedding: vec![0.2; 768],
                ..Default::default()
            },
        )
        .await
        .expect("add personal stub claim");
    let stub_head = repo.head().await;
    assert_ne!(stub_head, head_before, "stub policy commits a file");
    let stub_key = stub_id
        .strip_prefix("memory:⟨")
        .unwrap()
        .strip_suffix("⟩")
        .unwrap();
    let file =
        std::fs::read_to_string(repo.root().join(format!(".ibis/claims/{stub_key}.md"))).unwrap();
    assert!(file.contains("redacted: stub"), "{file}");
    assert!(file.contains("visibility: personal"), "{file}");
    assert!(
        !file.contains("Secret reasoning"),
        "stub must not leak the body:\n{file}"
    );
    let row = memory_row(exec.as_ref(), &stub_id).await;
    assert_eq!(row.store, Some(Store::Git));
    assert_eq!(row.policy_applied.as_deref(), Some("stub"));
    assert_eq!(
        row.content, "Secret reasoning that must not leak.",
        "DB row keeps the full body under stub policy"
    );
    assert_eq!(row.introduced_in_oid.as_deref(), Some(stub_head.as_str()));

    // Personal + db_only: Surreal-only, no file, unanchored without sources.
    let svc = svc_with_policy(svc, PERSONAL_CLAIMS_DB_ONLY);
    let head_before = repo.head().await;
    let db_only_id = svc
        .add_memory(
            &ActorContext::with_actor("alice"),
            &mut AddMemoryParams {
                project_name: "h03_matrix".to_string(),
                kind: claims::KIND_DECISION.to_string(),
                content: "Private db only decision".to_string(),
                visibility: claims::VISIBILITY_PERSONAL.to_string(),
                workspace_root: repo.s().to_string(),
                embedding: vec![0.3; 768],
                ..Default::default()
            },
        )
        .await
        .expect("add personal db_only claim");
    assert_eq!(repo.head().await, head_before, "db_only must not commit");
    let row = memory_row(exec.as_ref(), &db_only_id).await;
    assert_eq!(row.store, Some(Store::Surreal));
    assert_eq!(row.introduced_in_oid, None);

    // Personal + in_repo: full body committed.
    let svc = svc_with_policy(svc, PERSONAL_CLAIMS_IN_REPO);
    let head_before = repo.head().await;
    let in_repo_id = svc
        .add_memory(
            &ActorContext::with_actor("alice"),
            &mut AddMemoryParams {
                project_name: "h03_matrix".to_string(),
                kind: claims::KIND_RULE.to_string(),
                content: "Personal workflow rule.".to_string(),
                visibility: claims::VISIBILITY_PERSONAL.to_string(),
                workspace_root: repo.s().to_string(),
                embedding: vec![0.4; 768],
                ..Default::default()
            },
        )
        .await
        .expect("add personal in_repo claim");
    assert_ne!(repo.head().await, head_before, "in_repo commits the claim");
    let in_repo_key = in_repo_id
        .strip_prefix("memory:⟨")
        .unwrap()
        .strip_suffix("⟩")
        .unwrap();
    let file = std::fs::read_to_string(repo.root().join(format!(".ibis/claims/{in_repo_key}.md")))
        .unwrap();
    assert!(file.contains("Personal workflow rule."), "{file}");
    let row = memory_row(exec.as_ref(), &in_repo_id).await;
    assert_eq!(row.store, Some(Store::Git));
    assert_eq!(row.policy_applied.as_deref(), Some("in_repo"));

    drop_scratch(exec.as_ref(), &db_name).await;
}

fn svc_with_policy(mut svc: SearchService, policy: &str) -> SearchService {
    svc.personal_claims_policy = policy.to_string();
    svc
}

// ---------------------------------------------------------------------------
// supersession of git-owned claims ( TestAddMemorySupersedeGitOwnedDeletesOldFile
// + TestAddMemoryRefusesSupersedeGitOwnedWithNonEligibleKind)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_store_supersede_git_owned_claim() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let repo = init_repo("h03_supersede").await;
    repo.commit_file("src/base.txt", b"base\n", "base").await;
    let (svc, _committer) = service(&exec, false);

    let old_id = svc
        .add_memory(
            &ActorContext::LOCAL,
            &mut AddMemoryParams {
                project_name: "h03_supersede".to_string(),
                kind: claims::KIND_DECISION.to_string(),
                title: "Old decision".to_string(),
                content: "Old body".to_string(),
                visibility: claims::VISIBILITY_TEAM.to_string(),
                workspace_root: repo.s().to_string(),
                embedding: vec![0.1; 768],
                ..Default::default()
            },
        )
        .await
        .expect("add old claim");
    let old_key = old_id
        .strip_prefix("memory:⟨")
        .unwrap()
        .strip_suffix("⟩")
        .unwrap()
        .to_string();
    let old_path = format!(".ibis/claims/{old_key}.md");
    let head_before = repo.head().await;

    let new_id = svc
        .add_memory(
            &ActorContext::LOCAL,
            &mut AddMemoryParams {
                project_name: "h03_supersede".to_string(),
                kind: claims::KIND_DECISION.to_string(),
                title: "New decision".to_string(),
                content: "Replaces the old decision.".to_string(),
                visibility: claims::VISIBILITY_TEAM.to_string(),
                supersedes: old_id.clone(),
                workspace_root: repo.s().to_string(),
                embedding: vec![0.2; 768],
                ..Default::default()
            },
        )
        .await
        .expect("supersede git-owned claim");
    let new_head = repo.head().await;
    assert_ne!(new_head, head_before, "superseding claims commit");
    assert!(
        !repo.root().join(&old_path).exists(),
        "old claim file deleted in the same commit"
    );
    let message = repo.run(&["log", "--format=%s", "-1"]).await;
    assert!(
        message.contains(&format!("supersedes {old_key}")),
        "commit message: {message}"
    );
    // New row anchored to the new claims commit; old row superseded with X.
    let new_row = memory_row(exec.as_ref(), &new_id).await;
    assert_eq!(new_row.store, Some(Store::Git));
    assert_eq!(
        new_row.introduced_in_oid.as_deref(),
        Some(new_head.as_str())
    );
    let old_row = memory_row(exec.as_ref(), &old_id).await;
    assert_eq!(old_row.status, MemoryStatus::Superseded);
    assert_eq!(
        old_row.invalidated_at_oid.as_deref(),
        Some(new_head.as_str()),
        "X(old) = the superseding claims commit"
    );
    let prov = old_row.provenance.as_ref().expect("old provenance");
    assert!(prov.invalidated_at_commit.is_some());

    // A git-owned claim can only be superseded by another git-owned claim
    // (kind fact would resurrect the old file at ingest).
    let err = svc
        .add_memory(
            &ActorContext::LOCAL,
            &mut AddMemoryParams {
                project_name: "h03_supersede".to_string(),
                kind: "fact".to_string(),
                content: "Fact replaces a decision".to_string(),
                supersedes: old_id.clone(),
                embedding: vec![0.3; 768],
                ..Default::default()
            },
        )
        .await
        .expect_err("non-eligible supersede must refuse");
    assert!(err.to_string().contains("git-owned claim"), "{err}");
    assert_eq!(repo.head().await, new_head, "no commit on refusal");

    // Without a claim writer the supersede of a git-owned claim refuses
    // before any DB write ( TestAddMemoryRefusesSupersedeGitOwnedWithoutWriter).
    let mut bare = SearchService::new(Some(exec.clone()), None);
    bare.git_anchors = Some(Arc::new(GitAnchorRunner::new(GitRunner::anonymous())));
    let err = bare
        .add_memory(
            &ActorContext::LOCAL,
            &mut AddMemoryParams {
                project_name: "h03_supersede".to_string(),
                kind: claims::KIND_DECISION.to_string(),
                content: "No writer replacement".to_string(),
                supersedes: old_id.clone(),
                embedding: vec![0.4; 768],
                ..Default::default()
            },
        )
        .await
        .expect_err("supersede without claim writer must refuse");
    assert!(err.to_string().contains("claim writer"), "{err}");

    drop_scratch(exec.as_ref(), &db_name).await;
}

// ---------------------------------------------------------------------------
// ask at-ref: HOLDAt unions, unanchored exclusion, visibility defense, channel
// contracts ( + unions)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ask_ref_holdat_visibility_and_channels() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let repo = init_repo("h03_askref").await;
    let c1 = repo
        .commit_file("src/h03_cached.txt", b"bytes at ref one\n", "c1")
        .await;
    let c2 = repo
        .commit_file("src/h03_cached.txt", b"bytes at ref two\n", "c2")
        .await;
    let c3 = repo
        .commit_file("future/h03_new.txt", b"future bytes\n", "c3")
        .await;
    // Divergent line for m3.
    repo.run(&["checkout", "-q", "-b", "side", c1.as_str()])
        .await;
    let side = repo
        .commit_file("side/h03_div.txt", b"side\n", "side")
        .await;
    repo.run(&["checkout", "-q", "-"]).await;

    async fn seed_team(
        exec: &dyn Executor,
        key: &str,
        content: &str,
        introduced: &str,
        invalidated: Option<&str>,
    ) {
        let mut fields = vec![
            ("content", Value::from_t(content)),
            ("kind", Value::from_t(claims::KIND_DECISION)),
            ("title", Value::from_t(content)),
            ("visibility", Value::from_t(claims::VISIBILITY_TEAM)),
            ("store", Value::from_t("surreal")),
            ("embedding", emb(content)),
            ("introduced_in_oid", Value::from_t(introduced)),
        ];
        if let Some(x) = invalidated {
            fields.push(("invalidated_at_oid", Value::from_t(x)));
        }
        seed_memory(exec, key, "h03_ask", fields).await;
    }
    seed_team(
        exec.as_ref(),
        "h03_m1",
        "superseded claim that holds",
        &c1,
        None,
    )
    .await;
    exec.smart_query(
        "UPDATE memory:⟨h03_m1⟩ SET status = $status;",
        vars_of(vec![("status", Value::from_t("superseded"))]),
    )
    .await
    .expect("supersede m1");
    seed_team(
        exec.as_ref(),
        "h03_m2",
        "invalidated at the ref",
        &c1,
        Some(c2.as_str()),
    )
    .await;
    seed_team(exec.as_ref(), "h03_m3", "divergent line claim", &side, None).await;
    // Unanchored: the SQL prefilter excludes it before HOLDAt.
    seed_memory(
        exec.as_ref(),
        "h03_m4",
        "h03_ask",
        vec![
            ("content", Value::from_t("unanchored claim")),
            ("kind", Value::from_t("fact")),
            ("visibility", Value::from_t(claims::VISIBILITY_TEAM)),
            ("embedding", emb("unanchored claim")),
        ],
    )
    .await;
    seed_memory(
        exec.as_ref(),
        "h03_m5",
        "h03_ask",
        vec![
            ("content", Value::from_t("personal scratch claim")),
            ("kind", Value::from_t("working_state")),
            ("visibility", Value::from_t(claims::VISIBILITY_PERSONAL)),
            ("owner", Value::from_t("alice")),
            ("embedding", emb("personal scratch claim")),
            ("introduced_in_oid", Value::from_t(c1.as_str())),
        ],
    )
    .await;
    // Other project: must never surface under the h03_ask scope.
    seed_memory(
        exec.as_ref(),
        "h03_other",
        "h03_other",
        vec![
            ("content", Value::from_t("other project claim")),
            ("visibility", Value::from_t(claims::VISIBILITY_TEAM)),
            ("embedding", emb("other project claim")),
            ("introduced_in_oid", Value::from_t(c1.as_str())),
        ],
    )
    .await;

    // Δ commit_chunk channel: reachable + future commits.
    exec.smart_query(
        &format!(
            "CREATE commit_chunk:⟨h03_cc_reach⟩ SET content = $content, commit_hash = '{c2}', embedding = $embedding;"
        ),
        vars_of(vec![
            ("content", Value::from_t("reachable change")),
            ("embedding", emb("reachable change")),
        ]),
    )
    .await
    .expect("seed commit chunk");
    exec.smart_query(
        &format!(
            "CREATE commit_chunk:⟨h03_cc_future⟩ SET content = $content, commit_hash = '{c3}', embedding = $embedding;"
        ),
        vars_of(vec![
            ("content", Value::from_t("future change")),
            ("embedding", emb("future change")),
        ]),
    )
    .await
    .expect("seed future commit chunk");

    // Reasoning channel (HEAD-contract utility signal).
    exec.smart_query(
        "CREATE reasoning:⟨h03_r1⟩ SET question = $q, outcome = $o, embedding = $embedding;",
        vars_of(vec![
            ("q", Value::from_t("why did this change?")),
            ("o", Value::from_t("utility note")),
            ("embedding", emb("utility note")),
        ]),
    )
    .await
    .expect("seed reasoning");

    // Code channel fixtures: f1's path exists in tree(c2), f2's only at c3.
    exec.execute("CREATE source_file:⟨h03_sf1⟩ SET rel_path = 'src/h03_cached.txt', path = 'src/h03_cached.txt', hash = 'h1';")
        .await
        .expect("seed source file");
    exec.smart_query(
        "CREATE file_chunk:⟨h03_fc1⟩ SET file = source_file:⟨h03_sf1⟩, content = $content, hash = 'h1', embedding = $embedding;",
        vars_of(vec![
            ("content", Value::from_t("HEAD bytes")),
            ("embedding", emb("HEAD bytes")),
        ]),
    )
    .await
    .expect("seed file chunk");
    exec.execute("CREATE source_file:⟨h03_sf2⟩ SET rel_path = 'future/h03_new.txt', path = 'future/h03_new.txt', hash = 'h2';")
        .await
        .expect("seed source file 2");
    exec.smart_query(
        "CREATE file_chunk:⟨h03_fc2⟩ SET file = source_file:⟨h03_sf2⟩, content = $content, hash = 'h2', embedding = $embedding;",
        vars_of(vec![
            ("content", Value::from_t("HEAD bytes 2")),
            ("embedding", emb("HEAD bytes 2")),
        ]),
    )
    .await
    .expect("seed file chunk 2");

    let (svc, _committer) = service(&exec, true);

    // --- ask at c2, anonymous actor ----------------------------------------
    let results = svc
        .ask_project_at_ref(
            &ActorContext::LOCAL,
            "h03 why did this change",
            "h03_ask",
            repo.s(),
            &c2,
            "",
        )
        .await
        .expect("ask at ref c2");
    let ids = result_ids(&results);
    let has = |id: &str| ids.iter().any(|i| i == id);

    // HOLDAt wins over Oct status: m1 (superseded) holds at c2; m2 (current)
    // is invalidated at the ref and must be dropped.
    assert!(has("memory:h03_m1"), "superseded claim must hold: {ids:?}");
    assert!(
        !has("memory:h03_m2"),
        "claim invalidated at ref must drop: {ids:?}"
    );
    assert!(!has("memory:h03_m3"), "divergent claim must drop");
    assert!(!has("memory:h03_m4"), "unanchored claim must drop");
    assert!(!has("memory:h03_m5"), "personal claim leaked");
    assert!(!has("memory:h03_other"), "project scope leaked");
    assert!(
        has("commit_chunk:h03_cc_reach"),
        "reachable changelog hit kept"
    );
    assert!(
        !has("commit_chunk:h03_cc_future"),
        "unreachable changelog hit is noise"
    );
    assert!(has("reasoning:h03_r1"), "reasoning channel kept");

    for r in &results {
        assert_eq!(r.as_of_ref, c2, "every hit carries as_of_ref");
        if r.source_kind == "memory"
            || r.source_kind == "commit_chunk"
            || r.source_kind == "file_chunk"
        {
            assert!(!r.is_current, "historical hits are never current: {r:?}");
        }
        assert!(
            r.content_source.is_empty() || r.content_source == "git_show",
            "unexpected content source: {r:?}"
        );
    }
    let m1 = results
        .iter()
        .find(|r| r.id == "memory:h03_m1")
        .expect("m1 hit");
    assert_eq!(m1.status, "superseded", "HOLDAt wins over Oct status");
    // Code channel: f1 served with bytes at R via git_show (HEAD bytes must
    // never masquerade); f2's path is absent at R → dropped.
    let fc1 = results
        .iter()
        .find(|r| r.id == "file_chunk:h03_fc1")
        .expect("code hit for path in tree(R)");
    assert_eq!(fc1.content, "bytes at ref two\n");
    assert_eq!(fc1.content_source, "git_show");
    assert_eq!(fc1.path, "src/h03_cached.txt");

    // --- ask at c1: the invalidated-at-c2 claim holds there -----------------
    let results = svc
        .ask_project_at_ref(
            &ActorContext::LOCAL,
            "h03 why",
            "h03_ask",
            repo.s(),
            &c1,
            "",
        )
        .await
        .expect("ask at ref c1");
    let ids = result_ids(&results);
    assert!(ids.contains(&"memory:h03_m2".to_string()), "{ids:?}");
    let fc1 = results
        .iter()
        .find(|r| r.id == "file_chunk:h03_fc1")
        .expect("code hit at c1");
    assert_eq!(fc1.content, "bytes at ref one\n");

    // --- owner actor sees their personal claim at the ref -------------------
    let results = svc
        .ask_project_at_ref(
            &ActorContext::LOCAL,
            "h03 why",
            "h03_ask",
            repo.s(),
            &c2,
            "alice",
        )
        .await
        .expect("ask at ref as owner");
    assert!(result_ids(&results).contains(&"memory:h03_m5".to_string()));

    // --- fail-closed guards (no anchor source / empty repo path) ------------
    let mut bare = SearchService::new(Some(exec.clone()), Some(Arc::new(H03Ai)));
    let err = bare
        .ask_project_at_ref(&ActorContext::LOCAL, "q", "", repo.s(), &c2, "")
        .await
        .expect_err("ask at_ref without anchor source must fail closed");
    assert!(err.to_string().contains("anchor source"), "{err}");
    bare.git_anchors = Some(Arc::new(GitAnchorRunner::new(GitRunner::anonymous())));
    let err = bare
        .ask_project_at_ref(&ActorContext::LOCAL, "q", "", "  ", &c2, "")
        .await
        .expect_err("ask at_ref without repo path must fail closed");
    assert!(err.to_string().contains("repo path"), "{err}");

    drop_scratch(exec.as_ref(), &db_name).await;
}

// ---------------------------------------------------------------------------
// at_ref reply cap with the claim_file floor over a full ranked board (PR #34)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ask_ref_claim_file_floor_at_cap() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let repo = init_repo("h03_floor").await;
    repo.commit_file("src/base.txt", b"base\n", "base").await;
    let c1 = repo.head().await;
    let (svc, committer) = service(&exec, true);

    for i in 0..20 {
        seed_memory(
            exec.as_ref(),
            &format!("h03_fill_{i:02}"),
            "h03_floor",
            vec![
                ("content", Value::from_t(format!("filler claim {i:02}"))),
                ("kind", Value::from_t(claims::KIND_DECISION)),
                ("visibility", Value::from_t(claims::VISIBILITY_TEAM)),
                ("embedding", emb(&format!("filler claim {i:02}"))),
                ("introduced_in_oid", Value::from_t(c1.as_str())),
            ],
        )
        .await;
    }

    // Two tree claim files land after the fillers: the ranked board is full,
    // the claim_file floor must keep both in the capped reply.
    let writes = one_write(
        ".ibis/claims/h03_tree_a.md",
        claim_bytes(
            "h03_tree_a",
            claims::KIND_RULE,
            "Tree claim A",
            claims::VISIBILITY_TEAM,
            "Team tree claim A body.",
        ),
    );
    let writes_b = one_write(
        ".ibis/claims/h03_tree_b.md",
        claim_bytes(
            "h03_tree_b",
            claims::KIND_RULE,
            "Tree claim B",
            claims::VISIBILITY_TEAM,
            "Team tree claim B body.",
        ),
    );
    let mut both = writes;
    both.extend(writes_b);
    repo.commit_claim(&committer, "chore(claims): tree floor fixtures", &both, &[])
        .await;
    let c2 = repo.head().await;

    let results = svc
        .ask_project_at_ref(
            &ActorContext::LOCAL,
            "h03 filler claims",
            "h03_floor",
            repo.s(),
            &c2,
            "",
        )
        .await
        .expect("capped ask at ref");
    assert_eq!(results.len(), 15, "at_ref reply is capped");
    let tree: Vec<&ibis_search::SearchResult> = results
        .iter()
        .filter(|r| r.source_kind == "claim_file")
        .collect();
    assert_eq!(tree.len(), 2, "claim_file floor kept both tree claims");
    for t in &tree {
        assert_eq!(t.as_of_ref, c2);
        assert_eq!(t.score, 0.0, "tree claims are unranked");
    }
    assert!(tree
        .iter()
        .any(|t| t.content.contains("Team tree claim A body.")));
    assert!(tree
        .iter()
        .any(|t| t.content.contains("Team tree claim B body.")));

    drop_scratch(exec.as_ref(), &db_name).await;
}

// ---------------------------------------------------------------------------
// as-of claims: HOLDAt roundtrip + personal exclusion + tree union
//
// ---------------------------------------------------------------------------

#[tokio::test]
async fn as_of_claims_holdat_roundtrip_and_tree_union() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let repo = init_repo("h03_asof").await;
    let c1 = repo.commit_file("f.txt", b"1\n", "c1").await;
    let c2 = repo.commit_file("f.txt", b"2\n", "c2").await;
    repo.commit_file("f.txt", b"3\n", "c3").await;
    let (svc, committer) = service(&exec, true);

    seed_memory(
        exec.as_ref(),
        "h03_old",
        "h03_asof",
        vec![
            ("content", Value::from_t("old truth")),
            ("kind", Value::from_t(claims::KIND_DECISION)),
            ("title", Value::from_t("Old truth")),
            ("status", Value::from_t("superseded")),
            ("introduced_in_oid", Value::from_t(c1.as_str())),
            ("invalidated_at_oid", Value::from_t(c2.as_str())),
        ],
    )
    .await;
    seed_memory(
        exec.as_ref(),
        "h03_new",
        "h03_asof",
        vec![
            ("content", Value::from_t("new truth")),
            ("kind", Value::from_t(claims::KIND_DECISION)),
            ("title", Value::from_t("New truth")),
            ("introduced_in_oid", Value::from_t(c2.as_str())),
        ],
    )
    .await;
    seed_memory(
        exec.as_ref(),
        "h03_secret",
        "h03_asof",
        vec![
            ("content", Value::from_t("personal scratch")),
            ("kind", Value::from_t("working_state")),
            ("visibility", Value::from_t(claims::VISIBILITY_PERSONAL)),
            ("owner", Value::from_t("alice")),
            ("introduced_in_oid", Value::from_t(c1.as_str())),
        ],
    )
    .await;

    // as_of at c1 (ancestor set {c1}): old holds (X=c2 not an ancestor).
    let (claims, truncated, as_of_ref) = svc
        .as_of_claims("h03_asof", repo.s(), &c1, "")
        .await
        .expect("as_of at c1");
    assert!(!truncated);
    assert_eq!(as_of_ref, c1);
    assert_eq!(claims.len(), 1, "{claims:?}");
    assert_eq!(claims[0].id, "memory:h03_old");
    assert_eq!(claims[0].source_kind, "claim");
    assert_eq!(claims[0].introduced_in, c1);
    assert_eq!(claims[0].invalidated_at, c2);

    // as_of at c2: invalidated at the ref → gone; new truth holds.
    let (claims, _, as_of_ref) = svc
        .as_of_claims("h03_asof", repo.s(), &c2, "")
        .await
        .expect("as_of at c2");
    assert_eq!(as_of_ref, c2);
    let ids: Vec<&str> = claims.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["memory:h03_new"], "{ids:?}");

    // Personal claims never surface without their owner.
    let (claims, _, _) = svc
        .as_of_claims("h03_asof", repo.s(), &c2, "bob")
        .await
        .expect("as_of as non-owner");
    assert!(claims.iter().all(|c| c.id != "memory:h03_secret"));
    let (claims, _, _) = svc
        .as_of_claims("h03_asof", repo.s(), &c2, "alice")
        .await
        .expect("as_of as owner");
    assert!(claims.iter().any(|c| c.id == "memory:h03_secret"));

    // Tree union (claim store 4d): a claim file present at tree(R) joins the
    // HOLDAt result as a claim_file entry.
    let writes = one_write(
        ".ibis/claims/h03_asof_tree.md",
        claim_bytes(
            "h03_asof_tree",
            claims::KIND_RULE,
            "As of tree claim",
            claims::VISIBILITY_TEAM,
            "Tree claim body at c3.",
        ),
    );
    repo.commit_claim(&committer, "chore(claims): asof tree fixture", &writes, &[])
        .await;
    let c4 = repo.head().await;
    let (claims, _, as_of_ref) = svc
        .as_of_claims("h03_asof", repo.s(), &c4, "")
        .await
        .expect("as_of at c4 with tree claim");
    assert_eq!(as_of_ref, c4);
    let tree = claims.iter().find(|c| c.source_kind == "claim_file");
    let tree = tree.expect("tree claim unioned into as_of");
    assert_eq!(tree.id, "memory:⟨h03_asof_tree⟩");
    assert_eq!(tree.title, "As of tree claim");
    assert!(claims.iter().any(|c| c.id == "memory:h03_new"));

    drop_scratch(exec.as_ref(), &db_name).await;
}

// ---------------------------------------------------------------------------
// knowledge diff: ± HOLDAt symmetric difference, tree ± unions, direction
// swap, churn cap, since_divergence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn knowledge_diff_supersession_directions_cap_and_divergence() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let (svc, committer) = service(&exec, true);

    // --- main repo: supersession pair + late claim + personal + tree files ---
    let repo = init_repo("h03_kdiff").await;
    let writes = one_write(
        ".ibis/claims/h03_diffgone.md",
        claim_bytes(
            "h03_diffgone",
            claims::KIND_DECISION,
            "Gone claim",
            claims::VISIBILITY_TEAM,
            "Gone claim body.",
        ),
    );
    repo.commit_claim(&committer, "chore(claims): add h03_diffgone", &writes, &[])
        .await;
    let c1 = repo.head().await;
    let writes = one_write(
        ".ibis/claims/h03_diffnew.md",
        claim_bytes(
            "h03_diffnew",
            claims::KIND_DECISION,
            "New claim",
            claims::VISIBILITY_TEAM,
            "New claim body.",
        ),
    );
    let delete = vec![".ibis/claims/h03_diffgone.md".to_string()];
    repo.commit_claim(
        &committer,
        "chore(claims): swap h03_diffgone for h03_diffnew",
        &writes,
        &delete,
    )
    .await;
    let c2 = repo.head().await;
    repo.commit_file("other.txt", b"x\n", "c3").await;
    let c3 = repo.head().await;

    seed_memory(
        exec.as_ref(),
        "h03_old",
        "h03_kdiff",
        vec![
            ("content", Value::from_t("old truth")),
            ("kind", Value::from_t(claims::KIND_DECISION)),
            ("status", Value::from_t("superseded")),
            ("introduced_in_oid", Value::from_t(c1.as_str())),
            ("invalidated_at_oid", Value::from_t(c2.as_str())),
        ],
    )
    .await;
    seed_memory(
        exec.as_ref(),
        "h03_new",
        "h03_kdiff",
        vec![
            ("content", Value::from_t("new truth")),
            ("kind", Value::from_t(claims::KIND_DECISION)),
            ("introduced_in_oid", Value::from_t(c2.as_str())),
        ],
    )
    .await;
    seed_memory(
        exec.as_ref(),
        "h03_late",
        "h03_kdiff",
        vec![
            ("content", Value::from_t("late rule")),
            ("kind", Value::from_t(claims::KIND_RULE)),
            ("introduced_in_oid", Value::from_t(c3.as_str())),
        ],
    )
    .await;
    seed_memory(
        exec.as_ref(),
        "h03_secret",
        "h03_kdiff",
        vec![
            ("content", Value::from_t("scratch")),
            ("visibility", Value::from_t(claims::VISIBILITY_PERSONAL)),
            ("owner", Value::from_t("alice")),
            ("introduced_in_oid", Value::from_t(c1.as_str())),
        ],
    )
    .await;

    // diff(A, C): ± HOLDAt symmetric difference + tree ± unions — never
    // phantom content_changed entries.
    let diff = svc
        .knowledge_diff("h03_kdiff", repo.s(), &c1, &c3, "", DiffMode::Explicit, 0)
        .await
        .expect("diff c1 → c3");
    assert_eq!(diff.from_ref, c1);
    assert_eq!(diff.to_ref, c3);
    assert!(!diff.degraded);
    let added: Vec<&str> = diff.added_claims.iter().map(|c| c.id.as_str()).collect();
    let removed: Vec<&str> = diff.removed_claims.iter().map(|c| c.id.as_str()).collect();
    assert!(added.contains(&"memory:h03_new"), "{added:?}");
    assert!(added.contains(&"memory:h03_late"), "{added:?}");
    assert!(
        added.contains(&"memory:⟨h03_diffnew⟩"),
        "tree addition unioned: {added:?}"
    );
    assert!(removed.contains(&"memory:h03_old"), "{removed:?}");
    assert!(
        removed.contains(&"memory:⟨h03_diffgone⟩"),
        "tree removal unioned with X from the removing commit: {removed:?}"
    );
    assert!(!added.contains(&"memory:h03_old") && !removed.contains(&"memory:h03_new"));
    assert!(
        !added.contains(&"memory:h03_secret"),
        "personal claim leaked to anonymous actor"
    );
    let gone = diff
        .removed_claims
        .iter()
        .find(|c| c.id == "memory:⟨h03_diffgone⟩")
        .expect("gone claim entry");
    assert_eq!(gone.invalidated_at, c2, "X from the removing commit");
    assert_eq!(gone.status, "archived", "withdrawal convention");
    assert!(
        diff.changed_paths.contains(&"other.txt".to_string()),
        "git path diff: {:?}",
        diff.changed_paths
    );

    // Direction swap ( TestKnowledgeDiffReversesDirection).
    let backward = svc
        .knowledge_diff("h03_kdiff", repo.s(), &c3, &c1, "", DiffMode::Explicit, 0)
        .await
        .expect("diff c3 → c1");
    let back_added: Vec<&str> = backward
        .added_claims
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    let back_removed: Vec<&str> = backward
        .removed_claims
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(back_added.len(), removed.len());
    assert_eq!(back_removed.len(), added.len());
    for id in &added {
        assert!(back_removed.contains(id), "{id} must swap to removed");
    }
    for id in &removed {
        assert!(back_added.contains(id), "{id} must swap to added");
    }

    // Churn cap truncates deterministically and flags.
    let diff = svc
        .knowledge_diff("h03_kdiff", repo.s(), &c1, &c3, "", DiffMode::Explicit, 1)
        .await
        .expect("capped diff");
    assert!(diff.truncated, "cap must flag truncation");
    assert!(
        diff.added_claims.len() + diff.removed_claims.len() <= 1,
        "cap exceeded: {:?}",
        diff
    );

    // --- divergence repo: since_divergence diffs from the merge base ---------
    let div_repo = init_repo("h03_div").await;
    let base = div_repo.commit_file("base.txt", b"b\n", "base").await;
    let default_branch = div_repo.branch().await;
    div_repo.run(&["checkout", "-q", "-b", "side"]).await;
    let side = div_repo.commit_file("side.txt", b"s\n", "side").await;
    div_repo
        .run(&["checkout", "-q", default_branch.as_str()])
        .await;
    let m1 = div_repo.commit_file("main.txt", b"m1\n", "main c1").await;
    div_repo.commit_file("main.txt", b"m2\n", "main c2").await;
    let main_tip = div_repo.head().await;

    seed_memory(
        exec.as_ref(),
        "h03_aside",
        "h03_div",
        vec![
            ("content", Value::from_t("A-side truth")),
            ("kind", Value::from_t(claims::KIND_DECISION)),
            ("introduced_in_oid", Value::from_t(side.as_str())),
        ],
    )
    .await;
    seed_memory(
        exec.as_ref(),
        "h03_cside",
        "h03_div",
        vec![
            ("content", Value::from_t("C-side truth")),
            ("kind", Value::from_t(claims::KIND_RULE)),
            ("introduced_in_oid", Value::from_t(m1.as_str())),
        ],
    )
    .await;

    let since = svc
        .knowledge_diff(
            "h03_div",
            div_repo.s(),
            &side,
            &main_tip,
            "",
            DiffMode::SinceDivergence,
            0,
        )
        .await
        .expect("since_divergence diff");
    assert_eq!(since.from_ref, base, "from ref is the merge base");
    let since_added: Vec<&str> = since.added_claims.iter().map(|c| c.id.as_str()).collect();
    assert!(since_added.contains(&"memory:h03_cside"), "{since_added:?}");
    assert!(
        !since_added.contains(&"memory:h03_aside"),
        "A-side claim is not on the B-side delta: {since_added:?}"
    );
    assert!(
        since.removed_claims.is_empty(),
        "{:?}",
        since.removed_claims
    );

    let explicit = svc
        .knowledge_diff(
            "h03_div",
            div_repo.s(),
            &side,
            &main_tip,
            "",
            DiffMode::Explicit,
            0,
        )
        .await
        .expect("explicit diff across divergence");
    let ex_added: Vec<&str> = explicit
        .added_claims
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    let ex_removed: Vec<&str> = explicit
        .removed_claims
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(explicit.from_ref, side);
    assert!(ex_removed.contains(&"memory:h03_aside"), "{ex_removed:?}");
    assert!(ex_added.contains(&"memory:h03_cside"), "{ex_added:?}");

    // Fail-closed without the anchor source (no DB round-trips).
    let bare = SearchService::new(Some(exec.clone()), None);
    let err = bare
        .knowledge_diff("p", repo.s(), &c1, &c3, "", DiffMode::Explicit, 0)
        .await
        .expect_err("knowledge_diff without anchor source must fail closed");
    assert!(err.to_string().contains("anchor source"), "{err}");

    drop_scratch(exec.as_ref(), &db_name).await;
}

// ---------------------------------------------------------------------------
// hot-ref code byte cache over the real sidecar :
// hit serves without git, miss populates + warms, non-UTF-8 never cached,
// hostile keys bypass, kill-switch, empty file, TTL prune
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hot_cache_read_through_over_real_git() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let repo = init_repo("h03_hotcache").await;
    // All four fixtures land before the cached oid: every read below targets
    // the same immutable commit.
    let _ = repo
        .commit_file("src/h03_cached.txt", b"bytes from git\n", "c1")
        .await;
    let _ = repo.commit_file("h03_empty.txt", b"", "empty").await;
    let legacy: &[u8] = &[0xFF, 0xFE, b'l', b'e', b'g', b'a', b'c', b'y', 0x80];
    let _ = repo.commit_file("h03_legacy.txt", legacy, "legacy").await;
    let _ = repo
        .commit_file("h03_semi;colon.txt", b"hostile bytes\n", "hostile")
        .await;
    let c1 = repo.head().await;

    let (mut svc, _committer) = service(&exec, true);

    // Cache HIT: a pre-seeded row serves WITHOUT spawning git — the stored
    // bytes differ from what git would return ( counts fakeAnchors calls;
    // live the served marker content is the observable).
    let path = "src/h03_cached.txt";
    let record_id = format!("file_at_commit:⟨{c1}:{path}⟩");
    exec.smart_query(
        &format!("UPSERT {record_id} SET oid = $oid, path = $path, hash = 'marker', content = $content, created_at = time::now();"),
        vars_of(vec![
            ("oid", Value::from_t(c1.as_str())),
            ("path", Value::from_t(path)),
            ("content", Value::from_t("CACHED-MARKER")),
        ]),
    )
    .await
    .expect("seed cache row");
    let results = svc
        .as_of_code(path, repo.s(), &c1)
        .await
        .expect("as_of code exact path");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].content, "CACHED-MARKER",
        "hit must serve the cache"
    );
    assert_eq!(results[0].content_source, "git_show");

    // Cache MISS: fall through to git, populate the row best-effort.
    exec.execute(&format!("DELETE {record_id};"))
        .await
        .expect("drop cache row");
    let results = svc
        .as_of_code(path, repo.s(), &c1)
        .await
        .expect("as_of code after miss");
    assert_eq!(results[0].content, "bytes from git\n");
    assert_eq!(
        cache_row_content(exec.as_ref(), &c1, path).await.as_deref(),
        Some("bytes from git\n"),
        "miss must populate the cache"
    );
    // WARM: a poisoned row now wins again — the read served from the cache.
    exec.smart_query(
        &format!("UPDATE {record_id} SET content = $content;"),
        vars_of(vec![("content", Value::from_t("WARM-MARKER"))]),
    )
    .await
    .expect("poison cache row");
    let results = svc
        .as_of_code(path, repo.s(), &c1)
        .await
        .expect("as_of code warm");
    assert_eq!(results[0].content, "WARM-MARKER", "warm cache must serve");
    exec.execute(&format!("DELETE {record_id};"))
        .await
        .expect("cleanup cache row");

    // Empty file: a hit with empty content, not a miss.
    let results = svc
        .as_of_code("h03_empty.txt", repo.s(), &c1)
        .await
        .expect("empty file read");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "");
    assert_eq!(
        cache_row_content(exec.as_ref(), &c1, "h03_empty.txt")
            .await
            .as_deref(),
        Some(""),
        "empty file must enter the cache"
    );

    // Non-UTF-8 blob read: served lossy per D-A13, but NEVER cached —
    // GitRunner::mask_secrets is byte-exact (no lossy conversion), so the
    // hot-cache raw-bytes guard ( f40bf82) fires upstream: non-UTF-8 blobs
    // always read from git.
    let results = svc
        .as_of_code("h03_legacy.txt", repo.s(), &c1)
        .await
        .expect("legacy blob read");
    assert_eq!(results.len(), 1);
    assert!(
        results[0].content.contains(char::REPLACEMENT_CHARACTER),
        "lossy conversion expected: {:?}",
        results[0].content
    );
    let served = results[0].content.clone();
    assert_eq!(
        cache_row_content(exec.as_ref(), &c1, "h03_legacy.txt").await,
        None,
        "non-UTF-8 blobs must not enter the cache"
    );
    let again = svc
        .as_of_code("h03_legacy.txt", repo.s(), &c1)
        .await
        .expect("legacy blob re-read");
    assert_eq!(again[0].content, served, "byte-stable reads");

    // Hostile key (path with `;`): bypasses the cache, still reads git.
    let results = svc
        .as_of_code("h03_semi;colon.txt", repo.s(), &c1)
        .await
        .expect("hostile key read");
    assert_eq!(results[0].content, "hostile bytes\n");
    assert_eq!(
        cache_row_content(exec.as_ref(), &c1, "h03_semi;colon.txt").await,
        None,
        "hostile keys must bypass the cache"
    );

    // Kill-switch (world_state.code_cache_disabled): every read stays on git,
    // the cache table is never touched.
    svc.hot_ref_cache_disabled = true;
    let results = svc
        .as_of_code(path, repo.s(), &c1)
        .await
        .expect("kill-switch read");
    assert_eq!(results[0].content, "bytes from git\n");
    assert_eq!(
        cache_row_content(exec.as_ref(), &c1, path).await,
        None,
        "kill-switch must never populate the cache"
    );
    let pruned = svc.prune_file_at_commit_cache().await.expect("prune");
    assert_eq!(pruned, 0, "kill-switch must skip the prune");
    svc.hot_ref_cache_disabled = false;

    // TTL prune: rows older than 30d drop, fresh rows stay
    // ( TestPruneFileAtCommitCache).
    exec.smart_query(
        &format!(
            "UPSERT file_at_commit:⟨{c1}:prune/h03_old.txt⟩ SET oid = $oid, path = $path, hash = 'h', content = 'old', created_at = time::now() - 40d;"
        ),
        vars_of(vec![
            ("oid", Value::from_t(c1.as_str())),
            ("path", Value::from_t("prune/h03_old.txt")),
        ]),
    )
    .await
    .expect("seed stale cache row");
    exec.smart_query(
        &format!(
            "UPSERT file_at_commit:⟨{c1}:prune/h03_fresh.txt⟩ SET oid = $oid, path = $path, hash = 'h', content = 'fresh', created_at = time::now();"
        ),
        vars_of(vec![
            ("oid", Value::from_t(c1.as_str())),
            ("path", Value::from_t("prune/h03_fresh.txt")),
        ]),
    )
    .await
    .expect("seed fresh cache row");
    let pruned = svc.prune_file_at_commit_cache().await.expect("prune");
    assert_eq!(pruned, 1, "only the stale row prunes");
    assert_eq!(
        cache_row_content(exec.as_ref(), &c1, "prune/h03_old.txt").await,
        None
    );
    assert_eq!(
        cache_row_content(exec.as_ref(), &c1, "prune/h03_fresh.txt")
            .await
            .as_deref(),
        Some("fresh")
    );
    let pruned = svc.prune_file_at_commit_cache().await.expect("prune again");
    assert_eq!(pruned, 0, "prune is idempotent");

    // Degraded mode: no DB → no cache interactions, prune is a no-op.
    let bare = SearchService::new(None, None);
    let pruned = bare
        .prune_file_at_commit_cache()
        .await
        .expect("degraded prune");
    assert_eq!(pruned, 0);

    drop_scratch(exec.as_ref(), &db_name).await;
}

// ---------------------------------------------------------------------------
// ancestry TTL cache : one rev-list per query —
// not directly observable through the production GitRunner, so the live pins
// are result equivalence + cache entry state (put/get/invalidate/TTL expiry)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ancestry_ttl_cache_and_service_level_equivalence() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    init_ddl(exec.as_ref()).await;
    let repo = init_repo("h03_ancestry").await;
    let c1 = repo.commit_file("f.txt", b"1\n", "c1").await;
    let c2 = repo.commit_file("f.txt", b"2\n", "c2").await;
    let _ = repo.commit_file("f.txt", b"3\n", "c3").await;
    let c3 = repo.head().await;
    let anchors = GitAnchorRunner::new(GitRunner::anonymous());

    // Cache entry state over the real repo: first query builds, second is a
    // hit (same resolved oid → same content).
    let clock = Arc::new(AtomicU64::new(0));
    let base = Instant::now();
    let tick = clock.clone();
    let cache = AncestorCache::with_clock(Box::new(move || {
        base.checked_add(std::time::Duration::from_secs(tick.load(Ordering::SeqCst)))
            .unwrap_or(base)
    }));
    let set = ancestor_set_for(&anchors, &cache, repo.s(), &c3)
        .await
        .expect("build ancestor set");
    assert_eq!(set.size(), 3);
    assert!(set.contains(&c1) && set.contains(&c2));
    let again = ancestor_set_for(&anchors, &cache, repo.s(), &c3)
        .await
        .expect("cached ancestor set");
    assert_eq!(again.size(), 3);
    assert!(cache.get(repo.s(), &c3).is_some(), "cache entry present");

    // TTL expiry drops the entry on read (D-A06: expired entries evicted).
    clock.store(301, Ordering::SeqCst);
    assert!(
        cache.get(repo.s(), &c3).is_none(),
        "expired entry must be dropped on read"
    );
    let fresh = ancestor_set_for(&anchors, &cache, repo.s(), &c3)
        .await
        .expect("rebuild after expiry");
    assert_eq!(fresh.size(), 3);

    // Invalidation is repo-scoped.
    cache.put(repo.s(), &c2, AncestorSet::new(c2.clone(), [c2.clone()]));
    cache.invalidate(repo.s());
    assert!(cache.get(repo.s(), &c2).is_none());

    // Service-level: HOLDAt bounds derived from the cache — two as_of queries
    // at the same oid answer identically, invalidation recomputes honestly.
    seed_memory(
        exec.as_ref(),
        "h03_bound",
        "h03_ancestry",
        vec![
            ("content", Value::from_t("bound claim")),
            ("kind", Value::from_t(claims::KIND_DECISION)),
            ("introduced_in_oid", Value::from_t(c1.as_str())),
        ],
    )
    .await;
    let (svc, _committer) = service(&exec, true);
    let (first, truncated, as_of_ref) = svc
        .as_of_claims("h03_ancestry", repo.s(), &c3, "")
        .await
        .expect("first as_of");
    assert!(!truncated);
    assert_eq!(as_of_ref, c3);
    let (second, _, _) = svc
        .as_of_claims("h03_ancestry", repo.s(), &c3, "")
        .await
        .expect("cached as_of");
    assert_eq!(first, second, "cached query must answer identically");
    invalidate_ancestors(&svc, repo.s());
    let (third, _, _) = svc
        .as_of_claims("h03_ancestry", repo.s(), &c3, "")
        .await
        .expect("post-invalidation as_of");
    assert_eq!(third, first, "recomputed bounds must not drift");

    drop_scratch(exec.as_ref(), &db_name).await;
}
