//! Durable SeaORM-backed store over `memory_claims`.
//!
//! Append, supersede, and recall each run in a tenant-scoped transaction so
//! PostgreSQL row-level security and the explicit tenant predicates agree.

use crate::durable::model::{AppendClaim, Claim, RecallHit, RecallRequest};
use chrono::{DateTime, Utc};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, QueryResult,
    Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use uuid::Uuid;

#[derive(Clone)]
pub struct MemoryStore {
    database: DatabaseConnection,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("claim not found")]
    NotFound,
    #[error("claim belongs to another tenant")]
    TenantMismatch,
    #[error(transparent)]
    Database(#[from] DbErr),
}

impl MemoryStore {
    pub fn new(database: DatabaseConnection) -> Self {
        Self { database }
    }

    pub fn database(&self) -> &DatabaseConnection {
        &self.database
    }

    /// Apply the embedded durable and epistemic schemas in migration order.
    pub async fn migrate(&self) -> Result<(), DbErr> {
        crate::db::apply_schema(&self.database).await
    }

    pub async fn ping(&self) -> Result<(), DbErr> {
        self.database
            .query_one(Statement::from_string(DbBackend::Postgres, "select 1"))
            .await?;
        Ok(())
    }

    pub async fn append(
        &self,
        input: &AppendClaim,
        embedding: String,
    ) -> Result<Claim, StoreError> {
        let transaction = self.database.begin().await?;
        bind_tenant(&transaction, input.tenant_id).await?;
        let claim = insert_claim(&transaction, input, embedding).await?;
        transaction.commit().await?;
        Ok(claim)
    }

    pub async fn supersede(
        &self,
        old_id: Uuid,
        tenant_id: Uuid,
        input: &AppendClaim,
        embedding: String,
    ) -> Result<Claim, StoreError> {
        let transaction = self.database.begin().await?;
        bind_tenant(&transaction, tenant_id).await?;
        let updated = transaction
            .execute(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "update memory_claims set valid_until = coalesce(valid_until, now()) where claim_id = $1 and tenant_id = $2 and valid_until is null",
                [old_id.into(), tenant_id.into()],
            ))
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        let replacement = input.clone_for_supersede(old_id, tenant_id);
        let claim = insert_claim(&transaction, &replacement, embedding).await?;
        transaction.commit().await?;
        Ok(claim)
    }

    pub async fn recall(
        &self,
        request: &RecallRequest,
        embedding: String,
    ) -> Result<Vec<RecallHit>, StoreError> {
        let transaction = self.database.begin().await?;
        bind_tenant(&transaction, request.tenant_id).await?;
        let lexical_weight = 1.0 - request.semantic_weight;
        let rows = transaction
            .query_all(Statement::from_sql_and_values(
                DbBackend::Postgres,
                include_str!("../../sql/recall.sql"),
                [
                    request.tenant_id.into(),
                    request.query.clone().into(),
                    embedding.into(),
                    request.semantic_weight.into(),
                    lexical_weight.into(),
                    request.limit.into(),
                ],
            ))
            .await?;
        let hits = rows
            .iter()
            .map(|row| {
                Ok(RecallHit {
                    claim: claim_from_row(row)?,
                    lexical_score: row.try_get("", "lexical_score")?,
                    semantic_score: row.try_get("", "semantic_score")?,
                    score: row.try_get("", "score")?,
                })
            })
            .collect::<Result<Vec<_>, DbErr>>()?;
        transaction.commit().await?;
        Ok(hits)
    }
}

/// Bind the tenant GUC with transaction-local semantics for FORCEd RLS.
async fn bind_tenant(transaction: &DatabaseTransaction, tenant: Uuid) -> Result<(), DbErr> {
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "select set_config('fiducia.tenant_id', $1, true)",
            [tenant.to_string().into()],
        ))
        .await?;
    Ok(())
}

async fn insert_claim(
    transaction: &DatabaseTransaction,
    input: &AppendClaim,
    embedding: String,
) -> Result<Claim, DbErr> {
    let digest = sha256_hex(input.content.as_bytes());
    let row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "insert into memory_claims (tenant_id, subject, predicate, object, source, confidence, content, content_sha256, embedding, valid_from, valid_until, supersedes_claim_id) values ($1,$2,$3,$4,$5,$6,$7,$8,$9::vector,coalesce($10,now()),$11,$12) returning claim_id,tenant_id,subject,predicate,object,source,confidence,content,content_sha256,valid_from,valid_until,supersedes_claim_id,created_at",
            [
                input.tenant_id.into(),
                input.subject.trim().into(),
                input.predicate.trim().into(),
                input.object.clone().into(),
                input.source.clone().into(),
                input.confidence.into(),
                input.content.trim().into(),
                digest.into(),
                embedding.into(),
                input.valid_from.into(),
                input.valid_until.into(),
                input.supersedes_claim_id.into(),
            ],
        ))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("claim insert returned no row".into()))?;
    claim_from_row(&row)
}

fn claim_from_row(row: &QueryResult) -> Result<Claim, DbErr> {
    Ok(Claim {
        claim_id: row.try_get("", "claim_id")?,
        tenant_id: row.try_get("", "tenant_id")?,
        subject: row.try_get("", "subject")?,
        predicate: row.try_get("", "predicate")?,
        object: row.try_get("", "object")?,
        source: row.try_get("", "source")?,
        confidence: row.try_get("", "confidence")?,
        content: row.try_get("", "content")?,
        content_sha256: row.try_get("", "content_sha256")?,
        valid_from: row.try_get::<DateTime<Utc>>("", "valid_from")?,
        valid_until: row.try_get::<Option<DateTime<Utc>>>("", "valid_until")?,
        supersedes_claim_id: row.try_get("", "supersedes_claim_id")?,
        created_at: row.try_get::<DateTime<Utc>>("", "created_at")?,
    })
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
