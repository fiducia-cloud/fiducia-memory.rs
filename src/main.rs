//! `fiducia-memory` HTTP service — the shared brain's UNIFIED API surface.
//!
//! This binary wires two merged implementations over ONE shared `PgPool`:
//!
//!   * **Durable storage floor** (`durable::*`): `POST /v1/claims` (append),
//!     `POST /v1/claims/{id}/supersede` (atomic supersede), `POST /v1/recall`
//!     (raw hybrid recall over `memory_claims`).
//!   * **Epistemic layer** (mine): `POST /v1/memories` (trust from provenance),
//!     the contestable claim ledger `POST /v1/claims/{assert,support,contest,
//!     resolve}` + `GET /v1/claims/consensus`, and `POST /v1/recall/fused` —
//!     the durable SQL recall feeding my explainable fusion (see
//!     [`fiducia_memory::fusion`]).
//!
//! On boot the service runs `sqlx::migrate!` over the unified `migrations/`
//! directory, applying BOTH schemas (durable `memory_claims` + the epistemic
//! memories/ledger/edges/recall-log/RLS schema). `fiducia-memory --migrate`
//! runs migrations and exits. `DATABASE_URL` selects the customer's own
//! Postgres or the Fiducia-hosted default; `FIDUCIA_MEMORY_BIND` overrides the
//! listen address (default `127.0.0.1:8100`).
//!
//! Scope: a single service instance is the authoritative in-process ledger while
//! running; Postgres is the durable mirror + audit log + external query surface.

use std::{collections::BTreeMap, sync::Arc, sync::Mutex, time::Duration};

