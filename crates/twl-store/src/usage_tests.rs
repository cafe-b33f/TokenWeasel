//! Unit tests for the usage store: token recording, per-model stats, daily
//! aggregation, energy recording, and bucket queries.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::Database;
use crate::usage::UsageStore;
use crate::Usage;

fn temp_db() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current time")
        .as_nanos();
    std::env::temp_dir().join(format!("twl-{}-{nonce}.db", std::process::id()))
}

#[test]
fn usage_write_is_persisted() {
    let path = temp_db();
    {
        let db = Database::open(path.to_str().expect("UTF-8 temp path")).expect("open database");
        let store = UsageStore::new(&db).expect("create usage store");
        store.record(&Usage {
            model: "test-model".into(),
            endpoint: "/v1/chat/completions".into(),
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 2,
            total_tokens: 15,
            api_key_id: None,
        });
    }

    let conn = rusqlite::Connection::open(&path).expect("reopen database");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM token_usage", [], |row| row.get(0))
        .expect("count persisted rows");
    assert_eq!(count, 1);
    drop(conn);
    let _ = std::fs::remove_file(path);
}

#[test]
fn energy_record_and_retrieve() {
    let path = temp_db();
    {
        let db = Database::open(path.to_str().expect("UTF-8 temp path")).expect("open database");
        let store = UsageStore::new(&db).expect("create usage store");
        store.record_energy(3600.0, 500.0);
    }

    let conn = rusqlite::Connection::open(&path).expect("reopen database");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM energy", [], |row| row.get(0))
        .expect("count energy rows");
    assert_eq!(count, 1);
    drop(conn);
    let _ = std::fs::remove_file(path);
}

