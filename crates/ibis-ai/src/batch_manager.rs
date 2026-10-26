//! Background batch manager: every 10s it extracts
//! pending commit patches into line-aligned chunks and embeds all pending
//! file/commit chunks in single transactions.
//!
//! reached into `db.Executor` and raw `exec.Command` directly; here those
//! dependencies are narrow local traits ([`ChunkStore`], [`CommitPatchSource`],
//! [`CommitGitDirResolver`]) so other lanes wire the real implementations.
//!
//! F-009: commit→git-dir resolution must go through the commit's
//! repo-record linkage first (repo → contains → branch → pointed_to) and only
//! fall back to discovery-root basename probing — use
//! [`ChainedGitDirResolver`] with a repo-record resolver as primary and
//! [`DiscoveryRootProbe`] as fallback.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::AiError;
use crate::traits::AIProvider;
use crate::types::{TABLE_COMMIT_CHUNK, TABLE_FILE_CHUNK};

/// Chunk rows per sweep.
pub const BATCH_SIZE: usize = 100;
/// Sweep cadence.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_MAX_DELTA_SIZE: usize = 8000;

// --- ChunkStore ---

/// One pending chunk row. `id`/`content` are `None` when the adapter could not
/// coerce them (mirrors the skip counting for unusable rows).
#[derive(Debug, Clone, Default)]
pub struct PendingChunkRow {
    pub id: Option<String>,
    pub content: Option<String>,
}

/// One pending commit row (`SELECT id, hash, date, repo.name as repo_name,
/// message FROM commit WHERE batch_status = 'pending' LIMIT n;`). The adapter
/// coerces `date` to its RFC3339 string rendering.
#[derive(Debug, Clone, Default)]
pub struct PendingCommitRow {
    pub id: Option<String>,
    pub hash: String,
    pub repo_name: String,
    pub message: String,
    pub date: Option<String>,
}

/// One commit_chunk upsert + RELATE. `chunk_key` is the raw id key
/// `<hash>_chunk<i>`; the adapter formats the `commit_chunk` record id via its
/// own `FormatRecordID` (ibis-db owns record-ID formatting) and binds
/// `content`/`hash`/`date` as bound variables (F-002), never interpolated.
#[derive(Debug, Clone)]
pub struct CommitChunkWrite {
    /// Commit record id exactly as the DB returned it (RELATE source).
    pub commit_record: String,
    /// `<hash>_chunk<i>` key segment.
    pub chunk_key: String,
    pub content: String,
    pub hash: String,
    /// `Some(rfc3339)` → `commit_date` datetime; `None` → `NONE`.
    pub date: Option<String>,
}

/// The result of one commit sweep, applied in ONE transaction ( assembles a
/// `BEGIN TRANSACTION … COMMIT` batch).
#[derive(Debug, Default)]
pub struct CommitTxn {
    pub chunk_writes: Vec<CommitChunkWrite>,
    /// Commit record ids flipped to `batch_status = 'completed'`.
    pub completed_commits: Vec<String>,
    /// (record id, error code) flipped to `batch_status = 'failed'` +
    /// `batch_error = <code>`.
    pub failed_commits: Vec<(String, String)>,
}

#[async_trait]
pub trait ChunkStore: Send + Sync {
    /// Mirrors `SELECT id, content FROM <table> WHERE batch_status =
    /// 'pending' LIMIT <limit>;` — `table` is `file_chunk` or `commit_chunk`.
    async fn pending_chunks(
        &self,
        table: &str,
        limit: usize,
    ) -> Result<Vec<PendingChunkRow>, AiError>;

    /// Mirrors `SELECT id, hash, date, repo.name as repo_name, message
    /// FROM commit WHERE batch_status = 'pending' LIMIT <limit>;`
    async fn pending_commits(&self, limit: usize) -> Result<Vec<PendingCommitRow>, AiError>;

    /// One transaction: `UPDATE <chunk> SET embedding = <vector>,
    /// batch_status = 'completed';` for every (record id, embedding) pair.
    async fn save_chunk_embeddings(&self, updates: &[(String, Vec<f32>)]) -> Result<(), AiError>;

    /// One transaction applying the commit sweep: chunk upserts + RELATE
    /// `<commit>->has_commit_chunk-><chunk>` + status flips.
    async fn apply_commit_txn(&self, txn: &CommitTxn) -> Result<(), AiError>;
}

// --- CommitPatchSource ---

#[async_trait]
pub trait CommitPatchSource: Send + Sync {
    /// `git diff <hash>^ <hash>` falling back to
    /// `git show --format= --patch <hash>`.
    /// Production wiring should inject an ibis-git `GitRunner`-backed
    /// implementation (D-A07); [`GitPatchSource`] is the raw-exec default.
    async fn commit_patch(&self, repo_path: &Path, hash: &str) -> Result<String, AiError>;
}

