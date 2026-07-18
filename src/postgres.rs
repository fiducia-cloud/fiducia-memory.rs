//! SeaORM + pgvector persistence for the epistemic shared-brain layer.
//!
//! Tenant-scoped operations bind `fiducia.tenant_id` inside the same database
//! transaction that executes the query, combining FORCEd RLS with explicit
//! tenant predicates. Embeddings are finite-validated text binds cast to
//! PostgreSQL `vector`; no direct SQLx API is used.

use crate::domain::{Claim, Memory, MemoryId, TenantId};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, Statement,
    TransactionTrait,
};
use std::future::Future;
use uuid::Uuid;

type TenantTx = DatabaseTransaction;

#[derive(Clone)]
pub struct PostgresMemory {
    database: DatabaseConnection,
}

/// A pre-scored recall candidate from vector search, before pure fusion/rerank.
#[derive(Debug, Clone)]
pub struct ScoredRow {
    pub memory_id: MemoryId,
    pub content: String,
    pub semantic_score: f32,
}

impl PostgresMemory {
    pub async fn connect(url: &str) -> Result<Self, DbErr> {
        let database = crate::db::connect_database(url, 20).await?;
        Ok(Self { database })
    }

    /// Share an established SeaORM connection pool with the durable store.
    pub fn from_database(database: DatabaseConnection) -> Self {
        Self { database }
    }

    pub fn database(&self) -> &DatabaseConnection {
        &self.database
    }

    pub async fn ready(&self) -> Result<(), DbErr> {
        self.database
            .query_one(Statement::from_string(DbBackend::Postgres, "select 1"))
            .await?;
        Ok(())
    }

    pub async fn migrate(&self) -> Result<(), DbErr> {
        crate::db::apply_schema(&self.database).await
    }

    /// Run a closure inside a transaction with transaction-local tenant RLS.
    pub async fn with_tenant<T, F, Fut>(&self, tenant: TenantId, f: F) -> Result<T, DbErr>
    where
        F: FnOnce(TenantTx) -> Fut,
        Fut: Future<Output = Result<(TenantTx, T), DbErr>>,
    {
        let transaction = self.database.begin().await?;
        bind_tenant(&transaction, tenant).await?;
        let (transaction, output) = f(transaction).await?;
        transaction.commit().await?;
        Ok(output)
    }

