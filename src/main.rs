//! `fiducia-memory` HTTP service — the shared brain's API surface.
//!
//! Thin, honest wiring over the tested library core:
//!   * memories persist to Postgres with a trust score derived from provenance;
//!   * the contestable claim ledger enforces its lifecycle invariants in-process
//!     (`assert → support/contest → resolve`) and mirrors every mutation to
//!     Postgres durably;
//!   * only an authorized `resolve` yields an authoritative consensus value.
//!
//! Run `fiducia-memory --migrate` once to apply the schema (needs pgvector),
//! then `fiducia-memory` to serve. `DATABASE_URL` selects the customer's own
//! Postgres or the Fiducia-hosted default; `FIDUCIA_MEMORY_BIND` overrides the
//! listen address (default `127.0.0.1:8100`).
//!
//! Scope: a single service instance is the authoritative ledger while running;
//! Postgres is the durable mirror + audit log + external query surface.
//! Hydrating the in-process ledger from Postgres on boot (for restart-durable
//! mutation and horizontal scale-out) is the next step — see the README.

use std::{collections::BTreeMap, sync::Arc, sync::Mutex};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use fiducia_memory::{
    claims::{Assertion, ClaimError, ClaimLedger},
    domain::{Memory, MemoryType, Provenance},
    memory::trust_from,
    postgres::PostgresMemory,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    pg: PostgresMemory,
    // The in-process authoritative ledger. Held only across synchronous ledger
    // ops; the resulting claim is cloned out before any `.await`, so the guard is
    // never held across a suspension point.
    ledger: Arc<Mutex<ClaimLedger>>,
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
    let pg = PostgresMemory::connect(&database_url).await?;

    if std::env::args().any(|a| a == "--migrate") {
        pg.migrate().await?;
        tracing::info!("schema applied");
        println!("fiducia-memory: schema applied");
        return Ok(());
    }

    pg.ready().await?;
    let state = AppState {
        pg,
        ledger: Arc::new(Mutex::new(ClaimLedger::new())),
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/v1/memories", post(create_memory))
        .route("/v1/claims/assert", post(assert_claim))
        .route("/v1/claims/support", post(support_claim))
        .route("/v1/claims/contest", post(contest_claim))
        .route("/v1/claims/resolve", post(resolve_claim))
        .route("/v1/claims/consensus", get(consensus))
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