/// Default patch extractor using direct `git` invocation, byte-for-byte the
/// Scenario: behavior (stdout captured, stderr only in the combined error).
pub struct GitPatchSource;

#[async_trait]
impl CommitPatchSource for GitPatchSource {
    async fn commit_patch(&self, repo_path: &Path, hash: &str) -> Result<String, AiError> {
        let diff_args = ["diff".to_string(), format!("{hash}^"), hash.to_string()];
        let diff = run_git(repo_path, &diff_args).await;
        match diff {
            Ok(out) => Ok(String::from_utf8_lossy(&out).into_owned()),
            Err(diff_err) => {
                let show_args = [
                    "show".to_string(),
                    "--format=".to_string(),
                    "--patch".to_string(),
                    hash.to_string(),
                ];
                let show = run_git(repo_path, &show_args).await;
                match show {
                    Ok(out) => Ok(String::from_utf8_lossy(&out).into_owned()),
                    Err(show_err) => Err(AiError::other(format!(
                        "git diff/show failed in {} for {}: {} / {}",
                        repo_path.display(),
                        hash,
                        diff_err,
                        show_err
                    ))),
                }
            }
        }
    }
}

async fn run_git(dir: &Path, args: &[String]) -> Result<Vec<u8>, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(dir);
    for a in args {
        cmd.arg(a);
    }
    let output = cmd.output().await.map_err(|e| e.to_string())?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(format!(
            "exit status {}: {}",
            code,
            String::from_utf8_lossy(output.stderr.trim_ascii())
        ));
    }
    Ok(output.stdout)
}

// --- CommitGitDirResolver ---

#[async_trait]
pub trait CommitGitDirResolver: Send + Sync {
    /// Resolves the absolute git working tree for a repo/project name.
    /// Implementations must return only verified working trees
    /// (a `.git` file or directory must exist).
    async fn resolve_git_dir(&self, repo_name: &str) -> Option<PathBuf>;
}

/// Chains resolvers: the primary (repo-record linkage, F-009) answer is
/// accepted only when it is an actual git dir; otherwise the fallback
/// (discovery-root probe) decides. Mirrors the `ok && isGitDir(path)`
/// validation before the discovery-root probe.
pub struct ChainedGitDirResolver {
    primary: Arc<dyn CommitGitDirResolver>,
    fallback: Arc<dyn CommitGitDirResolver>,
}

impl ChainedGitDirResolver {
    pub fn new(
        primary: Arc<dyn CommitGitDirResolver>,
        fallback: Arc<dyn CommitGitDirResolver>,
    ) -> Self {
        ChainedGitDirResolver { primary, fallback }
    }
}

#[async_trait]
impl CommitGitDirResolver for ChainedGitDirResolver {
    async fn resolve_git_dir(&self, repo_name: &str) -> Option<PathBuf> {
        if let Some(path) = self.primary.resolve_git_dir(repo_name).await {
            if is_git_dir(&path) {
                return Some(path);
            }
        }
        self.fallback.resolve_git_dir(repo_name).await
    }
}

/// Last-resort resolver: probes the discovery root and
/// `discovery_root/dynamic/<name>`.
pub struct DiscoveryRootProbe {
    pub discovery_root: String,
}

impl DiscoveryRootProbe {
    pub fn new(discovery_root: impl Into<String>) -> Self {
        DiscoveryRootProbe {
            discovery_root: discovery_root.into(),
        }
    }
}

#[async_trait]
impl CommitGitDirResolver for DiscoveryRootProbe {
    async fn resolve_git_dir(&self, repo_name: &str) -> Option<PathBuf> {
        resolve_commit_git_dir(&self.discovery_root, repo_name)
    }
}

