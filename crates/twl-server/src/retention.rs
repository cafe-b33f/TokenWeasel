//! Background data-retention pruner.
//!
//! Periodically removes expired usage/energy rows from
//! the database according to the configured retention period. Pruning runs
//! once at startup and then once per day, so no per-request overhead is
//! incurred.

use std::sync::Arc;

use twl_store::{now_unix_secs, UsageStore};

const SECONDS_PER_DAY: i64 = 86_400;

fn retention_seconds(retention_days: u64) -> Option<i64> {
    i64::try_from(retention_days)
        .ok()?
        .checked_mul(SECONDS_PER_DAY)
}

fn cutoff_timestamp(now: i64, retention_secs: i64) -> Option<i64> {
    now.checked_sub(retention_secs)
}

/// Spawn a background task that prunes old usage records.
///
/// * `usage` – shared usage store for deleting expired token/energy rows.
/// * `retention_days` – how many days of data to keep. A value of `0`
///   disables pruning entirely.
pub fn spawn_pruner(usage: Arc<UsageStore>, retention_days: u64) {
    if retention_days == 0 {
        tracing::info!("retention pruning is disabled");
        return;
    }

    let Some(retention_secs) = retention_seconds(retention_days) else {
        tracing::error!(
            retention_days,
            "retention pruning disabled: retention period is too large"
        );
        return;
    };

    tokio::spawn(async move {
        loop {
            let Some(now) = now_unix_secs() else {
                tracing::error!("retention pruner: failed to get current time, skipping iteration");
                tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
                continue;
            };

            let Some(cutoff) = cutoff_timestamp(now, retention_secs) else {
                tracing::error!(
                    now,
                    retention_secs,
                    "retention pruner: cutoff timestamp overflow, skipping iteration"
                );
                tokio::time::sleep(std::time::Duration::from_secs(86_400)).await;
                continue;
            };

            match usage.prune_older_than(cutoff) {
                Ok(total) => {
                    tracing::info!(
                        deleted_usage_rows = total,
                        "retention: pruned usage and energy rows older than cutoff"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "retention: failed to prune usage data");
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{cutoff_timestamp, retention_seconds};

    #[test]
    fn retention_seconds_checks_conversion_and_multiplication() {
        assert_eq!(retention_seconds(0), Some(0));
        assert_eq!(retention_seconds(1), Some(86_400));

        let largest_safe = (i64::MAX / 86_400) as u64;
        assert_eq!(
            retention_seconds(largest_safe),
            Some((largest_safe as i64) * 86_400)
        );
        assert_eq!(retention_seconds(largest_safe + 1), None);
        assert_eq!(retention_seconds(u64::MAX), None);
    }

    #[test]
    fn cutoff_timestamp_checks_subtraction() {
        assert_eq!(cutoff_timestamp(100_000, 86_400), Some(13_600));
        assert_eq!(cutoff_timestamp(i64::MIN, 1), None);
    }
}
