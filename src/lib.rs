//! The Fiducia shared brain: durable, contestable, provenance-tracked agent
//! memory with hybrid recall.
//!
//! This service is deliberately SEPARATE from `fiducia-node`. The node owns
//! authoritative coordination (who owns a task, whether a lease is valid); this
//! owns cognition (what agents have learned). The dividing invariant runs
//! through every module:
//!
//! > **Vectors can suggest relevant knowledge. They must never determine
//! > authoritative state.**
//!
//! Concretely: authoritative facts live in the [`claims`] ledger and only reach
//! `Accepted` through an explicit authorized resolution; [`recall`] applies
//! authorization and validity as *hard filters before ranking*, so embedding
//! similarity can rank candidates but never include an unauthorized or invalid
//! one.

pub mod api;
pub mod claims;
pub mod db;
pub mod domain;
pub mod memory;
pub mod model;
pub mod postgres;
pub mod recall;
pub mod store;

pub use claims::{Assertion, ClaimError, ClaimLedger};
pub use domain::*;
pub use memory::{trust_from, InMemoryStore, MemoryError, MemoryStore};
pub use postgres::{PostgresMemory, ScoredRow};
pub use recall::{
    estimate_tokens, recall, recall_with_weights, Candidate, ContextPack, RecallQuery,
    RecallWeights, RetrievedMemory,
};

use axum::{
    routing::{get, post},
    Router,
};

/// Compatibility router for the original append/supersede/recall service API.
///
/// The canonical binary exposes the richer contestable-memory API. Keeping this
/// router public preserves existing clients while they migrate deliberately.
pub fn router(store: store::MemoryStore) -> Router {
    Router::new()
        .route("/healthz", get(api::health))
        .route("/readyz", get(api::ready))
        .route("/v1/claims", post(api::append_claim))
        .route(
            "/v1/claims/{claim_id}/supersede",
            post(api::supersede_claim),
        )
        .route("/v1/recall", post(api::recall))
        .with_state(store)
}
