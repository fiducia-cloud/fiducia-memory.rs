# sqlx → SeaORM migration plan (fiducia-memory.rs)

Status: **not started.** This crate is still 100% sqlx. It is the hardest of the
four service migrations because it combines pgvector, `sqlx::migrate!`, per-request
RLS, and a `#[sqlx(flatten)]` recall row. Read this whole file before touching code.

## House target idiom (reference: fiducia-messaging.rs)

`fiducia-messaging.rs` is the migrated reference. The convention:

- The DB handle is `sea_orm::DatabaseConnection`, not `sqlx::PgPool`.
- Prefer the **entity API** (`Entity::insert/find/update_many` + `ActiveModel`)
  for every query it can express.
- Drop to raw SQL via **`sea_orm::Statement::from_sql_and_values(DbBackend::Postgres, SQL, [values])`**
  for anything the entity API can't say (pgvector ops, `ON CONFLICT … WHERE`,
  CTEs, `set_config`, `<=>` distance). Read rows back with a
  `#[derive(FromQueryResult)]` struct via `Model::find_by_statement(...)`.
- Schema application is `conn.execute_unprepared(SQL)` — it sends the whole
  multi-statement batch unprepared, so `$$`-delimited plpgsql and multiple
  statements work (the sqlx `raw_sql` replacement).
- Public error type becomes `sea_orm::DbErr`.

## Dependency changes

Current (`Cargo.toml:28`):
```toml
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "tls-rustls-ring-webpki", "postgres", "uuid", "json", "chrono", "migrate", "macros", "derive"] }
```
Target:
```toml
sea-orm = { version = "1", default-features = false, features = [
    "sqlx-postgres", "runtime-tokio-rustls", "macros",
    "with-uuid", "with-chrono", "with-json",
] }
```

### ⚠️ Pitfall 1 — the whole graph drops from sqlx 0.9 to 0.8

SeaORM 1.x pins **sqlx 0.8** as its driver. This crate is on **sqlx 0.9** today.
After the swap, sqlx exists only transitively at 0.8. Two consequences to verify:
- **pgvector** (`durable/store.rs`, `durable/model.rs`) must switch from its
  `sqlx`/`sqlx-0.9` feature to its **`sea-orm`** feature, and that feature must be
  built against the same sqlx 0.8 SeaORM uses. Confirm the pgvector version
  exposes a `sea-orm` feature *and* that `sea_query::Value: From<pgvector::Vector>`
  resolves. If pgvector's SeaORM support is missing/incompatible, fall back to
  binding embeddings as **pgvector text literals cast in SQL** (`$1::vector`) —
  which `postgres.rs` already does via `pgvector_literal()`. That fallback removes
  the pgvector Rust dependency from the query path entirely and is the lowest-risk
  path; prefer it unless the typed binding is confirmed working.
- The lockfile will churn broadly. Re-run the full suite, not just `cargo build`.

## What each file needs

### `src/db.rs` (13 lines) — trivial
`apply_schema(pool)` → take `&DatabaseConnection`, replace
`sqlx::raw_sql(SCHEMA).execute(pool)` with `conn.execute_unprepared(SCHEMA)`.
Return `Result<(), DbErr>`.

### `src/postgres.rs` (332 lines) — the epistemic layer
- `PostgresMemory { pool: PgPool }` → `{ conn: DatabaseConnection }`.
- `connect()` → `Database::connect(ConnectOptions)`. `from_pool`/`pool()` become
  `from_conn`/`conn()` returning `&DatabaseConnection` (durable store shares it).
- **`with_tenant` (the RLS-critical bit) — keep the exact semantics.** Today it
  opens `pool.begin()`, binds the tenant GUC on that transaction's connection,
  runs the closure, commits. In SeaORM: `let txn = conn.begin().await?;` gives a
  `DatabaseTransaction` that owns its connection — the same guarantee that the
  GUC and the queries land on one connection. Keep the closure-owns-`txn`,
  returns-`(txn, T)` shape so caller refs don't get forced to `'static`.
- `bind_tenant` → `txn.execute(Statement::from_sql_and_values(Postgres,
  "select set_config('fiducia.tenant_id', $1, true)", [tenant.to_string().into()]))`.
  **Do not** collapse this into a pool-level call — RLS correctness depends on it
  running on the same connection as the queries (the module header explains why).
- `insert_memory`, `upsert_embedding`, `upsert_claim` — plain
  `Statement::from_sql_and_values` executed on the `txn` (they already use
  positional binds and `ON CONFLICT`). `$3::vector` literal binds stay strings.
- `semantic_candidates` — raw SQL with `<=>` cosine distance; read back with a
  `#[derive(FromQueryResult)] struct ScoredRow { id: Uuid, content: String, semantic: f64 }`
  via `ScoredRow::find_by_statement(...).all(&txn)`. Map `semantic as f32`.
