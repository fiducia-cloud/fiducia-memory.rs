# Fiducia Memory Agent Instructions

This `AGENTS.md` is the canonical instruction file for this repository. Tool-specific instruction files must point here rather than duplicate this content.

## Scope and precedence

When an agent starts from a working directory, it must:

1. Resolve the working directory to an absolute real path.
2. Walk only that directory and its ancestors through the filesystem root.
3. Collect every readable `AGENTS.md` encountered.
4. Resolve symlinks, deduplicate files by resolved path, and report unreadable files or cycles.
5. Apply the collected instructions from the filesystem root toward the working directory.

Do not search sibling directories. A nested `AGENTS.md` may refine a parent instruction for its subtree but must not silently discard a parent safety rule.

## Repository role

`fiducia-cloud/fiducia-memory.rs` is the canonical Rust memory service. The older non-suffixed `fiducia-cloud/fiducia-memory` repository is historical and must not receive new implementation work.

## Git and pull-request policy

- Work directly on the current `main` branch; do not create feature branches or Git worktrees.
- Preserve unrelated work and inspect both sides before resolving conflicts.
- Resolve conflicts semantically; never apply a repository-wide `ours` or `theirs` choice.
- Do not rebase shared branches or force-push `main`.
- Run the repository checks before publishing completed work to `origin/main`.
- Verify the published commit on `main`; do not claim completion from an unpushed local commit.

## Required validation

Run the checks relevant to the changed surface, including:

```sh
python3 scripts/check-agent-instructions.py --repo . --probe src
make -B -C vendor/flags-2-env all
vendor/flags-2-env/build/flags2env audit .cli-flags.toml
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
# RUSTSEC-2026-0235 is reachable only through rust_decimal's inactive rkyv feature.
cargo audit --ignore RUSTSEC-2026-0235
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

Before non-trivial work, search the project for an existing issue and update it instead of creating a duplicate. Link published changes and any pull requests to the canonical issue, keep status and blockers current, and file concrete follow-up work for any intentionally deferred scope.

## Repository-local Git worktrees

- Create or use a Git worktree only when the human operator explicitly authorizes it for the current task. Concurrency or a dirty checkout is not permission by itself.
- Put every authorized worktree at `<repository-root>/tmp/worktrees/<name>`; from the repository root, use `./tmp/worktrees/<name>`. Never place worktrees beside repositories or organization directories.
- Keep `tmp`, `temp`, `tmp/worktrees`, and `temp/worktrees` ignored in the repository-root `.gitignore`. Do not commit files from those directories.
- Relocate or remove a worktree only when the operator explicitly requests it. Before removal, preserve and publish intended changes, verify its commit is represented on the target branch, and confirm there are no tracked, untracked, ignored-sensitive, or in-use files that must survive. Remove it with `git worktree remove <path>` without `--force`; never delete a worktree directory with `rm`.

<!-- BEGIN ores-agents-pointer: managed by ORESoftware/my-ai; edit there, not here -->

## Canonical agent instructions

Before doing anything else in this repository, also read:

    .ores/agents/AGENTS.md

That path is a symlink to `~/codes/oresoftware/my-ai/AGENTS.md`, whose canonical copy is
<https://github.com/ORESoftware/my-ai/blob/main/AGENTS.md>.

It exists at a fixed path *inside* the repository because some agents cannot walk up past
the repository root, so machine-wide instructions one or more directories above are
invisible to them. This pointer plus that path make the same file reachable from a working
directory anywhere in the tree.

The symlink is deliberately **not committed**: it names an absolute path that is only valid
on a machine with `~/codes/oresoftware/my-ai` checked out, so committing it would produce a
broken link for everyone else and for CI. `.ores/` is git-ignored for that reason. If
`.ores/agents/AGENTS.md` is missing on your machine, create it with:

    mkdir -p .ores/agents
    ln -sfn "$HOME/codes/oresoftware/my-ai/AGENTS.md" .ores/agents/AGENTS.md

or run `~/codes/oresoftware/my-ai/scripts/link-repo-agents.sh` once to do it for every git
repository under `~/codes`, and `--check` to verify them.

A missing `.ores/agents/AGENTS.md` is a setup gap on the reader's machine, never a reason to
skip the canonical instructions: fetch them from the URL above instead.

<!-- END ores-agents-pointer -->
