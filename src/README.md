# src

The durable memory service: claim/recall HTTP API in `main.rs` + the epistemic
store, with the original durable `memory_claims` layer preserved under
`durable/`. Auth middleware is exercised end-to-end in `tests/`.
