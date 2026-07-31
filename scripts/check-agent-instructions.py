#!/usr/bin/env python3
"""Validate canonical, hierarchical agent instructions for this repository."""

from __future__ import annotations

import argparse
import os
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

CANONICAL_NAME = "agents.md"
ROOT_COMPAT_POINTER = Path("AGENTS.md")
TOOL_POINTERS = (
    Path(".claude/CLAUDE.md"),
    Path(".gemini/GEMINI.md"),
    Path(".openai/AGENTS.md"),
)
MAX_POINTER_BYTES = 1024
MAX_POINTER_LINES = 12


class ValidationError(RuntimeError):
    """Raised when the agent-instruction contract is invalid."""


@dataclass(frozen=True)
class InstructionFile:
    source: Path
    resolved: Path


def discover_instruction_chain(start: Path) -> list[InstructionFile]:
    """Return readable lowercase agents.md files from filesystem root to start."""

    try:
        current = start.resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise ValidationError(f"cannot resolve working path {start}: {error}") from error
    if current.is_file():
        current = current.parent
    if not current.is_dir():
        raise ValidationError(f"working path is not a directory: {current}")

    discovered: list[InstructionFile] = []
    seen: set[Path] = set()
    for directory in (current, *current.parents):
        candidate = directory / CANONICAL_NAME
        if not candidate.exists() and not candidate.is_symlink():
            continue
        try:
            resolved = candidate.resolve(strict=True)
        except (OSError, RuntimeError) as error:
            raise ValidationError(
                f"cannot resolve instruction file {candidate}: {error}"
            ) from error
        if not resolved.is_file():
            raise ValidationError(f"instruction path is not a file: {candidate}")
        if not os.access(resolved, os.R_OK):
            raise ValidationError(f"instruction file is not readable: {candidate}")
        if resolved in seen:
            continue
        seen.add(resolved)
        discovered.append(InstructionFile(source=candidate, resolved=resolved))

    discovered.reverse()
    return discovered


