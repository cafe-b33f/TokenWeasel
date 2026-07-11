//! SQLite-backed store for token usage and energy recording/querying.
//!
//! Uses an r2d2 connection pool so recording is synchronous and immediately
//! visible. Reads go through pooled connections as well.

use crate::db::{now_unix_secs, Database, DbError, DbPool};
use twl_types::{DailyEnergy, DailyStats, KeyStats, ModelStats, TodayBucket, Usage};

// ---------------------------------------------------------------------------
// UsageStore
// ---------------------------------------------------------------------------

/// Lightweight SQLite-backed store for per-model token accounting.
///
/// Uses a shared [`r2d2`] connection pool so writes are synchronous and
/// immediately visible. Reads also go through pooled connections.
///
/// Created via [`UsageStore::new`] from a [`Database`].
#[derive(Debug)]
pub struct UsageStore {
    pool: DbPool,
}

/// Fill sparse query results into exactly 288 five-minute buckets
/// (indices 0–287), zero-filling missing intervals.
fn fill_288_buckets(sparse: Vec<TodayBucket>) -> Vec<TodayBucket> {
    let mut all = Vec::with_capacity(288);
    let mut idx = 0;
    for i in 0u16..288 {
        if idx < sparse.len() && sparse[idx].bucket == i {
            all.push(sparse[idx].clone());
            idx += 1
        } else {
            all.push(TodayBucket {
                bucket: i,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                requests: 0,
            });
        }
    }
    all
}

impl UsageStore {
    /// Create a usage store backed by the database's connection pool.
    ///
    /// Returns [`DbError`] if the pool cannot be obtained.
    pub fn new(db: &Database) -> Result<Self, DbError> {
        Ok(UsageStore { pool: db.pool() })
    }

