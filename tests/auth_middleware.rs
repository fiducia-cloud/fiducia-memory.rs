//! End-to-end tests for the internal-auth middleware that guards every `/v1`
//! route. These drive the REAL [`require_internal_auth`] + [`resolve_tenant`]
//! through the axum stack (no database — auth runs before any handler touches
//! Postgres), proving the two audit findings are closed: unauthenticated access
//! and caller-supplied `tenant_id`.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::from_fn_with_state,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use fiducia_memory::auth::{
    require_internal_auth, resolve_tenant, AuthConfig, AuthTenant, INTERNAL_AUTH_HEADER,
    ORG_ID_HEADER,
};
use serde::Deserialize;
use serde_json::json;
use tower::ServiceExt; // for `oneshot`
use uuid::Uuid;

#[derive(Deserialize)]
struct TenantBody {
    tenant_id: Uuid,
}

/// A stand-in protected handler applying the same tenant guard the real handlers
/// use; echoes the effective tenant on success.
async fn echo(auth: AuthTenant, Json(body): Json<TenantBody>) -> Response {
    match resolve_tenant(auth, body.tenant_id) {
        Ok(tenant) => (StatusCode::OK, tenant.to_string()).into_response(),
        Err(resp) => resp,
    }
}

fn app(config: AuthConfig) -> Router {
    Router::new()
        .route("/v1/echo", post(echo))
        .route_layer(from_fn_with_state(config, require_internal_auth))
}

fn post_echo(secret: Option<&str>, org: Option<Uuid>, body_tenant: Uuid) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/echo")
        .header("content-type", "application/json");
    if let Some(secret) = secret {
        builder = builder.header(INTERNAL_AUTH_HEADER, secret);
    }
    if let Some(org) = org {
        builder = builder.header(ORG_ID_HEADER, org.to_string());
    }
    builder
        .body(Body::from(
            serde_json::to_vec(&json!({ "tenant_id": body_tenant })).unwrap(),
        ))
        .unwrap()
}

fn enforced() -> AuthConfig {
    AuthConfig::new(Some("topsecret".into()), false)
}

#[tokio::test]
async fn enforced_rejects_missing_or_wrong_secret() {
    let t = Uuid::new_v4();
    // No secret header.
    let resp = app(enforced())
        .oneshot(post_echo(None, Some(t), t))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // Wrong secret.
    let resp = app(enforced())
        .oneshot(post_echo(Some("nope"), Some(t), t))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn enforced_requires_org_header() {
    let t = Uuid::new_v4();
    let resp = app(enforced())
        .oneshot(post_echo(Some("topsecret"), None, t))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn enforced_rejects_tenant_mismatch() {
    let org = Uuid::new_v4();
    let other = Uuid::new_v4();
    // Authenticated as `org` but the body names another tenant → 403.
    let resp = app(enforced())
        .oneshot(post_echo(Some("topsecret"), Some(org), other))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn enforced_allows_matching_org_and_tenant() {
    let org = Uuid::new_v4();
    let resp = app(enforced())
        .oneshot(post_echo(Some("topsecret"), Some(org), org))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&body), org.to_string());
}

#[tokio::test]
async fn unconfigured_fails_closed() {
    let t = Uuid::new_v4();
    // Neither a secret nor the insecure flag: refuse everything.
    let resp = app(AuthConfig::new(None, false))
        .oneshot(post_echo(Some("anything"), Some(t), t))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn insecure_dev_allows_without_headers() {
    let t = Uuid::new_v4();
    // Explicit local-dev opt-in: no headers, body tenant trusted.
    let resp = app(AuthConfig::new(None, true))
        .oneshot(post_echo(None, None, t))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&body), t.to_string());
}
