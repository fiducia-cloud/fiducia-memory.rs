# docs

Long-form engineering notes for fiducia-memory.rs. The repo-root `README.md` is
the overview; deeper design and migration material lives here.

- `sqlx-to-seaorm-migration.md` — the plan to move this crate off sqlx onto
  SeaORM (the fleet DB convention, already applied in fiducia-messaging.rs /
  admin.rs / customer.rs). Documents the pgvector, `sqlx::migrate!`, RLS-GUC, and
  `#[sqlx(flatten)]` pitfalls and how to shore each up. Status: not started.
