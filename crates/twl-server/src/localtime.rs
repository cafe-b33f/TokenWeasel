//! Convert a calendar date to the earliest valid local Unix timestamp.
//!
//! Tries each hour 00:00..23:00; returns the first valid timestamp.
//! If all 24 hours are skipped (DST gap), advances the date and retries
//! up to 366 days. Never panics.

use chrono::NaiveDate;

/// Convert a calendar date to the earliest valid local Unix timestamp.
///
/// Finds the earliest valid instant on the given local date by trying
/// each hour 00:00 through 23:00 and returning the first that produces a
/// valid timezone-aware datetime.
///
/// | Local result | Behavior |
/// |--------------|----------|
/// | **Single** | Midnight is unambiguous; return its timestamp. |
/// | **Ambiguous** (fall-back) | Return the earliest offset (standard time). |
/// | **None** (spring-forward gap) | Try hours 01:00 through 23:00; if all fail,
///   advance the calendar date and retry up to 366 days (one year + leap
///   guard). If exhausted, returns the next midnight as UTC. |
///
/// Never panics. All `chrono::LocalResult` variants are handled and the
/// day-advance loop is bounded.
pub fn date_to_start_timestamp<T: chrono::TimeZone>(tz: &T, date: NaiveDate) -> i64 {
    for hour in 0..24 {
        if let Some(naive) = date.and_hms_opt(hour, 0, 0) {
            if let Some(dt) = tz.from_local_datetime(&naive).earliest() {
                return dt.timestamp();
            }
        }
    }
    // Entire date was skipped: advance calendar date and retry.
    // Bound to MAX_DAYS so pathological timezones cannot loop forever.
    const MAX_DAYS: u32 = 366; // one year + leap guard
    let mut cur = date;
    for _ in 0..MAX_DAYS {
        cur = cur.succ_opt().unwrap_or(cur);
        if let Some(naive) = cur.and_hms_opt(0, 0, 0) {
            if let Some(dt) = tz.from_local_datetime(&naive).earliest() {
                return dt.timestamp();
            }
        }
    }
    // Fallback: return next midnight as UTC when bounded retry is exhausted.
    cur.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use chrono::{naive::NaiveDateTime, FixedOffset, LocalResult, NaiveDate, TimeZone, Timelike};

    use super::date_to_start_timestamp;

    fn fo(hours: i32) -> FixedOffset {
        FixedOffset::east_opt(hours * 3600).unwrap()
    }

    #[test]
    fn fixed_offset_midnight_returns_correct_timestamp() {
        let tz = fo(5);
        let date = NaiveDate::from_ymd_opt(2025, 6, 15).unwrap();
        let ts = date_to_start_timestamp(&tz, date);
        assert_eq!(ts, 1_749_927_600);
    }

    #[test]
    fn fixed_offset_negative_offset() {
        let tz = fo(-5);
        let date = NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
        let ts = date_to_start_timestamp(&tz, date);
        assert_eq!(ts, 1_735_707_600);
    }

    #[test]
    fn fixed_offset_year_boundary() {
        let tz = fo(0);
        let date = NaiveDate::from_ymd_opt(2025, 12, 31).unwrap();
        let ts = date_to_start_timestamp(&tz, date);
        assert_eq!(ts, 1_767_139_200);
    }

    #[derive(Clone)]
    struct RejectHours {
        reject: u32,
        offset: FixedOffset,
    }

    impl TimeZone for RejectHours {
        type Offset = FixedOffset;

        fn from_offset(offset: &FixedOffset) -> Self {
            Self {
                reject: 0,
                offset: *offset,
            }
        }

        fn offset_from_local_date(&self, _local: &NaiveDate) -> LocalResult<Self::Offset> {
            LocalResult::Single(self.offset)
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            if local.hour() < self.reject {
                LocalResult::None
            } else {
                LocalResult::Single(self.offset)
            }
        }

        fn offset_from_utc_date(&self, _utc: &NaiveDate) -> Self::Offset {
            self.offset
        }

        fn offset_from_utc_datetime(&self, _utc: &NaiveDateTime) -> Self::Offset {
            self.offset
        }
    }

    #[derive(Clone)]
    struct RejectFirstDate {
        reject_start: NaiveDate,
        offset: FixedOffset,
    }

    impl TimeZone for RejectFirstDate {
        type Offset = FixedOffset;

        fn from_offset(offset: &FixedOffset) -> Self {
            Self {
                reject_start: NaiveDate::MIN,
                offset: *offset,
            }
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<Self::Offset> {
            if *local >= self.reject_start {
                LocalResult::Single(self.offset)
            } else {
                LocalResult::None
            }
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            if local.date() >= self.reject_start {
                LocalResult::Single(self.offset)
            } else {
                LocalResult::None
            }
        }

        fn offset_from_utc_date(&self, _utc: &NaiveDate) -> Self::Offset {
            self.offset
        }

        fn offset_from_utc_datetime(&self, _utc: &NaiveDateTime) -> Self::Offset {
            self.offset
        }
    }

    #[test]
    fn hour_fallback_skips_rejected_hours() {
        let tz = RejectHours {
            reject: 1,
            offset: FixedOffset::east_opt(0).unwrap(),
        };
        let date = NaiveDate::from_ymd_opt(2025, 6, 15).unwrap();
        let ts = date_to_start_timestamp(&tz, date);
        assert_eq!(ts, 1_749_949_200);
    }

    #[test]
    fn hour_fallback_accepts_midnight_when_not_rejected() {
        let tz = RejectHours {
            reject: 0,
            offset: FixedOffset::east_opt(0).unwrap(),
        };
        let date = NaiveDate::from_ymd_opt(2025, 6, 15).unwrap();
        let ts = date_to_start_timestamp(&tz, date);
        assert_eq!(ts, 1_749_945_600);
    }

    #[test]
    fn full_date_skip_advances_and_applies_offset() {
        let reject_start = NaiveDate::from_ymd_opt(2025, 6, 16).unwrap();
        let tz = RejectFirstDate {
            reject_start,
            offset: FixedOffset::east_opt(3 * 3600).unwrap(),
        };
        let date = NaiveDate::from_ymd_opt(2025, 6, 15).unwrap();
        let ts = date_to_start_timestamp(&tz, date);
        assert_eq!(ts, 1_750_021_200);
        assert_ne!(ts, 1_750_032_000);
    }

    #[test]
    fn local_timezone_no_panic() {
        let tz = chrono::Local;
        for m in [1, 3, 6, 9, 11] {
            let date = NaiveDate::from_ymd_opt(2025, m, 15).unwrap();
            let _ts = date_to_start_timestamp(&tz, date);
        }
    }

    #[test]
    fn local_fall_back_no_panic() {
        let tz = chrono::Local;
        let date = NaiveDate::from_ymd_opt(2025, 11, 2).unwrap();
        let ts = date_to_start_timestamp(&tz, date);
        assert!(ts > 0);
    }
}
