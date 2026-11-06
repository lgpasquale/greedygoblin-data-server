-- Initial schema: rulesets, auction houses, scans, item metadata and the
-- price_point hypertable with columnstore compression.

CREATE EXTENSION IF NOT EXISTS timescaledb;

-- Rulesets, with readable names. Ids are Blizzard's.
CREATE TABLE ruleset (
  id   smallint PRIMARY KEY,
  slug text     NOT NULL UNIQUE,
  name text     NOT NULL
);

INSERT INTO ruleset (id, slug, name) VALUES
  (1,  'pvp',    'PvP'),
  (2,  'normal', 'Normal'),
  (3,  'rp',     'Roleplay'),
  (15, 'rp-pvp', 'Roleplay PvP');

-- One row per region and Blizzard auction house id (4 x 15). Prices refer to
-- these by id, so region, ruleset and faction are never repeated in price rows.
CREATE TABLE auction_house (
  id          smallint PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
  region      text     NOT NULL CHECK (region IN ('eu', 'us', 'kr', 'tw')),
  blizzard_id smallint NOT NULL,
  ruleset_id  smallint REFERENCES ruleset (id),
  faction     text     NOT NULL CHECK (faction IN ('alliance', 'horde', 'blackwater')),
  UNIQUE (region, blizzard_id)
);

-- The ruleset in the API URL is ignored: each region has the same 15 auction
-- houses, in 5 groups of Alliance / Horde / Blackwater, one group per ruleset.
-- Group 3 (112, 115, 118) is empty everywhere; its ruleset is not known yet.
INSERT INTO auction_house (region, blizzard_id, ruleset_id, faction)
SELECT region.code, mapping.blizzard_id, mapping.ruleset_id, mapping.faction
FROM (VALUES ('eu'), ('us'), ('kr'), ('tw')) AS region (code)
CROSS JOIN (VALUES
  (110, 1,    'alliance'), (113, 1,    'horde'), (116, 1,    'blackwater'),
  (111, 2,    'alliance'), (114, 2,    'horde'), (117, 2,    'blackwater'),
  (112, NULL, 'alliance'), (115, NULL, 'horde'), (118, NULL, 'blackwater'),
  (119, 3,    'alliance'), (120, 3,    'horde'), (121, 3,    'blackwater'),
  (123, 15,   'alliance'), (124, 15,   'horde'), (125, 15,   'blackwater')
) AS mapping (blizzard_id, ruleset_id, faction)
ORDER BY region.code, mapping.blizzard_id;

-- Auction houses by name. Use this in queries instead of Blizzard ids:
--   WHERE region = 'eu' AND ruleset = 'pvp' AND faction = 'alliance'
CREATE VIEW auction_house_info AS
SELECT ah.id, ah.region, r.slug AS ruleset, ah.faction, ah.blizzard_id
FROM auction_house ah
LEFT JOIN ruleset r ON r.id = ah.ruleset_id;

-- One collector run for one region.
CREATE TABLE scan (
  id          integer     PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
  region      text        NOT NULL CHECK (region IN ('eu', 'us', 'kr', 'tw')),
  started_at  timestamptz NOT NULL,
  finished_at timestamptz
);

CREATE INDEX scan_region_started_at_idx ON scan (region, started_at);

-- What happened to each auction house in a scan. Tells "item not listed" apart
-- from "this auction house failed in this scan".
CREATE TABLE scan_auction_house (
  scan_id          integer     NOT NULL REFERENCES scan (id),
  auction_house_id smallint    NOT NULL REFERENCES auction_house (id),
  status           text        NOT NULL CHECK (status IN ('ok', 'unchanged', 'error')),
  last_modified    timestamptz,
  listings         integer,
  error            text,
  PRIMARY KEY (scan_id, auction_house_id)
);

-- Item metadata, en_US only. Fetched once, when an item is first seen.
CREATE TABLE item (
  id         integer     PRIMARY KEY,
  name       text,
  quality    text,
  item_class integer,
  updated_at timestamptz
);

-- One row per (auction house, item, scan). All prices in copper.
-- ts is the scan's started_at, the same for every row of a scan: it is the
-- partitioning column. scan_id links a row to its scan.
CREATE TABLE price_point (
  ts               timestamptz NOT NULL,
  scan_id          integer     NOT NULL,
  auction_house_id smallint    NOT NULL,
  item_id          integer     NOT NULL,
  min_price        bigint      NOT NULL,
  p10_price        bigint      NOT NULL,
  p50_price        bigint      NOT NULL,
  p90_price        bigint      NOT NULL,
  quantity         integer     NOT NULL,
  auctions         integer     NOT NULL,
  PRIMARY KEY (auction_house_id, item_id, ts)
);

SELECT create_hypertable('price_point', by_range('ts', INTERVAL '7 days'));

-- Chunks older than 7 days are converted to the columnstore, grouped by series
-- (one auction house + one item) and ordered by time, so one item's history is
-- read from a few compressed batches.
ALTER TABLE price_point SET (
  timescaledb.enable_columnstore = true,
  timescaledb.segmentby = 'auction_house_id, item_id',
  timescaledb.orderby = 'ts'
);

CALL add_columnstore_policy('price_point', after => INTERVAL '7 days');
