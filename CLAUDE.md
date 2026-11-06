# CLAUDE.md

## Project

`greedygoblin-data-server` collects auction house data for **WoW: Forever** from the official
Blizzard Battle.net API and keeps it as **time series**. Blizzard serves only the
current auction state, not past states. The history that this project builds is
the data we keep.

There are two parts:

1. **Collector**: a long-running service that scans every region 6 times per day
   (every 4 hours). It downloads the auction data, reduces it to per-item
   statistics and writes them to the database.
2. **API server**: a read-only HTTP API that serves the stored history.

Both run on a **small VM**, next to the database, with Docker Compose. Disk use
is the main constraint (the history only grows). About 1 GB of RAM is available
for the database. Keep CPU and RAM use of our own services low.

## Conventions

- All code, comments, identifiers, commit messages and docs are in **English**.
- Prices are integers in **copper**. Never use floating point for prices.
- Timestamps are `timestamptz` in the database, UTC everywhere.
- Never write secrets (Blizzard client id and secret) into the repo. Read them from
  environment variables (`BLIZZARD_CLIENT_ID`, `BLIZZARD_CLIENT_SECRET`,
  `DATABASE_URL`). Locally they are set in `.env` (not committed; see
  `.env.example`); on the VM they come from GitHub secrets (see Deployment).
- All configuration comes from environment variables. No config file.
- The main branch is `master`.
- Build only what is asked. Do not add features, storage or data that were not
  requested; propose them instead.

## Stack

- **Language: Rust** (stable). It is a Cargo workspace:
  - `crates/core` (package `greedygoblin-core`; not `core`, which would shadow the
    standard library crate): shared types (`Region`), migrations (`MIGRATOR`),
    connection setup.
  - `crates/collector`: a long-running service with its own schedule (see below).
    `collector scan <region>` runs one scan now and exits (for testing).
    Modules: `blizzard` (API client), `stats` (per-item statistics), `scan`
    (one region scan and its storage), `schedule` (scan slots).
  - `crates/api` (not created yet): an `axum` + `tokio` HTTP server.
- **HTTP client**: `reqwest` with `rustls`, gzip and JSON. Parse the auction
  payloads with `serde`. Deserialize only the fields we use.
- **Database: PostgreSQL 18 + TimescaleDB 2.31** (image
  `timescale/timescaledb:2.31.0-pg18`, not `-oss`: compression needs the full
  image), in `docker-compose.yml` as service `db`. Access through `sqlx`
  (`postgres`, `runtime-tokio`, `tls-rustls`) with compile-time checked queries
  and `sqlx::migrate!`. The collector is the only writer.
- **Scheduling**: inside the collector. For each region it runs a scan every 4
  hours at 00:30, 04:30, ..., 20:30 UTC, the same for every region
  (`schedule::SCAN_MINUTE`). The minute is not tied to Blizzard's refresh,
  which moves (see the Blizzard API section); the real data time is stored
  in `scan_auction_house.last_modified`.
  Regions listed in `REGIONS` (comma-separated, default all four). One region's scan
  failing must not affect the others. On start, it does not catch up on missed
  scans; it waits for the next slot.
- **Docker**: one image, `greedygoblin-data-server`, built by `Dockerfile` from this
  workspace with every binary (`collector`, later `api`) on `debian:trixie-slim`.
  The build uses `SQLX_OFFLINE=true`. `docker-compose.yml` runs `db` and
  `collector` (`api` once it exists); the services use
  `greedygoblin-data-server:${IMAGE_TAG:-latest}`. Their `DATABASE_URL` is built from
  `POSTGRES_*`, so the password must not contain URL special characters.
