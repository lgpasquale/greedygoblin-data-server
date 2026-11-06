//! Collector service: scans the auction houses of every configured region on
//! a fixed schedule and stores per-item statistics.
//!
//! Usage:
//!   collector               run the schedule until stopped
//!   collector scan <region> run one scan of one region now, then exit

mod blizzard;
mod scan;
mod schedule;
mod stats;

use std::env;
use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, bail};
use chrono::Utc;
use sqlx::PgPool;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use wow_core::Region;

use crate::blizzard::Client;

/// Database connections: regions scan in parallel and item metadata is
/// written while other regions scan.
const DB_CONNECTIONS: u32 = 4;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let database_url = required_env("DATABASE_URL")?;
    let client = Client::new(
        required_env("BLIZZARD_CLIENT_ID")?,
        required_env("BLIZZARD_CLIENT_SECRET")?,
    )?;
    let pool = wow_core::connect(&database_url, DB_CONNECTIONS)
        .await
        .context("cannot connect to the database")?;

    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [] => run_schedule(pool, Arc::new(client), regions()?).await,
        [command, region] if command == "scan" => scan::run(&pool, &client, region.parse()?).await,
        _ => bail!("usage: collector [scan <region>]"),
    }
}

/// Runs each region's schedule in its own task until SIGTERM or Ctrl-C. A scan
/// in progress at that moment is abandoned; its transaction is not committed.
async fn run_schedule(
    pool: PgPool,
    client: Arc<Client>,
    regions: Vec<Region>,
) -> anyhow::Result<()> {
    for region in regions {
        let (pool, client) = (pool.clone(), client.clone());
        tokio::spawn(async move {
            loop {
                let next = schedule::next_slot(Utc::now());
                tracing::info!(%region, %next, "next scan");
                let wait = (next - Utc::now()).to_std().unwrap_or_default();
                tokio::time::sleep(wait).await;
                if let Err(error) = scan::run(&pool, &client, region).await {
                    tracing::error!(%region, error = format!("{error:#}"), "scan failed");
                }
            }
        });
    }

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("shutting down");
    Ok(())
}

/// Regions to scan, from `REGIONS` (comma-separated, default all).
fn regions() -> anyhow::Result<Vec<Region>> {
    match env::var("REGIONS") {
        Ok(value) if !value.trim().is_empty() => value
            .split(',')
            .map(|r| r.parse().map_err(Into::into))
            .collect(),
        _ => Ok(Region::ALL.to_vec()),
    }
}

fn required_env(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| format!("environment variable {name} is not set"))
}