/// Scenario: resolveCommitGitDir : personal/
/// plugin modes often set the discovery root to the live working tree itself,
/// so basename matches are probed too.
pub fn resolve_commit_git_dir(root: &str, repo_name: &str) -> Option<PathBuf> {
    let root = root.trim();
    let name = repo_name.trim();
    if root.is_empty() || name.is_empty() {
        return None;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if name == "." || name == std::path::MAIN_SEPARATOR.to_string() {
        candidates.push(PathBuf::from(root));
    } else {
        candidates.push(path_clean(&Path::new(root).join(name)));
        candidates.push(path_clean(&Path::new(root).join("dynamic").join(name)));
        candidates.push(path_clean(Path::new(root)));
        let root_base = Path::new(root).file_name();
        if root_base.is_some_and(|b| b == name)
            || root_base.is_some_and(|b| Path::new(name).file_name().is_some_and(|nb| nb == b))
        {
            candidates.insert(0, path_clean(Path::new(root)));
        }
    }

    let mut seen = std::collections::HashSet::new();
    candidates
        .into_iter()
        .find(|cand| seen.insert(cand.clone()) && is_git_dir(cand))
}

/// Approximates `filepath.Clean: collapses duplicate separators and `.`.
fn path_clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// A `.git` entry (directory or worktree file) must exist.
pub fn is_git_dir(dir: &Path) -> bool {
    std::fs::metadata(dir.join(".git"))
        .map(|m| m.is_dir() || m.is_file())
        .unwrap_or(false)
}

// --- BatchManager ---

pub struct BatchManager {
    store: Option<Arc<dyn ChunkStore>>,
    ai: Option<Arc<dyn AIProvider>>,
    patch_source: Arc<dyn CommitPatchSource>,
    resolver: Arc<dyn CommitGitDirResolver>,
    max_delta_size: usize,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    tasks: std::sync::Mutex<Option<tokio::task::JoinSet<()>>>,
}

impl BatchManager {
    pub fn new(
        store: Option<Arc<dyn ChunkStore>>,
        ai: Option<Arc<dyn AIProvider>>,
        patch_source: Arc<dyn CommitPatchSource>,
        resolver: Arc<dyn CommitGitDirResolver>,
        max_delta_size: usize,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        BatchManager {
            store,
            ai,
            patch_source,
            resolver,
            max_delta_size,
            shutdown_tx,
            shutdown_rx,
            tasks: std::sync::Mutex::new(None),
        }
    }

    fn take_tasks(&self) -> Option<tokio::task::JoinSet<()>> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Spawns the 10s loop (first sweep after 10s, like the ticker).
    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let mut set = tokio::task::JoinSet::new();
        set.spawn(async move { this.run_loop().await });
        *self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(set);
    }

    /// Signals shutdown and waits for the in-flight sweep to finish.
    pub async fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(mut set) = self.take_tasks() {
            while let Some(res) = set.join_next().await {
                let _ = res;
            }
        }
    }

    async fn run_loop(self: Arc<Self>) {
        tracing::info!("BatchManager: Started background loop");
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // tokio intervals fire immediately; discard that first tick so the
        // first sweep happens at POLL_INTERVAL ( time.NewTicker semantics).
        ticker.tick().await;
        let mut shutdown = self.shutdown_rx.clone();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    tracing::info!("BatchManager: Stopping...");
                    return;
                }
                _ = ticker.tick() => {
                    if self.store.is_some() {
                        self.process_pending_commits().await;
                        self.process_pending_chunks().await;
                    }
                }
            }
        }
    }

    /// Pending file_chunk+commit_chunk rows → embed → one transaction.
    pub async fn process_pending_chunks(&self) {
        let (Some(store), Some(ai)) = (&self.store, &self.ai) else {
            return;
        };
        if !ai.is_functional().await {
            return;
        }

        let mut rows: Vec<PendingChunkRow> = Vec::new();
        for table in [TABLE_FILE_CHUNK, TABLE_COMMIT_CHUNK] {
            match store.pending_chunks(table, BATCH_SIZE).await {
                Ok(mut found) => rows.append(&mut found),
                Err(e) => tracing::error!(
                    "BatchManager: Failed to fetch pending chunks from {table}: {e}"
                ),
            }
            if rows.len() >= BATCH_SIZE {
                rows.truncate(BATCH_SIZE);
                break;
            }
        }
        if rows.is_empty() {
            return;
        }

        let mut chunk_ids: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        for row in rows {
            let id = row.id.filter(|s| !s.is_empty());
            let content = row.content.filter(|s| !s.is_empty());
            match (id, content) {
                (Some(id), Some(content)) => {
                    chunk_ids.push(id);
                    texts.push(content);
                }
                _ => skipped += 1,
            }
        }

        if chunk_ids.is_empty() {
            tracing::warn!(
                "BatchManager: {} pending rows but 0 usable chunk IDs (skipped={skipped})",
                chunk_ids.len() + skipped
            );
            return;
        }

        tracing::info!(
            "BatchManager: Found {} chunks. Embedding synchronously...",
            chunk_ids.len()
        );
        let embeddings = match ai.batch_embed_text(&texts).await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("BatchManager: Failed to embed batch: {e}");
                return;
            }
        };
        if embeddings.len() != chunk_ids.len() {
            tracing::error!(
                "BatchManager: provider returned {} embeddings for {} chunks; leaving pending",
                embeddings.len(),
                chunk_ids.len()
            );
            return;
        }

        let updates: Vec<(String, Vec<f32>)> = chunk_ids.into_iter().zip(embeddings).collect();
        match store.save_chunk_embeddings(&updates).await {
            Ok(()) => tracing::info!(
                "BatchManager: Successfully saved {} embeddings",
                updates.len()
            ),
            Err(e) => tracing::error!("BatchManager: Failed to save batch results: {e}"),
        }
    }

    /// Pending commits → git patch → 8000-char line-aligned chunks →
    /// commit_chunk rows, in one transaction.
    pub async fn process_pending_commits(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let rows = match store.pending_commits(BATCH_SIZE).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("BatchManager: Failed to fetch pending commits: {e}");
                return;
            }
        };
        if rows.is_empty() {
            return;
        }

        let mut txn = CommitTxn::default();
        let mut processed = 0usize;

        for row in rows {
            let Some(id) = row.id.filter(|s| !s.is_empty()) else {
                continue;
            };
            if row.hash.is_empty() || row.repo_name.is_empty() {
                continue;
            }

            let Some(repo_path) = self.resolver.resolve_git_dir(&row.repo_name).await else {
                tracing::error!(
                    "BatchManager: cannot resolve git dir for repo {:?}; leaving commit {} pending",
                    row.repo_name,
                    row.hash
                );
                txn.failed_commits
                    .push((id, "unresolved_git_dir".to_string()));
                continue;
            };

            let patch = match self.patch_source.commit_patch(&repo_path, &row.hash).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(
                        "BatchManager: patch extract failed for {} in {}: {}; leaving pending",
                        row.hash,
                        repo_path.display(),
                        e
                    );
                    continue;
                }
            };

            if patch.is_empty() && row.message.is_empty() {
                // Empty root/merge with no message: nothing to embed.
                txn.completed_commits.push(id);
                processed += 1;
                continue;
            }

            let max_delta = if self.max_delta_size == 0 {
                DEFAULT_MAX_DELTA_SIZE
            } else {
                self.max_delta_size
            };

            let full_text = format!("COMMIT MESSAGE:\n{}\n\nPATCH:\n{}", row.message, patch);
            let chunks = chunk_string(&full_text, max_delta);
            if chunks.is_empty() {
                tracing::error!(
                    "BatchManager: chunking produced zero chunks for {}; leaving pending",
                    row.hash
                );
                continue;
            }

            let date = row
                .date
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            for (i, chunk_text) in chunks.iter().enumerate() {
                txn.chunk_writes.push(CommitChunkWrite {
                    commit_record: id.clone(),
                    chunk_key: format!("{}_chunk{}", row.hash, i),
                    content: chunk_text.clone(),
                    hash: row.hash.clone(),
                    date: date.clone(),
                });
            }
            txn.completed_commits.push(id);
            processed += 1;
        }

        let has_updates = !txn.chunk_writes.is_empty()
            || !txn.completed_commits.is_empty()
            || !txn.failed_commits.is_empty();
        if has_updates {
            match store.apply_commit_txn(&txn).await {
                Ok(()) => {
                    tracing::info!(
                        "BatchManager: Processed {processed} pending commits into chunks."
                    );
                }
                Err(e) => {
                    tracing::error!("BatchManager: Failed to save commit chunks: {e}");
                }
            }
        }
    }
}