- **Deployment**: GitHub Actions on push to `master`. The runner builds the image,
  tags it with the commit SHA, and copies it to the VM over SSH without a
  registry: `docker save <image> | zstd | ssh <vm> 'zstd -d | docker load'`.
  Then it copies `docker-compose.yml` to the VM, runs `IMAGE_TAG=<sha> docker
  compose up -d` there and removes older images. The services use the local
  image (`pull_policy: never`). Workflow: `.github/workflows/deploy.yml`; it
  needs the secrets `SSH_PRIVATE_KEY`, `SSH_KNOWN_HOSTS`,
  `BLIZZARD_CLIENT_ID`, `BLIZZARD_CLIENT_SECRET` and `POSTGRES_PASSWORD`. Host,
  port, user, directory and `DEPLOY_VOLUMES_DIR` (database data) are in the
  workflow's `env`. At each deploy the workflow writes `.env` in the deploy
  directory on the VM from these secrets, so compose can also be run by hand
  there. The values are put in the SSH command line: keep them free of
  `$`, `"`, `\` and backticks. The TimescaleDB image is pulled from Docker Hub
  as usual. A reverse proxy (Caddy or nginx) provides TLS in front of the API. The
  database port is bound to `127.0.0.1` only.

Reasons: the collector and the API share the same types and queries. A Rust
binary uses a few MB of RAM and needs no runtime. TimescaleDB compresses old
time-series data several times (6.6× measured; columnar storage grouped by series),
which matters because disk is the constraint, and it can downsample old data
with continuous aggregates. Plain PostgreSQL would take more disk than SQLite
for small rows (~28 bytes of overhead per row).

## Blizzard API (WoW: Forever)

The API is not officially documented. Reference:
<https://mhuijskens.com/WoWForever/Api/>. Endpoints can change without notice, so
code against them defensively and log any non-200 response clearly.

- Auth: client-credentials OAuth. `POST https://oauth.battle.net/token` with
  `grant_type=client_credentials` and HTTP basic auth. The token lasts 24 h
  (`expires_in: 86399`). Request one per run, or cache it and request a new one
  on 401.
- Hosts: `{region}.api.blizzard.com` (`eu`, `us`, `kr`, `tw`).
- Namespaces: `dynamic-forever-{region}` (auctions, rulesets) and
  `static-forever-{region}` (item metadata). Pass the namespace as the
  `namespace` query parameter.
- Realms and connected realms **do not exist** in Forever. The API exposes
  **rulesets** (1 PvP, 2 Normal, 3 Roleplay, 15 Roleplay PvP; KR/TW have only 1
  and 2), but **the ruleset in the auction URLs is ignored**: every valid ruleset
  of a region returns the same 15 auction houses with byte-identical data
  (verified by hashing the listings; an unknown ruleset id gives 404). Fetch each
  auction house **once per region**, through any valid ruleset (use the first one
  from the ruleset index), never once per ruleset.
  - `GET /data/wow/ruleset/index`
  - `GET /data/wow/ruleset/{rulesetId}/auctions/index`: list of auction houses.
    `name` is a **localized object** (`{"en_GB": ..., "it_IT": ...}`), not a string.
  - `GET /data/wow/ruleset/{rulesetId}/auctions/{ahId}`: non-commodity listings.
    Fields: `id`, `item{id, bonus_lists?, context?, modifiers?}`, `buyout`, `bid?`,
    `quantity` (always 1 observed), `time_left`.
  - `GET /data/wow/ruleset/{rulesetId}/auctions/{ahId}/commodities`: commodity
    listings. Fields: `id`, `item{id}`, `quantity`, `unit_price`, `time_left`.
  - There is no region-wide commodities endpoint.
- Item metadata: `GET /data/wow/item/{id}?locale=en_US` (static namespace).
  Store the **`en_US`** name only. Fetch an item once, when it is first seen.
  **About 35% of listed items return 404** (e.g. 4564, 15248: gear with random
  suffixes). They are stored with `name` NULL and retried once a week.
- Rate limits are not documented for Forever. Assume the standard Blizzard limits
  (100 req/s, 36,000 req/h) and stay well below them: fetch auction houses one
  after another or with low concurrency.
- Responses include a `last-modified` header (header names are lowercase, so
  match them case-insensitively). Auction endpoints honour `If-Modified-Since`
  and answer `304 Not Modified` with an empty body: the collector sends the
  previous scan's `last-modified` and stores status `unchanged` (no price rows)
  when both lists of an auction house return 304.
- Data refreshes **hourly**, but the minute changes: EU refreshed at about :47 on
  2026-11-05 and at about :17 on 2026-11-06. Within one refresh, groups 1 and 2
  get a `last-modified` a few seconds later than the others.
- Errors happen: one endpoint (`ruleset/1/auctions/121/commodities`, EU) returned
  HTTP 500 during probing. One failing auction house must not abort the whole
  scan. Retry 3 times with exponential backoff, then store status `error` for that
  auction house and continue.
- Ignore `cache-control: max-age=86400` on auction responses; the data changes
  more often than that.

### Auction houses (probed 2026-11-05)

Every region has the same 15 auction house ids, 110–125 (except 122), and an
auction house is identified by `(region, ah_id)`. They form 5 groups of one
Alliance, one Horde and one Blackwater (neutral) auction house:

