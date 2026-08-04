//! Durable-layer request/row types (adopted from codex `model.rs`).
//!
//! RECONCILE: `durable::model::Claim` is a provenance-bearing FACT row from the
//! `memory_claims` table (subject/predicate/object + embedding + temporal
//! supersession). It is deliberately DISTINCT from the epistemic ledger
//! [`crate::domain::Claim`] (a contestable assert→support→contest→resolve
//! assertion). The two model different things and are both kept, under clear
//! names, rather than collapsed.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const EMBEDDING_DIMENSIONS: usize = 1536;

#[derive(Debug, Clone, Serialize)]
pub struct Claim {
    pub claim_id: Uuid,
    pub tenant_id: Uuid,
    pub subject: String,
    pub predicate: String,
    pub object: serde_json::Value,
    pub source: serde_json::Value,
    pub confidence: f32,
    pub content: String,
    pub content_sha256: String,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub supersedes_claim_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppendClaim {
    pub tenant_id: Uuid,
    pub subject: String,
    pub predicate: String,
    pub object: serde_json::Value,
    pub source: serde_json::Value,
    pub confidence: f32,
    pub content: String,
    pub embedding: Vec<f32>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub supersedes_claim_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct SupersedeClaim {
    pub tenant_id: Uuid,
    pub replacement: AppendClaim,
}

#[derive(Debug, Deserialize)]
pub struct RecallRequest {
    pub tenant_id: Uuid,
    pub query: String,
    pub embedding: Vec<f32>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default = "default_semantic_weight")]
    pub semantic_weight: f32,
}

#[derive(Debug, Serialize)]
pub struct RecallHit {
    pub claim: Claim,
    pub lexical_score: f32,
    pub semantic_score: f32,
    pub score: f32,
}

fn default_limit() -> i64 {
    20
}
fn default_semantic_weight() -> f32 {
    0.7
}

impl AppendClaim {
    pub fn validate(&self) -> Result<String, &'static str> {
        if self.subject.trim().is_empty()
            || self.predicate.trim().is_empty()
            || self.content.trim().is_empty()
        {
            return Err("subject, predicate, and content must be non-empty");
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            return Err("confidence must be between 0 and 1");
        }
        if let (Some(valid_from), Some(valid_until)) = (&self.valid_from, &self.valid_until) {
            if valid_until <= valid_from {
                return Err("valid_until must be after valid_from");
            }
        }
        if self.embedding.len() != EMBEDDING_DIMENSIONS {
            return Err("embedding must contain exactly 1536 values");
        }
        crate::vector::pgvector_literal(&self.embedding)
    }
}

impl RecallRequest {
    pub fn validate(&self) -> Result<String, &'static str> {
        if self.query.trim().is_empty() {
            return Err("query must be non-empty");
        }
        if !(1..=100).contains(&self.limit) {
            return Err("limit must be between 1 and 100");
        }
        if !(0.0..=1.0).contains(&self.semantic_weight) {
            return Err("semantic_weight must be between 0 and 1");
        }
        if self.embedding.len() != EMBEDDING_DIMENSIONS {
            return Err("embedding must contain exactly 1536 values");
        }
        crate::vector::pgvector_literal(&self.embedding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use serde_json::json;

    fn valid_embedding() -> Vec<f32> {
        let mut embedding = vec![0.0; EMBEDDING_DIMENSIONS];
        embedding[0] = 1.0;
        embedding
    }

    fn append_claim() -> AppendClaim {
        AppendClaim {
            tenant_id: Uuid::nil(),
            subject: "invoice-42".into(),
            predicate: "payment_status".into(),
            object: json!({"status": "paid"}),
            source: json!({"service": "billing"}),
            confidence: 0.9,
            content: "Invoice 42 was paid".into(),
            embedding: valid_embedding(),
            valid_from: None,
            valid_until: None,
            supersedes_claim_id: None,
        }
    }

    fn recall_request() -> RecallRequest {
        RecallRequest {
            tenant_id: Uuid::nil(),
            query: "invoice payment status".into(),
            embedding: valid_embedding(),
            limit: 20,
            semantic_weight: 0.7,
        }
    }

    #[test]
    fn append_accepts_a_forward_validity_window() {
        let start = Utc
            .with_ymd_and_hms(2026, 8, 3, 12, 0, 0)
            .single()
            .expect("valid timestamp");
        let mut input = append_claim();
        input.valid_from = Some(start);
        input.valid_until = Some(start + Duration::seconds(1));

        assert!(input.validate().is_ok());
    }

    #[test]
    fn append_rejects_equal_or_backward_validity_windows() {
        let start = Utc
            .with_ymd_and_hms(2026, 8, 3, 12, 0, 0)
            .single()
            .expect("valid timestamp");

        for end in [start, start - Duration::seconds(1)] {
            let mut input = append_claim();
            input.valid_from = Some(start);
            input.valid_until = Some(end);
            assert_eq!(
                input.validate(),
                Err("valid_until must be after valid_from")
            );
        }
    }

    #[test]
    fn append_and_recall_reject_zero_magnitude_embeddings() {
        let mut append = append_claim();
        append.embedding = vec![0.0; EMBEDDING_DIMENSIONS];
        assert_eq!(
            append.validate(),
            Err("embedding must have non-zero magnitude")
        );

        let mut recall = recall_request();
        recall.embedding = vec![-0.0; EMBEDDING_DIMENSIONS];
        assert_eq!(
            recall.validate(),
            Err("embedding must have non-zero magnitude")
        );
    }

    #[test]
    fn append_and_recall_reject_non_finite_embeddings() {
        let mut append = append_claim();
        append.embedding[EMBEDDING_DIMENSIONS - 1] = f32::NAN;
        assert_eq!(append.validate(), Err("embedding values must be finite"));

        let mut recall = recall_request();
        recall.embedding[EMBEDDING_DIMENSIONS - 1] = f32::INFINITY;
        assert_eq!(recall.validate(), Err("embedding values must be finite"));
    }

    #[test]
    fn dimension_and_recall_bounds_fail_closed() {
        let mut append = append_claim();
        append.embedding.pop();
        assert_eq!(
            append.validate(),
            Err("embedding must contain exactly 1536 values")
        );

        let mut recall = recall_request();
        recall.limit = 0;
        assert_eq!(recall.validate(), Err("limit must be between 1 and 100"));

        recall.limit = 20;
        recall.semantic_weight = f32::NAN;
        assert_eq!(
            recall.validate(),
            Err("semantic_weight must be between 0 and 1")
        );
    }
}