    /// Record a usage record synchronously. If the pool cannot provide a
    /// connection or the insert fails, the error is logged and the record
    /// is silently dropped.
    pub fn record(&self, u: &Usage) {
        let Some(ts) = now_unix_secs() else {
            tracing::error!(
                "system clock before Unix epoch, dropping record for model {}",
                u.model
            );
            return;
        };
        let conn = match self.pool.get() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("pool error, dropping usage record: {e}");
                return;
            }
        };
        if let Err(e) = conn.execute(
            "INSERT INTO token_usage
               (ts, model, endpoint, input_tokens, output_tokens, cached_tokens, total_tokens, api_key_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                ts,
                u.model,
                u.endpoint,
                u.input_tokens,
                u.output_tokens,
                u.cached_tokens,
                u.total_tokens,
                u.api_key_id
            ],
        ) {
            tracing::error!("usage record insert failed: {e}");
        }
    }

    /// Per-model totals for records since `since_ts` (0 = all time).
    /// Returns entries sorted by `total_tokens` descending.
    pub fn stats(&self, since_ts: i64) -> Result<Vec<ModelStats>, DbError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT model,
                    COUNT(*),
                    COALESCE(SUM(input_tokens),0),
                    COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(cached_tokens),0),
                    COALESCE(SUM(total_tokens),0)
             FROM token_usage
             WHERE ts >= ?1
             GROUP BY model
             ORDER BY SUM(total_tokens) DESC",
        )?;
        let rows = stmt.query_map([since_ts], |r| {
            Ok(ModelStats {
                model: r.get(0)?,
                requests: r.get(1)?,
                input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                cached_tokens: r.get(4)?,
                total_tokens: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Per-user totals for records since `since_ts` (0 = all time), collapsing
    /// every API key a user owns into a single row labeled with the username.
    /// Returns entries sorted by `total_tokens` descending.
    ///
    /// Keyless traffic is grouped under `"GENERIC"`; usage recorded against a
    /// since-deleted key (no owning user) appears as `"(deleted key #id)"`.
    pub fn stats_by_user(&self, since_ts: i64) -> Result<Vec<KeyStats>, DbError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id,
                   u.username,
                   MAX(t.api_key_id),
                   COUNT(*),
                   COALESCE(SUM(t.input_tokens),0),
                   COALESCE(SUM(t.output_tokens),0),
                   COALESCE(SUM(t.cached_tokens),0),
                   COALESCE(SUM(t.total_tokens),0)
            FROM token_usage t
            LEFT JOIN api_key_attribution a ON a.api_key_id = t.api_key_id
            LEFT JOIN users u ON u.id = a.user_id
            WHERE t.ts >= ?1
            GROUP BY CASE
                         WHEN t.api_key_id IS NULL THEN 'generic'
                         WHEN u.id IS NULL THEN 'deleted:' || t.api_key_id
                         ELSE 'user:' || u.id
                     END
            ORDER BY SUM(t.total_tokens) DESC
            "#,
        )?;
        let rows = stmt.query_map([since_ts], |r| {
            let user_id: Option<i64> = r.get(0)?;
            let username: Option<String> = r.get(1)?;
            let dangling_key_id: Option<i64> = r.get(2)?;
            // A user row aggregates several keys, so it carries no single
            // `api_key_id`; only the dangling (deleted-key) group keeps one.
            let (api_key_id, label) = match (user_id, username, dangling_key_id) {
                (Some(_), Some(name), _) => (None, name),
                (None, None, Some(id)) => (Some(id), format!("(deleted key #{id})")),
                _ => (None, "GENERIC".to_string()),
            };
            Ok(KeyStats {
                api_key_id,
                label,
                requests: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cached_tokens: r.get(6)?,
                total_tokens: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Per-day, per-model buckets for records since `since_ts` (0 = all time).
    /// Returns entries sorted by day then model.
    pub fn daily(&self, since_ts: i64) -> Result<Vec<DailyStats>, DbError> {
        let since_ts = since_ts.max(0);

        let conn = self.pool.get()?;

        let mut stmt = conn.prepare(
            r#"
            SELECT DATE(ts, 'unixepoch', 'localtime') AS day,
                   model,
                   COUNT(*) AS requests,
                   COALESCE(SUM(input_tokens), 0) AS input_tokens,
                   COALESCE(SUM(output_tokens), 0) AS output_tokens,
                   COALESCE(SUM(cached_tokens), 0) AS cached_tokens,
                   COALESCE(SUM(total_tokens), 0) AS total_tokens
            FROM token_usage
            WHERE ts >= ?1
            GROUP BY day, model
            ORDER BY day, model
            "#,
        )?;
        let rows = stmt.query_map(rusqlite::params![since_ts], |r| {
            Ok(DailyStats {
                day: r.get(0)?,
                model: r.get(1)?,
                requests: r.get(2)?,
                input_tokens: r.get(3)?,
                output_tokens: r.get(4)?,
                cached_tokens: r.get(5)?,
                total_tokens: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record an energy record synchronously.
    ///
    /// Non-finite or negative values are rejected with a log before recording.
    pub fn record_energy(&self, elapsed_secs: f64, gpu_watts: f64) {
        for (name, value) in [("elapsed_secs", elapsed_secs), ("gpu_watts", gpu_watts)] {
            if !value.is_finite() {
                tracing::error!("{name}={value} is non-finite, dropping energy record");
                return;
            }
            if value < 0.0 {
                tracing::error!("{name}={value} is negative, dropping energy record");
                return;
            }
        }
        let Some(ts) = now_unix_secs() else {
            tracing::error!("system clock before Unix epoch, dropping energy record");
            return;
        };
        let conn = match self.pool.get() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("pool error, dropping energy record: {e}");
                return;
            }
        };
        if let Err(e) = conn.execute(
            "INSERT INTO energy (ts, elapsed_secs, gpu_watts)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![ts, elapsed_secs, gpu_watts],
        ) {
            tracing::error!("energy record insert failed: {e}");
        }
    }

    /// Per-day aggregated energy (kWh) for records since `since_ts` (0 = all time).
    /// kWh = SUM(elapsed_secs * gpu_watts) / 3_600_000.0.
    pub fn daily_energy(&self, since_ts: i64) -> Result<Vec<DailyEnergy>, DbError> {
        let since_ts = since_ts.max(0);

        let conn = self.pool.get()?;

        let mut stmt = conn.prepare(
            r#"
            SELECT DATE(ts, 'unixepoch', 'localtime') AS day,
                   SUM(elapsed_secs * gpu_watts) / 3600000.0 AS kwh
            FROM energy
            WHERE ts >= ?1
            GROUP BY day
            ORDER BY day
            "#,
        )?;
        let rows = stmt.query_map(rusqlite::params![since_ts], |r| {
            Ok(DailyEnergy {
                day: r.get(0)?,
                kwh: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// All 288 five-minute buckets for the day starting at `day_start_ts`
    /// up to (not including) `end_ts`. On DST transitions, bucket indices are
    /// clamped to [0, 287] so the dashboard always receives exactly 288 entries.
    /// Buckets with no traffic are returned with zeroed counters.
    pub fn today_buckets(
        &self,
        day_start_ts: i64,
        end_ts: i64,
    ) -> Result<Vec<TodayBucket>, DbError> {
        let conn = self.pool.get()?;

        let mut stmt = conn.prepare(
            r#"
            SELECT MIN((ts - ?1) / 300, 287) AS bucket,
                   COUNT(*)        AS requests,
                   COALESCE(SUM(input_tokens),      0) AS input_tokens,
                   COALESCE(SUM(output_tokens),      0) AS output_tokens,
                   COALESCE(SUM(cached_tokens), 0) AS cached_tokens
            FROM token_usage
            WHERE ts >= ?1 AND ts < ?2
            GROUP BY bucket
            ORDER BY bucket
            "#,
        )?;
        let rows = stmt
            .query_map(rusqlite::params![day_start_ts, end_ts], |r| {
                Ok(TodayBucket {
                    bucket: r.get(0)?,
                    requests: r.get(1)?,
                    input_tokens: r.get(2)?,
                    output_tokens: r.get(3)?,
                    cached_tokens: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(fill_288_buckets(rows))
    }

    /// Per-model totals for records since `since_ts` (0 = all time), scoped to a
    /// single user identified by `user_id`. Returns entries sorted by
    /// `total_tokens` descending.
    pub fn stats_for_user(&self, since_ts: i64, user_id: i64) -> Result<Vec<ModelStats>, DbError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT t.model,
                    COUNT(*),
                    COALESCE(SUM(t.input_tokens),0),
                    COALESCE(SUM(t.output_tokens),0),
                    COALESCE(SUM(t.cached_tokens),0),
                    COALESCE(SUM(t.total_tokens),0)
             FROM token_usage t
             INNER JOIN api_key_attribution a ON a.api_key_id = t.api_key_id
             WHERE t.ts >= ?1 AND a.user_id = ?2
             GROUP BY t.model
             ORDER BY SUM(t.total_tokens) DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![since_ts, user_id], |r| {
            Ok(ModelStats {
                model: r.get(0)?,
                requests: r.get(1)?,
                input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                cached_tokens: r.get(4)?,
                total_tokens: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Per-day, per-model buckets for records since `since_ts` (0 = all time),
    /// scoped to a single user identified by `user_id`. Returns entries sorted
    /// by day then model.
    pub fn daily_for_user(&self, since_ts: i64, user_id: i64) -> Result<Vec<DailyStats>, DbError> {
        let since_ts = since_ts.max(0);

        let conn = self.pool.get()?;

        let mut stmt = conn.prepare(
            r#"
            SELECT DATE(t.ts, 'unixepoch', 'localtime') AS day,
                   t.model,
                   COUNT(*) AS requests,
                   COALESCE(SUM(t.input_tokens), 0) AS input_tokens,
                   COALESCE(SUM(t.output_tokens), 0) AS output_tokens,
                   COALESCE(SUM(t.cached_tokens), 0) AS cached_tokens,
                   COALESCE(SUM(t.total_tokens), 0) AS total_tokens
            FROM token_usage t
            INNER JOIN api_key_attribution a ON a.api_key_id = t.api_key_id
            WHERE t.ts >= ?1 AND a.user_id = ?2
            GROUP BY day, t.model
            ORDER BY day, t.model
            "#,
        )?;
        let rows = stmt.query_map(rusqlite::params![since_ts, user_id], |r| {
            Ok(DailyStats {
                day: r.get(0)?,
                model: r.get(1)?,
                requests: r.get(2)?,
                input_tokens: r.get(3)?,
                output_tokens: r.get(4)?,
                cached_tokens: r.get(5)?,
                total_tokens: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Per-key totals for records since `since_ts` (0 = all time), scoped to a
    /// single user identified by `user_id`. Returns entries sorted by
    /// `total_tokens` descending.
    ///
    /// Each row is labeled by the key's original name (or its `twl_` prefix
    /// when unnamed) so revoked keys remain distinguishable in history.
    pub fn stats_by_key_for_user(
        &self,
        since_ts: i64,
        user_id: i64,
    ) -> Result<Vec<KeyStats>, DbError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT t.api_key_id,
                   a.name,
                   a.prefix,
                   COUNT(*),
                   COALESCE(SUM(t.input_tokens),0),
                   COALESCE(SUM(t.output_tokens),0),
                   COALESCE(SUM(t.cached_tokens),0),
                   COALESCE(SUM(t.total_tokens),0)
            FROM token_usage t
            INNER JOIN api_key_attribution a ON a.api_key_id = t.api_key_id
            WHERE t.ts >= ?1 AND a.user_id = ?2
            GROUP BY t.api_key_id
            ORDER BY SUM(t.total_tokens) DESC
            "#,
        )?;
        let rows = stmt.query_map(rusqlite::params![since_ts, user_id], |r| {
            let api_key_id: Option<i64> = r.get(0)?;
            let name: Option<String> = r.get(1)?;
            let prefix: String = r.get(2)?;
            let label = match &name {
                Some(n) if !n.is_empty() => n.clone(),
                _ => prefix,
            };
            Ok(KeyStats {
                api_key_id,
                label,
                requests: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cached_tokens: r.get(6)?,
                total_tokens: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes usage and energy rows recorded strictly before the cutoff
    /// timestamp and returns how many rows were deleted.
    pub fn prune_older_than(&self, cutoff: i64) -> Result<usize, DbError> {
        let conn = self.pool.get()?;
        let usage_count: usize = conn.execute("DELETE FROM token_usage WHERE ts < ?1", [cutoff])?;
        let energy_count: usize = conn.execute("DELETE FROM energy WHERE ts < ?1", [cutoff])?;
        Ok(usage_count + energy_count)
    }

    /// All 288 five-minute buckets for the day starting at `day_start_ts`
    /// up to (not including) `end_ts`, scoped to a single user identified by
    /// `user_id`. On DST transitions, bucket indices are clamped to [0, 287]
    /// so the dashboard always receives exactly 288 entries. Buckets with no
    /// traffic are returned with zeroed counters.
    pub fn today_buckets_for_user(
        &self,
        day_start_ts: i64,
        end_ts: i64,
        user_id: i64,
    ) -> Result<Vec<TodayBucket>, DbError> {
        let conn = self.pool.get()?;

        let mut stmt = conn.prepare(
            r#"
            SELECT MIN((t.ts - ?1) / 300, 287) AS bucket,
                   COUNT(*)        AS requests,
                   COALESCE(SUM(t.input_tokens),      0) AS input_tokens,
                   COALESCE(SUM(t.output_tokens),      0) AS output_tokens,
                   COALESCE(SUM(t.cached_tokens), 0) AS cached_tokens
            FROM token_usage t
            INNER JOIN api_key_attribution a ON a.api_key_id = t.api_key_id
            WHERE t.ts >= ?1 AND t.ts < ?2 AND a.user_id = ?3
            GROUP BY bucket
            ORDER BY bucket
            "#,
        )?;
        let rows = stmt
            .query_map(rusqlite::params![day_start_ts, end_ts, user_id], |r| {
                Ok(TodayBucket {
                    bucket: r.get(0)?,
                    requests: r.get(1)?,
                    input_tokens: r.get(2)?,
                    output_tokens: r.get(3)?,
                    cached_tokens: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(fill_288_buckets(rows))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
