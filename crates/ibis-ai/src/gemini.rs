//! Gemini provider: per-key worker pool with RPM pacing
//! (0.9 safety factor), TPM token bucket, RPD daily hard stop (sleep until
//! the day rolls, `allow_overage` bypass), usage persisted under
//! `key_usage:<hash8>_<yyyymmdd>`, 429 retryDelay-aware retries (3), and the
//! async batch endpoints.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex};
use tokio::task::JoinSet;

use crate::error::AiError;
use crate::runner::http_client;
use crate::traits::AIProvider;
use crate::types::{
    estimate_tokens, usage_id_for_key, BatchJobStatus, Candidate, Content, GenerationConfig,
    KeyConfig, UsageMetadata,
};

pub const EMBEDDING_MODEL: &str = "models/text-embedding-004";
pub const GENERATION_MODEL: &str = "models/gemini-3.6-flash";
pub const PREFERRED_GENERATION_MODEL: &str = "models/gemini-3.1-pro-preview";
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Generation models tried in order; the preferred model first, per-model
/// failover on 404.
pub(crate) const GENERATION_MODELS: [&str; 4] = [
    PREFERRED_GENERATION_MODEL,
    GENERATION_MODEL,
    "models/gemini-3.5-flash",
    "models/gemini-2.5-flash",
];

/// Worker HTTP client timeout.
const WORKER_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 3;
const QUEUE_BUFFER: usize = 100;
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const RPD_PAUSE: Duration = Duration::from_secs(10 * 60);
/// Safety factor applied to RPM/TPM to prevent edge-case overages.
const RATE_SAFETY_FACTOR: f64 = 0.9;

/// Persistence seam for per-key daily usage (`db.Executor calls in
/// ). The SQL contract, so a db-lane adapter can mirror it
/// exactly:
/// - load: `SELECT requests FROM <usage_id>;` (missing row → 0)
/// - save: `UPDATE <usage_id> SET requests = $req, last_updated = time::now;`
///   falling back on error to
///   `CREATE <usage_id> SET requests = $req, owner = $owner, date = time::now;`
///   where `usage_id` = `key_usage:<hash8>_<yyyymmdd>` (bound vars for values).
#[async_trait]
pub trait UsageStore: Send + Sync {
    async fn load_requests(&self, usage_id: &str) -> Result<f64, AiError>;
    async fn save_requests(
        &self,
        usage_id: &str,
        requests: i64,
        owner: &str,
    ) -> Result<(), AiError>;
}

#[derive(Debug)]
pub(crate) enum StepErr {
    Retry(Duration),
    Fatal(AiError),
}

type StepResult<T> = Result<T, StepErr>;

struct WorkerCfg {
    api_key: String,
    owner: String,
    http: reqwest::Client,
    base_url: String,
    /// Time to wait per request, based on RPM (`Duration::ZERO` = no limit).
    cost_interval: Duration,
    limit_tpm: i64,
    refill_rate: f64,
    max_bucket: f64,
    limit_rpd: i64,
    flush_interval: Duration,
    allow_overage: bool,
    store: Option<Arc<dyn UsageStore>>,
}

impl WorkerCfg {
    fn new(cfg: &KeyConfig, base_url: &str, store: Option<Arc<dyn UsageStore>>) -> Self {
        let safe_rpm = (cfg.rpm as f64 * RATE_SAFETY_FACTOR) as i64;
        let safe_rpm = if safe_rpm < 1 && cfg.rpm > 0 {
            1
        } else {
            safe_rpm
        };
        let cost_interval = if safe_rpm <= 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(60)
                .checked_div(safe_rpm.max(1) as u32)
                .unwrap_or(Duration::ZERO)
        };
        let safe_tpm = (cfg.tpm as f64 * RATE_SAFETY_FACTOR) as i64;
        WorkerCfg {
            api_key: cfg.key.clone(),
            owner: cfg.owner.clone(),
            http: http_client(WORKER_HTTP_TIMEOUT),
            base_url: base_url.to_string(),
            cost_interval,
            limit_tpm: safe_tpm,
            refill_rate: safe_tpm as f64 / 60.0,
            max_bucket: safe_tpm as f64,
            limit_rpd: cfg.rpd,
            flush_interval: cfg.flush_interval.unwrap_or(DEFAULT_FLUSH_INTERVAL),
            allow_overage: cfg.allow_overage,
            store,
        }
    }

    fn short_key(&self) -> &str {
        short_key(&self.api_key)
    }
}

pub(crate) fn short_key(api_key: &str) -> &str {
    if api_key.len() > 8 {
        &api_key[api_key.len() - 4..]
    } else {
        "????"
    }
}

struct SharedWorkerState {
    tpm_bucket: f64,
    last_refill: tokio::time::Instant,
    used_rpd: i64,
    last_reset_rpd: NaiveDate,
    usage_id: String,
    next_available: tokio::time::Instant,
}

impl SharedWorkerState {
    fn new(cfg: &WorkerCfg) -> Self {
        let today = Local::now().date_naive();
        SharedWorkerState {
            tpm_bucket: cfg.max_bucket,
            last_refill: tokio::time::Instant::now(),
            used_rpd: 0,
            last_reset_rpd: today,
            usage_id: usage_id_for_key(&cfg.api_key, today),
            next_available: tokio::time::Instant::now(),
        }
    }
}

type EmbedReply = oneshot::Sender<Result<Vec<Vec<f32>>, AiError>>;
type GenerateReply = oneshot::Sender<Result<Candidate, AiError>>;

enum Job {
    Embed {
        texts: Vec<String>,
        reply: EmbedReply,
    },
    Generate {
        contents: Vec<Content>,
        config: GenerationConfig,
        reply: GenerateReply,
    },
}

fn sleep_or_shutdown(
    shutdown: &watch::Receiver<bool>,
    d: Duration,
) -> impl Future<Output = bool> + '_ {
    let mut shutdown = shutdown.clone();
    async move {
        tokio::select! {
            _ = tokio::time::sleep(d) => true,
            _ = shutdown.changed() => false,
        }
    }
}

fn shutdown_triggered(shutdown: &watch::Receiver<bool>) -> bool {
    shutdown.has_changed().unwrap_or(true)
}

