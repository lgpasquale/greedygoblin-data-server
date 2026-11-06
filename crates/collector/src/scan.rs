//! One scan of one region: fetch every auction house, reduce the listings to
//! per-item statistics and store them.

use anyhow::Context;
use chrono::{DateTime, SubsecRound, Utc};
use futures_util::{StreamExt, stream};
use greedygoblin_core::Region;
use sqlx::PgPool;

use crate::blizzard::{AuctionKind, Client, Fetched};
use crate::stats::{self, ItemStats, Listing};

/// Item metadata requests in flight at the same time.
const ITEM_FETCH_CONCURRENCY: usize = 4;

/// What happened to one auction house in a scan.
struct HouseResult {
    auction_house_id: i16,
    status: &'static str,
    last_modified: Option<DateTime<Utc>>,
    listings: Option<i32>,
    error: Option<String>,
    stats: Vec<ItemStats>,
}

/// Scans every auction house of `region` and stores the result. One failing
/// auction house is recorded as `error` and does not stop the scan.
pub async fn run(pool: &PgPool, client: &Client, region: Region) -> anyhow::Result<()> {
    let started_at = Utc::now().trunc_subsecs(0);
    tracing::info!(%region, "scan started");

    let ruleset_id = client.first_ruleset_id(region).await?;
    let houses = sqlx::query!(
        r#"
        SELECT ah.id, ah.blizzard_id,
               (SELECT sah.last_modified
                  FROM scan_auction_house sah
                 WHERE sah.auction_house_id = ah.id AND sah.last_modified IS NOT NULL
                 ORDER BY sah.scan_id DESC
                 LIMIT 1) AS previous_last_modified
          FROM auction_house ah
         WHERE ah.region = $1
         ORDER BY ah.blizzard_id
        "#,
        region.as_str()
    )
    .fetch_all(pool)
    .await?;

    let api_ids = client.auction_house_ids(region, ruleset_id).await?;
    for id in &api_ids {
        if !houses.iter().any(|h| i64::from(h.blizzard_id) == *id) {
            tracing::warn!(%region, blizzard_id = id, "auction house not in the database, skipped");
        }
    }

    let mut results = Vec::with_capacity(houses.len());
    for house in &houses {
        let result = fetch_house(
            client,
            region,
            ruleset_id,
            i64::from(house.blizzard_id),
            house.previous_last_modified,
        )
        .await;
        results.push(match result {
            Ok(fetched) => HouseResult {
                auction_house_id: house.id,
                ..fetched
            },
            Err(error) => {
                let error = format!("{error:#}");
                tracing::error!(%region, blizzard_id = house.blizzard_id, error, "auction house failed");
                HouseResult {
                    auction_house_id: house.id,
                    status: "error",
                    last_modified: None,
                    listings: None,
                    error: Some(error),
                    stats: Vec::new(),
                }
            }
        });
    }

    let scan_id = store(pool, region, started_at, &results).await?;
    let rows: usize = results.iter().map(|r| r.stats.len()).sum();
    let failed = results.iter().filter(|r| r.status == "error").count();
    let unchanged = results.iter().filter(|r| r.status == "unchanged").count();
    tracing::info!(%region, scan_id, rows, failed, unchanged, "scan stored");

    let mut item_ids: Vec<i32> = results
        .iter()
        .flat_map(|r| r.stats.iter().map(|s| s.item_id))
        .collect();
    item_ids.sort_unstable();
    item_ids.dedup();
    if let Err(error) = fetch_missing_items(pool, client, region, &item_ids).await {
        tracing::error!(%region, error = format!("{error:#}"), "item metadata update failed");
    }
    Ok(())
}

/// Fetches the listings and commodities of one auction house. Both lists are
/// requested with `If-Modified-Since` set to the previous scan's
/// `last-modified`; if both are unchanged the result is `unchanged`.
async fn fetch_house(
    client: &Client,
    region: Region,
    ruleset_id: i64,
    blizzard_id: i64,
    previous: Option<DateTime<Utc>>,
) -> anyhow::Result<HouseResult> {
    let fetch = |kind, since| client.auctions(region, ruleset_id, blizzard_id, kind, since);

    let items = fetch(AuctionKind::Items, previous)
        .await
        .context("listings")?;
    let commodities = fetch(AuctionKind::Commodities, previous)
        .await
        .context("commodities")?;

    // Statistics combine both lists, so if only one changed the other is
    // fetched again in full.
    let (items, commodities) = match (items, commodities) {
        (Fetched::NotModified, Fetched::NotModified) => {
            return Ok(HouseResult {
                auction_house_id: 0,
                status: "unchanged",
                last_modified: previous,
                listings: None,
                error: None,
                stats: Vec::new(),
            });
        }
        (Fetched::NotModified, c) => (
            fetch(AuctionKind::Items, None).await.context("listings")?,
            c,
        ),
        (i, Fetched::NotModified) => (
            i,
            fetch(AuctionKind::Commodities, None)
                .await
                .context("commodities")?,
        ),
        both => both,
    };

    let mut last_modified = None;
    let mut total_listings = 0;
    let mut listings: Vec<Listing> = Vec::new();
    for fetched in [items, commodities] {
        if let Fetched::Data {
            last_modified: lm,
            listings: l,
            total_listings: t,
        } = fetched
        {
            last_modified = last_modified.max(lm);
            total_listings += t;
            listings.extend(l);
        }
    }

    Ok(HouseResult {
        auction_house_id: 0,
        status: "ok",
        last_modified,
        listings: Some(i32::try_from(total_listings).unwrap_or(i32::MAX)),
        error: None,
        stats: stats::aggregate(listings),
    })
}