use axum::{
    extract::{FromRef, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use fiducia_memory::{
    claims::{Assertion, ClaimError, ClaimLedger},
    domain::{Memory, MemoryType, Provenance},
    durable::{self, store::MemoryStore},
    fusion::candidates_from_hits,
    memory::trust_from,
    postgres::PostgresMemory,
    recall::{recall, RecallQuery},
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower_http::{limit::RequestBodyLimitLayer, timeout::TimeoutLayer, trace::TraceLayer};
use uuid::Uuid;

/// Unified application state over a single shared `PgPool`.
#[derive(Clone)]
struct AppState {
    /// Epistemic-layer Postgres access (memories, ledger mirror, embeddings).
    pg: PostgresMemory,
    /// Durable storage floor (append-only `memory_claims`, hybrid recall).
    durable: MemoryStore,
    /// The in-process authoritative ledger. Held only across synchronous ledger
    /// ops; the resulting claim is cloned out before any `.await`, so the guard
    /// is never held across a suspension point.
    ledger: Arc<Mutex<ClaimLedger>>,
}

// Let codex's `State<MemoryStore>` handlers extract the durable store from the
// unified state.
impl FromRef<AppState> for MemoryStore {
    fn from_ref(state: &AppState) -> Self {
        state.durable.clone()
    }
}

impl FromRef<AppState> for PostgresMemory {
    fn from_ref(state: &AppState) -> Self {
        state.pg.clone()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fiducia_memory=info,tower_http=info".into()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL must be set (the customer's Postgres or the Fiducia default)")?;

    // ONE shared pool feeds both layers.
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await?;

    let durable = MemoryStore::new(pool.clone());
    let pg = PostgresMemory::from_pool(pool);

    // Apply ALL migrations (durable `memory_claims` + epistemic schema).
    durable.migrate().await?;

    if std::env::args().any(|a| a == "--migrate") {
        tracing::info!("schema applied");
        println!("fiducia-memory: schema applied");
        return Ok(());
    }

    durable.ping().await?;
    let state = AppState {
        pg,
        durable,
        ledger: Arc::new(Mutex::new(ClaimLedger::new())),
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        // ---- durable storage floor (codex) ----
        .route("/v1/claims", post(durable::api::append_claim))
        .route(
            "/v1/claims/{claim_id}/supersede",
            post(durable::api::supersede_claim),
        )
        .route("/v1/recall", post(durable::api::recall))
        // ---- epistemic layer (mine) ----
        .route("/v1/memories", post(create_memory))
        .route("/v1/recall/fused", post(fused_recall))
        .route("/v1/claims/assert", post(assert_claim))
        .route("/v1/claims/support", post(support_claim))
        .route("/v1/claims/contest", post(contest_claim))
        .route("/v1/claims/resolve", post(resolve_claim))
        .route("/v1/claims/consensus", get(consensus))
        .layer(RequestBodyLimitLayer::new(2 * 1024 * 1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(10),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let bind = std::env::var("FIDUCIA_MEMORY_BIND").unwrap_or_else(|_| "127.0.0.1:8100".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "fiducia-memory listening");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---- error mapping ---------------------------------------------------------

/// Uniform API error → HTTP status. Claim-lifecycle violations are client
/// errors (4xx); storage failures are 5xx.
enum ApiError {
    Claim(ClaimError),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Db(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Claim(ClaimError::NotFound) => {
                (StatusCode::NOT_FOUND, "claim not found".to_string())
            }
            ApiError::Claim(e @ ClaimError::Terminal(_)) => (StatusCode::CONFLICT, e.to_string()),
            ApiError::Db(e) => {
                tracing::error!(error = %e, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage backend error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

// ---- handlers --------------------------------------------------------------

async fn readyz(State(state): State<AppState>) -> Response {
    match state.pg.ready().await {
        Ok(()) => (StatusCode::OK, "ready").into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "not ready");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
        }
    }
}

fn default_namespace() -> String {
    "default".to_string()
}

fn default_confidence() -> f32 {
    0.5
}

#[derive(Deserialize)]
struct CreateMemory {
    tenant_id: Uuid,
    #[serde(default = "default_namespace")]
    namespace: String,
    memory_type: MemoryType,
    content: String,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    provenance: Provenance,
    importance: Option<f32>,
    /// Override the derived trust; normally left unset so trust follows provenance.
    trust_score: Option<f32>,
    valid_until: Option<DateTime<Utc>>,
}

async fn create_memory(
    State(state): State<AppState>,
    Json(body): Json<CreateMemory>,
) -> Result<Response, ApiError> {
    let trust = body
        .trust_score
        .unwrap_or_else(|| trust_from(&body.provenance, 0, 0))
        .clamp(0.0, 1.0);
    let memory = Memory {
        id: Uuid::new_v4(),
        tenant_id: body.tenant_id,
        namespace: body.namespace,
        memory_type: body.memory_type,
        content: body.content,
        metadata: body.metadata,
        provenance: body.provenance,
        trust_score: trust,
        importance: body.importance.unwrap_or(0.5).clamp(0.0, 1.0),
        valid_from: Utc::now(),
        valid_until: body.valid_until,
        superseded_by: None,
    };
    state.pg.insert_memory(&memory).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": memory.id, "trust_score": memory.trust_score })),
    )
        .into_response())
}

// ---- fused recall: durable candidate generation → epistemic fusion ---------

fn default_max_tokens() -> usize {
    4000
}

/// Body for `POST /v1/recall/fused`: the durable [`durable::model::RecallRequest`]
/// (tenant, query, embedding, limit, semantic_weight) plus the epistemic-fusion
/// knobs (namespace/type/permission filters, token budget, recency preference).
#[derive(Deserialize)]
struct FusedRecallBody {
    #[serde(flatten)]
    durable: durable::model::RecallRequest,
    namespace: Option<String>,
    #[serde(default)]
    memory_types: Vec<MemoryType>,
    #[serde(default)]
    required_permissions: Vec<String>,
    #[serde(default = "default_max_tokens")]
    max_tokens: usize,
    #[serde(default)]
    prefer_recent: bool,
}

/// Durable SQL recall (`sql/recall.sql`, HNSW + full-text) generates candidates;
/// the epistemic [`recall`] fusion then applies authorization/validity HARD
/// filters, ranks by lexical+semantic+trust+freshness, down-ranks contradicted
/// memories, dedupes, and returns an explained, token-bounded pack.
async fn fused_recall(
    State(state): State<AppState>,
    Json(body): Json<FusedRecallBody>,
) -> Response {
    let embedding = match body.durable.validate() {
        Ok(v) => v,
        Err(detail) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request", "detail": detail })),
            )
                .into_response()
        }
    };

    // 1. Candidate GENERATION in Postgres (durable store).
    let hits = match state.durable.recall(&body.durable, embedding).await {
        Ok(hits) => hits,
        Err(error) => {
            tracing::error!(%error, "durable recall failed");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "storage_unavailable" })),
            )
                .into_response();
        }
    };

    // 2. Fusion / filter / explain (pure).
    let candidates = candidates_from_hits(hits);
    let query = RecallQuery {
        tenant_id: body.durable.tenant_id,
        query: body.durable.query.clone(),
        namespace: body.namespace,
        memory_types: body.memory_types,
        required_permissions: body.required_permissions,
        max_tokens: body.max_tokens,
        prefer_recent: body.prefer_recent,
        now: Utc::now(),
    };
    let pack = recall(&query, candidates);
    Json(pack).into_response()
}

// ---- contestable claim ledger (mine) ---------------------------------------

#[derive(Deserialize)]
struct AssertBody {
    tenant_id: Uuid,
    #[serde(default = "default_namespace")]
    namespace: String,
    subject: String,
    predicate: String,
    value: Value,
    #[serde(default = "default_confidence")]
    confidence: f32,
    author: String,
    #[serde(default)]
    evidence: Vec<String>,
}

async fn assert_claim(
    State(state): State<AppState>,
    Json(body): Json<AssertBody>,
) -> Result<Response, ApiError> {
    let claim = {
        let mut ledger = state.ledger.lock().expect("ledger lock");
        ledger
            .assert(Assertion {
                tenant_id: body.tenant_id,
                namespace: body.namespace,
                subject: body.subject,
                predicate: body.predicate,
                value: body.value,
                confidence: body.confidence,
                author: body.author,
                evidence: body.evidence,
            })
            .cloned()
    }
    .map_err(ApiError::Claim)?;
    state.pg.upsert_claim(&claim).await?;
    Ok(Json(claim).into_response())
}

#[derive(Deserialize)]
struct SupportBody {
    tenant_id: Uuid,
    #[serde(default = "default_namespace")]
    namespace: String,
    subject: String,
    predicate: String,
    agent: String,
}

async fn support_claim(
    State(state): State<AppState>,
    Json(body): Json<SupportBody>,
) -> Result<Response, ApiError> {
    let claim = {
        let mut ledger = state.ledger.lock().expect("ledger lock");
        ledger
            .support(
                body.tenant_id,
                &body.namespace,
                &body.subject,
                &body.predicate,
                &body.agent,
            )
            .cloned()
    }
    .map_err(ApiError::Claim)?;
    state.pg.upsert_claim(&claim).await?;
    Ok(Json(claim).into_response())
}

#[derive(Deserialize)]
struct ContestBody {
    tenant_id: Uuid,
    #[serde(default = "default_namespace")]
    namespace: String,
    subject: String,
    predicate: String,
    agent: String,
    #[serde(default)]
    reason: String,
}

async fn contest_claim(
    State(state): State<AppState>,
    Json(body): Json<ContestBody>,
) -> Result<Response, ApiError> {
    let claim = {
        let mut ledger = state.ledger.lock().expect("ledger lock");
        ledger
            .contest(
                body.tenant_id,
                &body.namespace,
                &body.subject,
                &body.predicate,
                &body.agent,
                &body.reason,
            )
            .cloned()
    }
    .map_err(ApiError::Claim)?;
    state.pg.upsert_claim(&claim).await?;
    Ok(Json(claim).into_response())
}

#[derive(Deserialize)]
struct ResolveBody {
    tenant_id: Uuid,
    #[serde(default = "default_namespace")]
    namespace: String,
    subject: String,
    predicate: String,
    accepted: bool,
    resolver: String,
}

async fn resolve_claim(
    State(state): State<AppState>,
    Json(body): Json<ResolveBody>,
) -> Result<Response, ApiError> {
    let claim = {
        let mut ledger = state.ledger.lock().expect("ledger lock");
        ledger
            .resolve(
                body.tenant_id,
                &body.namespace,
                &body.subject,
                &body.predicate,
                body.accepted,
                &body.resolver,
            )
            .cloned()
    }
    .map_err(ApiError::Claim)?;
    state.pg.upsert_claim(&claim).await?;
    Ok(Json(claim).into_response())
}

#[derive(Deserialize)]
struct ConsensusParams {
    tenant_id: Uuid,
    #[serde(default = "default_namespace")]
    namespace: String,
    subject: String,
    predicate: String,
}

/// The authoritative value for a subject/predicate, or `null` if no claim has
/// been accepted. Reads the running instance's ledger — the source of truth for
/// what has actually been resolved this session.
async fn consensus(
    State(state): State<AppState>,
    Query(params): Query<ConsensusParams>,
) -> Response {
    let value = {
        let ledger = state.ledger.lock().expect("ledger lock");
        ledger
            .consensus(
                params.tenant_id,
                &params.namespace,
                &params.subject,
                &params.predicate,
            )
            .cloned()
    };
    Json(json!({
        "subject": params.subject,
        "predicate": params.predicate,
        "authoritative_value": value,
    }))
    .into_response()
}
