# Source layout

The crate combines the contestable epistemic model, hybrid recall, PostgreSQL
persistence, HTTP service, and the durable-to-fusion seam. `durable/` owns the
append-only fact store; the remaining modules own claims, trust, and ranking.
