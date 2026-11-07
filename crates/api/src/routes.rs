//! Routes, handlers and their responses.

use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Auction houses change only through migrations.
const CACHE_AUCTION_HOUSES: &str = "public, max-age=86400";
/// A new scan arrives every 4 hours; clients revalidate with the ETag.
const CACHE_SCANS: &str = "public, max-age=300";
/// The data of a scan never changes.
const CACHE_SNAPSHOT: &str = "public, max-age=31536000, immutable";
/// Item names are fetched once, when an item is first seen.
const CACHE_ITEMS: &str = "public, max-age=3600";
/// A new scan arrives every 4 hours.
const CACHE_HISTORY: &str = "public, max-age=300";
/// The page is embedded in the binary: revalidate after each deploy.
const CACHE_PAGE: &str = "no-cache";

/// Scans returned by one request when `limit` is not given, and at most.
const DEFAULT_SCAN_LIMIT: i64 = 1000;
const MAX_SCAN_LIMIT: i64 = 1000;

/// Items returned by a search when `limit` is not given, and at most.
const DEFAULT_ITEM_LIMIT: i64 = 20;
const MAX_ITEM_LIMIT: i64 = 50;
/// Longest accepted search text, in characters.
const MAX_QUERY_CHARS: usize = 100;

/// Item history: time range when `from` is not given, and at most.
const DEFAULT_HISTORY_DAYS: i64 = 30;
const MAX_HISTORY_DAYS: i64 = 366;

/// The web page: item search and price history chart.
const INDEX_HTML: &str = include_str!("../web/index.html");

pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/v1/auction-houses", get(auction_houses))
        .route("/v1/auction-houses/{id}/scans", get(scans))
        .route("/v1/auction-houses/{id}/scans/{scan_id}", get(snapshot))
        .route(
            "/v1/auction-houses/{id}/items/{item_id}/history",
            get(item_history),
        )
        .route("/v1/items", get(items))
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

#[derive(Deserialize)]
struct HistoryQuery {
    /// Start of the time range (inclusive). Default: `to` minus 30 days.
    from: Option<DateTime<Utc>>,
    /// End of the time range (inclusive). Default: now.
    to: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct History {
    auction_house_id: i16,
    item_id: i32,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    points: Vec<HistoryPoint>,
}

/// Statistics of one item in one scan. Prices are unit prices in copper.
#[derive(Serialize)]
struct HistoryPoint {
    /// When the scan started.
    ts: DateTime<Utc>,
    scan_id: i32,
    min_price: i64,
    p10_price: i64,
    p50_price: i64,
    p90_price: i64,
    quantity: i32,
    auctions: i32,
}

/// One item's statistics in one auction house, oldest first. Only scans that
/// stored a row for the item are returned: none for `unchanged` and `error`
/// scans, and none when the item was not listed.
async fn item_history(
    State(pool): State<PgPool>,
    Path((auction_house_id, item_id)): Path<(i16, i32)>,
    Query(query): Query<HistoryQuery>,
) -> Result<Response, ApiError> {
    let (from, to) = history_range(query.from, query.to, Utc::now())?;

    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM auction_house WHERE id = $1) AS "exists!""#,
        auction_house_id
    )
    .fetch_one(&pool)
    .await?;
    if !exists {
        return Err(ApiError::NotFound("no such auction house"));
    }

    let points = sqlx::query_as!(
        HistoryPoint,
        r#"
        SELECT ts, scan_id, min_price, p10_price, p50_price, p90_price, quantity, auctions
          FROM price_point
         WHERE auction_house_id = $1 AND item_id = $2 AND ts BETWEEN $3 AND $4
         ORDER BY ts
        "#,
        auction_house_id,
        item_id,
        from,
        to
    )
    .fetch_all(&pool)
    .await?;

    let history = History {
        auction_house_id,
        item_id,
        from,
        to,
        points,
    };
    Ok(([(CACHE_CONTROL, CACHE_HISTORY)], Json(history)).into_response())
}

/// The time range of a history request, checked and with its defaults.
fn history_range(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError> {
    let to = to.unwrap_or(now);
    let from = from.unwrap_or(to - TimeDelta::days(DEFAULT_HISTORY_DAYS));
    if from > to {
        return Err(ApiError::BadRequest("from must not be after to".to_owned()));
    }
    if to - from > TimeDelta::days(MAX_HISTORY_DAYS) {
        return Err(ApiError::BadRequest(format!(
            "the time range must not exceed {MAX_HISTORY_DAYS} days"
        )));
    }
    Ok((from, to))
}

#[derive(Deserialize)]
struct ItemsQuery {
    /// Text in the item name (case-insensitive), or an item id.
    q: String,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct Item {
    id: i32,
    /// en_US name; null when Blizzard has no metadata for the item.
    name: Option<String>,
    /// `POOR`, `COMMON`, `UNCOMMON`, `RARE`, ...; null when unknown.
    quality: Option<String>,
}

/// Items whose name contains `q`, plus the item whose id is `q`. The id match
/// comes first, then names starting with `q`, then shorter names.
async fn items(
    State(pool): State<PgPool>,
    Query(query): Query<ItemsQuery>,
) -> Result<Response, ApiError> {
    let text = query.q.trim();
    if text.is_empty() || text.chars().count() > MAX_QUERY_CHARS {
        return Err(ApiError::BadRequest(format!(
            "q must have 1 to {MAX_QUERY_CHARS} characters"
        )));
    }
    let limit = query.limit.unwrap_or(DEFAULT_ITEM_LIMIT);
    if !(1..=MAX_ITEM_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {MAX_ITEM_LIMIT}"
        )));
    }

    let id = text.parse::<i32>().ok();
    let escaped = escape_like(text);
    let items = sqlx::query_as!(
        Item,
        r#"
        SELECT id, name, quality
          FROM item
         WHERE id = $1 OR name ILIKE '%' || $2 || '%'
         ORDER BY id = $1 IS TRUE DESC,
                  name ILIKE $2 || '%' DESC,
                  length(name), name, id
         LIMIT $3
        "#,
        id,
        escaped,
        limit
    )
    .fetch_all(&pool)
    .await?;
    Ok(([(CACHE_CONTROL, CACHE_ITEMS)], Json(items)).into_response())
}

/// Escapes the `LIKE` wildcards `%` and `_` (and the escape character `\`) so
/// that `text` matches literally.
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

async fn index() -> Response {
    ([(CACHE_CONTROL, CACHE_PAGE)], Html(INDEX_HTML)).into_response()
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

    #[test]
    fn escape_like_escapes_wildcards() {
        assert_eq!(escape_like("Linen Cloth"), "Linen Cloth");
        assert_eq!(escape_like("100%_a\\b"), "100\\%\\_a\\\\b");
    }

    #[test]
    fn history_range_defaults_and_limits() {
        let now: DateTime<Utc> = "2026-11-07T12:00:00Z".parse().unwrap();
        let (from, to) = history_range(None, None, now).unwrap();
        assert_eq!(to, now);
        assert_eq!(to - from, TimeDelta::days(DEFAULT_HISTORY_DAYS));

        let early = now - TimeDelta::days(MAX_HISTORY_DAYS);
        assert!(history_range(Some(early), None, now).is_ok());
        let too_early = early - TimeDelta::seconds(1);
        assert!(history_range(Some(too_early), None, now).is_err());
        assert!(history_range(Some(now), Some(now - TimeDelta::seconds(1)), now).is_err());
    }
}
