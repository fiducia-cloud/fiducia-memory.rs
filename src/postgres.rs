//! PostgreSQL + pgvector persistence for the shared brain.
//!
//! Runtime `sqlx::query(...).bind(...)` (no compile-time `DATABASE_URL`).
//! Embeddings are passed as pgvector literals (`[a,b,c]`) cast to `vector` in
//! SQL, so the crate needs no pgvector Rust binding to compile.
//!
//! Tenant isolation is enforced in **two** layers (defense in depth):
//!
//! 1. **Row-level security (the backstop).** Every tenant-scoped query runs
//!    inside a transaction opened by [`PostgresMemory::with_tenant`], which
//!    first binds the per-request `fiducia.tenant_id` GUC with
//!    `set_config('fiducia.tenant_id', $tenant, true)` — the bindable form of
//!    `SET LOCAL`, so the setting is scoped to *that* transaction and released
//!    on commit/rollback. Because the GUC is set on the *same* pooled
//!    connection that then runs the query, the RLS policies (`migrations/0002`
//!    + `migrations/0003`, which additionally `FORCE` RLS so it applies even to
//!    the table owner) filter every row. A pooled connection therefore never
//!    leaks tenant state between requests.
//! 2. **Code-level `tenant_id = $n` filters (belt-and-suspenders).** Every
//!    query *also* filters on `tenant_id` explicitly (and, for the claims
//!    ledger, the full `(tenant, namespace, subject, predicate)` key), so a
//!    query is correct even if RLS were somehow disabled.

use crate::domain::{Claim, Memory, MemoryId, TenantId};
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Row, Transaction};
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

/// A boxed, `Send` future borrowing a tenant-scoped transaction for `'c`.
/// Returned by the closure passed to [`PostgresMemory::with_tenant`].
type TxFuture<'c, T> = Pin<Box<dyn Future<Output = Result<T, sqlx::Error>> + Send + 'c>>;

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

    /// Run `f` inside a transaction whose `fiducia.tenant_id` GUC is bound to
    /// `tenant`, then commit. This is the ONE place per-request RLS is wired:
    ///
    /// * `set_config('fiducia.tenant_id', $tenant, true)` is the bindable
    ///   equivalent of `SET LOCAL` (`is_local = true`), so the GUC is scoped to
    ///   this transaction and released on commit/rollback — a pooled connection
    ///   never carries tenant state between requests.
    /// * The GUC is set on the SAME connection the closure's statements run on
    ///   (the transaction), so the RLS policies actually filter the query. This
    ///   correctness point is why every tenant-scoped query MUST go through here
    ///   rather than run on `&self.pool` directly — with a pool, a query on a
    ///   bare pool handle could land on a different connection than the one the
    ///   GUC was set on, and RLS would then hide every row.
    ///
    /// `tenant` is bound as text and cast per the policies'
    /// `nullif(current_setting('fiducia.tenant_id', true), '')::uuid`.
    pub async fn with_tenant<T, F>(&self, tenant: TenantId, f: F) -> Result<T, sqlx::Error>
    where
        F: for<'c> FnOnce(&'c mut Transaction<'static, Postgres>) -> TxFuture<'c, T>,
    {
        let mut tx = self.pool.begin().await?;
        bind_tenant(&mut tx, tenant).await?;
        let out = f(&mut tx).await?;
        tx.commit().await?;
        Ok(out)
    }

    pub async fn insert_memory(&self, memory: &Memory) -> Result<(), sqlx::Error> {
        self.with_tenant(memory.tenant_id, |tx| {
            Box::pin(async move {
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
                .execute(&mut **tx)
                .await?;
                Ok(())
            })
        })
        .await
    }

    /// Store (or replace) a memory's embedding for a given model. The embedding
    /// is written as a pgvector literal cast in SQL.
    ///
    /// `tenant` is required because `memory_embeddings` carries no `tenant_id`
    /// of its own: its RLS policy (and INSERT `WITH CHECK`) is scoped through
    /// the parent `memories` row, so the `fiducia.tenant_id` GUC MUST be set to
    /// the owning tenant for the write to be admitted.
    pub async fn upsert_embedding(
        &self,
        tenant: TenantId,
        memory_id: MemoryId,
        model: &str,
        embedding: &[f32],
    ) -> Result<(), sqlx::Error> {
        let literal = pgvector_literal(embedding);
        self.with_tenant(tenant, |tx| {
            Box::pin(async move {
                sqlx::query(
                    "insert into memory_embeddings (memory_id, model, embedding) values ($1,$2,$3::vector) \
                     on conflict (memory_id, model) do update set embedding = excluded.embedding, created_at = now()",
                )
                .bind(memory_id)
                .bind(model)
                .bind(literal)
                .execute(&mut **tx)
                .await?;
                Ok(())
            })
        })
        .await
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
