//! threski-ai — AI provider plumbing:
//! providers (openai-compat, claude, gemini pool, ollama embed/chat, IDE LM
//! bridge), the hybrid local/cloud router, cloud failover pool, cost tracker,
//! batch embedding manager and the ollama lifecycle runner.
//!
//! The fake-injection boundary for other lanes is [`AIProvider`]; [`Client`]
//! is the orchestrator consumers hold.

pub mod batch_manager;
pub mod claude;
pub mod client;
pub mod cloud_pool;
pub mod config;
pub mod cost;
pub mod cursor_lm;
pub mod error;
pub mod gemini;
pub mod ollama;
pub mod ollama_chat;
pub mod openai_compat;
pub mod router;
pub mod runner;
pub mod traits;
pub mod types;

pub use batch_manager::{
    BatchManager, ChainedGitDirResolver, ChunkStore, CommitChunkWrite, CommitGitDirResolver,
    CommitPatchSource, CommitTxn, DiscoveryRootProbe, GitPatchSource, PendingChunkRow,
    PendingCommitRow, BATCH_SIZE, POLL_INTERVAL,
};
pub use claude::ClaudeProvider;
pub use client::Client;
pub use cloud_pool::{normalize_cloud_name, CloudPool};
pub use cost::{CostStore, CostTracker};
pub use cursor_lm::{CursorLMProvider, DEFAULT_IDE_BRIDGE_URL};
pub use error::AiError;
pub use gemini::{
    GeminiProvider, UsageStore, DEFAULT_GEMINI_BASE_URL, EMBEDDING_MODEL, GENERATION_MODEL,
    PREFERRED_GENERATION_MODEL,
};
pub use ollama::OllamaProvider;
pub use ollama_chat::OllamaChatProvider;
pub use openai_compat::OpenAICompatProvider;
pub use router::{ReasoningRouter, RouterConfig};
pub use runner::{
    ensure_ollama, ensure_ollama_binary, find_ollama_exec, start_ollama_daemon,
    EnsureOllamaOutcome, OllamaProcess,
};
pub use traits::AIProvider;
pub use types::{
    estimate_tokens, usage_id_for_key, BatchJobStatus, Candidate, Content, GenerationConfig,
    KeyConfig, Part, UsageMetadata,
};

#[cfg(test)]
pub(crate) mod test_support {
    use axum::Router;

    pub async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }
}
