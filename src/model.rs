use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const EMBEDDING_DIMENSIONS: usize = 1536;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RecallHit {
    #[sqlx(flatten)]
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
    pub fn validate(&self) -> Result<Vector, &'static str> {
        if self.subject.trim().is_empty()
            || self.predicate.trim().is_empty()
            || self.content.trim().is_empty()
        {
            return Err("subject, predicate, and content must be non-empty");
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            return Err("confidence must be between 0 and 1");
        }
        if self.embedding.len() != EMBEDDING_DIMENSIONS {
            return Err("embedding must contain exactly 1536 values");
        }
        Ok(Vector::from(self.embedding.clone()))
    }
}

impl RecallRequest {
    pub fn validate(&self) -> Result<Vector, &'static str> {
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
        Ok(Vector::from(self.embedding.clone()))
    }
}
