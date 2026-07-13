use axum::http::StatusCode;
use fiducia_memory::{router, store::MemoryStore};
use sqlx::postgres::PgPoolOptions;
use std::{net::SocketAddr, time::Duration};
use tokio::net::TcpListener;
use tower_http::{limit::RequestBodyLimitLayer, timeout::TimeoutLayer, trace::TraceLayer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL must be configured")?;
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await?;
    let store = MemoryStore::new(pool);
    store.migrate().await?;
    let app = router(store)
        .layer(RequestBodyLimitLayer::new(2 * 1024 * 1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(10),
        ))
        .layer(TraceLayer::new_for_http());
    let address: SocketAddr = std::env::var("FIDUCIA_MEMORY_COMPAT_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8090".into())
        .parse()?;
    let listener = TcpListener::bind(address).await?;
    tracing::info!(%address, "fiducia-memory compatibility API listening");
    axum::serve(listener, app).await?;
    Ok(())
}
