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
//! # Two layers, one crate
//!
//! This crate is the SEMANTIC MERGE of two independent implementations:
//!
//! * **The durable storage floor** ([`durable`]) — a real `PgPool` store over an
//!   append-only `memory_claims` table (generated `tsvector` search document,
//!   HNSW cosine index, `content_sha256` active-dedup, temporal supersession).
//!   It is the durable system of record and the candidate-GENERATION engine for
//!   recall (`sql/recall.sql`).
//! * **The epistemic layer** ([`claims`], [`domain`], [`memory`], [`recall`],
//!   [`postgres`]) — the contestable claim ledger
//!   (assert→support→contest→resolve→consensus; a claim is authoritative only
//!   when resolved-`Accepted`, the anti-poisoning invariant), the five memory
//!   types + provenance trust, and the explainable hybrid recall FUSION
//!   (authorization/validity HARD filters BEFORE ranking, then
//!   lexical+semantic+trust+freshness, contradiction down-rank, token-bounded
//!   pack).
//!
//! [`fusion`] is the seam: durable SQL recall generates candidates; the
//! epistemic fusion ranks, filters, and explains them.
//!
//! Concretely: authoritative facts live in the [`claims`] ledger and only reach
//! `Accepted` through an explicit authorized resolution; [`recall`] applies
//! authorization and validity as *hard filters before ranking*, so embedding
//! similarity can rank candidates but never include an unauthorized or invalid
//! one.

pub mod claims;
pub mod db;
pub mod domain;
pub mod durable;
pub mod fusion;
pub mod memory;
pub mod postgres;
pub mod recall;

pub use claims::{Assertion, ClaimError, ClaimLedger};
pub use domain::*;
pub use fusion::{candidate_from_hit, candidates_from_hits};
pub use memory::{trust_from, InMemoryStore, MemoryError, MemoryStore};
pub use postgres::{PostgresMemory, ScoredRow};
pub use recall::{
    estimate_tokens, recall, recall_with_weights, Candidate, ContextPack, RecallQuery,
    RecallWeights, RetrievedMemory,
};