#[test]
fn daily_energy_aggregation() {
    let path = temp_db();
    {
        let db = Database::open(path.to_str().expect("UTF-8 temp path")).expect("open database");
        let store = UsageStore::new(&db).expect("create usage store");
        store.record_energy(3600.0, 500.0);
        store.record_energy(1800.0, 500.0);
        store.record_energy(2700.0, 300.0);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current time")
        .as_secs() as i64;
    let since = now - 86400;

    let db = Database::open(path.to_str().expect("UTF-8 temp path")).expect("open database");
    let store = UsageStore::new(&db).expect("create usage store");
    let daily = store.daily_energy(since).expect("aggregate daily energy");

    assert_eq!(daily.len(), 1, "expected exactly one day bucket");
    let kwh = daily[0].kwh;
    assert!(
        (kwh - 0.975).abs() < 0.000_1,
        "expected ~0.975 kWh, got {kwh}"
    );

    drop(store);
    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn daily_energy_preserves_small_nonzero_values() {
    let path = temp_db();
    {
        let db = Database::open(path.to_str().expect("UTF-8 temp path")).expect("open database");
        let store = UsageStore::new(&db).expect("create usage store");
        store.record_energy(36000.0, 1.0);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current time")
        .as_secs() as i64;
    let since = now - 86400;

    let db = Database::open(path.to_str().expect("UTF-8 temp path")).expect("open database");
    let store = UsageStore::new(&db).expect("create usage store");
    let daily = store.daily_energy(since).expect("daily_energy");

    assert_eq!(daily.len(), 1);
    let kwh = daily[0].kwh;
    assert!(kwh > 0.0, "energy should be nonzero, got {kwh}");
    assert!((kwh - 0.01).abs() < 0.0001, "expected ~0.01 kWh, got {kwh}");

    drop(store);
    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn energy_record_rejects_invalid_values() {
    let path = temp_db();
    {
        let db = Database::open(path.to_str().expect("UTF-8 temp path")).expect("open database");
        let store = UsageStore::new(&db).expect("create usage store");

        store.record_energy(-1.0, 100.0);
        store.record_energy(f64::NAN, 100.0);
        store.record_energy(f64::INFINITY, 100.0);
        store.record_energy(f64::NEG_INFINITY, 100.0);

        store.record_energy(3600.0, -50.0);
        store.record_energy(3600.0, f64::NAN);
        store.record_energy(3600.0, f64::INFINITY);
        store.record_energy(3600.0, f64::NEG_INFINITY);

        store.record_energy(0.0, 0.0);
        store.record_energy(3600.0, 500.0);

        drop(store);
        drop(db);
    }

    let conn = rusqlite::Connection::open(&path).expect("reopen database");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM energy", [], |row| row.get(0))
        .expect("count energy rows");
    assert_eq!(count, 2, "expected exactly 2 valid rows (0,0 + 3600,500)");
    drop(conn);
    let _ = std::fs::remove_file(path);
}

#[test]
fn today_buckets_returns_full_day_and_aggregates() {
    let path = temp_db();

    let now = chrono::Local::now();
    let start_date = now.date_naive();
    let day_start = start_date
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_local_timezone(chrono::Local)
        .earliest()
        .expect("valid local time")
        .timestamp();

    let db = Database::open(path.to_str().expect("UTF-8 path")).expect("open db");

    {
        let conn = rusqlite::Connection::open(&path).expect("open db");
        conn.execute(
                "INSERT INTO token_usage (ts, model, endpoint, input_tokens, output_tokens, cached_tokens, total_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    day_start + 100 * 300 + 100i64,
                    "m1", "/v1/chat", 10i64, 5i64, 2i64, 15i64,
                ],
            ).expect("insert 1");
        conn.execute(
                "INSERT INTO token_usage (ts, model, endpoint, input_tokens, output_tokens, cached_tokens, total_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    day_start + 100 * 300 + 200i64,
                    "m2", "/v1/chat", 20i64, 10i64, 5i64, 30i64,
                ],
            ).expect("insert 2");
        conn.execute(
                "INSERT INTO token_usage (ts, model, endpoint, input_tokens, output_tokens, cached_tokens, total_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    day_start + 200 * 300 + 50i64,
                    "m1", "/v1/chat", 3i64, 2i64, 1i64, 5i64,
                ],
            ).expect("insert 3");
    }

    let store = UsageStore::new(&db).expect("create usage store");
    let buckets = store
        .today_buckets(day_start, day_start + 86_400)
        .expect("today_buckets");

    assert_eq!(buckets.len(), 288, "expected 288 buckets");

    assert_eq!(buckets[0].bucket, 0);
    assert_eq!(buckets[0].input_tokens, 0);
    assert_eq!(buckets[0].output_tokens, 0);

    assert_eq!(buckets[100].bucket, 100);
    assert_eq!(buckets[100].input_tokens, 30);
    assert_eq!(buckets[100].output_tokens, 15);
    assert_eq!(buckets[100].cached_tokens, 7);
    assert_eq!(buckets[100].requests, 2);

    assert_eq!(buckets[200].bucket, 200);
    assert_eq!(buckets[200].input_tokens, 3);
    assert_eq!(buckets[200].output_tokens, 2);
    assert_eq!(buckets[200].cached_tokens, 1);
    assert_eq!(buckets[200].requests, 1);

    for (i, b) in buckets.iter().enumerate() {
        if i != 100 && i != 200 {
            assert_eq!(b.input_tokens, 0, "bucket {i} should be zero");
            assert_eq!(b.output_tokens, 0, "bucket {i} should be zero");
        }
    }

    drop(store);
    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn record_with_and_without_api_key_id() {
    let path = temp_db();
    let db = Database::open(path.to_str().expect("UTF-8 path")).expect("open db");
    let store = UsageStore::new(&db).expect("create usage store");

    store.record(&Usage {
        model: "no-key-model".into(),
        endpoint: "/v1/chat".into(),
        input_tokens: 10,
        output_tokens: 5,
        cached_tokens: 2,
        total_tokens: 15,
        api_key_id: None,
    });

    store.record(&Usage {
        model: "keyed-model".into(),
        endpoint: "/v1/chat".into(),
        input_tokens: 20,
        output_tokens: 10,
        cached_tokens: 5,
        total_tokens: 30,
        api_key_id: Some(42),
    });

    drop(store);
    drop(db);

    let conn = rusqlite::Connection::open(&path).expect("reopen db");
    let mut stmt = conn
        .prepare("SELECT model, api_key_id FROM token_usage ORDER BY model")
        .expect("prepare query");
    let rows: Vec<(String, Option<i64>)> = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
        })
        .and_then(|iter| iter.collect())
        .expect("read rows");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], ("keyed-model".to_string(), Some(42)));
    assert_eq!(rows[1], ("no-key-model".to_string(), None));

    drop(stmt);
    drop(conn);
    let _ = std::fs::remove_file(path);
}