| Group | Alliance | Horde | Blackwater | EU listings (A/H/B) | US      | KR / TW |
|-------|----------|-------|------------|---------------------|---------|---------|
| 1     | 110      | 113   | 116        | 102k / 126k / 6k    | active  | active  |
| 2     | 111      | 114   | 117        | 113k / 104k / 4k    | active  | active  |
| 3     | 112      | 115   | 118        | empty               | empty   | empty   |
| 4     | 119      | 120   | 121        | 12k / 7k / 50       | active  | empty   |
| 5     | 123      | 124   | 125        | 9k / 9k / 80        | active  | empty   |

**One group per ruleset** (confirmed). EU and US have 4 rulesets and 4
active groups; KR and TW have 2 rulesets and 2 active groups. Group 3 is empty
everywhere: probably reserved for Hardcore, not active yet. Mapping:

- Group 1 = **PvP** (confirmed: two full in-game scans from the GreedyGoblin addon
  on `CAMWOW EU5 Realm 8 PvP`, taken in two different Alliance cities, both match
  110 alone: 961 of 1,313 items with the same minimum price, median quantity ratio
  0.94. Merging 110 with any other auction house matches worse).
- Group 4 = **Roleplay** (confirmed: full in-game Horde scan on an RP realm at
  19:16 UTC vs API data from 18:47 UTC matches 120 alone: 442 of 504 items with
  the same minimum price, 382 quantities within ±10%, median quantity ratio 1.00).
- Group 2 = **Normal** and group 5 = **Roleplay PvP**: deduced by elimination
  (it also follows the order of the ruleset ids 1, 2, 3, 15) and confirmed by the
  project owner.

Other facts:

- **Not one auction house per city**: all cities of a faction share one auction
  house (see the scans above; prices checked by hand in two cities agree).
- They are **separate markets**: prices and quantities differ between groups.
- Auction ids are unique across all auction houses of a region (one counter).
- `last-modified` of groups 1 and 2 is a few seconds later than the others.
- The in-game realm name is not in the API response.
- A large auction house has ~1,200–1,400 distinct items (growing). The payload is
  ~300–500 KB gzipped (~4–6 MB JSON) per large auction house, and each request
  takes about 0.2 s. A full region is 30 requests.
- Track all auction houses, including empty ones: they cost almost nothing and
  may become active later.

## Data model

**Scope: every region (eu, us, kr, tw) and every auction house** (all rulesets,
all factions, empty ones included). Regions are listed in the config file.

Each collector run creates one **scan** per region: it fetches all 15 auction
houses of that region. Each auction house's response is reduced to one row per
item. Listings and commodities from the same auction house are combined per item.

Items are keyed by **`item_id` only**: variants of the same item (different
`bonus_lists`, e.g. random suffixes, or `modifiers`) are combined. `modifiers`
(mostly type 28, values like 5240 or 7000, meaning unknown) are ignored for now. About 40% of
non-commodity items have more than one variant (EU 110, 2026-11-06).

Per-item statistics, all in copper:

- `min_price`: lowest unit price (`buyout / quantity` for listings, `unit_price` for
  commodities). Ignore auctions that have no buyout.
- `p10_price`, `p50_price` (median), `p90_price`: percentiles of the unit price
  **weighted by quantity** (each unit counts once), using the nearest-rank method
  so they are always an actual listed price (an integer, no interpolation).
- `quantity`: total units listed.
- `auctions`: number of listings.

The schema is in `crates/core/migrations/0001_init.sql` (tables `ruleset`,
`auction_house`, view `auction_house_info`, `scan`, `scan_auction_house`, `item`,
hypertable `price_point`). That file is the source of truth; read it instead of
repeating the DDL here. Key points:

- Auction houses are referred to by name through `auction_house_info`
  (`region`, `ruleset` slug, `faction`); the group -> ruleset mapping is seeded
  in the migration.
- `price_point` is a hypertable on `ts` (7-day chunks). `ts` is the scan's
  `started_at`, the same value for every row of a scan; `scan_id` links a row to
  its scan. Primary key `(auction_house_id, item_id, ts)`.
- Columnstore (compression): `timescaledb.enable_columnstore`, segmentby
  `auction_house_id, item_id`, orderby `ts`, policy `add_columnstore_policy`
  after 7 days. These are the current TimescaleDB 2.31 names. To convert chunks
  by hand use `SELECT compress_chunk(c) FROM show_chunks('price_point') c`
  (`convert_to_columnstore` is a procedure and cannot be used in a SELECT).
