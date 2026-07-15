<<<<<<< HEAD
# Source layout

The crate combines the contestable epistemic model, hybrid recall, PostgreSQL
persistence, HTTP service, and the durable-to-fusion seam. `durable/` owns the
append-only fact store; the remaining modules own claims, trust, and ranking.
=======
# src

The durable memory service: claim/recall HTTP API in `main.rs` + the epistemic
store, with the original durable `memory_claims` layer preserved under
`durable/`. Auth middleware is exercised end-to-end in `tests/`.
>>>>>>> b404e81 (Adopt fiducia-telemetry; add catch-panic layer; tracing for the migrate-only run)
