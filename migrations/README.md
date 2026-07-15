# migrations

Forward-only sqlx migrations, applied automatically at service start (and by
`--migrate` / `FIDUCIA_MEMORY_MIGRATE=true` for a migrate-then-exit run).
sqlx checksums applied migrations: never edit an applied file — add a new one.