- Item history in a time window:
  `WHERE auction_house_id = $1 AND item_id = $2 AND ts BETWEEN $3 AND $4`.
  Uncompressed chunks use the primary key; compressed chunks read only the
  batches of that series.
- Whole snapshot (one auction house, one scan): `WHERE auction_house_id = $1 AND
  ts = $2`. Only one chunk is read (chunk exclusion on `ts`).
- Write each scan in **a single transaction**. Price rows are inserted with one
  `INSERT ... SELECT ... FROM UNNEST(...)` (one array per column), not one
  statement per row.
- Measured on a real EU scan (2026-11-06): ~12k price rows, ~4 s for the whole
  scan (30 requests + write), ~2.4 s when everything is unchanged (304),
  collector RSS ~30 MB.
- An item that is missing from an auction house in a scan whose
  `scan_auction_house.status` is `ok` was not listed. Do not write a zero row for
  it. If the status is `error`, the data is unknown, not zero.
- If an auction house's `last-modified` equals the one in the previous scan, store
  status `unchanged` and no price rows.
- Change the schema only through numbered migrations in `crates/core/migrations/`.

### Size estimate

Measured on 2026-11-05 (the day after launch): ~7k item rows per snapshot for
**all 15 auction houses** of EU (only items actually listed get a row), ~1,200–
1,400 distinct items in the largest auction houses. This is a lower bound: the
number of items on sale grows quickly after launch (quantities more than doubled
in 5 hours on 2026-11-05).

Today (2026-11-05, 19:30 UTC), one scan of every region is ~23.6k rows (EU 9.9k,
US 9.5k, KR 2.4k, TW 1.8k), so ~52M rows/year at 6 scans/day. Worst case: 10k
items in each of the 36 active auction houses (12 in EU and US, 6 in KR and TW)
is ~790 million rows/year.

Measured on TimescaleDB 2.31 with 4 weeks of synthetic EU-like data (1.6M rows,
prices fluctuating around a per-item base, 2026-11-06):

- uncompressed: **142 bytes/row** (row + primary key index);
- compressed (columnstore): **21.5 bytes/row**, ratio **6.6×**;
- one item's 4-week history from compressed chunks: **0.3 ms**;
- one auction house's whole snapshot (1,400 items) from a compressed chunk: **2 ms**.

So today's volume (~52M rows/year) is ~1.1 GB/year, and the worst case (~790M
rows/year) is ~17 GB/year. The synthetic data adds random noise to every price
in every scan; real prices often do not change between scans, so real
compression should be at least as good. Measure it on real data with
`chunk_columnstore_stats('price_point')` after the first weeks. Start by storing
everything.

If disk use becomes a problem, downsample old data (for example, daily min/avg
after 90 days) with a TimescaleDB continuous aggregate. Do not delete history
without asking first.

## API server

- Read-only. JSON responses. CORS can be configured.
- Planned endpoints (version prefix `/v1`):
  - `GET /v1/auction-houses`
  - `GET /v1/items/{itemId}`: metadata
  - `GET /v1/auction-houses/{id}/items/{itemId}/history?from=&to=`: time series
  - `GET /v1/auction-houses/{id}/latest`: latest snapshot for all items
- Always limit the time range and the number of rows returned.
- Set HTTP caching headers (`Cache-Control`, `ETag` from the latest scan id). The data
  changes only every 4 hours.

## Commands

```sh
cp .env.example .env                 # then fill in the secrets
docker compose up -d db              # start TimescaleDB
docker compose exec db psql -U wow -d wow
set -a; . ./.env; set +a             # cargo run does not read .env by itself
cargo sqlx migrate run --source crates/core/migrations   # apply migrations
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run -p collector -- scan eu    # one scan of one region, then exit
cargo run -p collector               # run the schedule
cargo sqlx prepare --workspace       # refresh .sqlx/ after changing a query
docker build -t greedygoblin-data-server .    # build the image locally
docker compose run --rm collector collector scan eu   # one scan in the container
```

- `sqlx::query!` checks queries against the database at compile time, through
  `DATABASE_URL` (sqlx reads `.env`). Builds without a database (CI, Docker) use
  `SQLX_OFFLINE=true` and the committed `.sqlx/` directory, so run
  `cargo sqlx prepare --workspace` after changing any query.
- The collector applies pending migrations itself at startup.

## Open questions

- None at the moment.