async fn save_usage(cfg: &WorkerCfg, usage_id: &str, used_rpd: i64) {
    if let Some(store) = &cfg.store {
        if let Err(e) = store.save_requests(usage_id, used_rpd, &cfg.owner).await {
            tracing::warn!(
                "AI Worker (...{}) usage persist failed for {}: {}",
                cfg.short_key(),
                usage_id,
                e
            );
        }
    }
}

/// Rolls the daily counters when the local date changed; flushes the previous
/// day's usage under its old record id first.
async fn roll_day_if_needed(cfg: &WorkerCfg, shared: &AsyncMutex<SharedWorkerState>) {
    let today = Local::now().date_naive();
    let rollover = {
        let st = shared.lock().await;
        today != st.last_reset_rpd
    };
    if !rollover {
        return;
    }
    let (old_id, used) = {
        let st = shared.lock().await;
        (st.usage_id.clone(), st.used_rpd)
    };
    save_usage(cfg, &old_id, used).await;
    let mut st = shared.lock().await;
    st.used_rpd = 0;
    st.last_reset_rpd = today;
    st.usage_id = usage_id_for_key(&cfg.api_key, today);
}

async fn wait_rate_limits(
    cfg: &WorkerCfg,
    shared: &AsyncMutex<SharedWorkerState>,
    shutdown: &watch::Receiver<bool>,
    texts: &[String],
) {
    let estimated: usize = texts.iter().map(|t| estimate_tokens(t)).sum();

    roll_day_if_needed(cfg, shared).await;

    let exhausted = {
        let st = shared.lock().await;
        !cfg.allow_overage && cfg.limit_rpd > 0 && st.used_rpd + 1 > cfg.limit_rpd
    };
    if exhausted {
        tracing::warn!(
            "AI Worker (...{}) exhausted RPD ({}). Pausing...",
            cfg.short_key(),
            cfg.limit_rpd
        );
        loop {
            if !sleep_or_shutdown(shutdown, RPD_PAUSE).await {
                return;
            }
            roll_day_if_needed(cfg, shared).await;
            let still_exhausted = {
                let st = shared.lock().await;
                !cfg.allow_overage && cfg.limit_rpd > 0 && st.used_rpd + 1 > cfg.limit_rpd
            };
            if !still_exhausted {
                break;
            }
        }
    }

    // TPM token bucket.
    if cfg.limit_tpm > 0 {
        let cost = estimated as f64;
        let mut wait = Duration::ZERO;
        {
            let mut st = shared.lock().await;
            let now = tokio::time::Instant::now();
            let elapsed = now.saturating_duration_since(st.last_refill).as_secs_f64();
            st.tpm_bucket += elapsed * cfg.refill_rate;
            if st.tpm_bucket > cfg.max_bucket {
                st.tpm_bucket = cfg.max_bucket;
            }
            st.last_refill = now;
            if st.tpm_bucket >= cost {
                st.tpm_bucket -= cost;
            } else if cfg.refill_rate > 0.0 {
                let needed = cost - st.tpm_bucket;
                wait = Duration::from_secs_f64(needed / cfg.refill_rate);
            }
        }
        if wait > Duration::ZERO {
            if wait > Duration::from_secs(5) {
                tracing::debug!(
                    "AI: Rate limit throttling ...{}. Waiting {:?} (Cost: {})",
                    cfg.short_key(),
                    wait,
                    estimated
                );
            }
            if !sleep_or_shutdown(shutdown, wait).await {
                return;
            }
            let mut st = shared.lock().await;
            st.tpm_bucket = 0.0;
            st.last_refill = tokio::time::Instant::now();
        }
    }

    // RPM interval pacing.
    {
        let wait = {
            let st = shared.lock().await;
            let now = tokio::time::Instant::now();
            if now < st.next_available {
                st.next_available - now
            } else {
                Duration::ZERO
            }
        };
        if wait > Duration::ZERO && !sleep_or_shutdown(shutdown, wait).await {
            return;
        }
        let mut st = shared.lock().await;
        st.next_available = tokio::time::Instant::now() + cfg.cost_interval;
        st.used_rpd += 1;
    }
}

async fn flush_loop(
    cfg: Arc<WorkerCfg>,
    shared: Arc<AsyncMutex<SharedWorkerState>>,
    shutdown: watch::Receiver<bool>,
    initial_rpd: i64,
    initial_id: String,
) {
    let mut ticker = tokio::time::interval(cfg.flush_interval);
    // Baseline captured at construction (the flushLoop starts with the
    // worker's at-construction counters; Rust tasks start lazily, so the
    // baseline must be passed in to avoid racing the first job).
    let mut last_flushed_rpd = initial_rpd;
    let mut last_flushed_id = initial_id;
    let mut shutdown = shutdown;
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                let (rpd, id) = {
                    let st = shared.lock().await;
                    (st.used_rpd, st.usage_id.clone())
                };
                if rpd != last_flushed_rpd || id != last_flushed_id {
                    save_usage(&cfg, &id, rpd).await;
                }
                return;
            }
            _ = ticker.tick() => {
                let (rpd, id) = {
                    let st = shared.lock().await;
                    (st.used_rpd, st.usage_id.clone())
                };
                if rpd != last_flushed_rpd || id != last_flushed_id {
                    save_usage(&cfg, &id, rpd).await;
                    last_flushed_rpd = rpd;
                    last_flushed_id = id;
                }
            }
        }
    }
}

async fn worker_loop(
    cfg: Arc<WorkerCfg>,
    shared: Arc<AsyncMutex<SharedWorkerState>>,
    rx: Arc<AsyncMutex<mpsc::Receiver<Job>>>,
    shutdown: watch::Receiver<bool>,
) {
    if let Some(store) = &cfg.store {
        let usage_id = {
            let st = shared.lock().await;
            st.usage_id.clone()
        };
        if let Ok(requests) = store.load_requests(&usage_id).await {
            let mut st = shared.lock().await;
            st.used_rpd = requests as i64;
            tracing::info!(
                "AI: Loaded RPD usage for {} ({}): {}/{}",
                cfg.owner,
                usage_id,
                st.used_rpd,
                cfg.limit_rpd
            );
        }
    }

    tracing::info!("AI Worker started for key ...{}", cfg.short_key());

    let mut shutdown = shutdown;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            job = async { rx.lock().await.recv().await } => {
                match job {
                    Some(Job::Embed { texts, reply }) => {
                        process_embed(&cfg, &shared, &shutdown, (texts, reply)).await
                    }
                    Some(Job::Generate { contents, config, reply }) => {
                        process_generate(&cfg, &shared, &shutdown, (contents, config, reply)).await
                    }
                    None => return,
                }
            }
        }
    }
}