def read_utf8(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ValidationError(f"cannot read UTF-8 file {path}: {error}") from error


def validate_pointer(path: Path, canonical: Path, required_reference: str) -> None:
    if not path.exists() and not path.is_symlink():
        raise ValidationError(f"missing agent pointer: {path}")

    if path.is_symlink():
        try:
            resolved = path.resolve(strict=True)
        except (OSError, RuntimeError) as error:
            raise ValidationError(f"broken or cyclic agent pointer {path}: {error}") from error
        if resolved != canonical:
            raise ValidationError(
                f"agent pointer {path} resolves to {resolved}, expected {canonical}"
            )
        return

    if not path.is_file():
        raise ValidationError(f"agent pointer is not a regular file or symlink: {path}")

    try:
        size = path.stat().st_size
    except OSError as error:
        raise ValidationError(f"cannot stat agent pointer {path}: {error}") from error
    if size > MAX_POINTER_BYTES:
        raise ValidationError(
            f"agent pointer {path} is {size} bytes; pointer files must stay under "
            f"{MAX_POINTER_BYTES} bytes"
        )

    content = read_utf8(path)
    if required_reference not in content:
        raise ValidationError(
            f"agent pointer {path} must reference {required_reference}"
        )
    if len(content.splitlines()) > MAX_POINTER_LINES:
        raise ValidationError(
            f"agent pointer {path} duplicates guidance instead of remaining a pointer"
        )
    if content.strip() == read_utf8(canonical).strip():
        raise ValidationError(f"agent pointer {path} duplicates canonical instructions")


def assert_chain(
    actual: Sequence[InstructionFile],
    expected_resolved: Sequence[Path],
    context: str,
) -> None:
    actual_resolved = [entry.resolved for entry in actual]
    if actual_resolved != list(expected_resolved):
        actual_text = ", ".join(str(path) for path in actual_resolved) or "<empty>"
        expected_text = ", ".join(str(path) for path in expected_resolved) or "<empty>"
        raise ValidationError(
            f"{context} chain mismatch; expected [{expected_text}], got [{actual_text}]"
        )


def validate_discovery_fixture() -> None:
    """Prove root-to-leaf order, sibling exclusion, and real-path deduplication."""

    with tempfile.TemporaryDirectory(prefix="fiducia-agent-instructions-") as temp:
        root = Path(temp).resolve()
        root_agents = root / CANONICAL_NAME
        root_agents.write_text("# fixture root\n", encoding="utf-8")

        service = root / "service"
        deep = service / "src" / "nested"
        deep.mkdir(parents=True)
        service_agents = service / CANONICAL_NAME
        service_agents.write_text("# fixture service\n", encoding="utf-8")

        sibling = root / "sibling"
        sibling.mkdir()
        (sibling / CANONICAL_NAME).write_text("# must not be loaded\n", encoding="utf-8")

        alias = deep / CANONICAL_NAME
        try:
            alias.symlink_to(service_agents)
        except OSError:
            # Some local platforms disallow unprivileged symlinks. The Linux CI
            # runner exercises the deduplication path; order and sibling
            # exclusion remain covered everywhere.
            pass

        chain = discover_instruction_chain(deep)
        assert_chain(
            chain,
            (root_agents.resolve(), service_agents.resolve()),
            "nested fixture",
        )


def validate_repository(repo_arg: Path, probe_arg: Path) -> list[InstructionFile]:
    try:
        repo = repo_arg.resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise ValidationError(f"cannot resolve repository {repo_arg}: {error}") from error
    if not repo.is_dir():
        raise ValidationError(f"repository path is not a directory: {repo}")

    canonical_path = repo / CANONICAL_NAME
    if canonical_path.is_symlink() or not canonical_path.is_file():
        raise ValidationError(
            f"canonical {CANONICAL_NAME} must be a regular file: {canonical_path}"
        )
    if not os.access(canonical_path, os.R_OK):
        raise ValidationError(f"canonical instructions are unreadable: {canonical_path}")
    if not read_utf8(canonical_path).strip():
        raise ValidationError(f"canonical instructions are empty: {canonical_path}")
    canonical = canonical_path.resolve(strict=True)

    validate_pointer(repo / ROOT_COMPAT_POINTER, canonical, "./agents.md")
    for relative in TOOL_POINTERS:
        validate_pointer(repo / relative, canonical, "../agents.md")

    probe = probe_arg if probe_arg.is_absolute() else repo / probe_arg
    try:
        probe = probe.resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise ValidationError(f"cannot resolve probe path {probe}: {error}") from error
    try:
        probe.relative_to(repo)
    except ValueError as error:
        raise ValidationError(f"probe path escapes repository: {probe}") from error

    chain = discover_instruction_chain(probe)
    canonical_occurrences = sum(entry.resolved == canonical for entry in chain)
    if canonical_occurrences != 1:
        raise ValidationError(
            f"expected canonical {canonical} exactly once in chain, "
            f"found {canonical_occurrences}"
        )

    validate_discovery_fixture()
    return chain


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Validate lowercase canonical agents.md, minimal tool pointers, and "
            "root-to-leaf hierarchical discovery."
        )
    )
    parser.add_argument("--repo", type=Path, default=Path("."))
    parser.add_argument(
        "--probe",
        type=Path,
        default=Path("src"),
        help="existing repository path from which to demonstrate discovery",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        chain = validate_repository(args.repo, args.probe)
    except ValidationError as error:
        print(f"agent instruction validation failed: {error}", file=os.sys.stderr)
        return 1

    print("agent instruction chain (filesystem root -> working directory):")
    for entry in chain:
        if entry.source == entry.resolved:
            print(f"- {entry.source}")
        else:
            print(f"- {entry.source} -> {entry.resolved}")
    print(
        "validated canonical agents.md, root compatibility pointer, "
        f"{len(TOOL_POINTERS)} tool pointers, and nested discovery fixture"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
