//! When scans run: every 4 hours at 00:30, 04:30, ..., 20:30 UTC, the same
//! for every region.

use chrono::{DateTime, Duration, DurationRound, Utc};

/// Hours between two scans of the same region.
pub const INTERVAL_HOURS: u32 = 4;

/// Minute past the hour at which scans run. Blizzard refreshes the data
/// hourly, but the refresh minute changes, so any fixed minute gives data up
/// to an hour old; the real data time is stored in `last_modified`.
pub const SCAN_MINUTE: u32 = 30;

/// First scan slot strictly after `now`: [`SCAN_MINUTE`] past an hour that is
/// a multiple of [`INTERVAL_HOURS`] (UTC).
pub fn next_slot(now: DateTime<Utc>) -> DateTime<Utc> {
    let day = now
        .duration_trunc(Duration::days(1))
        .expect("a day fits in a timestamp");
    (0..)
        .flat_map(|d| {
            (0..24)
                .step_by(INTERVAL_HOURS as usize)
                .map(move |h| (d, h))
        })
        .map(|(d, h)| {
            day + Duration::days(d) + Duration::hours(h) + Duration::minutes(i64::from(SCAN_MINUTE))
        })
        .find(|slot| *slot > now)
        .expect("a slot exists within two days")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 6, h, m, s).unwrap()
    }

    #[test]
    fn next_slot_is_later_the_same_day() {
        assert_eq!(next_slot(at(9, 0, 0)), at(12, 30, 0));
        assert_eq!(next_slot(at(0, 10, 0)), at(0, 30, 0));
    }

    #[test]
    fn a_slot_time_moves_to_the_following_slot() {
        assert_eq!(next_slot(at(4, 30, 0)), at(8, 30, 0));
    }

    #[test]
    fn after_the_last_slot_it_wraps_to_the_next_day() {
        let next = next_slot(at(20, 31, 0));
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 11, 7, 0, 30, 0).unwrap());
    }

    #[test]
    fn slots_are_the_six_daily_times() {
        let mut time = at(0, 0, 0);
        let mut slots = Vec::new();
        for _ in 0..6 {
            time = next_slot(time);
            slots.push(time);
        }
        let expected: Vec<_> = [0, 4, 8, 12, 16, 20].map(|h| at(h, 30, 0)).into();
        assert_eq!(slots, expected);
    }
}
