//! Reduces the listings of one auction house to per-item statistics.

use std::collections::HashMap;

/// One listing with a buyout price, reduced to what the statistics need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listing {
    pub item_id: i32,
    /// Price of one unit, in copper.
    pub unit_price: i64,
    pub quantity: i64,
}

/// Statistics of one item in one auction house at one time. Prices in copper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemStats {
    pub item_id: i32,
    pub min_price: i64,
    pub p10_price: i64,
    pub p50_price: i64,
    pub p90_price: i64,
    /// Total units listed.
    pub quantity: i64,
    /// Number of listings.
    pub auctions: i64,
}

/// Groups listings by item and computes their statistics, sorted by item id.
/// Listings with a quantity below 1 are ignored.
pub fn aggregate(listings: impl IntoIterator<Item = Listing>) -> Vec<ItemStats> {
    let mut by_item: HashMap<i32, Vec<(i64, i64)>> = HashMap::new();
    for listing in listings {
        if listing.quantity > 0 {
            by_item
                .entry(listing.item_id)
                .or_default()
                .push((listing.unit_price, listing.quantity));
        }
    }

    let mut stats: Vec<ItemStats> = by_item
        .into_iter()
        .map(|(item_id, mut prices)| {
            prices.sort_unstable();
            let quantity = prices.iter().map(|&(_, q)| q).sum();
            ItemStats {
                item_id,
                min_price: prices[0].0,
                p10_price: weighted_percentile(&prices, quantity, 10),
                p50_price: weighted_percentile(&prices, quantity, 50),
                p90_price: weighted_percentile(&prices, quantity, 90),
                quantity,
                auctions: prices.len() as i64,
            }
        })
        .collect();
    stats.sort_unstable_by_key(|s| s.item_id);
    stats
}

/// Percentile of the unit price where each unit counts once (nearest-rank
/// method): the price of the unit at rank ceil(p / 100 * total). The result is
/// always a listed price.
///
/// `prices` holds (unit price, quantity), sorted by price, and is not empty;
/// `total` is the sum of the quantities.
fn weighted_percentile(prices: &[(i64, i64)], total: i64, percent: i64) -> i64 {
    // Ceiling division; both operands are positive.
    let rank = ((percent * total + 99) / 100).max(1);
    let mut seen = 0;
    for &(price, quantity) in prices {
        seen += quantity;
        if seen >= rank {
            return price;
        }
    }
    prices[prices.len() - 1].0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(item_id: i32, unit_price: i64, quantity: i64) -> Listing {
        Listing {
            item_id,
            unit_price,
            quantity,
        }
    }

    #[test]
    fn single_listing_gives_its_price_everywhere() {
        let stats = aggregate([listing(7, 500, 3)]);
        assert_eq!(
            stats,
            vec![ItemStats {
                item_id: 7,
                min_price: 500,
                p10_price: 500,
                p50_price: 500,
                p90_price: 500,
                quantity: 3,
                auctions: 1,
            }]
        );
    }

    #[test]
    fn percentiles_are_weighted_by_quantity() {
        // 10 units: 1 at 100, 8 at 200, 1 at 1000.
        let stats = aggregate([listing(1, 1000, 1), listing(1, 200, 8), listing(1, 100, 1)]);
        let s = &stats[0];
        assert_eq!(s.min_price, 100);
        assert_eq!(s.p10_price, 100); // rank 1
        assert_eq!(s.p50_price, 200); // rank 5
        assert_eq!(s.p90_price, 200); // rank 9
        assert_eq!(s.quantity, 10);
        assert_eq!(s.auctions, 3);
    }

    #[test]
    fn one_cheap_unit_does_not_move_the_median() {
        // A single listing far below the rest changes the minimum only.
        let stats = aggregate([listing(1, 1, 1), listing(1, 5000, 99)]);
        assert_eq!(stats[0].min_price, 1);
        assert_eq!(stats[0].p10_price, 5000);
        assert_eq!(stats[0].p50_price, 5000);
    }

    #[test]
    fn items_are_separated_and_sorted() {
        let stats = aggregate([listing(9, 10, 1), listing(3, 20, 2), listing(9, 30, 1)]);
        let ids: Vec<i32> = stats.iter().map(|s| s.item_id).collect();
        assert_eq!(ids, vec![3, 9]);
        assert_eq!(stats[1].auctions, 2);
    }

    #[test]
    fn zero_quantity_listings_are_ignored() {
        assert!(aggregate([listing(1, 10, 0)]).is_empty());
    }
}
