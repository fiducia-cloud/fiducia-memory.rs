# Cargo audit policy

This directory contains project-local Cargo security-audit configuration.

`audit.toml` has one deliberately narrow exception: `RUSTSEC-2023-0071` is
present only through SQLx's inactive MySQL macro dependency in `Cargo.lock`.
Fiducia Memory enables PostgreSQL only, and `cargo tree --target all -i rsa`
does not show a reachable RSA private-key use in this service. Remove the
exception as soon as SQLx no longer locks that inactive dependency or an
upstream fixed release is available.

Do not add broad advisory suppressions here. Each exception must name the
affected dependency, explain why the service is not exposed, and state the
condition for removal.
