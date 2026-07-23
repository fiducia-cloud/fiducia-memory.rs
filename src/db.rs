//! SeaORM connection policy and canonical schema bootstrap for the shared brain.
//!
//! Migration files are embedded and executed in order as complete statements.
//! A SeaORM-owned ledger prevents replay and imports successful versions from
//! SQLx's legacy ledger so existing deployments upgrade without rerunning DDL.

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, DbErr, RuntimeErr,
    Statement, TransactionTrait,
};
use sha2::{Digest, Sha256, Sha384};
use std::{fmt::Write as _, net::IpAddr, time::Duration};
use url::Url;

pub const SCHEMA: &str = include_str!("../sql/fiducia_memory.sql");

#[derive(Clone, Copy)]
struct EmbeddedMigration {
    version: i64,
    description: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[EmbeddedMigration] = &[
    EmbeddedMigration {
        version: 1,
        description: "memory",
        sql: include_str!("../migrations/0001_memory.sql"),
    },
    EmbeddedMigration {
        version: 2,
        description: "fiducia memory",
        sql: include_str!("../migrations/0002_fiducia_memory.sql"),
    },
    EmbeddedMigration {
        version: 3,
        description: "force tenant row level security",
        sql: include_str!("../migrations/0003_rls_force.sql"),
    },
];

const CREATE_MIGRATION_LEDGER: &str = r#"
create table if not exists fiducia_memory_schema_migrations (
    version bigint primary key,
    description text not null,
    checksum text not null,
    source text not null check (source in ('legacy_sqlx', 'seaorm')),
    applied_at timestamptz not null default now()
)
"#;

const LEGACY_VERSIONS_SQL: &str =
    "select version, checksum from _sqlx_migrations where success = true order by version";

const MIGRATION_LOCK_KEY: i64 = 7_605_182_024_071_801;

pub async fn apply_schema(database: &DatabaseConnection) -> Result<(), DbErr> {
    let transaction = database.begin().await?;

    transaction
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "select pg_advisory_xact_lock($1)",
            [MIGRATION_LOCK_KEY.into()],
        ))
        .await?;
    transaction
        .execute_unprepared(CREATE_MIGRATION_LEDGER)
        .await?;
    import_legacy_sqlx_versions(&transaction).await?;

    for migration in MIGRATIONS {
        let checksum = migration_checksum(migration.sql);
        let existing = transaction
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "select checksum from fiducia_memory_schema_migrations where version = $1",
                [migration.version.into()],
            ))
            .await?;
        if let Some(existing) = existing {
            let recorded: String = existing.try_get("", "checksum")?;
            if recorded != checksum {
                return Err(DbErr::Migration(format!(
                    "embedded migration {} ({}) differs from the recorded checksum",
                    migration.version, migration.description
                )));
            }
            continue;
        }

        transaction.execute_unprepared(migration.sql).await?;
        record_migration(&transaction, migration, &checksum, "seaorm").await?;
    }
    transaction.commit().await
}

async fn import_legacy_sqlx_versions(
    transaction: &sea_orm::DatabaseTransaction,
) -> Result<(), DbErr> {
    let row = transaction
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "select to_regclass('_sqlx_migrations') is not null as present",
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("could not inspect the legacy migration ledger".into()))?;
    if !row.try_get::<bool>("", "present")? {
        return Ok(());
    }

    let legacy_versions = transaction
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            LEGACY_VERSIONS_SQL,
        ))
        .await?;
    for row in legacy_versions {
        let version: i64 = row.try_get("", "version")?;
        let Some(migration) = MIGRATIONS
            .iter()
            .find(|migration| migration.version == version)
        else {
            continue;
        };
        let legacy_checksum: Vec<u8> = row.try_get("", "checksum")?;
        let expected_legacy_checksum = Sha384::digest(migration.sql.as_bytes());
        if legacy_checksum.as_slice() != expected_legacy_checksum.as_slice() {
            return Err(DbErr::Migration(format!(
                "legacy SQLx migration {} ({}) differs from the embedded migration",
                migration.version, migration.description
            )));
        }
        let checksum = migration_checksum(migration.sql);
        record_migration(transaction, migration, &checksum, "legacy_sqlx").await?;
    }
    Ok(())
}