#[test]
fn stats_by_key_with_null_valid_and_dangling() {
    let path = temp_db();

    {
        let db = Database::open(path.to_str().expect("UTF-8 path")).expect("open db");
        let identity = crate::identity::IdentityStore::new(&db).expect("create identity store");
        let store = UsageStore::new(&db).expect("create usage store");
        let user_id = identity
            .create_user("testuser", "hash", false)
            .expect("create user");
        let key_id = identity
            .insert_api_key(user_id, Some("my-key".into()), "hash123", "twl_")
            .expect("insert key");
        let key_id2 = identity
            .insert_api_key(user_id, Some("my-key-2".into()), "hash456", "twl_")
            .expect("insert key 2");

        store.record(&Usage {
            model: "m1".into(),
            endpoint: "/v1/chat".into(),
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 2,
            total_tokens: 15,
            api_key_id: Some(key_id),
        });

        // A second key for the same user must collapse into the user's row.
        store.record(&Usage {
            model: "m1".into(),
            endpoint: "/v1/chat".into(),
            input_tokens: 4,
            output_tokens: 4,
            cached_tokens: 1,
            total_tokens: 8,
            api_key_id: Some(key_id2),
        });

        store.record(&Usage {
            model: "m2".into(),
            endpoint: "/v1/chat".into(),
            input_tokens: 20,
            output_tokens: 10,
            cached_tokens: 5,
            total_tokens: 30,
            api_key_id: Some(9999),
        });

        store.record(&Usage {
            model: "m3".into(),
            endpoint: "/v1/chat".into(),
            input_tokens: 3,
            output_tokens: 2,
            cached_tokens: 0,
            total_tokens: 5,
            api_key_id: None,
        });

        drop(store);
        drop(identity);
        drop(db);
    }

    let db = Database::open(path.to_str().expect("UTF-8 path")).expect("reopen db");
    let store = UsageStore::new(&db).expect("create usage store");
    let stats = store.stats_by_user(0).expect("stats_by_user");

    assert_eq!(stats[0].api_key_id, Some(9999));
    assert_eq!(stats[0].label, "(deleted key #9999)");
    assert_eq!(stats[0].requests, 1);
    assert_eq!(stats[0].total_tokens, 30);

    // Both of testuser's keys aggregate into a single username-labeled row.
    assert_eq!(stats[1].api_key_id, None);
    assert_eq!(stats[1].label, "testuser");
    assert_eq!(stats[1].requests, 2);
    assert_eq!(stats[1].total_tokens, 23);

    assert_eq!(stats[2].api_key_id, None);
    assert_eq!(stats[2].label, "GENERIC");
    assert_eq!(stats[2].requests, 1);
    assert_eq!(stats[2].total_tokens, 5);

    drop(store);
    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn prune_older_than_removes_old_rows() {
    let path = temp_db();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current time")
        .as_secs() as i64;
    let old_ts = now - 86400;
    let cutoff = now - 43200;

    {
        let db = Database::open(path.to_str().expect("UTF-8 path")).expect("open db");
        let conn = db.connect().expect("raw conn");
        conn.execute(
            "INSERT INTO token_usage (ts, model, endpoint, input_tokens, output_tokens, cached_tokens, total_tokens)
             VALUES (?1, 'old-model', '/v1/chat', 10, 5, 2, 15)",
            rusqlite::params![old_ts],
        ).expect("insert old token_usage");
        conn.execute(
            "INSERT INTO energy (ts, elapsed_secs, gpu_watts)
             VALUES (?1, 3600.0, 500.0)",
            rusqlite::params![old_ts],
        )
        .expect("insert old energy");
        drop(conn);
        drop(db);
    }

    {
        let db = Database::open(path.to_str().expect("UTF-8 path")).expect("open db");
        let store = UsageStore::new(&db).expect("create usage store");
        store.record(&Usage {
            model: "recent-model".into(),
            endpoint: "/v1/chat".into(),
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 2,
            total_tokens: 15,
            api_key_id: None,
        });
        store.record_energy(3600.0, 500.0);

        let deleted = store.prune_older_than(cutoff).expect("prune_older_than");
        assert_eq!(deleted, 2, "expected 2 rows deleted");

        drop(store);

        let conn = db.connect().expect("reopen db");

        // Recent token_usage should remain
        let recent_tokens: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM token_usage WHERE model = 'recent-model'",
                [],
                |r| r.get(0),
            )
            .expect("count recent token");
        assert_eq!(recent_tokens, 1, "recent token_usage should remain");

        // Recent energy should remain
        let recent_energy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM energy WHERE elapsed_secs = 3600.0 AND gpu_watts = 500.0",
                [],
                |r| r.get(0),
            )
            .expect("count recent energy");
        assert_eq!(recent_energy, 1, "recent energy should remain");

        // Old rows should be gone
        let old_tokens: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM token_usage WHERE ts < ?1",
                rusqlite::params![cutoff],
                |r| r.get(0),
            )
            .expect("count old token");
        assert_eq!(old_tokens, 0, "old token_usage should be deleted");

        let old_energy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM energy WHERE ts < ?1",
                rusqlite::params![cutoff],
                |r| r.get(0),
            )
            .expect("count old energy");
        assert_eq!(old_energy, 0, "old energy should be deleted");
    }

    let _ = std::fs::remove_file(path);
}