/// Writes the scan, the state of each auction house and the price rows in one
/// transaction. Returns the scan id.
async fn store(
    pool: &PgPool,
    region: Region,
    started_at: DateTime<Utc>,
    results: &[HouseResult],
) -> anyhow::Result<i32> {
    let mut tx = pool.begin().await?;

    let scan_id = sqlx::query_scalar!(
        "INSERT INTO scan (region, started_at, finished_at) VALUES ($1, $2, now()) RETURNING id",
        region.as_str(),
        started_at
    )
    .fetch_one(&mut *tx)
    .await?;

    for r in results {
        sqlx::query!(
            r#"
            INSERT INTO scan_auction_house
                (scan_id, auction_house_id, status, last_modified, listings, error)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            scan_id,
            r.auction_house_id,
            r.status,
            r.last_modified,
            r.listings,
            r.error
        )
        .execute(&mut *tx)
        .await?;
    }

    let rows = results
        .iter()
        .flat_map(|r| r.stats.iter().map(move |s| (r.auction_house_id, s)));
    let capacity = results.iter().map(|r| r.stats.len()).sum();
    let mut columns = PriceColumns::with_capacity(capacity);
    for (auction_house_id, s) in rows {
        columns.push(auction_house_id, s);
    }
    sqlx::query!(
        r#"
        INSERT INTO price_point
            (ts, scan_id, auction_house_id, item_id,
             min_price, p10_price, p50_price, p90_price, quantity, auctions)
        SELECT $1, $2, *
          FROM UNNEST($3::smallint[], $4::integer[], $5::bigint[], $6::bigint[],
                      $7::bigint[], $8::bigint[], $9::integer[], $10::integer[])
        "#,
        started_at,
        scan_id,
        &columns.auction_house_id,
        &columns.item_id,
        &columns.min_price,
        &columns.p10_price,
        &columns.p50_price,
        &columns.p90_price,
        &columns.quantity,
        &columns.auctions
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(scan_id)
}

/// Price rows as one array per column, for `UNNEST`.
#[derive(Default)]
struct PriceColumns {
    auction_house_id: Vec<i16>,
    item_id: Vec<i32>,
    min_price: Vec<i64>,
    p10_price: Vec<i64>,
    p50_price: Vec<i64>,
    p90_price: Vec<i64>,
    quantity: Vec<i32>,
    auctions: Vec<i32>,
}

impl PriceColumns {
    fn with_capacity(n: usize) -> Self {
        Self {
            auction_house_id: Vec::with_capacity(n),
            item_id: Vec::with_capacity(n),
            min_price: Vec::with_capacity(n),
            p10_price: Vec::with_capacity(n),
            p50_price: Vec::with_capacity(n),
            p90_price: Vec::with_capacity(n),
            quantity: Vec::with_capacity(n),
            auctions: Vec::with_capacity(n),
        }
    }

    fn push(&mut self, auction_house_id: i16, s: &ItemStats) {
        self.auction_house_id.push(auction_house_id);
        self.item_id.push(s.item_id);
        self.min_price.push(s.min_price);
        self.p10_price.push(s.p10_price);
        self.p50_price.push(s.p50_price);
        self.p90_price.push(s.p90_price);
        self.quantity
            .push(i32::try_from(s.quantity).unwrap_or(i32::MAX));
        self.auctions
            .push(i32::try_from(s.auctions).unwrap_or(i32::MAX));
    }
}

/// Fetches metadata for items never seen before, and retries items the API
/// did not know (no name) once their last attempt is a week old. Failures are
/// logged and retried at the next scan.
async fn fetch_missing_items(
    pool: &PgPool,
    client: &Client,
    region: Region,
    item_ids: &[i32],
) -> anyhow::Result<()> {
    let missing = sqlx::query_scalar!(
        r#"
        SELECT t.id AS "id!"
          FROM UNNEST($1::integer[]) AS t(id)
         WHERE NOT EXISTS (
                 SELECT 1 FROM item i
                  WHERE i.id = t.id
                    AND (i.name IS NOT NULL OR i.updated_at > now() - interval '7 days'))
        "#,
        item_ids
    )
    .fetch_all(pool)
    .await?;
    if missing.is_empty() {
        return Ok(());
    }
    tracing::info!(%region, count = missing.len(), "fetching item metadata");

    let mut fetched = stream::iter(missing)
        .map(|id| async move { (id, client.item(region, id).await) })
        .buffer_unordered(ITEM_FETCH_CONCURRENCY);
    let (mut stored, mut unknown, mut failed) = (0, 0, 0);
    while let Some((id, result)) = fetched.next().await {
        let info = match result {
            Ok(info) => info,
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    item_id = id,
                    error = format!("{error:#}"),
                    "item metadata failed"
                );
                continue;
            }
        };
        if info.is_none() {
            unknown += 1;
        }
        let (name, quality, item_class) = match info {
            Some(i) => (Some(i.name), i.quality, i.item_class),
            None => (None, None, None),
        };
        sqlx::query!(
            r#"
            INSERT INTO item (id, name, quality, item_class, updated_at)
            VALUES ($1, $2, $3, $4, now())
            ON CONFLICT (id) DO UPDATE
               SET name = EXCLUDED.name, quality = EXCLUDED.quality,
                   item_class = EXCLUDED.item_class, updated_at = now()
            "#,
            id,
            name,
            quality,
            item_class
        )
        .execute(pool)
        .await?;
        stored += 1;
    }
    tracing::info!(%region, stored, unknown, failed, "item metadata updated");
    Ok(())
}
