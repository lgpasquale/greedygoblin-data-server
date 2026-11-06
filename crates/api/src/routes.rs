//! Routes, handlers and their responses.

use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Auction houses change only through migrations.
const CACHE_AUCTION_HOUSES: &str = "public, max-age=86400";
/// A new scan arrives every 4 hours; clients revalidate with the ETag.
const CACHE_SCANS: &str = "public, max-age=300";
/// The data of a scan never changes.
const CACHE_SNAPSHOT: &str = "public, max-age=31536000, immutable";

/// Scans returned by one request when `limit` is not given, and at most.
const DEFAULT_SCAN_LIMIT: i64 = 1000;
const MAX_SCAN_LIMIT: i64 = 1000;

pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/v1/auction-houses", get(auction_houses))
        .route("/v1/auction-houses/{id}/scans", get(scans))
        .route("/v1/auction-houses/{id}/scans/{scan_id}", get(snapshot))
        .fallback(|| async { ApiError::NotFound("no such endpoint") })
        .with_state(pool)
}

#[derive(Serialize)]
struct AuctionHouse {
    id: i16,
    region: String,
    /// Ruleset slug (`pvp`, `normal`, `rp`, `rp-pvp`); null for the group
    /// whose ruleset is not known yet.
    ruleset: Option<String>,
    faction: String,
    blizzard_id: i16,
}

async fn auction_houses(State(pool): State<PgPool>) -> Result<Response, ApiError> {
    let houses = sqlx::query_as!(
        AuctionHouse,
        r#"
        SELECT ah.id, ah.region, r.slug AS "ruleset?", ah.faction, ah.blizzard_id
          FROM auction_house ah
          LEFT JOIN ruleset r ON r.id = ah.ruleset_id
         ORDER BY ah.region, ah.blizzard_id
        "#
    )
    .fetch_all(&pool)
    .await?;
    Ok(([(CACHE_CONTROL, CACHE_AUCTION_HOUSES)], Json(houses)).into_response())
}

#[derive(Deserialize)]
struct ScansQuery {
    /// Only scans started at or after this time.
    from: Option<DateTime<Utc>>,
    /// Only scans with a greater id (to continue a previous request).
    after: Option<i32>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct Scan {
    id: i32,
    /// When the scan started: the time of its price rows.
    ts: DateTime<Utc>,
    /// `ok`, `unchanged` (same data as the previous scan) or `error` (no data).
    status: String,
    /// Blizzard's `last-modified`: when the data was produced.
    last_modified: Option<DateTime<Utc>>,
}

/// Scans of one auction house, oldest first.
async fn scans(
    State(pool): State<PgPool>,
    Path(auction_house_id): Path<i16>,
    Query(query): Query<ScansQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_SCAN_LIMIT);
    if !(1..=MAX_SCAN_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {MAX_SCAN_LIMIT}"
        )));
    }

    let latest = sqlx::query_scalar!(
        r#"
        SELECT (SELECT max(scan_id) FROM scan_auction_house WHERE auction_house_id = ah.id)
          FROM auction_house ah
         WHERE ah.id = $1
        "#,
        auction_house_id
    )
    .fetch_optional(&pool)
    .await?
    .ok_or(ApiError::NotFound("no such auction house"))?;

    // The list changes only when a new scan is stored.
    let etag = format!("\"{}\"", latest.unwrap_or(0));
    if if_none_match(&headers, &etag) {
        return Ok(not_modified(&etag, CACHE_SCANS));
    }

    let scans = sqlx::query_as!(
        Scan,
        r#"
        SELECT s.id, s.started_at AS ts, sah.status, sah.last_modified
          FROM scan_auction_house sah
          JOIN scan s ON s.id = sah.scan_id
         WHERE sah.auction_house_id = $1
           AND ($2::timestamptz IS NULL OR s.started_at >= $2)
           AND ($3::integer IS NULL OR s.id > $3)
         ORDER BY s.id
         LIMIT $4
        "#,
        auction_house_id,
        query.from,
        query.after,
        limit
    )
    .fetch_all(&pool)
    .await?;

    Ok((
        [
            (CACHE_CONTROL, HeaderValue::from_static(CACHE_SCANS)),
            (ETAG, header_value(&etag)),
        ],
        Json(scans),
    )
        .into_response())
}

