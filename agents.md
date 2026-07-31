# Fiducia Memory Agent Instructions

This lowercase `agents.md` is the canonical instruction file for this repository. Tool-specific instruction files must point here rather than duplicate this content.

## Scope and precedence

When an agent starts from a working directory, it must:

1. Resolve the working directory to an absolute real path.
2. Walk only that directory and its ancestors through the filesystem root.
3. Collect every readable lowercase `agents.md` encountered.
4. Resolve symlinks, deduplicate files by resolved path, and report unreadable files or cycles.
5. Apply the collected instructions from the filesystem root toward the working directory.

Do not search sibling directories. A nested `agents.md` may refine a parent instruction for its subtree but must not silently discard a parent safety rule.

## Repository role

`fiducia-cloud/fiducia-memory.rs` is the canonical Rust memory service. The older non-suffixed `fiducia-cloud/fiducia-memory` repository is historical and must not receive new implementation work.

## Git and pull-request policy

- Start non-trivial work from the current default branch on a focused feature branch.
- Preserve unrelated work and inspect both sides before resolving conflicts.
- Resolve conflicts semantically; never apply a repository-wide `ours` or `theirs` choice.
- Do not rebase shared branches or force-push `main`.
- Open a pull request, run the repository checks, and merge only when the final head is reviewable and green.
- Verify the merged commit on `main`; do not claim completion from a local or feature branch alone.

## Required validation

Run the checks relevant to the changed surface, including:

```sh
python3 scripts/check-agent-instructions.py --repo . --probe src
make -B -C vendor/flags-2-env all
vendor/flags-2-env/build/flags2env audit .cli-flags.toml
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo audit
```

Run `git diff --check` and scan recursively for unresolved conflict markers before publishing. Do not weaken tests or disable checks merely to make CI pass.

## Security and data handling

- Never commit credentials, tokens, private keys, customer data, production secrets, or copied secret values.
- Keep tenant identity and authorization decisions explicit at trust boundaries.
- Treat generated artifacts, caches, build outputs, and local environment files as non-source unless the repository deliberately tracks them.
- Record security-sensitive deferrals and live-environment blockers in Linear instead of describing unexecuted work as complete.

## GitHub and Linear coordination

- GitHub organization: `fiducia-cloud`
- Linear workspace/team: `denman` / `Denman` (`DEN`)
- Linear project: `github.com/fiducia-cloud`
- Current rollout issue: `DEN-133`

Before non-trivial work, search the project for an existing issue and update it instead of creating a duplicate. Link pull requests to the canonical issue, keep status and blockers current, and file concrete follow-up work for any intentionally deferred scope.
