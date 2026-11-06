//! Read-only HTTP API over the stored auction house history.
//!
//! Configuration (environment variables):
//!   DATABASE_URL         PostgreSQL connection string (required)
//!   API_BIND             listen address, default 0.0.0.0:7008
//!   CORS_ALLOW_ORIGINS   comma-separated origins allowed by CORS, or `*`;
//!                        unset or empty: no CORS headers

mod routes;

use std::env;
use std::io::IsTerminal;

use anyhow::Context;
use axum::http::{HeaderValue, Method};
use tokio::signal::unix::{SignalKind, signal};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing_subscriber::EnvFilter;

/// Database connections: requests are short single queries.
const DB_CONNECTIONS: u32 = 4;

const DEFAULT_BIND: &str = "0.0.0.0:7008";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let database_url =
        env::var("DATABASE_URL").context("environment variable DATABASE_URL is not set")?;
    let pool = greedygoblin_core::connect_read_only(&database_url, DB_CONNECTIONS)
        .await
        .context("cannot connect to the database")?;

    let mut app = routes::router(pool).layer(CompressionLayer::new().gzip(true));
    if let Some(cors) = cors_layer()? {
        app = app.layer(cors);
    }

    let bind = env::var("API_BIND")
        .ok()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("cannot listen on {bind}"))?;
    tracing::info!(%bind, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    tracing::info!("shut down");
    Ok(())
}

/// CORS from `CORS_ALLOW_ORIGINS`: `*` allows any origin, otherwise a
/// comma-separated list of origins. None when unset or empty.
fn cors_layer() -> anyhow::Result<Option<CorsLayer>> {
    let value = env::var("CORS_ALLOW_ORIGINS").unwrap_or_default();
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let origin = if value == "*" {
        AllowOrigin::any()
    } else {
        let origins = value
            .split(',')
            .map(|o| HeaderValue::from_str(o.trim()))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid origin in CORS_ALLOW_ORIGINS")?;
        AllowOrigin::list(origins)
    };
    Ok(Some(
        CorsLayer::new()
            .allow_origin(origin)
            .allow_methods([Method::GET, Method::HEAD]),
    ))
}

async fn shutdown() {
    let mut terminate = signal(SignalKind::terminate()).expect("cannot listen for SIGTERM");
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
