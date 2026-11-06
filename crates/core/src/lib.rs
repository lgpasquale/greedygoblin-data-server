//! Types and database access shared by the collector and the API.

use std::fmt;
use std::str::FromStr;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// Database migrations in `crates/core/migrations`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Opens a connection pool and applies pending migrations.
pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// Opens a connection pool whose transactions are read-only, without applying
/// migrations: for the API, as the collector is the only writer.
pub async fn connect_read_only(
    database_url: &str,
    max_connections: u32,
) -> Result<PgPool, sqlx::Error> {
    let options: PgConnectOptions = database_url.parse()?;
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options.options([("default_transaction_read_only", "on")]))
        .await
}

/// A Battle.net API region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    Eu,
    Us,
    Kr,
    Tw,
}

impl Region {
    pub const ALL: [Region; 4] = [Region::Eu, Region::Us, Region::Kr, Region::Tw];

    /// Lowercase code, as used in API hosts, namespaces and the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Region::Eu => "eu",
            Region::Us => "us",
            Region::Kr => "kr",
            Region::Tw => "tw",
        }
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown region `{0}` (expected eu, us, kr or tw)")]
pub struct UnknownRegion(String);

impl FromStr for Region {
    type Err = UnknownRegion;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "eu" => Ok(Region::Eu),
            "us" => Ok(Region::Us),
            "kr" => Ok(Region::Kr),
            "tw" => Ok(Region::Tw),
            _ => Err(UnknownRegion(s.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_round_trips_through_its_code() {
        for region in Region::ALL {
            assert_eq!(region.as_str().parse::<Region>().unwrap(), region);
        }
        assert_eq!(" EU ".parse::<Region>().unwrap(), Region::Eu);
        assert!("cn".parse::<Region>().is_err());
    }
}
