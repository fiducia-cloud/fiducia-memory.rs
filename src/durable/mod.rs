//! The DURABLE storage floor, adopted wholesale from the codex implementation.
//!
//! This layer is the real Postgres system of record for provenance-bearing
//! *facts*: an append-only `memory_claims` table with a generated `tsvector`
//! search document, an HNSW cosine index over 1536-d embeddings,
//! `content_sha256` active-dedup, and temporal supersession via
//! `valid_until`/`supersedes_claim_id`. It stores immutable claims, tracks
//! supersession without destroying history, and combines full-text + cosine
//! similarity for hybrid recall.
//!
//! It lives UNDER its own namespace so its types do not clash with the
//! epistemic layer at the crate root:
//!
//! * [`model`] — the durable request/row types (`durable::model::Claim` is a
//!   provenance FACT row, distinct from the ledger [`crate::domain::Claim`]).
//! * [`store`] — the `PgPool`-backed store: append / atomic supersede / hybrid
//!   recall (via `sql/recall.sql`) / migrate / ping.
//! * [`api`] — the axum handlers for the codex endpoint set
//!   (`POST /v1/claims`, `POST /v1/claims/{id}/supersede`, `POST /v1/recall`).
//!
//! The unified router (see `crate::router` / `main.rs`) mounts these handlers
//! alongside the epistemic-layer handlers over one shared `PgPool`.

pub mod api;
pub mod model;
pub mod store;
