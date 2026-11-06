//! Client for the (undocumented) WoW: Forever endpoints of the Battle.net API.

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, IF_MODIFIED_SINCE, LAST_MODIFIED};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use wow_core::Region;

use crate::stats::Listing;

const TOKEN_URL: &str = "https://oauth.battle.net/token";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Attempts per request, including the first one.
const ATTEMPTS: u32 = 3;
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Request a new token this long before the current one expires.
const TOKEN_MARGIN: Duration = Duration::from_secs(300);

pub struct Client {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    token: Mutex<Option<Token>>,
}

struct Token {
    value: String,
    expires_at: Instant,
}

/// Which list of an auction house to fetch.
#[derive(Debug, Clone, Copy)]
pub enum AuctionKind {
    /// Non-commodity listings (gear and other unique items).
    Items,
    Commodities,
}

impl AuctionKind {
    fn path_suffix(self) -> &'static str {
        match self {
            AuctionKind::Items => "",
            AuctionKind::Commodities => "/commodities",
        }
    }
}

/// Result of fetching one list of an auction house.
pub enum Fetched {
    /// The data has not changed since the time given in `If-Modified-Since`.
    NotModified,
    Data {
        last_modified: Option<DateTime<Utc>>,
        /// Listings with a buyout price. Listings with a bid only are dropped.
        listings: Vec<Listing>,
        /// All listings in the response, including bid-only ones.
        total_listings: usize,
    },
}

/// Item metadata, en_US.
pub struct ItemInfo {
    pub name: String,
    pub quality: Option<String>,
    pub item_class: Option<i32>,
}

impl Client {
    pub fn new(client_id: String, client_secret: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .gzip(true)
            .user_agent(concat!("wow-data-server/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            client_id,
            client_secret,
            token: Mutex::new(None),
        })
    }

    /// Id of the first ruleset of the region. The auction endpoints need a
    /// valid ruleset in the URL but return the same data for every ruleset.
    pub async fn first_ruleset_id(&self, region: Region) -> anyhow::Result<i64> {
        #[derive(Deserialize)]
        struct Index {
            rulesets: Vec<Ref>,
        }
        let url = self.url(region, "/data/wow/ruleset/index", Namespace::Dynamic)?;
        let index: Index = self.get_json(url).await?;
        index
            .rulesets
            .first()
            .map(|r| r.id)
            .ok_or_else(|| anyhow!("no rulesets in region {region}"))
    }

    /// Blizzard ids of the auction houses listed by the API for the region.
    pub async fn auction_house_ids(
        &self,
        region: Region,
        ruleset_id: i64,
    ) -> anyhow::Result<Vec<i64>> {
        #[derive(Deserialize)]
        struct Index {
            auctions: Vec<Ref>,
        }
        let path = format!("/data/wow/ruleset/{ruleset_id}/auctions/index");
        let url = self.url(region, &path, Namespace::Dynamic)?;
        let index: Index = self.get_json(url).await?;
        Ok(index.auctions.into_iter().map(|a| a.id).collect())
    }

