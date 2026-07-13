//! PostgreSQL + pgvector persistence for the shared brain.
//!
//! Runtime `sqlx::query(...).bind(...)` (no compile-time `DATABASE_URL`).
//! Embeddings are passed as pgvector literals (`[a,b,c]`) cast to `vector` in
//! SQL, so the crate needs no pgvector Rust binding to compile. Every query is
//! tenant-scoped; production also sets the `fiducia.tenant_id` RLS GUC per
//! request as a backstop.

use crate::domain::{Claim, Memory, MemoryId, TenantId};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use uuid::Uuid;

#[derive(Clone)]
pub struct PostgresMemory {
    pool: PgPool,
}

/// A pre-scored recall candidate straight from the vector + text search, before
/// the pure fusion/rerank pass in [`crate::recall`].
#[derive(Debug, Clone)]
pub struct ScoredRow {
    pub memory_id: MemoryId,
    pub content: String,
    pub semantic_score: f32,
}

impl PostgresMemory {
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        Ok(Self {
            pool: PgPoolOptions::new()
                .max_connections(10)
                .connect(url)
                .await?,
        })
    }

    /// Wrap an already-established pool so the epistemic layer and the durable
    /// store ([`crate::durable::store::MemoryStore`]) can share ONE `PgPool`.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying connection pool (shared with the durable store).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn ready(&self) -> Result<(), sqlx::Error> {
        sqlx::query("select 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        crate::db::apply_schema(&self.pool).await
    }

    /// Set the per-connection tenant GUC so row-level security scopes the session.
    pub async fn set_tenant(&self, tenant: TenantId) -> Result<(), sqlx::Error> {
        sqlx::query("select set_config('fiducia.tenant_id', $1, false)")
            .bind(tenant.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_memory(&self, memory: &Memory) -> Result<(), sqlx::Error> {
        sqlx::query(
            "insert into memories (id, tenant_id, namespace, memory_type, content, metadata, provenance, trust_score, importance, valid_from, valid_until) \
             values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(memory.id)
        .bind(memory.tenant_id)
        .bind(&memory.namespace)
        .bind(memory.memory_type.as_str())
        .bind(&memory.content)
        .bind(serde_json::to_value(&memory.metadata).expect("serializable"))
        .bind(serde_json::to_value(&memory.provenance).expect("serializable"))
        .bind(memory.trust_score)
        .bind(memory.importance)
        .bind(memory.valid_from)
        .bind(memory.valid_until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Store (or replace) a memory's embedding for a given model. The embedding
    /// is written as a pgvector literal cast in SQL.
    pub async fn upsert_embedding(
        &self,
        memory_id: MemoryId,
        model: &str,
        embedding: &[f32],
    ) -> Result<(), sqlx::Error> {
        let literal = pgvector_literal(embedding);
        sqlx::query(
            "insert into memory_embeddings (memory_id, model, embedding) values ($1,$2,$3::vector) \
             on conflict (memory_id, model) do update set embedding = excluded.embedding, created_at = now()",
        )
        .bind(memory_id)
        .bind(model)
        .bind(literal)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Nearest memories to `query_embedding` by cosine distance, scoped to the
    /// tenant and live validity window. Returns a semantic score in [0,1]
    /// (`1 - cosine_distance`) that the pure recall pass then fuses with lexical,
    /// trust, and freshness signals.
    pub async fn semantic_candidates(
        &self,
        tenant: TenantId,
        query_embedding: &[f32],
        model: &str,
        limit: i64,
    ) -> Result<Vec<ScoredRow>, sqlx::Error> {
        let literal = pgvector_literal(query_embedding);
        let rows = sqlx::query(
            "select m.id, m.content, 1 - (e.embedding <=> $1::vector) as semantic \
             from memories m join memory_embeddings e on e.memory_id = m.id \
             where m.tenant_id = $2 and e.model = $3 and m.forgotten_at is null \
               and m.superseded_by is null and (m.valid_until is null or m.valid_until > now()) \
             order by e.embedding <=> $1::vector asc limit $4",
        )
        .bind(literal)
        .bind(tenant)
        .bind(model)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ScoredRow {
                memory_id: row.get::<Uuid, _>("id"),
                content: row.get::<String, _>("content"),
                semantic_score: row.get::<f64, _>("semantic") as f32,
            })
            .collect())
    }

    /// Upsert a claim by (tenant, namespace, subject, predicate) — the durable
    /// mirror of [`crate::claims::ClaimLedger`]. The whole contest/support/resolve
    /// lifecycle is persisted so a restart reloads authoritative state faithfully.
    ///
    /// `author` is a free-form agent handle in the domain model but the column is
    /// a `uuid`; it parses when it is a UUID and is otherwise left null (the
    /// handle is still carried in-process). Every field recall or consensus reads
    /// — value, confidence, status, evidence, supporters, contests — round-trips.
    pub async fn upsert_claim(&self, claim: &Claim) -> Result<(), sqlx::Error> {
        let author_uuid = Uuid::parse_str(&claim.author).ok();
        sqlx::query(
            "insert into claims (id, tenant_id, namespace, subject, predicate, value, confidence, author_agent_id, status, evidence, supporters, contests, resolved_by, superseded_by, valid_until, claim_version) \
             values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) \
             on conflict (tenant_id, namespace, subject, predicate) do update set \
               value=excluded.value, confidence=excluded.confidence, status=excluded.status, evidence=excluded.evidence, \
               supporters=excluded.supporters, contests=excluded.contests, resolved_by=excluded.resolved_by, \
               superseded_by=excluded.superseded_by, valid_until=excluded.valid_until, \
               claim_version=excluded.claim_version, updated_at=now()",
        )
        .bind(claim.id)
        .bind(claim.tenant_id)
        .bind(&claim.namespace)
        .bind(&claim.subject)
        .bind(&claim.predicate)
        .bind(&claim.value)
        .bind(claim.confidence)
        .bind(author_uuid)
        .bind(format!("{:?}", claim.status).to_lowercase())
        .bind(serde_json::to_value(&claim.evidence).expect("serializable"))
        .bind(serde_json::to_value(&claim.supporters).expect("serializable"))
        .bind(serde_json::to_value(&claim.contests).expect("serializable"))
        .bind(&claim.resolved_by)
        .bind(claim.superseded_by)
        .bind(claim.valid_until)
        .bind(claim.claim_version as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch the accepted claim value, scoped to the ledger's full uniqueness
    /// key `(tenant, namespace, subject, predicate)`. `namespace` must be
    /// included: a tenant can hold the same `(subject, predicate)` in multiple
    /// namespaces, so omitting it could return a value from the wrong namespace.
    pub async fn accepted_claim_value(
        &self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
    ) -> Result<Option<serde_json::Value>, sqlx::Error> {
        let row = sqlx::query(
            "select value from claims where tenant_id=$1 and namespace=$2 and subject=$3 and predicate=$4 and status='accepted'",
        )
        .bind(tenant)
        .bind(namespace)
        .bind(subject)
        .bind(predicate)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| row.get::<serde_json::Value, _>("value")))
    }
}

/// Format an embedding as a pgvector text literal: `[1,2,3]`.
fn pgvector_literal(embedding: &[f32]) -> String {
    let mut out = String::with_capacity(embedding.len() * 8 + 2);
    out.push('[');
    for (i, value) in embedding.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&value.to_string());
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::pgvector_literal;

    #[test]
    fn embeddings_format_as_pgvector_literals() {
        assert_eq!(pgvector_literal(&[1.0, 2.5, -3.0]), "[1,2.5,-3]");
        assert_eq!(pgvector_literal(&[]), "[]");
    }
}
