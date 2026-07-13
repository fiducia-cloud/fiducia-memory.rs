//! Durable `PgPool`-backed store over `memory_claims` (adopted from codex
//! `store.rs`). Append / atomic supersede / hybrid recall / migrate / ping.
//!
//! `migrate()` runs `sqlx::migrate!()` over the crate's unified `migrations/`
//! directory, so it applies BOTH the durable `memory_claims` schema
//! (`0001_memory.sql`) and the epistemic-layer schema — memories, embeddings,
//! the claim ledger, edges, recall log, RLS (`0002_fiducia_memory.sql`).

use crate::durable::model::{AppendClaim, Claim, RecallHit, RecallRequest};
use pgvector::Vector;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::fmt::Write as _;
use uuid::Uuid;

#[derive(Clone)]
pub struct MemoryStore {
    pool: PgPool,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("claim not found")]
    NotFound,
    #[error("claim belongs to another tenant")]
    TenantMismatch,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl MemoryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Access the underlying pool (so the unified state can share one `PgPool`).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Apply ALL migrations in the unified `migrations/` dir (durable schema +
    /// epistemic-layer schema).
    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        sqlx::migrate!().run(&self.pool).await
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    pub async fn append(
        &self,
        input: &AppendClaim,
        embedding: Vector,
    ) -> Result<Claim, StoreError> {
        let mut tx = self.pool.begin().await?;
        // Wire per-request RLS: bind `fiducia.tenant_id` on THIS transaction's
        // connection so the (FORCEd) tenant policy on `memory_claims` admits and
        // scopes every statement below. Without this the INSERT's RLS WITH CHECK
        // would reject the row once RLS is forced.
        bind_tenant(&mut tx, input.tenant_id).await?;
        let claim = insert_claim(&mut tx, input, embedding).await?;
        tx.commit().await?;
        Ok(claim)
    }

    pub async fn supersede(
        &self,
        old_id: Uuid,
        tenant_id: Uuid,
        input: &AppendClaim,
        embedding: Vector,
    ) -> Result<Claim, StoreError> {
        let mut tx = self.pool.begin().await?;
        // Wire per-request RLS on this transaction's connection (see `append`).
        bind_tenant(&mut tx, tenant_id).await?;
        let updated = sqlx::query("UPDATE memory_claims SET valid_until = COALESCE(valid_until, now()) WHERE claim_id = $1 AND tenant_id = $2 AND valid_until IS NULL")
            .bind(old_id).bind(tenant_id).execute(&mut *tx).await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        let mut replacement = input.clone_for_supersede(old_id, tenant_id);
        let claim = insert_claim(&mut tx, &replacement, embedding).await?;
        replacement.content.clear();
        tx.commit().await?;
        Ok(claim)
    }

    pub async fn recall(
        &self,
        request: &RecallRequest,
        embedding: Vector,
    ) -> Result<Vec<RecallHit>, StoreError> {
        let lexical_weight = 1.0 - request.semantic_weight;
        // Read path also runs inside a tenant-scoped transaction so the FORCEd
        // RLS policy on `memory_claims` filters candidate rows to the tenant on
        // the same connection the query runs on. (A bare `&self.pool` read could
        // land on a different connection than any GUC was set on, so RLS would
        // return zero rows — the GUC MUST be bound on this very transaction.)
        let mut tx = self.pool.begin().await?;
        bind_tenant(&mut tx, request.tenant_id).await?;
        let hits = sqlx::query_as::<_, RecallHit>(include_str!("../../sql/recall.sql"))
            .bind(request.tenant_id)
            .bind(&request.query)
            .bind(embedding)
            .bind(request.semantic_weight)
            .bind(lexical_weight)
            .bind(request.limit)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(hits)
    }
}

/// Bind the per-request `fiducia.tenant_id` GUC on `tx` with `SET LOCAL`
/// semantics (`set_config(..., true)`), so the FORCEd RLS policies on
/// `memory_claims` filter this transaction's statements to `tenant`.
async fn bind_tenant(tx: &mut Transaction<'_, Postgres>, tenant: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("select set_config('fiducia.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn insert_claim(
    tx: &mut Transaction<'_, Postgres>,
    input: &AppendClaim,
    embedding: Vector,
) -> Result<Claim, sqlx::Error> {
    let digest = sha256_hex(input.content.as_bytes());
    sqlx::query_as::<_, Claim>("INSERT INTO memory_claims (tenant_id, subject, predicate, object, source, confidence, content, content_sha256, embedding, valid_from, valid_until, supersedes_claim_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,COALESCE($10,now()),$11,$12) RETURNING claim_id,tenant_id,subject,predicate,object,source,confidence,content,content_sha256,valid_from,valid_until,supersedes_claim_id,created_at")
        .bind(input.tenant_id).bind(input.subject.trim()).bind(input.predicate.trim())
        .bind(&input.object).bind(&input.source).bind(input.confidence).bind(input.content.trim())
        .bind(digest).bind(embedding).bind(input.valid_from).bind(input.valid_until)
        .bind(input.supersedes_claim_id).fetch_one(&mut **tx).await
}

fn sha256_hex(input: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(input) {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

impl AppendClaim {
    fn clone_for_supersede(&self, old_id: Uuid, tenant_id: Uuid) -> Self {
        Self {
            tenant_id,
            subject: self.subject.clone(),
            predicate: self.predicate.clone(),
            object: self.object.clone(),
            source: self.source.clone(),
            confidence: self.confidence,
            content: self.content.clone(),
            embedding: self.embedding.clone(),
            valid_from: self.valid_from,
            valid_until: self.valid_until,
            supersedes_claim_id: Some(old_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn sha256_hex_is_canonical_lowercase() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