#[test]
fn password_key_revocation_preserves_all_user_usage_views() {
    let path = temp_db();
    let db = Database::open(path.to_str().expect("UTF-8 path")).unwrap();
    let identity = crate::identity::IdentityStore::new(&db).unwrap();
    let store = UsageStore::new(&db).unwrap();
    let user_id = identity.create_user("alice", "old_hash", false).unwrap();
    let key_id = identity
        .insert_api_key(user_id, Some("retired key".into()), "key_hash", "twl_old")
        .unwrap();

    store.record(&Usage {
        model: "history-model".into(),
        endpoint: "/v1/chat".into(),
        input_tokens: 7,
        output_tokens: 3,
        cached_tokens: 1,
        total_tokens: 10,
        api_key_id: Some(key_id),
    });
    identity
        .replace_password_and_revoke_keys(user_id, "new_hash")
        .unwrap();
    assert!(identity.list_api_keys(user_id).unwrap().is_empty());

    let totals = store.stats_for_user(0, user_id).unwrap();
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].total_tokens, 10);
    assert_eq!(store.daily_for_user(0, user_id).unwrap().len(), 1);
    let keys = store.stats_by_key_for_user(0, user_id).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].api_key_id, Some(key_id));
    assert_eq!(keys[0].label, "retired key");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(
        store
            .today_buckets_for_user(now - 300, now + 300, user_id)
            .unwrap()
            .iter()
            .map(|bucket| bucket.requests)
            .sum::<i64>(),
        1
    );

    drop(store);
    drop(identity);
    drop(db);
    let _ = std::fs::remove_file(path);
}
