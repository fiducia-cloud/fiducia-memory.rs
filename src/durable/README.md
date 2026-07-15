<<<<<<< HEAD
# Durable memory floor

HTTP handlers, request/row models, and the PostgreSQL store for append-only
memory claims, temporal supersession, and recall candidate generation. This
layer produces candidates; it does not make similarity authoritative.
=======
# durable

The durable `memory_claims` layer (compat lineage): `api.rs` routes, `store.rs`
Postgres access, `model.rs` types. Kept alongside the epistemic schema so the
original wire/table contract keeps working.
>>>>>>> b404e81 (Adopt fiducia-telemetry; add catch-panic layer; tracing for the migrate-only run)