async fn process_embed(
    cfg: &WorkerCfg,
    shared: &AsyncMutex<SharedWorkerState>,
    shutdown: &watch::Receiver<bool>,
    job: (Vec<String>, EmbedReply),
) {
    let (texts, reply) = job;
    wait_rate_limits(cfg, shared, shutdown, &texts).await;

    let mut result: Result<Vec<Vec<f32>>, AiError> = Err(AiError::Cancelled);
    for _ in 0..=MAX_RETRIES {
        if shutdown_triggered(shutdown) {
            break;
        }
        tracing::debug!(
            "AI: Worker ...{} processing batch (size: {})",
            cfg.short_key(),
            texts.len()
        );
        match do_embed(cfg, &texts).await {
            Ok(embeddings) => {
                result = Ok(embeddings);
                break;
            }
            Err(StepErr::Retry(delay)) => {
                tracing::warn!(
                    "AI Worker (...{}) rate limited. Waiting {:?}",
                    cfg.short_key(),
                    delay
                );
                if !sleep_or_shutdown(shutdown, delay).await {
                    break;
                }
            }
            Err(StepErr::Fatal(err)) => {
                result = Err(err);
                break;
            }
        }
    }
    let _ = reply.send(result);
}

async fn process_generate(
    cfg: &WorkerCfg,
    shared: &AsyncMutex<SharedWorkerState>,
    shutdown: &watch::Receiver<bool>,
    job: (Vec<Content>, GenerationConfig, GenerateReply),
) {
    let (contents, config, reply) = job;
    let texts: Vec<String> = contents
        .iter()
        .flat_map(|c| c.parts.iter().map(|p| p.text.clone()))
        .collect();
    wait_rate_limits(cfg, shared, shutdown, &texts).await;

    let mut result: Result<Candidate, AiError> = Err(AiError::Cancelled);
    for _ in 0..=MAX_RETRIES {
        if shutdown_triggered(shutdown) {
            break;
        }
        tracing::debug!("AI: Worker ...{} processing generation", cfg.short_key());
        match do_generate(cfg, &contents, &config).await {
            Ok(candidate) => {
                result = Ok(candidate);
                break;
            }
            Err(StepErr::Retry(delay)) => {
                tracing::warn!(
                    "AI Worker (...{}) rate limited. Waiting {:?}",
                    cfg.short_key(),
                    delay
                );
                if !sleep_or_shutdown(shutdown, delay).await {
                    break;
                }
            }
            Err(StepErr::Fatal(err)) => {
                result = Err(err);
                break;
            }
        }
    }
    let _ = reply.send(result);
}

// --- Wire DTOs ---

#[derive(Debug, Serialize)]
pub(crate) struct EmbedRequestItem {
    pub model: String,
    pub content: Content,
}

