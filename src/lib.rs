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

pub mod claims;
pub mod db;
pub mod domain;
pub mod memory;
pub mod postgres;
pub mod recall;

pub use claims::{Assertion, ClaimError, ClaimLedger};
pub use domain::*;
pub use memory::{trust_from, InMemoryStore, MemoryError, MemoryStore};
pub use postgres::{PostgresMemory, ScoredRow};
pub use recall::{
    estimate_tokens, recall, recall_with_weights, Candidate, ContextPack, RecallQuery,
    RecallWeights, RetrievedMemory,
};
