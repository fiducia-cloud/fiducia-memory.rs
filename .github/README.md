<<<<<<< HEAD
# GitHub automation

Dependabot and locked CI workflows for the memory service. Workflows validate
formatting, tests, dependency advisories, CLI flags, and immutable build inputs.
=======
# .github

GitHub Actions for `fiducia-memory.rs` — CI (fmt, clippy `-D warnings`, locked tests,
`cargo audit`) plus the repo's deploy/docker/flags workflows where present.
Workflow actions are pinned to full commit SHAs per the fleet's
reproducible-build policy (audited by the monorepo's `audit-repo-state.sh`).
>>>>>>> b404e81 (Adopt fiducia-telemetry; add catch-panic layer; tracing for the migrate-only run)