- `accepted_claim_value` — `find_by_statement` returning a one-column row, or
  `txn.query_one(stmt)` + `row.try_get("", "value")`.

### `src/durable/store.rs` (180 lines) — the durable claims store
- `MemoryStore { pool }` → `{ conn }`. `append`/`supersede`/`recall` open a
  `DatabaseTransaction`, `bind_tenant`, run, commit — same as above.
- `insert_claim` uses `query_as::<_, Claim>(… RETURNING …)`. Replace with
  `Claim::find_by_statement(Statement::…).one(&txn)` where `Claim` derives
  `FromQueryResult` (see model.rs). Embedding bind: typed pgvector `Vector` if
  Pitfall-1 typed path is confirmed, else pass the literal string + `$9::vector`.
- **⚠️ Pitfall 2 — `sqlx::migrate!()` has no SeaORM equivalent.**
  `migrate()` currently runs `sqlx::migrate!()` over `migrations/` (0001/0002/0003)
  and records applied versions + checksums in `_sqlx_migrations`. SeaORM has no
  compile-time migration macro. Options, best first:
  1. **Adopt `sea-orm-migration`**: port 0001/0002/0003 into a `migration/`
     sub-crate with `Migration` impls (they can `execute_unprepared` the existing
     `.sql` files). Keeps version tracking + a real up/down history. Recommended
     for a store that ships forward migrations.
  2. **Idempotent apply in order** via `execute_unprepared` of each file
     concatenated. Simple, but **loses version/checksum tracking** — only safe if
     every migration is written `IF NOT EXISTS`/idempotent. Audit 0001–0003 for
     idempotency before choosing this; `0003_rls_force.sql` (FORCE RLS) likely is,
     `0001_memory.sql` may not be. Do **not** silently drop tracking without
     saying so in the module docs.
  Whichever you pick, `store.rs` and `postgres.rs` both call a `migrate()` — unify
  them so schema is applied once.

### `src/durable/model.rs` (116 lines) — row types
- `#[derive(sqlx::FromRow)]` on `Claim` → `#[derive(FromQueryResult)]`.
- **⚠️ Pitfall 3 — `RecallHit` uses `#[sqlx(flatten)]`** to embed `Claim` plus
  `lexical_score/semantic_score/score`. SeaORM's `FromQueryResult` supports
  nesting via `#[sea_orm(nested)]` (confirm your sea-orm minor supports it); if
  not, flatten manually — declare `RecallHit` with all of `Claim`'s columns
  inline plus the three scores, and construct the nested `Claim` in code. The
  `recall.sql` (`sql/recall.sql`) column list must line up with whichever you pick.

### `src/main.rs` — call sites
Update the shared-pool wiring (one `DatabaseConnection` shared by
`PostgresMemory` + `MemoryStore`) and propagate the `DbErr` return type.

## ⚠️ Pitfall 4 — public API error type churn

Nearly every method returns `Result<_, sqlx::Error>`, and `StoreError` has
`#[from] sqlx::Error`. Switching to `DbErr` is a breaking change across the HTTP
handlers. Change `StoreError::Database(#[from] sqlx::Error)` →
`#[from] sea_orm::DbErr` and fix each caller. `MigrateError` similarly becomes
the migration path's error.

## Adjacent issues worth fixing while here (not migration-blocking)

- `postgres.rs::upsert_claim` silently drops a non-UUID `author` to NULL
  (`Uuid::parse_str(...).ok()`). The in-process handle is kept but the durable
  row loses it. Consider a dedicated `author_handle text` column so provenance
  survives a reload. Call out, don't fix blind.
- `pgvector_literal` formats floats with `to_string()`; fine for finite values,
  but NaN/Inf would produce invalid vector literals. The `validate()` path
  should already reject those — verify it does before trusting the literal path.

## Verification checklist

- [ ] `cargo build` and `cargo build --all-features` (no `DATABASE_URL` needed —
      queries are runtime-checked, same as messaging.rs).
- [ ] `cargo test` — the pure unit tests (`pgvector_literal`, `sha256_hex`) must
      still pass; they don't touch the DB.
- [ ] `cargo clippy --all-targets`.
- [ ] Grep for leftover `sqlx` (only expected hit: none — unlike backend, this
      crate has no `sqlx_logging`; SeaORM's `ConnectOptions::sqlx_logging` will be
      the only token after migration).
- [ ] Manual: apply schema against a scratch pgvector Postgres, run one
      append/recall round, confirm RLS still filters by tenant (set the GUC to
      tenant A, confirm tenant B rows are invisible) — the GUC-on-same-connection
      property is the thing most likely to silently break.
