# Migrations

Ordered PostgreSQL migrations for the durable memory floor, epistemic schema,
and tenant RLS enforcement. They are embedded by SQLx and must remain additive,
idempotent where documented, and safe for existing memory rows.
