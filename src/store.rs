use crate::model::{AppendClaim, Claim, RecallHit, RecallRequest};
use pgvector::Vector;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
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
        Ok(
            sqlx::query_as::<_, RecallHit>(include_str!("../sql/recall.sql"))
                .bind(request.tenant_id)
                .bind(&request.query)
                .bind(embedding)
                .bind(request.semantic_weight)
                .bind(lexical_weight)
                .bind(request.limit)
                .fetch_all(&self.pool)
                .await?,
        )
    }
}

async fn insert_claim(
    tx: &mut Transaction<'_, Postgres>,
    input: &AppendClaim,
    embedding: Vector,
) -> Result<Claim, sqlx::Error> {
    let digest = format!("{:x}", Sha256::digest(input.content.as_bytes()));
    sqlx::query_as::<_, Claim>("INSERT INTO memory_claims (tenant_id, subject, predicate, object, source, confidence, content, content_sha256, embedding, valid_from, valid_until, supersedes_claim_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,COALESCE($10,now()),$11,$12) RETURNING claim_id,tenant_id,subject,predicate,object,source,confidence,content,content_sha256,valid_from,valid_until,supersedes_claim_id,created_at")
        .bind(input.tenant_id).bind(input.subject.trim()).bind(input.predicate.trim())
        .bind(&input.object).bind(&input.source).bind(input.confidence).bind(input.content.trim())
        .bind(digest).bind(embedding).bind(input.valid_from).bind(input.valid_until)
        .bind(input.supersedes_claim_id).fetch_one(&mut **tx).await
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