#[derive(Serialize)]
struct Snapshot {
    scan_id: i32,
    auction_house_id: i16,
    ts: DateTime<Utc>,
    last_modified: Option<DateTime<Utc>>,
    items: Vec<ItemPrice>,
}

/// Statistics of one item in one scan. Prices are unit prices in copper.
#[derive(Serialize)]
struct ItemPrice {
    item_id: i32,
    min_price: i64,
    p10_price: i64,
    p50_price: i64,
    p90_price: i64,
    quantity: i32,
    auctions: i32,
}

/// Every item of one auction house in one scan. A scan whose data was
/// `unchanged` returns the data of the last `ok` scan before it; a scan that
/// failed (`error`) has no data.
async fn snapshot(
    State(pool): State<PgPool>,
    Path((auction_house_id, scan_id)): Path<(i16, i32)>,
) -> Result<Response, ApiError> {
    let scan = sqlx::query!(
        r#"
        SELECT s.started_at, sah.status, sah.last_modified,
               CASE sah.status
                 WHEN 'ok' THEN s.started_at
                 WHEN 'unchanged' THEN (
                   SELECT ps.started_at
                     FROM scan_auction_house p
                     JOIN scan ps ON ps.id = p.scan_id
                    WHERE p.auction_house_id = sah.auction_house_id
                      AND p.status = 'ok'
                      AND p.scan_id < sah.scan_id
                    ORDER BY p.scan_id DESC
                    LIMIT 1)
               END AS data_ts
          FROM scan_auction_house sah
          JOIN scan s ON s.id = sah.scan_id
         WHERE sah.scan_id = $1 AND sah.auction_house_id = $2
        "#,
        scan_id,
        auction_house_id
    )
    .fetch_optional(&pool)
    .await?
    .ok_or(ApiError::NotFound("no such scan for this auction house"))?;

    let Some(data_ts) = scan.data_ts else {
        return Err(ApiError::NotFound(
            "this scan has no data for this auction house",
        ));
    };

    let items = sqlx::query_as!(
        ItemPrice,
        r#"
        SELECT item_id, min_price, p10_price, p50_price, p90_price, quantity, auctions
          FROM price_point
         WHERE auction_house_id = $1 AND ts = $2
         ORDER BY item_id
        "#,
        auction_house_id,
        data_ts
    )
    .fetch_all(&pool)
    .await?;

    let snapshot = Snapshot {
        scan_id,
        auction_house_id,
        ts: scan.started_at,
        last_modified: scan.last_modified,
        items,
    };
    Ok(([(CACHE_CONTROL, CACHE_SNAPSHOT)], Json(snapshot)).into_response())
}

/// Whether the request's `If-None-Match` lists `etag` (or is `*`).
fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|tag| tag.trim().trim_start_matches("W/"))
        .any(|tag| tag == etag || tag == "*")
}

fn not_modified(etag: &str, cache_control: &'static str) -> Response {
    (
        StatusCode::NOT_MODIFIED,
        [
            (CACHE_CONTROL, HeaderValue::from_static(cache_control)),
            (ETAG, header_value(etag)),
        ],
    )
        .into_response()
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("ETag is ASCII")
}

/// Errors returned to the client as `{"error": "..."}`.
#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    NotFound(&'static str),
    Database(sqlx::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        ApiError::Database(error)
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message.as_str()),
            ApiError::NotFound(message) => (StatusCode::NOT_FOUND, *message),
            ApiError::Database(error) => {
                tracing::error!(%error, "database error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn if_none_match_compares_entity_tags() {
        assert!(if_none_match(&headers("\"42\""), "\"42\""));
        assert!(if_none_match(&headers("\"1\", W/\"42\""), "\"42\""));
        assert!(if_none_match(&headers("*"), "\"42\""));
        assert!(!if_none_match(&headers("\"41\""), "\"42\""));
        assert!(!if_none_match(&HeaderMap::new(), "\"42\""));
    }
}