/// Line-aligned greedy chunking: never splits inside a
/// line, so a single line longer than `max_len` becomes its own oversized
/// chunk. Lengths are bytes, matching
pub(crate) fn chunk_string(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split('\n') {
        if current.len() + line.len() + 1 > max_len && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current = line.to_string();
        } else if !current.is_empty() {
            current.push('\n');
            current.push_str(line);
        } else {
            current = line.to_string();
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EDGE_HAS_COMMIT_CHUNK, TABLE_COMMIT};
    use std::sync::Mutex;

    // ---------- fakes ----------

    #[derive(Default)]
    struct FakeStoreInner {
        chunk_queries: Vec<(String, usize)>,
        chunk_calls: usize,
        commit_queries: Vec<usize>,
        saved: Vec<(String, Vec<f32>)>,
        txns: Vec<CommitTxn>,
    }

    struct FakeStore {
        chunk_rows: Vec<PendingChunkRow>,
        commit_rows: Vec<PendingCommitRow>,
        fail_queries: bool,
        inner: Mutex<FakeStoreInner>,
    }

    impl FakeStore {
        fn new(chunk_rows: Vec<PendingChunkRow>, commit_rows: Vec<PendingCommitRow>) -> Self {
            FakeStore {
                chunk_rows,
                commit_rows,
                fail_queries: false,
                inner: Mutex::new(FakeStoreInner::default()),
            }
        }
        fn failing() -> Self {
            FakeStore {
                chunk_rows: Vec::new(),
                commit_rows: Vec::new(),
                fail_queries: true,
                inner: Mutex::new(FakeStoreInner::default()),
            }
        }
    }

    #[async_trait]
    impl ChunkStore for FakeStore {
        async fn pending_chunks(
            &self,
            table: &str,
            limit: usize,
        ) -> Result<Vec<PendingChunkRow>, AiError> {
            let mut inner = self.inner.lock().unwrap();
            inner.chunk_queries.push((table.to_string(), limit));
            if self.fail_queries {
                return Err(AiError::other("db down"));
            }
            // The fake models `file_chunk` as the table holding the seeded
            // rows; `commit_chunk` comes back empty (matches the sweep's
            // LIMIT-100 union).
            let call = inner.chunk_calls;
            inner.chunk_calls += 1;
            if call == 0 {
                Ok(self.chunk_rows.clone())
            } else {
                Ok(Vec::new())
            }
        }
        async fn pending_commits(&self, limit: usize) -> Result<Vec<PendingCommitRow>, AiError> {
            self.inner.lock().unwrap().commit_queries.push(limit);
            if self.fail_queries {
                return Err(AiError::other("db down"));
            }
            Ok(self.commit_rows.clone())
        }
        async fn save_chunk_embeddings(
            &self,
            updates: &[(String, Vec<f32>)],
        ) -> Result<(), AiError> {
            self.inner.lock().unwrap().saved = updates.to_vec();
            Ok(())
        }
        async fn apply_commit_txn(&self, txn: &CommitTxn) -> Result<(), AiError> {
            self.inner.lock().unwrap().txns.push(CommitTxn {
                chunk_writes: txn.chunk_writes.clone(),
                completed_commits: txn.completed_commits.clone(),
                failed_commits: txn.failed_commits.clone(),
            });
            Ok(())
        }
    }

    struct FakeEmbedder;

    #[async_trait]
    impl AIProvider for FakeEmbedder {
        fn name(&self) -> &str {
            "mock"
        }
        async fn is_functional(&self) -> bool {
            true
        }
        async fn batch_embed_text(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
            Ok(texts.iter().map(|_| vec![0.25f32; 768]).collect())
        }
    }

    struct FixedResolver(Option<PathBuf>);

    #[async_trait]
    impl CommitGitDirResolver for FixedResolver {
        async fn resolve_git_dir(&self, _repo_name: &str) -> Option<PathBuf> {
            self.0.clone()
        }
    }

    struct FixedPatcher(Result<String, String>);

    #[async_trait]
    impl CommitPatchSource for FixedPatcher {
        async fn commit_patch(&self, _repo_path: &Path, _hash: &str) -> Result<String, AiError> {
            match &self.0 {
                Ok(patch) => Ok(patch.clone()),
                Err(msg) => Err(AiError::other(msg.clone())),
            }
        }
    }

    fn git_dir_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("somerepo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        (dir, root)
    }

    // ---------- pending chunks ( ports) ----------

    #[tokio::test]
    async fn pending_chunks_parse_and_embed() {
        let store = Arc::new(FakeStore::new(
            vec![
                PendingChunkRow {
                    id: Some("file_chunk:1".into()),
                    content: Some("text1".into()),
                },
                PendingChunkRow {
                    id: Some("file_chunk:2".into()),
                    content: Some("text2".into()),
                },
            ],
            Vec::new(),
        ));
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            Some(Arc::new(FakeEmbedder) as Arc<dyn AIProvider>),
            Arc::new(GitPatchSource),
            Arc::new(DiscoveryRootProbe::new(".")),
            8000,
        );
        bm.process_pending_chunks().await;

        let inner = store.inner.lock().unwrap();
        assert_eq!(
            inner.chunk_queries[0],
            (TABLE_FILE_CHUNK.to_string(), BATCH_SIZE)
        );
        assert_eq!(
            inner.chunk_queries[1],
            (TABLE_COMMIT_CHUNK.to_string(), BATCH_SIZE)
        );
        assert_eq!(inner.saved.len(), 2);
        assert_eq!(inner.saved[0].0, "file_chunk:1");
        assert_eq!(inner.saved[0].1.len(), 768);
    }

    #[tokio::test]
    async fn unusable_rows_are_skipped() {
        let store = Arc::new(FakeStore::new(
            vec![
                PendingChunkRow {
                    id: Some("file_chunk:ok".into()),
                    content: Some("text".into()),
                },
                PendingChunkRow {
                    id: None,
                    content: Some("orphan".into()),
                },
                PendingChunkRow {
                    id: Some("file_chunk:nocontent".into()),
                    content: None,
                },
                PendingChunkRow {
                    id: Some(String::new()),
                    content: Some("empty-id".into()),
                },
            ],
            Vec::new(),
        ));
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            Some(Arc::new(FakeEmbedder) as Arc<dyn AIProvider>),
            Arc::new(GitPatchSource),
            Arc::new(DiscoveryRootProbe::new(".")),
            8000,
        );
        bm.process_pending_chunks().await;

        let inner = store.inner.lock().unwrap();
        assert_eq!(inner.saved.len(), 1);
        assert_eq!(inner.saved[0].0, "file_chunk:ok");
    }

    #[tokio::test]
    async fn content_passes_through_unmangled() {
        let store = Arc::new(FakeStore::new(
            vec![PendingChunkRow {
                id: Some("file_chunk:with'quote".into()),
                content: Some("it's \"quoted\"; DELETE".into()),
            }],
            Vec::new(),
        ));
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            Some(Arc::new(FakeEmbedder) as Arc<dyn AIProvider>),
            Arc::new(GitPatchSource),
            Arc::new(DiscoveryRootProbe::new(".")),
            8000,
        );
        bm.process_pending_chunks().await;
        let inner = store.inner.lock().unwrap();
        assert_eq!(inner.saved[0].1.len(), 768);
    }

    #[tokio::test]
    async fn query_failure_leaves_pending() {
        let store = Arc::new(FakeStore::failing());
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            Some(Arc::new(FakeEmbedder) as Arc<dyn AIProvider>),
            Arc::new(GitPatchSource),
            Arc::new(DiscoveryRootProbe::new(".")),
            8000,
        );
        bm.process_pending_chunks().await;
        let inner = store.inner.lock().unwrap();
        assert!(inner.saved.is_empty());
    }

    // ---------- commit sweeps ----------

    #[tokio::test]
    async fn marks_failed_when_git_dir_unresolved() {
        let store = Arc::new(FakeStore::new(
            Vec::new(),
            vec![PendingCommitRow {
                id: Some("commit:abc".into()),
                hash: "abc1234".into(),
                repo_name: "nope".into(),
                message: "msg".into(),
                date: Some("2026-05-07T00:00:00Z".into()),
            }],
        ));
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            None,
            Arc::new(GitPatchSource),
            Arc::new(FixedResolver(None)),
            8000,
        );
        bm.process_pending_commits().await;

        let inner = store.inner.lock().unwrap();
        assert_eq!(inner.txns.len(), 1);
        assert_eq!(
            inner.txns[0].failed_commits,
            vec![("commit:abc".to_string(), "unresolved_git_dir".to_string())]
        );
        assert!(inner.txns[0].completed_commits.is_empty());
        assert!(inner.txns[0].chunk_writes.is_empty());
    }

    #[tokio::test]
    async fn empty_patch_and_message_marks_completed() {
        let (_dir, repo) = git_dir_repo();
        let store = Arc::new(FakeStore::new(
            Vec::new(),
            vec![PendingCommitRow {
                id: Some("commit:root".into()),
                hash: "deadbeef".into(),
                repo_name: "r".into(),
                message: String::new(),
                date: None,
            }],
        ));
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            None,
            Arc::new(FixedPatcher(Ok(String::new()))),
            Arc::new(FixedResolver(Some(repo))),
            8000,
        );
        bm.process_pending_commits().await;

        let inner = store.inner.lock().unwrap();
        assert_eq!(inner.txns[0].completed_commits, vec!["commit:root"]);
        assert!(inner.txns[0].chunk_writes.is_empty());
    }

    #[tokio::test]
    async fn patch_failure_leaves_commit_pending() {
        let (_dir, repo) = git_dir_repo();
        let store = Arc::new(FakeStore::new(
            Vec::new(),
            vec![PendingCommitRow {
                id: Some("commit:x".into()),
                hash: "h".into(),
                repo_name: "r".into(),
                message: "m".into(),
                date: None,
            }],
        ));
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            None,
            Arc::new(FixedPatcher(Err("git exploded".to_string()))),
            Arc::new(FixedResolver(Some(repo))),
            8000,
        );
        bm.process_pending_commits().await;
        let inner = store.inner.lock().unwrap();
        assert!(inner.txns.is_empty());
    }

    #[tokio::test]
    async fn chunks_written_with_relate_and_completion() {
        let (_dir, repo) = git_dir_repo();
        let store = Arc::new(FakeStore::new(
            Vec::new(),
            vec![PendingCommitRow {
                id: Some("commit:⟨abc⟩".into()),
                hash: "abc1234".into(),
                repo_name: "r".into(),
                message: "message line".into(),
                date: Some("2026-05-07T00:00:00Z".into()),
            }],
        ));
        let patch = "diff --git a/f b/f\n+line\n".repeat(900);
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            None,
            Arc::new(FixedPatcher(Ok(patch))),
            Arc::new(FixedResolver(Some(repo))),
            8000,
        );
        bm.process_pending_commits().await;

        let inner = store.inner.lock().unwrap();
        assert_eq!(inner.txns.len(), 1);
        let txn = &inner.txns[0];
        assert!(txn.chunk_writes.len() > 1);
        assert_eq!(txn.chunk_writes[0].chunk_key, "abc1234_chunk0");
        assert_eq!(txn.chunk_writes[0].commit_record, "commit:⟨abc⟩");
        assert_eq!(txn.chunk_writes[0].hash, "abc1234");
        assert_eq!(
            txn.chunk_writes[0].date.as_deref(),
            Some("2026-05-07T00:00:00Z")
        );
        for (i, w) in txn.chunk_writes.iter().enumerate() {
            assert_eq!(w.chunk_key, format!("abc1234_chunk{i}"));
            assert!(w.content.len() <= 8000);
        }
        assert_eq!(txn.completed_commits, vec!["commit:⟨abc⟩"]);
    }

    #[tokio::test]
    async fn rows_missing_fields_are_skipped_silently() {
        let store = Arc::new(FakeStore::new(
            Vec::new(),
            vec![
                PendingCommitRow {
                    id: None,
                    hash: "h".into(),
                    repo_name: "r".into(),
                    message: String::new(),
                    date: None,
                },
                PendingCommitRow {
                    id: Some("commit:y".into()),
                    hash: String::new(),
                    repo_name: "r".into(),
                    message: String::new(),
                    date: None,
                },
                PendingCommitRow {
                    id: Some("commit:z".into()),
                    hash: "h".into(),
                    repo_name: String::new(),
                    message: String::new(),
                    date: None,
                },
            ],
        ));
        let bm = BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            None,
            Arc::new(GitPatchSource),
            Arc::new(FixedResolver(Some(PathBuf::from(".")))),
            8000,
        );
        bm.process_pending_commits().await;
        let inner = store.inner.lock().unwrap();
        assert!(inner.txns.is_empty());
    }

    // ---------- resolver probe (resolveCommitGitDir ports) ----------

    fn make_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn probe_uses_discovery_root_when_already_repo() {
        let (_dir, root) = make_root();
        std::fs::create_dir(root.join(".git")).unwrap();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let got = resolve_commit_git_dir(&root.to_string_lossy(), &name);
        assert_eq!(got.as_ref().map(|p| path_clean(p)), Some(path_clean(&root)));
    }

    #[test]
    fn probe_rejects_missing_git() {
        let (_dir, root) = make_root();
        assert!(resolve_commit_git_dir(&root.to_string_lossy(), "missing").is_none());
    }

    #[test]
    fn probe_joins_root_and_dynamic() {
        let (_dir, root) = make_root();
        std::fs::create_dir_all(root.join("myrepo").join(".git")).unwrap();
        std::fs::create_dir_all(root.join("dynamic").join("dynrepo").join(".git")).unwrap();
        assert_eq!(
            resolve_commit_git_dir(&root.to_string_lossy(), "myrepo"),
            Some(path_clean(&root.join("myrepo")))
        );
        assert_eq!(
            resolve_commit_git_dir(&root.to_string_lossy(), "dynrepo"),
            Some(path_clean(&root.join("dynamic").join("dynrepo")))
        );
    }

    #[test]
    fn probe_dot_and_basename_fallback() {
        let (_dir, root) = make_root();
        std::fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(
            resolve_commit_git_dir(&root.to_string_lossy(), "."),
            Some(path_clean(&root))
        );
        // repo name differing from the folder name falls back to the root.
        assert_eq!(
            resolve_commit_git_dir(&root.to_string_lossy(), "othername"),
            Some(path_clean(&root))
        );
    }

    #[test]
    fn probe_empty_inputs_rejected() {
        assert!(resolve_commit_git_dir("", "x").is_none());
        assert!(resolve_commit_git_dir("/tmp", "").is_none());
        assert!(resolve_commit_git_dir("  ", "  ").is_none());
    }

    #[test]
    fn chained_resolver_prefers_verified_repo_record_linkage() {
        // Simulates F-009: primary resolves via repo record, fallback probes.
        let (dir, root) = make_root();
        std::fs::create_dir_all(dir.path().join("preferred").join(".git")).unwrap();
        std::fs::create_dir_all(root.join("fallback").join(".git")).unwrap();

        let primary = FixedResolver(Some(dir.path().join("preferred")));
        let fallback = DiscoveryRootProbe::new(root.to_string_lossy().into_owned());
        let chained = ChainedGitDirResolver::new(Arc::new(primary), Arc::new(fallback));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let got = rt.block_on(chained.resolve_git_dir("whatever"));
        assert_eq!(got, Some(dir.path().join("preferred")));

        // Primary returns a non-git path → falls back to the probe.
        let primary = FixedResolver(Some(dir.path().join("not-a-repo")));
        let chained = ChainedGitDirResolver::new(
            Arc::new(primary),
            Arc::new(DiscoveryRootProbe::new(root.to_string_lossy().into_owned())),
        );
        let got = rt.block_on(chained.resolve_git_dir("fallback"));
        assert_eq!(got, Some(path_clean(&root.join("fallback"))));
    }

    #[test]
    fn is_git_dir_accepts_file_and_dir() {
        let (dir, root) = make_root();
        std::fs::create_dir_all(root.join("d").join(".git")).unwrap();
        std::fs::create_dir(root.join("f")).unwrap();
        std::fs::write(root.join("f").join(".git"), "gitdir: /elsewhere").unwrap();
        std::fs::create_dir(root.join("none")).unwrap();
        assert!(is_git_dir(&root.join("d")));
        assert!(is_git_dir(&root.join("f")));
        assert!(!is_git_dir(&root.join("none")));
        assert!(!is_git_dir(&dir.path().join("missing")));
    }

    // ---------- chunking ----------

    #[test]
    fn chunk_string_short_text_single_chunk() {
        assert_eq!(chunk_string("hello", 8000), vec!["hello"]);
        assert_eq!(chunk_string("", 10), vec![String::new()]);
    }

    #[test]
    fn chunk_string_line_aligned() {
        let lines: Vec<String> = (0..100)
            .map(|i| format!("line{i:04} pad{}", "x".repeat(30)))
            .collect();
        let text = lines.join("\n");
        let chunks = chunk_string(&text, 200);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 200 + 40, "oversized chunk {}", c.len());
        }
        let rejoined = chunks.join("\n");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn chunk_string_oversized_line_becomes_own_chunk() {
        let big = "y".repeat(9000);
        let text = format!("{}\nnext line", big);
        let chunks = chunk_string(&text, 8000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], big);
        assert_eq!(chunks[1], "next line");
    }

    #[test]
    fn chunk_string_exact_boundary() {
        let a = "a".repeat(7999);
        let text = format!("{a}\nb");
        let chunks = chunk_string(&text, 8000);
        // 7999 + 1 + 1 = 8001 > 8000 → boundary split.
        assert_eq!(chunks.len(), 2);
        let single = "a".repeat(8000);
        assert_eq!(chunk_string(&single, 8000), vec![single]);
    }

    // ---------- loop lifecycle ----------

    #[tokio::test]
    async fn loop_runs_and_stops_cleanly() {
        let store = Arc::new(FakeStore::new(
            vec![PendingChunkRow {
                id: Some("file_chunk:1".into()),
                content: Some("text".into()),
            }],
            Vec::new(),
        ));
        let bm = Arc::new(BatchManager::new(
            Some(store.clone() as Arc<dyn ChunkStore>),
            Some(Arc::new(FakeEmbedder) as Arc<dyn AIProvider>),
            Arc::new(GitPatchSource),
            Arc::new(DiscoveryRootProbe::new(".")),
            8000,
        ));
        bm.start();
        // First sweep happens at POLL_INTERVAL; do not wait 10s — stop early.
        bm.stop().await;
        // Nothing should have run yet.
        let inner = store.inner.lock().unwrap();
        assert!(inner.saved.is_empty());
        assert!(inner.txns.is_empty());
    }

    #[tokio::test]
    async fn stop_without_start_is_fine() {
        let bm = BatchManager::new(
            None,
            None,
            Arc::new(GitPatchSource),
            Arc::new(DiscoveryRootProbe::new(".")),
            8000,
        );
        bm.stop().await;
    }

    #[test]
    fn schema_constants_match_reference() {
        assert_eq!(TABLE_COMMIT, "commit");
        assert_eq!(TABLE_FILE_CHUNK, "file_chunk");
        assert_eq!(TABLE_COMMIT_CHUNK, "commit_chunk");
        assert_eq!(EDGE_HAS_COMMIT_CHUNK, "has_commit_chunk");
    }
}
