//! The seam between the two implementations' recall paths.
//!
//! The durable store ([`crate::durable::store::MemoryStore::recall`], backed by
//! `sql/recall.sql`) is the **candidate-GENERATION** stage: it does the heavy,
//! index-accelerated work in Postgres — tenant + temporal filtering, full-text
//! `ts_rank_cd` lexical scoring, and HNSW cosine semantic scoring — and returns
//! a bounded set of [`RecallHit`]s.
//!
//! This module adapts those hits into [`Candidate`]s so my explainable hybrid
//! [`crate::recall`] fusion runs on top: authorization/validity HARD filters →
//! lexical + semantic + trust + freshness ranking → contradiction down-rank →
//! dedupe → token-bounded, explained pack.
//!
//! ```text
//! durable::store::recall (sql/recall.sql)   ← candidate generation (Postgres)
//!        │  Vec<RecallHit>
//!        ▼  candidates_from_hits
//! recall::recall_with_weights               ← fusion / filter / explain (pure)
//!        │
//!        ▼  ContextPack
//! ```
//!
//! RECONCILE: a durable `memory_claims` row (`durable::model::Claim`) is a
//! provenance FACT, not a `Memory`. We project each fact into a `Memory` of
//! type `Semantic` (a durable belief) so the fusion — which is defined over
//! `Memory` candidates — can rank it. Fields with no durable analogue take
//! honest defaults: `trust_score` = the claim's stored `confidence`,
//! `importance` = 0.5, and `contradicted_by_accepted_claim` = false (the
//! durable store carries no accepted-ledger-claim contradiction signal;
//! contradiction down-ranking is exercised by the epistemic path).

use crate::domain::{Memory, MemoryType, Provenance};
use crate::durable::model::RecallHit;
use crate::recall::Candidate;
use std::collections::BTreeMap;

/// Derive a coarse provenance from a durable claim's free-form `source` JSON.
/// If `source` carries a `"derivation"` string we keep it (so
/// [`Provenance::base_trust`] can weigh it); otherwise provenance is empty and
/// trust falls back to the claim's own confidence.
fn provenance_from_source(source: &serde_json::Value) -> Provenance {
    let derivation = source
        .get("derivation")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    Provenance {
        derivation,
        ..Default::default()
    }
}

/// Project one durable [`RecallHit`] into a fusion [`Candidate`].
pub fn candidate_from_hit(hit: RecallHit) -> Candidate {
    let claim = hit.claim;
    let mut metadata = BTreeMap::new();
    metadata.insert("subject".to_string(), claim.subject.clone());
    metadata.insert("predicate".to_string(), claim.predicate.clone());

    let memory = Memory {
        id: claim.claim_id,
        tenant_id: claim.tenant_id,
        // memory_claims has no namespace column; durable facts live in the
        // default namespace. A recall with `namespace: None` matches all;
        // `namespace: Some("default")` matches these.
        namespace: "default".to_string(),
        memory_type: MemoryType::Semantic,
        content: claim.content,
        metadata,
        provenance: provenance_from_source(&claim.source),
        // Trust follows the durable claim's stored confidence, clamped.
        trust_score: claim.confidence.clamp(0.0, 1.0),
        importance: 0.5,
        valid_from: claim.valid_from,
        valid_until: claim.valid_until,
        superseded_by: None,
    };

    Candidate {
        memory,
        // ts_rank_cd is unbounded above; clamp into the fusion's [0,1] contract.
        lexical_score: hit.lexical_score.clamp(0.0, 1.0),
        semantic_score: hit.semantic_score.clamp(0.0, 1.0),
        contradicted_by_accepted_claim: false,
    }
}

/// Adapt a whole batch of durable hits into fusion candidates.
pub fn candidates_from_hits(hits: Vec<RecallHit>) -> Vec<Candidate> {
    hits.into_iter().map(candidate_from_hit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::model::Claim;
    use chrono::Utc;
    use uuid::Uuid;

    fn hit(tenant: Uuid, content: &str, confidence: f32, lexical: f32, semantic: f32) -> RecallHit {
        RecallHit {
            claim: Claim {
                claim_id: Uuid::new_v4(),
                tenant_id: tenant,
                subject: "customer:219".into(),
                predicate: "refund_eligible".into(),
                object: serde_json::json!(true),
                source: serde_json::json!({ "derivation": "resolved_claim" }),
                confidence,
                content: content.into(),
                content_sha256: "deadbeef".into(),
                valid_from: Utc::now() - chrono::Duration::minutes(1),
                valid_until: None,
                supersedes_claim_id: None,
                created_at: Utc::now(),
            },
            lexical_score: lexical,
            semantic_score: semantic,
            score: 0.0,
        }
    }

    #[test]
    fn durable_hits_project_into_fusable_candidates() {
        let tenant = Uuid::new_v4();
        let candidates = candidates_from_hits(vec![hit(
            tenant,
            "refund via original method",
            0.9,
            0.4,
            0.8,
        )]);
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.memory.tenant_id, tenant);
        assert_eq!(c.memory.memory_type, MemoryType::Semantic);
        assert_eq!(c.memory.trust_score, 0.9);
        assert_eq!(c.memory.metadata.get("subject").unwrap(), "customer:219");
        assert!(c.memory.is_live(Utc::now()));
    }

    #[test]
    fn durable_candidates_feed_the_fusion_and_are_explained() {
        use crate::recall::{recall, RecallQuery};
        let tenant = Uuid::new_v4();
        let hits = vec![
            hit(
                tenant,
                "resolved: refund via original method",
                0.95,
                0.9,
                0.9,
            ),
            hit(tenant, "weak guess about refunds", 0.3, 0.2, 0.2),
        ];
        let candidates = candidates_from_hits(hits);
        let query = RecallQuery {
            tenant_id: tenant,
            query: "how are refunds handled".into(),
            namespace: None,
            memory_types: vec![],
            required_permissions: vec![],
            max_tokens: 10_000,
            prefer_recent: false,
            now: Utc::now(),
        };
        let pack = recall(&query, candidates);
        assert_eq!(pack.memories.len(), 2, "both live durable facts survive");
        assert!(
            pack.memories[0].combined_score >= pack.memories[1].combined_score,
            "fusion ranks the stronger durable fact first"
        );
        assert!(pack.memories[0].reason.starts_with("top signal"));
    }

    #[test]
    fn fusion_still_rejects_a_cross_tenant_durable_hit() {
        use crate::recall::{recall, RecallQuery};
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let candidates =
            candidates_from_hits(vec![hit(theirs, "another tenant fact", 0.99, 1.0, 1.0)]);
        let query = RecallQuery {
            tenant_id: mine,
            query: "x".into(),
            namespace: None,
            memory_types: vec![],
            required_permissions: vec![],
            max_tokens: 10_000,
            prefer_recent: false,
            now: Utc::now(),
        };
        // Even a maximal-similarity durable hit from another tenant is dropped by
        // the fusion's hard authorization filter — defence in depth over the SQL
        // tenant filter.
        assert!(recall(&query, candidates).memories.is_empty());
    }
}