#[derive(Debug, Serialize)]
pub(crate) struct BatchEmbedRequest {
    pub requests: Vec<EmbedRequestItem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchEmbedResponse {
    #[serde(default)]
    pub embeddings: Vec<EmbeddingValues>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EmbeddingValues {
    #[serde(default)]
    pub values: Vec<f32>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GenerateContentRequest {
    pub contents: Vec<Content>,
    #[serde(rename = "generationConfig")]
    pub generation_config: GenerationConfig,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(rename = "usageMetadata", default)]
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AsyncBatchEmbedPayload {
    pub model: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "inputConfig")]
    pub input_config: AsyncInputConfig<EmbedRequestItem>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AsyncBatchGeneratePayload {
    pub model: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "inputConfig")]
    pub input_config: AsyncInputConfig<GenerateContentRequest>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AsyncInputConfig<T> {
    pub requests: AsyncRequests<T>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AsyncRequests<T> {
    pub requests: Vec<T>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Operation {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub error: Option<OperationError>,
    #[serde(default)]
    pub response: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct OperationError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchGenerateResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ErrorResponse {
    #[serde(default)]
    pub error: ErrorBody,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ErrorBody {
    #[serde(default)]
    pub details: Vec<ErrorDetail>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ErrorDetail {
    #[serde(rename = "retryDelay", default)]
    pub retry_delay: String,
}

/// Parses the -duration `retryDelay` out of a Gemini 429 error body
/// (`{"error":{"details":[{"@type":"…RetryInfo","retryDelay":"23s"}]}}`).
pub(crate) fn parse_retry_delay(body: &[u8]) -> Option<Duration> {
    let resp: ErrorResponse = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(_) => return None,
    };
    for detail in resp.error.details {
        if !detail.retry_delay.is_empty() {
            if let Some(d) = parse_go_duration(&detail.retry_delay) {
                return Some(d);
            }
        }
    }
    None
}

/// Parses the `time.ParseDuration subset ("23s", "1.5s", "100ms",
/// "2m30s", "1h"); negative durations collapse to zero.
pub(crate) fn parse_go_duration(s: &str) -> Option<Duration> {
    const UNITS: [(&str, f64); 8] = [
        ("ns", 1.0),
        ("us", 1e3),
        ("µs", 1e3),
        ("μs", 1e3),
        ("ms", 1e6),
        ("s", 1e9),
        ("m", 60e9),
        ("h", 3600e9),
    ];
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    if s.starts_with('-') {
        return Some(Duration::ZERO);
    }
    let mut total_ns = 0.0f64;
    let mut rest = s;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        if num_end == 0 {
            return None;
        }
        let value: f64 = rest[..num_end].parse().ok()?;
        rest = &rest[num_end..];
        let (unit, factor) = UNITS.iter().find(|(u, _)| rest.starts_with(u))?;
        total_ns += value * factor;
        rest = &rest[unit.len()..];
    }
    if total_ns.is_nan() || total_ns < 0.0 {
        return None;
    }
    Some(Duration::from_nanos(total_ns as u64))
}

// --- HTTP calls ---

async fn read_body(resp: reqwest::Response) -> Result<Vec<u8>, StepErr> {
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| StepErr::Fatal(AiError::from(e)))?;
    Ok(bytes.to_vec())
}

fn body_lossy(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

async fn do_embed(cfg: &WorkerCfg, texts: &[String]) -> StepResult<Vec<Vec<f32>>> {
    let url = format!(
        "{}/{}:batchEmbedContents?key={}",
        cfg.base_url, EMBEDDING_MODEL, cfg.api_key
    );
    let items: Vec<EmbedRequestItem> = texts
        .iter()
        .map(|t| EmbedRequestItem {
            model: EMBEDDING_MODEL.to_string(),
            content: Content::text(t.clone()),
        })
        .collect();
    let payload = serde_json::to_string(&BatchEmbedRequest { requests: items })
        .map_err(|e| StepErr::Fatal(AiError::other(e.to_string())))?;

    let start = std::time::Instant::now();
    let resp = cfg
        .http
        .post(url)
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .map_err(|e| StepErr::Fatal(AiError::from(e)))?;
    let status = resp.status().as_u16();
    let body = read_body(resp).await?;
    let duration = start.elapsed();

    if status == 200 {
        tracing::debug!("AI: Worker ...{} success ({duration:?})", cfg.short_key());
        let result: BatchEmbedResponse = serde_json::from_slice(&body)
            .map_err(|e| StepErr::Fatal(AiError::other(format!("parsing error: {e}"))))?;
        return Ok(result.embeddings.into_iter().map(|e| e.values).collect());
    }

    if status == 429 {
        let retry_delay = parse_retry_delay(&body).unwrap_or(Duration::from_secs(5));
        return Err(StepErr::Retry(retry_delay));
    }

    Err(StepErr::Fatal(AiError::other(format!(
        "gemini api error {status}: {}",
        body_lossy(&body)
    ))))
}

async fn do_generate(
    cfg: &WorkerCfg,
    contents: &[Content],
    config: &GenerationConfig,
) -> StepResult<Candidate> {
    let mut last_err: Option<AiError> = None;

    for model in GENERATION_MODELS {
        let url = format!(
            "{}/{}:generateContent?key={}",
            cfg.base_url, model, cfg.api_key
        );
        let payload = serde_json::to_string(&GenerateContentRequest {
            contents: contents.to_vec(),
            generation_config: config.clone(),
        })
        .map_err(|e| StepErr::Fatal(AiError::other(e.to_string())))?;

        let start = std::time::Instant::now();
        let resp = match cfg
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(AiError::from(e));
                continue;
            }
        };
        let status = resp.status().as_u16();
        let body = read_body(resp).await?;
        let duration = start.elapsed();

        if status == 200 {
            tracing::debug!(
                "AI: Worker ...{} generation success with {} ({duration:?})",
                cfg.short_key(),
                model
            );
            let result: GenerateContentResponse = serde_json::from_slice(&body)
                .map_err(|e| StepErr::Fatal(AiError::other(format!("parsing error: {e}"))))?;
            let Some(mut cand) = result.candidates.into_iter().next() else {
                return Err(StepErr::Fatal(AiError::other("no candidates returned")));
            };
            cand.usage_metadata = result.usage_metadata;
            return Ok(cand);
        }

        if status == 429 {
            let retry_delay = parse_retry_delay(&body).unwrap_or(Duration::from_secs(5));
            return Err(StepErr::Retry(retry_delay));
        }

        if status == 404 {
            tracing::warn!("AI: Model {model} not found, trying next...");
            last_err = Some(AiError::other(format!(
                "gemini api error {status}: {}",
                body_lossy(&body)
            )));
            continue;
        }

        return Err(StepErr::Fatal(AiError::other(format!(
            "gemini api error {status}: {}",
            body_lossy(&body)
        ))));
    }

    Err(StepErr::Fatal(last_err.unwrap_or_else(|| {
        AiError::other("gemini generation failed")
    })))
}

struct WorkerMeta {
    api_key: String,
    http: reqwest::Client,
    base_url: String,
}

struct GeminiInner {
    tx: mpsc::Sender<Job>,
}

/// Per-key worker pool over the Gemini v1beta API.
pub struct GeminiProvider {
    inner: Option<GeminiInner>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    tasks: AsyncMutex<JoinSet<()>>,
    first: Option<WorkerMeta>,
}

impl GeminiProvider {
    pub fn new(keys: Vec<KeyConfig>, store: Option<Arc<dyn UsageStore>>) -> Self {
        Self::new_with_base_url(keys, store, DEFAULT_GEMINI_BASE_URL)
    }

    /// `base_url` overrides [`DEFAULT_GEMINI_BASE_URL`] ( tests swapped the
    /// package-level `BaseURL` var; this is the equivalent test seam).
    pub fn new_with_base_url(
        keys: Vec<KeyConfig>,
        store: Option<Arc<dyn UsageStore>>,
        base_url: &str,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        if keys.is_empty() {
            return GeminiProvider {
                inner: None,
                shutdown_tx,
                shutdown_rx,
                tasks: AsyncMutex::new(JoinSet::new()),
                first: None,
            };
        }

        let (tx, rx) = mpsc::channel(QUEUE_BUFFER);
        let rx = Arc::new(AsyncMutex::new(rx));
        let mut tasks = JoinSet::new();
        let mut first = None;

        for cfg in keys {
            let wcfg = Arc::new(WorkerCfg::new(&cfg, base_url, store.clone()));
            if first.is_none() {
                first = Some(WorkerMeta {
                    api_key: wcfg.api_key.clone(),
                    http: wcfg.http.clone(),
                    base_url: base_url.to_string(),
                });
            }
            let shared = Arc::new(AsyncMutex::new(SharedWorkerState::new(&wcfg)));
            let initial_id = usage_id_for_key(&wcfg.api_key, Local::now().date_naive());
            tasks.spawn(worker_loop(
                wcfg.clone(),
                shared.clone(),
                rx.clone(),
                shutdown_rx.clone(),
            ));
            tasks.spawn(flush_loop(wcfg, shared, shutdown_rx.clone(), 0, initial_id));
        }

        GeminiProvider {
            inner: Some(GeminiInner { tx }),
            shutdown_tx,
            shutdown_rx,
            tasks: AsyncMutex::new(tasks),
            first,
        }
    }

    fn worker_meta(&self) -> Result<&WorkerMeta, AiError> {
        self.first
            .as_ref()
            .ok_or_else(|| AiError::other("no active workers"))
    }

    /// Resolves once the provider is stopping (or its sender was dropped).
    fn shutdown_wait(&self) -> impl Future<Output = ()> + '_ {
        let mut shutdown_rx = self.shutdown_rx.clone();
        async move {
            let _ = shutdown_rx.changed().await;
        }
    }

    fn display_name(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}_{nanos}")
    }

