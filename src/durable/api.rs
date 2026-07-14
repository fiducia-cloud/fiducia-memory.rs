//! Durable-layer axum handlers (adopted from codex `api.rs`).
//!
//! Endpoints: `POST /v1/claims` (append), `POST /v1/claims/{id}/supersede`
//! (atomic supersede), `POST /v1/recall` (raw hybrid recall). These extract the
//! durable [`MemoryStore`] from the unified application state via `FromRef`.

use crate::auth::{resolve_tenant, AuthTenant};
use crate::durable::{
    model::{AppendClaim, RecallRequest, SupersedeClaim},
    store::{MemoryStore, StoreError},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use uuid::Uuid;

pub async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn ready(State(store): State<MemoryStore>) -> Response {
    match store.ping().await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "readiness database probe failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"storage_unavailable"})),
            )
                .into_response()
        }
    }
}

pub async fn append_claim(
    State(store): State<MemoryStore>,
    auth: AuthTenant,
    Json(input): Json<AppendClaim>,
) -> Response {
    if let Err(resp) = resolve_tenant(auth, input.tenant_id) {
        return resp;
    }
    let embedding = match input.validate() {
        Ok(value) => value,
        Err(error) => return bad_request(error),
    };
    match store.append(&input, embedding).await {
        Ok(claim) => (StatusCode::CREATED, Json(claim)).into_response(),
        Err(error) => internal(error),
    }
}

pub async fn supersede_claim(
    State(store): State<MemoryStore>,
    auth: AuthTenant,
    Path(claim_id): Path<Uuid>,
    Json(input): Json<SupersedeClaim>,
) -> Response {
    if let Err(resp) = resolve_tenant(auth, input.tenant_id) {
        return resp;
    }
    if input.replacement.tenant_id != input.tenant_id {
        return bad_request("replacement tenant_id must match tenant_id");
    }
    let embedding = match input.replacement.validate() {
        Ok(value) => value,
        Err(error) => return bad_request(error),
    };
    match store
        .supersede(claim_id, input.tenant_id, &input.replacement, embedding)
        .await
    {
        Ok(claim) => (StatusCode::CREATED, Json(claim)).into_response(),
        Err(StoreError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"claim_not_found"})),
        )
            .into_response(),
        Err(error) => internal(error),
    }
}

pub async fn recall(
    State(store): State<MemoryStore>,
    auth: AuthTenant,
    Json(input): Json<RecallRequest>,
) -> Response {
    if let Err(resp) = resolve_tenant(auth, input.tenant_id) {
        return resp;
    }
    let embedding = match input.validate() {
        Ok(value) => value,
        Err(error) => return bad_request(error),
    };
    match store.recall(&input, embedding).await {
        Ok(hits) => Json(json!({ "hits": hits })).into_response(),
        Err(error) => internal(error),
    }
}

fn bad_request(detail: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"invalid_request","detail":detail})),
    )
        .into_response()
}

fn internal(error: StoreError) -> Response {
    tracing::error!(%error, "memory operation failed");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error":"storage_unavailable"})),
    )
        .into_response()
}
