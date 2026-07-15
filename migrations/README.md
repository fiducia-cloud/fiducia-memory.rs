<<<<<<< HEAD
# Migrations

Ordered PostgreSQL migrations for the durable memory floor, epistemic schema,
and tenant RLS enforcement. They are embedded by SQLx and must remain additive,
idempotent where documented, and safe for existing memory rows.
=======
# migrations

Forward-only sqlx migrations, applied automatically at service start (and by
`--migrate` / `FIDUCIA_MEMORY_MIGRATE=true` for a migrate-then-exit run).
sqlx checksums applied migrations: never edit an applied file — add a new one.
>>>>>>> b404e81 (Adopt fiducia-telemetry; add catch-panic layer; tracing for the migrate-only run)