    /// `POST {base}/{model}:asyncBatchEmbedContent` — returns the Operation name.
    pub async fn create_batch_embed_job(&self, texts: &[String]) -> Result<String, AiError> {
        let w = self.worker_meta()?;
        let items: Vec<EmbedRequestItem> = texts
            .iter()
            .map(|t| EmbedRequestItem {
                model: EMBEDDING_MODEL.to_string(),
                content: Content::text(t.clone()),
            })
            .collect();
        let payload = AsyncBatchEmbedPayload {
            model: EMBEDDING_MODEL.to_string(),
            display_name: Self::display_name("batch"),
            input_config: AsyncInputConfig {
                requests: AsyncRequests { requests: items },
            },
        };
        let url = format!(
            "{}/{}:asyncBatchEmbedContent?key={}",
            w.base_url, EMBEDDING_MODEL, w.api_key
        );
        self.async_post(w, &url, &payload, "gemini async batch error")
            .await
    }

    /// `POST {base}/{model}:batchGenerateContent` — returns the Operation name.
    pub async fn create_batch_generate_job(
        &self,
        model: &str,
        contents: &[Vec<Content>],
        config: &GenerationConfig,
    ) -> Result<String, AiError> {
        let w = self.worker_meta()?;
        let model = if model.is_empty() {
            PREFERRED_GENERATION_MODEL
        } else {
            model
        };
        let items: Vec<GenerateContentRequest> = contents
            .iter()
            .map(|c| GenerateContentRequest {
                contents: c.clone(),
                generation_config: config.clone(),
            })
            .collect();
        let payload = AsyncBatchGeneratePayload {
            model: model.to_string(),
            display_name: Self::display_name("gen_batch"),
            input_config: AsyncInputConfig {
                requests: AsyncRequests { requests: items },
            },
        };
        let url = format!(
            "{}/{}:batchGenerateContent?key={}",
            w.base_url, model, w.api_key
        );
        self.async_post(w, &url, &payload, "gemini async batch gen error")
            .await
    }

    async fn async_post<T: Serialize>(
        &self,
        w: &WorkerMeta,
        url: &str,
        payload: &T,
        err_prefix: &str,
    ) -> Result<String, AiError> {
        let body = serde_json::to_string(payload).map_err(|e| AiError::other(e.to_string()))?;
        let resp = w
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(AiError::from)?;
        let status = resp.status().as_u16();
        let raw = resp.bytes().await.map_err(AiError::from)?;
        if status == 200 {
            let op: Operation = serde_json::from_slice(&raw)
                .map_err(|e| AiError::other(format!("parsing operation error: {e}")))?;
            return Ok(op.name);
        }
        Err(AiError::other(format!(
            "{err_prefix} {}: {}",
            status,
            String::from_utf8_lossy(&raw)
        )))
    }

    /// Polls an Operation / Batch resource; inline `embeddings` / `candidates`
    /// in the done-response are extracted.
    pub async fn get_batch_job(&self, name: &str) -> Result<BatchJobStatus, AiError> {
        let w = self.worker_meta()?;
        let url = format!("{}/{}?key={}", w.base_url, name, w.api_key);
        let resp = w.http.get(url).send().await.map_err(AiError::from)?;
        let status = resp.status().as_u16();
        let raw = resp.bytes().await.map_err(AiError::from)?;
        if status != 200 {
            return Err(AiError::other(format!(
                "get batch error {status}: {}",
                String::from_utf8_lossy(&raw)
            )));
        }
        let op: Operation = match serde_json::from_slice(&raw) {
            Ok(op) => op,
            Err(_) => return Err(AiError::other("unknown response format")),
        };
        if op.name.is_empty() {
            return Err(AiError::other("unknown response format"));
        }
        let mut out = BatchJobStatus {
            name: op.name.clone(),
            done: op.done,
            ..Default::default()
        };
        if let Some(err) = &op.error {
            out.error = Some(format!("operation error: {}", err.message));
        }
        if op.done {
            if let Some(response) = &op.response {
                if let Some(raw_embeddings) = response.get("embeddings") {
                    if let Ok(wrapped) = serde_json::from_value::<BatchEmbedResponse>(
                        serde_json::json!({ "embeddings": raw_embeddings }),
                    ) {
                        out.embeddings = wrapped.embeddings.into_iter().map(|e| e.values).collect();
                    }
                }
                if let Some(raw_candidates) = response.get("candidates") {
                    if let Ok(wrapped) = serde_json::from_value::<BatchGenerateResponse>(
                        serde_json::json!({ "candidates": raw_candidates }),
                    ) {
                        out.candidates = wrapped.candidates;
                    }
                }
            }
        }
        Ok(out)
    }

    /// parity: `GetBatchResults` was never implemented upstream — results
    /// live in the Operation's inline `response` (see `get_batch_job`).
    pub async fn get_batch_results(&self, _output_uri: &str) -> Result<Vec<Vec<f32>>, AiError> {
        Err(AiError::other(
            "not implemented: use get_batch_job and check for inline responses or file URI",
        ))
    }
}