async fn record_migration(
    transaction: &sea_orm::DatabaseTransaction,
    migration: &EmbeddedMigration,
    checksum: &str,
    source: &str,
) -> Result<(), DbErr> {
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "insert into fiducia_memory_schema_migrations (version, description, checksum, source) values ($1, $2, $3, $4) on conflict (version) do nothing",
            [
                migration.version.into(),
                migration.description.into(),
                checksum.into(),
                source.into(),
            ],
        ))
        .await?;
    Ok(())
}

fn migration_checksum(sql: &str) -> String {
    let digest = Sha256::digest(sql.as_bytes());
    let mut checksum = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut checksum, "{byte:02x}").expect("writing to a String cannot fail");
    }
    checksum
}

pub async fn connect_database(
    url: &str,
    max_connections: u32,
) -> Result<DatabaseConnection, DbErr> {
    let mut options = secure_pg_connect_options(url)?;
    options
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5));
    Database::connect(options).await
}

/// Require certificate and hostname verification whenever PostgreSQL is not a
/// loopback host or Unix-domain socket.
pub fn secure_pg_connect_options(url: &str) -> Result<ConnectOptions, DbErr> {
    let parsed = Url::parse(url).map_err(configuration_error)?;
    let host = parsed.host_str().unwrap_or_default();
    let has_unix_socket = parsed
        .query_pairs()
        .any(|(key, value)| key == "host" && value.starts_with('/'));
    let is_local = has_unix_socket || is_loopback_postgres_host(host);
    let ssl_mode = parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "sslmode").then(|| value.into_owned()));
    if !is_local && ssl_mode.as_deref() != Some("verify-full") {
        return Err(configuration_error(format!(
            "PostgreSQL host {host:?} is not loopback; DATABASE_URL must set sslmode=verify-full"
        )));
    }
    Ok(ConnectOptions::new(url.to_string()))
}

fn is_loopback_postgres_host(host: &str) -> bool {
    let host = host.trim();
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("localhost.")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn configuration_error(error: impl std::fmt::Display) -> DbErr {
    DbErr::Conn(RuntimeErr::Internal(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{migration_checksum, secure_pg_connect_options, LEGACY_VERSIONS_SQL, MIGRATIONS};
    use std::collections::BTreeSet;

    #[test]
    fn embedded_migrations_have_stable_unique_versions_and_checksums() {
        let versions = MIGRATIONS
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>();
        assert_eq!(versions, vec![1, 2, 3]);
        assert_eq!(
            versions.iter().copied().collect::<BTreeSet<_>>().len(),
            MIGRATIONS.len()
        );
        for migration in MIGRATIONS {
            let checksum = migration_checksum(migration.sql);
            assert_eq!(checksum.len(), 64);
            assert!(checksum
                .chars()
                .all(|character| character.is_ascii_hexdigit()));
            assert!(!migration.description.trim().is_empty());
            assert!(!migration.sql.trim().is_empty());
        }
    }

    #[test]
    fn legacy_import_accepts_only_successful_sqlx_versions() {
        assert!(LEGACY_VERSIONS_SQL.contains("success = true"));
        assert!(!LEGACY_VERSIONS_SQL.contains("description"));
        assert!(LEGACY_VERSIONS_SQL.contains("checksum"));
    }

    #[test]
    fn remote_postgres_requires_verify_full() {
        assert!(secure_pg_connect_options("postgres://db.example.com/memory").is_err());
        assert!(
            secure_pg_connect_options("postgres://db.example.com/memory?sslmode=verify-full")
                .is_ok()
        );
    }

    #[test]
    fn local_postgres_may_use_plaintext() {
        for url in [
            "postgres://localhost/memory?sslmode=disable",
            "postgres://127.0.0.1/memory?sslmode=disable",
            "postgres://[::1]/memory?sslmode=disable",
            "postgres:///?host=/var/run/postgresql/&sslmode=disable",
        ] {
            assert!(secure_pg_connect_options(url).is_ok(), "rejected {url}");
        }
    }
}