    pub async fn insert_memory(&self, memory: &Memory) -> Result<(), DbErr> {
        let memory = memory.clone();
        self.with_tenant(memory.tenant_id, move |transaction| async move {
            transaction
                .execute(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "insert into memories (id, tenant_id, namespace, memory_type, content, metadata, provenance, trust_score, importance, valid_from, valid_until) \
                     values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
                    [
                        memory.id.into(),
                        memory.tenant_id.into(),
                        memory.namespace.into(),
                        memory.memory_type.as_str().into(),
                        memory.content.into(),
                        serde_json::to_value(memory.metadata)
                            .expect("serializable metadata")
                            .into(),
                        serde_json::to_value(memory.provenance)
                            .expect("serializable provenance")
                            .into(),
                        memory.trust_score.into(),
                        memory.importance.into(),
                        memory.valid_from.into(),
                        memory.valid_until.into(),
                    ],
                ))
                .await?;
            Ok((transaction, ()))
        })
        .await
    }

    pub async fn upsert_embedding(
        &self,
        tenant: TenantId,
        memory_id: MemoryId,
        model: &str,
        embedding: &[f32],
    ) -> Result<(), DbErr> {
        let literal = vector_literal(embedding)?;
        let model = model.to_string();
        self.with_tenant(tenant, move |transaction| async move {
            transaction
                .execute(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "insert into memory_embeddings (memory_id, model, embedding) values ($1,$2,$3::vector) \
                     on conflict (memory_id, model) do update set embedding = excluded.embedding, created_at = now()",
                    [memory_id.into(), model.into(), literal.into()],
                ))
                .await?;
            Ok((transaction, ()))
        })
        .await
    }

    pub async fn semantic_candidates(
        &self,
        tenant: TenantId,
        query_embedding: &[f32],
        model: &str,
        limit: i64,
    ) -> Result<Vec<ScoredRow>, DbErr> {
        let literal = vector_literal(query_embedding)?;
        let model = model.to_string();
        self.with_tenant(tenant, move |transaction| async move {
            let rows = transaction
                .query_all(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "select m.id, m.content, 1 - (e.embedding <=> $1::vector) as semantic \
                     from memories m join memory_embeddings e on e.memory_id = m.id \
                     where m.tenant_id = $2 and e.model = $3 and m.forgotten_at is null \
                       and m.superseded_by is null and (m.valid_until is null or m.valid_until > now()) \
                     order by e.embedding <=> $1::vector asc limit $4",
                    [literal.into(), tenant.into(), model.into(), limit.into()],
                ))
                .await?;
            let rows = rows
                .into_iter()
                .map(|row| {
                    let semantic: f64 = row.try_get("", "semantic")?;
                    Ok(ScoredRow {
                        memory_id: row.try_get::<Uuid>("", "id")?,
                        content: row.try_get("", "content")?,
                        semantic_score: semantic as f32,
                    })
                })
                .collect::<Result<Vec<_>, DbErr>>()?;
            Ok((transaction, rows))
        })
        .await
    }

    pub async fn upsert_claim(&self, claim: &Claim) -> Result<(), DbErr> {
        let author_uuid = Uuid::parse_str(&claim.author).ok();
        let status = format!("{:?}", claim.status).to_lowercase();
        let claim = claim.clone();
        self.with_tenant(claim.tenant_id, move |transaction| async move {
            transaction
                .execute(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "insert into claims (id, tenant_id, namespace, subject, predicate, value, confidence, author_agent_id, status, evidence, supporters, contests, resolved_by, superseded_by, valid_until, claim_version) \
                     values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) \
                     on conflict (tenant_id, namespace, subject, predicate) do update set \
                       value=excluded.value, confidence=excluded.confidence, status=excluded.status, evidence=excluded.evidence, \
                       supporters=excluded.supporters, contests=excluded.contests, resolved_by=excluded.resolved_by, \
                       superseded_by=excluded.superseded_by, valid_until=excluded.valid_until, \
                       claim_version=excluded.claim_version, updated_at=now()",
                    [
                        claim.id.into(),
                        claim.tenant_id.into(),
                        claim.namespace.into(),
                        claim.subject.into(),
                        claim.predicate.into(),
                        claim.value.into(),
                        claim.confidence.into(),
                        author_uuid.into(),
                        status.into(),
                        serde_json::to_value(claim.evidence)
                            .expect("serializable evidence")
                            .into(),
                        serde_json::to_value(claim.supporters)
                            .expect("serializable supporters")
                            .into(),
                        serde_json::to_value(claim.contests)
                            .expect("serializable contests")
                            .into(),
                        claim.resolved_by.into(),
                        claim.superseded_by.into(),
                        claim.valid_until.into(),
                        checked_i64(claim.claim_version, "claim version")?.into(),
                    ],
                ))
                .await?;
            Ok((transaction, ()))
        })
        .await
    }

    pub async fn accepted_claim_value(
        &self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
    ) -> Result<Option<serde_json::Value>, DbErr> {
        let namespace = namespace.to_string();
        let subject = subject.to_string();
        let predicate = predicate.to_string();
        self.with_tenant(tenant, move |transaction| async move {
            let value = transaction
                .query_one(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "select value from claims where tenant_id=$1 and namespace=$2 and subject=$3 and predicate=$4 and status='accepted'",
                    [
                        tenant.into(),
                        namespace.into(),
                        subject.into(),
                        predicate.into(),
                    ],
                ))
                .await?
                .map(|row| row.try_get("", "value"))
                .transpose()?;
            Ok((transaction, value))
        })
        .await
    }
}

pub(crate) async fn bind_tenant(
    transaction: &DatabaseTransaction,
    tenant: TenantId,
) -> Result<(), DbErr> {
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "select set_config('fiducia.tenant_id', $1, true)",
            [tenant.to_string().into()],
        ))
        .await?;
    Ok(())
}

fn vector_literal(embedding: &[f32]) -> Result<String, DbErr> {
    crate::vector::pgvector_literal(embedding).map_err(|error| DbErr::Custom(error.to_string()))
}

fn checked_i64(value: u64, field: &str) -> Result<i64, DbErr> {
    i64::try_from(value).map_err(|_| DbErr::Custom(format!("{field} exceeds bigint")))
}
