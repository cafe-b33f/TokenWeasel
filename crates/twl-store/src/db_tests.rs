//! Unit tests for the SQLite core: connection management, schema creation, and
//! the writer queue mechanism.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::Database;

fn temp_db() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current time")
        .as_nanos();
    std::env::temp_dir().join(format!("twl-{}-{nonce}.db", std::process::id()))
}

#[test]
fn energy_table_exists_independently() {
    let path = temp_db();
    {
        let _db = Database::open(path.to_str().expect("UTF-8 temp path")).expect("open database");
    }

    let conn = rusqlite::Connection::open(&path).expect("reopen database");
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepare")
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect rows");

    assert!(tables.contains(&"token_usage".to_string()));
    assert!(tables.contains(&"energy".to_string()));
    assert!(tables.contains(&"users".to_string()));
    assert!(tables.contains(&"api_keys".to_string()));

    drop(conn);
    let _ = std::fs::remove_file(path);
}

#[test]
fn db_open_invalid_path_returns_err() {
    let bad_path = "/proc/nonexistent_twl_test_19f7a3e2/db.sqlite";
    let result = Database::open(bad_path);
    assert!(
        result.is_err(),
        "Database::open should fail for an unopenable path"
    );
}

#[test]
fn every_managed_connection_enforces_foreign_keys() {
    let path = temp_db();
    let db = Database::open(path.to_str().expect("UTF-8 temp path")).unwrap();

    let fresh_enabled: i64 = db
        .connect()
        .unwrap()
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .unwrap();
    let pooled_enabled: i64 = db
        .pool()
        .get()
        .unwrap()
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .unwrap();

    assert_eq!(fresh_enabled, 1);
    assert_eq!(pooled_enabled, 1);

    drop(db);
    let _ = std::fs::remove_file(path);
}
