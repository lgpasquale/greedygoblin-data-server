# greedygoblin-data-server

Price history of the **WoW: Forever** auction houses.

Blizzard's Battle.net API shows only the current state of each auction house.
This project scans every auction house of every region (EU, US, KR, TW) every 4
hours and keeps per-item statistics as a time series. A read-only HTTP API
serves that history, mainly to a companion program of the
[GreedyGoblin](https://github.com/lgpasquale/GreedyGoblin) add-on.

It has two services, both written in Rust:

- **collector**: scans at 00:30, 04:30, …, 20:30 UTC and stores the results in
  PostgreSQL + TimescaleDB.
- **api**: an HTTP server on port 7008, documented below. At `/` it also
  serves a web page to search an item and chart its price history.

## Data

Every region has 15 auction houses: one Alliance, one Horde and one neutral
(Blackwater) auction house for each ruleset. A **scan** fetches all of them.
For each auction house it stores one row per listed item:

| Field       | Meaning                                                      |
|-------------|--------------------------------------------------------------|
| `min_price` | lowest unit price                                            |
| `p10_price`, `p50_price`, `p90_price` | 10th, 50th and 90th percentile of the unit price, weighted by quantity (always an actually listed price) |
| `quantity`  | units on sale                                                |
| `auctions`  | number of auctions                                           |

Prices are integers in **copper**. Auctions without a buyout are ignored.
Variants of the same item (random suffixes, bonuses) are combined under one
`item_id`. An item missing from a successful scan was not on sale.

## API

Base URL: `/v1`. All responses are JSON and are gzip-compressed if the client
accepts it. No authentication is needed. Times are RFC 3339 in UTC.

Errors return `{"error": "message"}` with status 400 (bad parameter), 404 (not
found) or 500.

### `GET /v1/auction-houses`

Lists all auction houses. A client uses this list to find the `id` of its
auction house from region, ruleset and faction.

```json
[
  {"id": 1, "region": "eu", "ruleset": "pvp", "faction": "alliance", "blizzard_id": 110},
  {"id": 2, "region": "eu", "ruleset": "normal", "faction": "alliance", "blizzard_id": 111}
]
```

- `region`: `eu`, `us`, `kr` or `tw`.
- `ruleset`: `pvp`, `normal`, `rp` or `rp-pvp`. It is `null` for one group of
  auction houses whose ruleset is not known yet (empty so far, probably
  Hardcore).
- `faction`: `alliance`, `horde` or `blackwater` (neutral).

### `GET /v1/auction-houses/{id}/scans`

Lists the scans of one auction house, oldest first.

| Parameter | Meaning                                                       |
|-----------|---------------------------------------------------------------|
| `from`    | only scans started at or after this time, e.g. `2026-11-01T00:00:00Z` |
| `after`   | only scans with an id greater than this one                   |
| `limit`   | at most this many scans, 1–1000 (default 1000)                |

```json
[
  {"id": 2, "ts": "2026-11-06T08:17:33Z", "status": "ok", "last_modified": "2026-11-06T08:17:21Z"},
  {"id": 3, "ts": "2026-11-06T08:18:01Z", "status": "unchanged", "last_modified": "2026-11-06T08:17:21Z"}
]
```

- `ts`: when the scan started.
- `last_modified`: when Blizzard produced the data.
- `status`:
  - `ok`: the scan fetched data;
  - `unchanged`: Blizzard had not updated the data since the previous scan;
  - `error`: the scan failed, so there is no data.

To download only new scans, pass `after` = the last id already downloaded. If
exactly `limit` scans come back, there may be more: repeat the request with the
last id received.

### `GET /v1/auction-houses/{id}/scans/{scanId}`

Returns every item of one auction house in one scan, sorted by `item_id`.

```json
{
  "scan_id": 3,
  "auction_house_id": 1,
  "ts": "2026-11-06T08:18:01Z",
  "last_modified": "2026-11-06T08:17:21Z",
  "items": [
    {"item_id": 25, "min_price": 500000, "p10_price": 500000, "p50_price": 500000,
     "p90_price": 500000, "quantity": 1, "auctions": 1}
  ]
}
```

For an `unchanged` scan, the response contains the data of the last `ok` scan
before it, so a client can treat both statuses the same way. For an `error`
scan, the response is 404.

### `GET /v1/auction-houses/{id}/items/{itemId}/history`

Returns the statistics of one item in one auction house over time, oldest
first.

| Parameter | Meaning                                                       |
|-----------|---------------------------------------------------------------|
| `from`    | start of the time range (default: `to` minus 30 days)         |
| `to`      | end of the time range (default: now)                          |

The range may be at most 366 days.

```json
{
  "auction_house_id": 1,
  "item_id": 2589,
  "from": "2026-10-08T12:00:00Z",
  "to": "2026-11-07T12:00:00Z",
  "points": [
    {"ts": "2026-11-06T08:17:33Z", "scan_id": 2, "min_price": 26, "p10_price": 28,
     "p50_price": 32, "p90_price": 52, "quantity": 8822, "auctions": 345}
  ]
}
```

There is a point only for scans that stored data for the item: none when the
item was not listed, and none for `unchanged` or `error` scans.

### `GET /v1/items?q=&limit=`

Searches items by name (case-insensitive, any part of the name) or by id.

| Parameter | Meaning                                                       |
|-----------|---------------------------------------------------------------|
| `q`       | text to search, 1–100 characters; a number also matches that item id |
| `limit`   | at most this many items, 1–50 (default 20)                    |

```json
[
  {"id": 2589, "name": "Linen Cloth", "quality": "COMMON"}
]
```

The id match comes first, then names that start with `q`, then shorter names.
`name` and `quality` are `null` for items that Blizzard has no metadata for
(about a third of them); those can only be found by id.

### Caching

| Endpoint                | `Cache-Control`                     | Notes |
|-------------------------|-------------------------------------|-------|
| `/auction-houses`       | `public, max-age=86400`             | |
| `…/scans`               | `public, max-age=300`               | `ETag` changes when a new scan is stored; send it back in `If-None-Match` to get `304 Not Modified` |
| `…/scans/{scanId}`      | `public, max-age=31536000, immutable` | the data of a scan never changes |
| `…/items/{itemId}/history` | `public, max-age=300`            | |
| `/items`                | `public, max-age=3600`              | |

### Example: a companion that stays up to date

```sh
API=https://example.com   # wherever the API is served
# 1. Find the auction house (once)
curl -s $API/v1/auction-houses | jq '.[] | select(.region=="eu" and .ruleset=="pvp" and .faction=="horde") | .id'
# 2. List the scans not downloaded yet
curl -s "$API/v1/auction-houses/4/scans?after=$LAST_SCAN_ID"
# 3. Download each scan whose status is not "error"
curl -s --compressed $API/v1/auction-houses/4/scans/42
```

## Running it

You need Docker. To build or run the code outside Docker you also need
stable Rust and `sqlx-cli`.

```sh
cp .env.example .env              # then fill in the secrets
docker build -t greedygoblin-data-server .
docker compose up -d              # db, collector and api
curl -s localhost:7008/v1/auction-houses
```

Configuration comes only from environment variables (see `.env.example`):

| Variable                 | Used by       | Meaning |
|--------------------------|---------------|---------|
| `POSTGRES_PASSWORD`      | db            | database password; it goes into a URL, so avoid `@ / : ? # %` |
| `BLIZZARD_CLIENT_ID`, `BLIZZARD_CLIENT_SECRET` | collector | Battle.net API client ([develop.battle.net](https://develop.battle.net/access/clients)) |
| `REGIONS`                | collector     | regions to scan, comma-separated (default all) |
| `CORS_ALLOW_ORIGINS`     | api           | origins allowed by CORS, comma-separated, or `*` (default: no CORS) |
| `API_BIND`               | api           | listen address (default `0.0.0.0:7008`) |
| `RUST_LOG`               | collector, api | log level (default `info`) |

Development:

```sh
docker compose up -d db
set -a; . ./.env; set +a
cargo run -p collector -- scan eu   # one scan of one region, then exit
cargo run -p api
cargo test
cargo clippy --all-targets -- -D warnings
cargo sqlx prepare --workspace      # after changing a SQL query
```

Each push to `master` deploys to the VM through GitHub Actions
(`.github/workflows/deploy.yml`). TLS and rate limiting are left to a reverse
proxy in front of the API.

## License

MIT