#[async_trait]
impl AIProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    async fn is_functional(&self) -> bool {
        self.inner.is_some()
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        let res = self.batch_embed_text(&[text.to_string()]).await?;
        res.into_iter()
            .next()
            .ok_or_else(|| AiError::other("no embedding returned"))
    }

    async fn batch_embed_text(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        let Some(inner) = &self.inner else {
            return Err(AiError::NotInitialized);
        };
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = Job::Embed {
            texts: texts.to_vec(),
            reply: reply_tx,
        };
        tokio::select! {
            res = inner.tx.send(job) => {
                if res.is_err() {
                    return Err(AiError::Cancelled);
                }
            }
            _ = self.shutdown_wait() => return Err(AiError::Cancelled),
        }
        tokio::select! {
            res = reply_rx => match res {
                Ok(r) => r,
                Err(_) => Err(AiError::Cancelled),
            },
            _ = self.shutdown_wait() => Err(AiError::Cancelled),
        }
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        let Some(inner) = &self.inner else {
            return Err(AiError::NotInitialized);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = Job::Generate {
            contents: contents.to_vec(),
            config,
            reply: reply_tx,
        };
        tokio::select! {
            res = inner.tx.send(job) => {
                if res.is_err() {
                    return Err(AiError::Cancelled);
                }
            }
            _ = self.shutdown_wait() => return Err(AiError::Cancelled),
        }
        tokio::select! {
            res = reply_rx => match res {
                Ok(r) => r,
                Err(_) => Err(AiError::Cancelled),
            },
            _ = self.shutdown_wait() => Err(AiError::Cancelled),
        }
    }

    /// Graceful shutdown: cancels rate-limit sleeps, flushes final usage and
    /// waits for every worker/flush task.
    async fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
        let mut tasks = self.tasks.lock().await;
        while let Some(res) = tasks.join_next().await {
            let _ = res;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::atomic::{AtomicU64, Ordering};

    // ---------- fake usage store ----------

    struct FakeStore {
        loads: AtomicU64,
        saves: AtomicU64,
        last_saved_id: AsyncMutex<String>,
        initial_requests: f64,
    }

    impl FakeStore {
        fn new(initial: f64) -> Self {
            FakeStore {
                loads: AtomicU64::new(0),
                saves: AtomicU64::new(0),
                last_saved_id: AsyncMutex::new(String::new()),
                initial_requests: initial,
            }
        }
    }

    #[async_trait]
    impl UsageStore for FakeStore {
        async fn load_requests(&self, _usage_id: &str) -> Result<f64, AiError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(self.initial_requests)
        }
        async fn save_requests(
            &self,
            usage_id: &str,
            _requests: i64,
            _owner: &str,
        ) -> Result<(), AiError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            *self.last_saved_id.lock().await = usage_id.to_string();
            Ok(())
        }
    }

    // ---------- servers ----------

    fn ok_json(body: &str) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(body.to_string())
            .unwrap()
            .into_response()
    }

    async fn embed_server() -> (String, Arc<AtomicU64>) {
        let hits = Arc::new(AtomicU64::new(0));
        let handler_state = hits.clone();
        let app = Router::new().route(
            "/models/text-embedding-004:batchEmbedContents",
            post(move || {
                let hits = handler_state.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    ok_json(r#"{"embeddings":[{"values":[0.1,0.2,0.3]}]}"#)
                }
            }),
        );
        (serve(app).await, hits)
    }

    async fn keyed_embed_server() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler_state = hits.clone();
        let app = Router::new().route(
            "/models/text-embedding-004:batchEmbedContents",
            post(move |req: Request| {
                let hits = handler_state.clone();
                async move {
                    let query = req.uri().query().unwrap_or_default().to_string();
                    let key = query
                        .split('&')
                        .find_map(|p| p.strip_prefix("key="))
                        .unwrap_or("")
                        .to_string();
                    hits.lock().unwrap().push(key);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    ok_json(r#"{"embeddings":[{"values":[0.1,0.2,0.3]}]}"#)
                }
            }),
        );
        (serve(app).await, hits)
    }

    // ---------- worker pool ( ports) ----------

    #[tokio::test]
    async fn worker_pool_distributes_across_keys() {
        let (base_url, hits) = keyed_embed_server().await;
        let provider = Arc::new(GeminiProvider::new_with_base_url(
            vec![
                KeyConfig {
                    key: "key1".into(),
                    rpm: 600,
                    tpm: 1000,
                    rpd: 1000,
                    ..Default::default()
                },
                KeyConfig {
                    key: "key2".into(),
                    rpm: 600,
                    tpm: 1000,
                    rpd: 1000,
                    ..Default::default()
                },
            ],
            None,
            &base_url,
        ));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let p = provider.clone();
            handles.push(tokio::spawn(async move { p.embed_text("test").await }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }

        {
            let keys = hits.lock().unwrap();
            assert_eq!(keys.len(), 10);
            let k1 = keys.iter().filter(|k| k.contains("key1")).count();
            let k2 = keys.iter().filter(|k| k.contains("key2")).count();
            assert_eq!(k1 + k2, 10);
        }
        provider.stop().await;
    }

    #[tokio::test]
    async fn rpm_pacing_delays_second_request() {
        let (base_url, _) = embed_server().await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "key1".into(),
                rpm: 60,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let start = std::time::Instant::now();
        provider.embed_text("1").await.unwrap();
        provider.embed_text("2").await.unwrap();
        let duration = start.elapsed();
        assert!(
            duration >= Duration::from_millis(900),
            "expected pacing delay (~1s), took {duration:?}"
        );
        provider.stop().await;
    }

    #[tokio::test]
    async fn tpm_token_bucket_throttles_burst() {
        let (base_url, _) = embed_server().await;
        let text60 = "a".repeat(60);
        // TPM 134 → safe ≈120.6 tokens, refill ≈2.01 tok/s; 6 requests × 21
        // tokens deplete the bucket and the last waits ~2.7s.
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "keyLowTPM".into(),
                rpm: 1000,
                tpm: 134,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let start = std::time::Instant::now();
        for _ in 0..6 {
            provider.embed_text(&text60).await.unwrap();
        }
        let duration = start.elapsed();
        assert!(
            duration >= Duration::from_secs(2),
            "rate limit failed: expected >2s, took {duration:?}"
        );
        provider.stop().await;
    }

    const GENERATE_FIXTURE: &str = r#"{
        "candidates": [
            {
                "content": { "parts": [{"text": "Hello world"}], "role": "model" },
                "finishReason": "STOP"
            }
        ]
    }"#;

    #[tokio::test]
    async fn generate_content_returns_candidate() {
        let app = Router::new().route(
            "/models/gemini-3.1-pro-preview:generateContent",
            post(|| async { ok_json(GENERATE_FIXTURE) }),
        );
        let base_url = serve(app).await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "keyGen".into(),
                rpm: 60,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let resp = provider
            .generate_content(&[Content::text("Hi")], GenerationConfig::default())
            .await
            .unwrap();
        assert_eq!(resp.content.parts[0].text, "Hello world");
        assert_eq!(resp.content.role, "model");
        assert_eq!(resp.finish_reason, "STOP");
        provider.stop().await;
    }

    // ---------- usage persistence ----------

    #[tokio::test]
    async fn usage_id_format_and_store_calls() {
        let (base_url, _) = embed_server().await;
        let store = Arc::new(FakeStore::new(0.0));
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "testkey".into(),
                rpm: 60000,
                tpm: 100000,
                rpd: 1000,
                owner: "Test".into(),
                flush_interval: Some(Duration::from_millis(50)),
                ..Default::default()
            }],
            Some(store.clone() as Arc<dyn UsageStore>),
            &base_url,
        );
        provider.embed_text("x").await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        provider.stop().await;

        let saved_id = store.last_saved_id.lock().await.clone();
        assert!(saved_id.starts_with("key_usage:"));
        let (_, rest) = saved_id.split_once(':').unwrap();
        let (hash, date_part) = rest.split_once('_').unwrap();
        assert_eq!(hash.len(), 8);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!date_part.contains('-'));
        assert_eq!(date_part.len(), 8);
        assert!(store.loads.load(Ordering::SeqCst) >= 1);
        assert!(store.saves.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn usage_loaded_from_store_on_start() {
        let (base_url, _) = embed_server().await;
        let store = Arc::new(FakeStore::new(42.0));
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "k".into(),
                rpm: 60000,
                rpd: 40,
                allow_overage: true,
                ..Default::default()
            }],
            Some(store.clone() as Arc<dyn UsageStore>),
            &base_url,
        );
        // allow_overage bypasses the RPD hard stop even when the loaded usage
        // already exhausted the quota.
        provider.embed_text("x").await.unwrap();
        provider.stop().await;
    }

    #[tokio::test]
    async fn usage_flush_batches_updates() {
        let (base_url, _) = embed_server().await;
        let store = Arc::new(FakeStore::new(0.0));
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "testKey".into(),
                rpm: 10000,
                tpm: 100000,
                rpd: 100000,
                flush_interval: Some(Duration::from_millis(100)),
                ..Default::default()
            }],
            Some(store.clone() as Arc<dyn UsageStore>),
            &base_url,
        );
        for _ in 0..50 {
            provider.embed_text("test").await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        provider.stop().await;
        let updates = store.saves.load(Ordering::SeqCst);
        assert!(
            updates < 10,
            "Too many DB updates! Got {updates}, expected < 10"
        );
    }

    // ---------- wire contracts ----------

    #[tokio::test]
    async fn embed_wire_contract_payload_and_query_key() {
        let captured = Arc::new(AsyncMutex::new(Vec::new()));
        let handler_state = captured.clone();
        let app = Router::new().route(
            "/models/text-embedding-004:batchEmbedContents",
            post(move |req: Request| {
                let captured = handler_state.clone();
                async move {
                    let query = req.uri().query().unwrap_or_default().to_string();
                    let content_type = req
                        .headers()
                        .get("content-type")
                        .map(|v| v.to_str().unwrap().to_string())
                        .unwrap_or_default();
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    *captured.lock().await = vec![
                        query,
                        String::from_utf8_lossy(&body).into_owned(),
                        content_type,
                    ];
                    ok_json(r#"{"embeddings":[{"values":[1.0]}]}"#)
                }
            }),
        );
        let base_url = serve(app).await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "K&Y".into(),
                rpm: 6000,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        provider
            .batch_embed_text(&["hello".to_string(), "world".to_string()])
            .await
            .unwrap();
        provider.stop().await;

        let captured = captured.lock().await;
        assert!(
            captured[0].starts_with("key=K&Y"),
            "raw query parity: {}",
            captured[0]
        );
        let payload: serde_json::Value = serde_json::from_str(&captured[1]).unwrap();
        let requests = payload["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], "models/text-embedding-004");
        assert_eq!(requests[0]["content"]["parts"][0]["text"], "hello");
        assert_eq!(captured[2], "application/json");
    }

    #[tokio::test]
    async fn rate_limit_retry_uses_retry_delay() {
        let hits = Arc::new(AtomicU64::new(0));
        let handler_state = hits.clone();
        let app = Router::new().route(
            "/models/text-embedding-004:batchEmbedContents",
            post(move || {
                let hits = handler_state.clone();
                async move {
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        return Response::builder()
                            .status(StatusCode::TOO_MANY_REQUESTS)
                            .body(
                                r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"1s"}]}}"#
                                    .to_string(),
                            )
                            .unwrap()
                            .into_response();
                    }
                    ok_json(r#"{"embeddings":[{"values":[0.5]}]}"#)
                }
            }),
        );
        let base_url = serve(app).await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "k".into(),
                rpm: 60000,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let emb = provider.embed_text("x").await.unwrap();
        assert_eq!(emb, vec![0.5]);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        provider.stop().await;
    }

    #[tokio::test]
    async fn generation_model_failover_order_on_404() {
        let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler_state = requested.clone();
        let app = Router::new().route(
            "/models/{model_op}",
            post(move |req: Request| {
                let requested = handler_state.clone();
                async move {
                    let path = req.uri().path().to_string();
                    let is_unavailable = path.contains("gemini-3.1-pro-preview")
                        || path.contains("gemini-3.6-flash");
                    requested.lock().unwrap().push(path);
                    if is_unavailable {
                        return Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(r#"{"error":{"code":404,"message":"not found"}}"#.to_string())
                            .unwrap()
                            .into_response();
                    }
                    ok_json(GENERATE_FIXTURE)
                }
            }),
        );
        let base_url = serve(app).await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "k".into(),
                rpm: 60000,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let cand = provider
            .generate_content(&[Content::text("Hi")], GenerationConfig::default())
            .await
            .unwrap();
        assert_eq!(cand.content.parts[0].text, "Hello world");
        {
            let requested = requested.lock().unwrap();
            assert!(requested[0].contains("gemini-3.1-pro-preview"));
            assert!(requested[1].contains("gemini-3.6-flash"));
            assert!(requested[2].contains("gemini-3.5-flash"));
            assert_eq!(requested.len(), 3);
        }
        provider.stop().await;
    }

    // ---------- async batch endpoints ----------

    #[tokio::test]
    async fn async_batch_embed_operation() {
        let captured = Arc::new(AsyncMutex::new(String::new()));
        let handler_state = captured.clone();
        let app = Router::new().route(
            "/models/text-embedding-004:asyncBatchEmbedContent",
            post(move |req: Request| {
                let captured = handler_state.clone();
                async move {
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    *captured.lock().await = String::from_utf8_lossy(&body).into_owned();
                    ok_json(r#"{"name":"operations/batch-1"}"#)
                }
            }),
        );
        let base_url = serve(app).await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "k".into(),
                rpm: 60000,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let name = provider
            .create_batch_embed_job(&["a".to_string()])
            .await
            .unwrap();
        assert_eq!(name, "operations/batch-1");
        let payload: serde_json::Value =
            serde_json::from_str(captured.lock().await.as_str()).unwrap();
        assert_eq!(payload["model"], "models/text-embedding-004");
        assert!(payload["displayName"]
            .as_str()
            .unwrap()
            .starts_with("batch_"));
        assert_eq!(
            payload["inputConfig"]["requests"]["requests"][0]["content"]["parts"][0]["text"],
            "a"
        );
        provider.stop().await;
    }

    #[tokio::test]
    async fn async_batch_generate_operation() {
        let captured = Arc::new(AsyncMutex::new(String::new()));
        let handler_state = captured.clone();
        let app = Router::new().route(
            "/models/gemini-3.1-pro-preview:batchGenerateContent",
            post(move |req: Request| {
                let captured = handler_state.clone();
                async move {
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    *captured.lock().await = String::from_utf8_lossy(&body).into_owned();
                    ok_json(r#"{"name":"operations/gen-1"}"#)
                }
            }),
        );
        let base_url = serve(app).await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "k".into(),
                rpm: 60000,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let name = provider
            .create_batch_generate_job(
                "",
                &[vec![Content::text("hi")]],
                &GenerationConfig::default(),
            )
            .await
            .unwrap();
        assert_eq!(name, "operations/gen-1");
        let payload: serde_json::Value =
            serde_json::from_str(captured.lock().await.as_str()).unwrap();
        assert_eq!(payload["model"], "models/gemini-3.1-pro-preview");
        assert!(payload["displayName"]
            .as_str()
            .unwrap()
            .starts_with("gen_batch_"));
        assert_eq!(
            payload["inputConfig"]["requests"]["requests"][0]["contents"][0]["parts"][0]["text"],
            "hi"
        );
        provider.stop().await;
    }

    #[tokio::test]
    async fn get_batch_job_extracts_inline_results() {
        let app = Router::new().route(
            "/operations/batch-1",
            get(|| async {
                ok_json(
                    r#"{
                        "name": "operations/batch-1",
                        "done": true,
                        "response": {
                            "embeddings": [{"values": [1.0, 2.0]}],
                            "candidates": [{"content": {"parts": [{"text": "res"}]}}]
                        }
                    }"#,
                )
            }),
        );
        let base_url = serve(app).await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "k".into(),
                rpm: 60000,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let status = provider.get_batch_job("operations/batch-1").await.unwrap();
        assert!(status.done);
        assert_eq!(status.embeddings, vec![vec![1.0, 2.0]]);
        assert_eq!(status.candidates[0].content.parts[0].text, "res");
        assert!(status.error.is_none());
        provider.stop().await;
    }

    #[tokio::test]
    async fn get_batch_job_reports_operation_error() {
        let app = Router::new().route(
            "/operations/boom",
            get(|| async {
                ok_json(
                    r#"{"name":"operations/boom","done":true,"error":{"code":13,"message":"internal"}}"#,
                )
            }),
        );
        let base_url = serve(app).await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "k".into(),
                rpm: 60000,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        let status = provider.get_batch_job("operations/boom").await.unwrap();
        assert_eq!(status.error.as_deref(), Some("operation error: internal"));
        provider.stop().await;
    }

    #[tokio::test]
    async fn no_keys_is_not_functional() {
        let provider = GeminiProvider::new(Vec::new(), None);
        assert!(!provider.is_functional().await);
        let err = provider.embed_text("x").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "AI client not initialized or no keys available"
        );
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_cancels_callers() {
        let (base_url, _) = embed_server().await;
        let provider = GeminiProvider::new_with_base_url(
            vec![KeyConfig {
                key: "k".into(),
                rpm: 60000,
                ..Default::default()
            }],
            None,
            &base_url,
        );
        provider.embed_text("x").await.unwrap();
        provider.stop().await;
        provider.stop().await;
        let err = provider.embed_text("y").await.unwrap_err();
        assert_eq!(err.to_string(), "context canceled");
    }

    // ---------- pure helpers ----------

    #[test]
    fn retry_delay_parsing() {
        let body = br#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"23s"}]}}"#;
        assert_eq!(parse_retry_delay(body), Some(Duration::from_secs(23)));
        let body = br#"{"error":{"details":[{"retryDelay":"1.5s"},{"retryDelay":"2s"}]}}"#;
        assert_eq!(parse_retry_delay(body), Some(Duration::from_millis(1500)));
        assert_eq!(parse_retry_delay(br#"{"error":{}}"#), None);
        assert_eq!(parse_retry_delay(b"not json"), None);
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_go_duration("23s"), Some(Duration::from_secs(23)));
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration("100ms"), Some(Duration::from_millis(100)));
        assert_eq!(parse_go_duration("2m30s"), Some(Duration::from_secs(150)));
        assert_eq!(parse_go_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_go_duration("500us"), Some(Duration::from_micros(500)));
        assert_eq!(parse_go_duration("bogus"), None);
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("-3s"), Some(Duration::ZERO));
    }

    #[test]
    fn short_key_helper_matches_reference() {
        assert_eq!(short_key("averylongapikey"), "ikey");
        assert_eq!(short_key("short"), "????");
    }
}
