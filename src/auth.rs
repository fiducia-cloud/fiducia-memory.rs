//! Internal-hop authentication for the shared brain's HTTP surface.
//!
//! `fiducia-memory` sits behind the platform edge / load-balancer exactly like
//! `fiducia-node`: the LB authenticates the customer and forwards the trusted
//! internal hop carrying two headers —
//!
//!   * `x-fiducia-internal-auth: <FIDUCIA_INTERNAL_SECRET>` proves the caller is
//!     an internal service. It is constant-time compared and **fail-CLOSED**: if
//!     no secret is configured the service refuses every `/v1` request unless the
//!     operator explicitly opts into insecure local dev
//!     (`FIDUCIA_ALLOW_INSECURE_INTERNAL=1`).
//!   * `x-fiducia-org-id: <uuid>` is the **authenticated tenant**. The service
//!     treats it as authoritative and rejects any request whose body/query
//!     `tenant_id` disagrees — so a caller can no longer read or write another
//!     tenant's memories or claims simply by naming it in the payload.
//!
//! This closes the two findings from the security audit: previously every
//! endpoint was unauthenticated and `tenant_id` was trusted from the request
//! body (including `POST /v1/claims/resolve`, the only path to authoritative
//! truth). Per-actor identity (operator-vs-service, who-may-resolve) remains a
//! platform-wide item tracked separately; this establishes the same
//! service-authentication + tenant-scoping boundary every other fiducia service
//! already enforces.

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{request::Parts, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::convert::Infallible;
use uuid::Uuid;

pub const INTERNAL_AUTH_HEADER: &str = "x-fiducia-internal-auth";
pub const ORG_ID_HEADER: &str = "x-fiducia-org-id";

/// Auth policy resolved once at startup from the environment.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// The shared internal secret. `None` disables service authentication (only
    /// legal together with `allow_insecure`).
    secret: Option<String>,
    /// Local-dev escape hatch: serve `/v1` with no secret configured.
    allow_insecure: bool,
}

impl AuthConfig {
    /// Resolve the policy from `FIDUCIA_INTERNAL_SECRET` /
    /// `FIDUCIA_ALLOW_INSECURE_INTERNAL`.
    pub fn from_env() -> Self {
        let secret = std::env::var("FIDUCIA_INTERNAL_SECRET")
            .ok()
            .filter(|s| !s.is_empty());
        let allow_insecure = std::env::var("FIDUCIA_ALLOW_INSECURE_INTERNAL").as_deref() == Ok("1");
        Self { secret, allow_insecure }
    }

    /// Construct an explicit policy (tests, or callers wiring config themselves).
    pub fn new(secret: Option<String>, allow_insecure: bool) -> Self {
        Self { secret: secret.filter(|s| !s.is_empty()), allow_insecure }
    }

    /// True when a secret is configured, i.e. service authentication is enforced
    /// and the authenticated tenant (`x-fiducia-org-id`) is required.
    pub fn enforced(&self) -> bool {
        self.secret.is_some()
    }

    /// True when the service is safe to serve `/v1`: either a secret is set, or
    /// the operator explicitly opted into insecure dev.
    pub fn is_configured(&self) -> bool {
        self.secret.is_some() || self.allow_insecure
    }
}

/// The authenticated tenant for a request, injected by [`require_internal_auth`].
/// `None` only in insecure-dev mode with no `x-fiducia-org-id` header, where the
/// handler falls back to the body's `tenant_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthTenant(pub Option<Uuid>);

impl<S> FromRequestParts<S> for AuthTenant
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<AuthTenant>()
            .copied()
            .unwrap_or(AuthTenant(None)))
    }
}

/// Constant-time byte comparison (short-circuits only on length, which is not
/// secret for a fixed-length shared token).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn reject(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// Axum middleware: authenticate the internal hop (fail-closed) and inject the
/// authenticated tenant. Applied to every `/v1` route; `/healthz` and `/readyz`
/// stay public for probes.
pub async fn require_internal_auth(
    State(config): State<AuthConfig>,
    mut req: Request,
    next: Next,
) -> Response {
    // 1. Service authentication.
    match &config.secret {
        Some(secret) => {
            let presented = req
                .headers()
                .get(INTERNAL_AUTH_HEADER)
                .map(|h| constant_time_eq(h.as_bytes(), secret.as_bytes()))
                .unwrap_or(false);
            if !presented {
                return reject(StatusCode::UNAUTHORIZED, "missing or invalid internal auth");
            }
        }
        None => {
            if !config.allow_insecure {
                // Fail-closed: refuse rather than silently serve unauthenticated.
                return reject(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "internal auth not configured",
                );
            }
        }
    }

    // 2. Authenticated tenant (LB-injected org id).
    let org = req
        .headers()
        .get(ORG_ID_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());
    if config.enforced() && org.is_none() {
        return reject(
            StatusCode::UNAUTHORIZED,
            "missing or invalid x-fiducia-org-id",
        );
    }
    req.extensions_mut().insert(AuthTenant(org));

    next.run(req).await
}

/// Validate the request's declared `tenant_id` against the authenticated tenant.
///
/// * enforced mode — the org header is authoritative; a body `tenant_id` that
///   disagrees is rejected `403`.
/// * insecure-dev mode with no org header — the body `tenant_id` is trusted.
///
/// Returns the effective tenant (always equal to `body_tenant` on success) or a
/// ready `403` response. Usable from both the binary and library handlers.
pub fn resolve_tenant(auth: AuthTenant, body_tenant: Uuid) -> Result<Uuid, Response> {
    match auth.0 {
        Some(org) if org == body_tenant => Ok(body_tenant),
        Some(_) => Err(reject(
            StatusCode::FORBIDDEN,
            "tenant_id does not match authenticated org",
        )),
        None => Ok(body_tenant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_identical() {
        assert!(constant_time_eq(b"s3cret", b"s3cret"));
        assert!(!constant_time_eq(b"s3cret", b"s3creT"));
        assert!(!constant_time_eq(b"s3cret", b"s3cre"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn enforced_mode_rejects_body_tenant_that_differs_from_org() {
        let org = Uuid::new_v4();
        let other = Uuid::new_v4();
        // Authenticated org matches the body: allowed.
        assert_eq!(resolve_tenant(AuthTenant(Some(org)), org).ok(), Some(org));
        // Authenticated org differs from the body tenant: rejected.
        assert!(resolve_tenant(AuthTenant(Some(org)), other).is_err());
    }

    #[test]
    fn insecure_mode_without_org_falls_back_to_body_tenant() {
        let body = Uuid::new_v4();
        assert_eq!(resolve_tenant(AuthTenant(None), body).ok(), Some(body));
    }

    #[test]
    fn config_flags() {
        assert!(AuthConfig::new(Some("x"), false).enforced());
        assert!(AuthConfig::new(Some("x"), false).is_configured());
        assert!(!AuthConfig::new(None, false).enforced());
        assert!(!AuthConfig::new(None, false).is_configured());
        assert!(AuthConfig::new(None, true).is_configured());
    }
}
