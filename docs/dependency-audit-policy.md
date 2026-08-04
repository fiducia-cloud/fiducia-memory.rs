# Dependency audit policy

Fiducia memory runs `cargo audit` in CI. An advisory may be ignored only when the
repository also carries a machine-enforced proof that the affected package is
absent from the active Cargo feature graph.

## RUSTSEC-2026-0235 / `rkyv 0.7.46`

`rust_decimal 1.42.1` declares optional support for `rkyv 0.7.46`. The package is
retained in this repository's lockfile metadata through SeaORM's optional
decimal dependency, even though the control-plane feature set does not enable
`with-rust_decimal` or the `rkyv` feature.

The CI exception is therefore paired with this required guard:

```sh
reverse_tree="$(cargo tree --locked -i rkyv@0.7.46)"
test -z "$reverse_tree"
```

CI fails before `cargo audit --ignore RUSTSEC-2026-0235` whenever any current or
future feature makes the vulnerable package reachable. This is not a general
allowlist and does not claim that the vulnerable code is safe. It proves the
package is not linked into the tested feature graph.

A full lockfile regeneration was reviewed and rejected because it would combine
the advisory cleanup with dozens of unrelated dependency upgrades, including
SeaORM and Arrow. `cargo update --workspace` produced no change, confirming that
Cargo intentionally retains the optional package metadata.

## Removal criteria

Remove the exception immediately when one of these becomes true:

- `rust_decimal` stops declaring the vulnerable `rkyv` dependency;
- SeaORM no longer carries the optional decimal path in the resolved lockfile;
- the repository deliberately upgrades the database dependency graph through a
  separately reviewed PR;
- Cargo or cargo-audit gains a supported active-feature-only audit mode.

Any feature request that enables decimal or rkyv support must first replace the
vulnerable dependency path and delete the exception in the same change.
