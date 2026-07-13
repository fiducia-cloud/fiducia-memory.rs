//! Applies the canonical shared-brain schema (embedded from
//! `sql/fiducia_memory.sql`) to whichever PostgreSQL `DATABASE_URL` targets —
//! the customer's own database or the Fiducia-hosted default. Requires the
//! `vector` (pgvector) extension. Idempotent.

use sqlx::PgPool;

pub const SCHEMA: &str = include_str!("../sql/fiducia_memory.sql");

pub async fn apply_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(SCHEMA).execute(pool).await?;
    Ok(())
}