    /// Fetches one list of an auction house. With `if_modified_since`, returns
    /// `Fetched::NotModified` when the data has not changed since then.
    pub async fn auctions(
        &self,
        region: Region,
        ruleset_id: i64,
        auction_house: i64,
        kind: AuctionKind,
        if_modified_since: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Fetched> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(default)]
            auctions: Option<Vec<Auction>>,
        }
        #[derive(Deserialize)]
        struct Auction {
            item: AuctionItem,
            quantity: i64,
            unit_price: Option<i64>,
            buyout: Option<i64>,
        }
        #[derive(Deserialize)]
        struct AuctionItem {
            id: i32,
        }

        let path = format!(
            "/data/wow/ruleset/{ruleset_id}/auctions/{auction_house}{}",
            kind.path_suffix()
        );
        let url = self.url(region, &path, Namespace::Dynamic)?;
        let mut headers = HeaderMap::new();
        if let Some(since) = if_modified_since {
            headers.insert(IF_MODIFIED_SINCE, http_date(since).parse()?);
        }

        let response = self.send(url, headers).await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified);
        }
        let last_modified = response
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_http_date);
        let body: Response = response.json().await?;
        let auctions = body.auctions.unwrap_or_default();
        let total_listings = auctions.len();
        let listings = auctions
            .into_iter()
            .filter(|a| a.quantity > 0)
            .filter_map(|a| {
                let unit_price = a.unit_price.or(a.buyout.map(|b| b / a.quantity))?;
                Some(Listing {
                    item_id: a.item.id,
                    unit_price,
                    quantity: a.quantity,
                })
            })
            .collect();
        Ok(Fetched::Data {
            last_modified,
            listings,
            total_listings,
        })
    }

    /// Item metadata in en_US, or `None` if the API does not know the item
    /// (some items, e.g. gear with random suffixes, return 404).
    pub async fn item(&self, region: Region, item_id: i32) -> anyhow::Result<Option<ItemInfo>> {
        #[derive(Deserialize)]
        struct Item {
            name: String,
            quality: Option<Quality>,
            item_class: Option<Ref>,
        }
        #[derive(Deserialize)]
        struct Quality {
            #[serde(rename = "type")]
            kind: String,
        }

        let mut url = self.url(
            region,
            &format!("/data/wow/item/{item_id}"),
            Namespace::Static,
        )?;
        url.query_pairs_mut().append_pair("locale", "en_US");
        let response = self.send(url, HeaderMap::new()).await;
        let response = match response {
            Ok(r) => r,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let item: Item = response.json().await?;
        Ok(Some(ItemInfo {
            name: item.name,
            quality: item.quality.map(|q| q.kind),
            item_class: item.item_class.and_then(|c| i32::try_from(c.id).ok()),
        }))
    }

    fn url(&self, region: Region, path: &str, namespace: Namespace) -> anyhow::Result<Url> {
        let mut url = Url::parse(&format!("https://{region}.api.blizzard.com{path}"))?;
        url.query_pairs_mut().append_pair(
            "namespace",
            &format!("{}-forever-{region}", namespace.prefix()),
        );
        Ok(url)
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> anyhow::Result<T> {
        Ok(self.send(url, HeaderMap::new()).await?.json().await?)
    }

    /// Sends a GET request with the bearer token. Retries network errors, 429
    /// and 5xx with exponential backoff; requests a new token once on 401.
    /// Returns the response for 2xx and 304, an error otherwise.
    async fn send(&self, url: Url, headers: HeaderMap) -> anyhow::Result<reqwest::Response> {
        let mut delay = FIRST_RETRY_DELAY;
        let mut token_refreshed = false;
        let mut attempt = 1;
        loop {
            let token = self.token().await?;
            let result = self
                .http
                .get(url.clone())
                .headers(headers.clone())
                .bearer_auth(&token)
                .send()
                .await;

            let error = match result {
                Ok(r) if r.status().is_success() || r.status() == StatusCode::NOT_MODIFIED => {
                    return Ok(r);
                }
                Ok(r) if r.status() == StatusCode::UNAUTHORIZED && !token_refreshed => {
                    token_refreshed = true;
                    *self.token.lock().await = None;
                    continue;
                }
                Ok(r) => {
                    let status = r.status();
                    let error = anyhow::Error::new(HttpStatus(status))
                        .context(format!("GET {} returned {status}", redact(&url)));
                    if !(status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS) {
                        return Err(error);
                    }
                    error
                }
                Err(e) => anyhow::Error::new(e).context(format!("GET {} failed", redact(&url))),
            };

            if attempt >= ATTEMPTS {
                return Err(error);
            }
            tracing::warn!(
                attempt,
                error = format!("{error:#}"),
                "retrying in {delay:?}"
            );
            tokio::time::sleep(delay).await;
            delay *= 2;
            attempt += 1;
        }
    }

    /// A valid access token, requested from Battle.net when missing or about
    /// to expire.
    async fn token(&self) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }

        let mut token = self.token.lock().await;
        if let Some(t) = token.as_ref()
            && t.expires_at > Instant::now() + TOKEN_MARGIN
        {
            return Ok(t.value.clone());
        }

        let response = self
            .http
            .post(TOKEN_URL)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("grant_type=client_credentials")
            .send()
            .await
            .context("token request failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("token request returned {status}");
        }
        let body: TokenResponse = response.json().await.context("invalid token response")?;
        *token = Some(Token {
            value: body.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(body.expires_in),
        });
        Ok(body.access_token)
    }
}

#[derive(Deserialize)]
struct Ref {
    id: i64,
}

#[derive(Clone, Copy)]
enum Namespace {
    Dynamic,
    Static,
}

impl Namespace {
    fn prefix(self) -> &'static str {
        match self {
            Namespace::Dynamic => "dynamic",
            Namespace::Static => "static",
        }
    }
}

/// A non-success HTTP status, kept as the error source so callers can test it.
#[derive(Debug, thiserror::Error)]
#[error("HTTP {0}")]
struct HttpStatus(StatusCode);

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HttpStatus>()
        .is_some_and(|s| s.0 == StatusCode::NOT_FOUND)
}

/// The URL without its query string, for logs.
fn redact(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.to_string()
}

/// Formats a time as an HTTP date (RFC 7231 IMF-fixdate).
fn http_date(time: DateTime<Utc>) -> String {
    time.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Parses an HTTP date such as `Fri, 6 Nov 2026 07:17:16 GMT` (Blizzard writes
/// single-digit days without padding).
fn parse_http_date(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_blizzard_last_modified() {
        let parsed = parse_http_date("Fri, 6 Nov 2026 07:17:16 GMT").unwrap();
        assert_eq!(
            parsed,
            Utc.with_ymd_and_hms(2026, 11, 6, 7, 17, 16).unwrap()
        );
    }

    #[test]
    fn http_date_round_trips() {
        let time = Utc.with_ymd_and_hms(2026, 11, 6, 7, 17, 16).unwrap();
        assert_eq!(http_date(time), "Fri, 06 Nov 2026 07:17:16 GMT");
        assert_eq!(parse_http_date(&http_date(time)), Some(time));
    }
}
