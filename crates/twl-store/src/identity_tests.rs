//! Unit tests for the identity store: user CRUD, API key management, session
//! lifecycle, and the last-admin guard.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::Database;
use crate::identity::{DeleteUserOutcome, IdentityStore};

fn temp_db() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current time")
        .as_nanos();
    std::env::temp_dir().join(format!("twl-{}-{nonce}.db", std::process::id()))
}

fn setup() -> (Database, IdentityStore, std::path::PathBuf) {
    let path = temp_db();
    let db = Database::open(path.to_str().expect("UTF-8 path")).expect("open db");
    let store = IdentityStore::new(&db).expect("create identity store");
    (db, store, path)
}

#[test]
fn create_and_count_users() {
    let (_db, store, path) = setup();
    assert_eq!(store.count_users().unwrap(), 0);

    let id = store.create_user("alice", "hash_alice", false).unwrap();
    assert!(id > 0);
    assert_eq!(store.count_users().unwrap(), 1);

    let id2 = store.create_user("bob", "hash_bob", true).unwrap();
    assert!(id2 > id);
    assert_eq!(store.count_users().unwrap(), 2);

    let _ = std::fs::remove_file(path);
}

#[test]
fn get_user_by_username() {
    let (_db, store, path) = setup();
    store.create_user("alice", "hash_alice", false).unwrap();

    let user = store.get_user("alice").unwrap().expect("user exists");
    assert_eq!(user.username, "alice");
    assert_eq!(user.password_hash, "hash_alice");
    assert!(!user.is_admin);
    assert!(user.created_ts > 0);

    assert!(store.get_user("nonexistent").unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn get_user_by_id() {
    let (_db, store, path) = setup();
    let id = store.create_user("alice", "hash_alice", true).unwrap();

    let user = store.get_user_by_id(id).unwrap().expect("user exists");
    assert_eq!(user.username, "alice");
    assert!(user.is_admin);

    assert!(store.get_user_by_id(9999).unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn list_users() {
    let (_db, store, path) = setup();
    store.create_user("bob", "hash_bob", true).unwrap();
    store.create_user("alice", "hash_alice", false).unwrap();

    let users = store.list_users().unwrap();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0].username, "bob");
    assert_eq!(users[1].username, "alice");

    let _ = std::fs::remove_file(path);
}

#[test]
fn update_password() {
    let (_db, store, path) = setup();
    let id = store.create_user("alice", "old_hash", false).unwrap();

    let rows = store.update_password(id, "new_hash").unwrap();
    assert_eq!(rows, 1);

    let user = store.get_user_by_id(id).unwrap().unwrap();
    assert_eq!(user.password_hash, "new_hash");

    let _ = std::fs::remove_file(path);
}

#[test]
fn password_and_key_revocation_roll_back_together() {
    let (db, store, path) = setup();
    let id = store.create_user("alice", "old_hash", false).unwrap();
    store
        .insert_api_key(id, None, "key_hash", "twl_key")
        .unwrap();

    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_key_revocation
             BEFORE DELETE ON api_keys
             BEGIN
               SELECT RAISE(ABORT, 'injected revocation failure');
             END;",
        )
        .unwrap();

    assert!(store
        .replace_password_and_revoke_keys(id, "new_hash")
        .is_err());
    assert_eq!(
        store.get_user_by_id(id).unwrap().unwrap().password_hash,
        "old_hash"
    );
    assert_eq!(store.list_api_keys(id).unwrap().len(), 1);

    drop(store);
    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn stale_password_compare_and_swap_cannot_overwrite_reset() {
    let (_db, store, path) = setup();
    let id = store.create_user("alice", "original_hash", false).unwrap();
    store
        .insert_api_key(id, None, "old_key_hash", "twl_old")
        .unwrap();

    // The administrator's reset replaces the hash and revokes the credentials
    // that existed before recovery.
    assert_eq!(
        store
            .replace_password_and_revoke_keys(id, "admin_reset_hash")
            .unwrap(),
        1
    );
    assert!(store.list_api_keys(id).unwrap().is_empty());

    // A credential created after recovery must not be revoked by the stale
    // request either: a failed compare-and-swap has no side effects.
    store
        .insert_api_key(id, None, "recovery_key_hash", "twl_new")
        .unwrap();
    let updated = store
        .compare_and_swap_password_and_revoke_keys(id, "original_hash", "stale_change_hash")
        .unwrap();

    assert_eq!(updated, 0);
    assert_eq!(
        store.get_user_by_id(id).unwrap().unwrap().password_hash,
        "admin_reset_hash"
    );
    assert_eq!(store.list_api_keys(id).unwrap().len(), 1);

    let _ = std::fs::remove_file(path);
}

#[test]
fn compare_and_swap_password_and_key_revocation_are_atomic() {
    let (db, store, path) = setup();
    let id = store.create_user("alice", "old_hash", false).unwrap();
    store
        .insert_api_key(id, None, "key_hash", "twl_key")
        .unwrap();

    let conn = db.connect().unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_cas_key_revocation
         BEFORE DELETE ON api_keys
         BEGIN
           SELECT RAISE(ABORT, 'injected revocation failure');
         END;",
    )
    .unwrap();

    assert!(store
        .compare_and_swap_password_and_revoke_keys(id, "old_hash", "new_hash")
        .is_err());
    assert_eq!(
        store.get_user_by_id(id).unwrap().unwrap().password_hash,
        "old_hash"
    );
    assert_eq!(store.list_api_keys(id).unwrap().len(), 1);

    conn.execute_batch("DROP TRIGGER reject_cas_key_revocation;")
        .unwrap();
    assert_eq!(
        store
            .compare_and_swap_password_and_revoke_keys(id, "old_hash", "new_hash")
            .unwrap(),
        1
    );
    assert_eq!(
        store.get_user_by_id(id).unwrap().unwrap().password_hash,
        "new_hash"
    );
    assert!(store.list_api_keys(id).unwrap().is_empty());

    drop(conn);
    drop(store);
    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn delete_user_also_deletes_keys() {
    let (_db, store, path) = setup();
    let user_id = store.create_user("alice", "hash", false).unwrap();
    store
        .insert_api_key(user_id, Some("key1".into()), "hash1", "twl_")
        .unwrap();
    store
        .insert_api_key(user_id, Some("key2".into()), "hash2", "twl_")
        .unwrap();
    assert_eq!(store.list_api_keys(user_id).unwrap().len(), 2);

    let outcome = store.delete_user_guarded(user_id).unwrap();
    assert_eq!(outcome, DeleteUserOutcome::Deleted);
    assert!(store.get_user_by_id(user_id).unwrap().is_none());
    assert_eq!(store.list_api_keys(user_id).unwrap().len(), 0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn api_key_crud() {
    let (_db, store, path) = setup();
    let user_id = store.create_user("alice", "hash", false).unwrap();

    let key_id = store
        .insert_api_key(user_id, Some("test key".into()), "keyhash", "twl_")
        .unwrap();
    assert!(key_id > 0);

    let keys = store.list_api_keys(user_id).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].name.as_deref(), Some("test key"));
    assert_eq!(keys[0].prefix, "twl_");

    let keys = store.list_api_keys(9999).unwrap();
    assert!(keys.is_empty());

    let _ = std::fs::remove_file(path);
}

#[test]
fn deleted_or_missing_users_cannot_gain_api_keys() {
    let (db, store, path) = setup();

    // Covers the creation-after-deletion side of the session/delete race:
    // the stale user id must fail at the insert itself.
    let user_id = store.create_user("alice", "hash", false).unwrap();
    assert_eq!(
        store.delete_user_guarded(user_id).unwrap(),
        DeleteUserOutcome::Deleted
    );
    assert!(store
        .insert_api_key(user_id, None, "stale_hash", "twl_stale")
        .is_err());

    // A legacy/imported orphan may predate FK enforcement. It still must not
    // validate after upgrade.
    let legacy = rusqlite::Connection::open(&path).unwrap();
    legacy.pragma_update(None, "foreign_keys", "OFF").unwrap();
    legacy
        .execute(
            "INSERT INTO api_keys (user_id, name, key_hash, prefix, created_ts)
             VALUES (?1, NULL, ?2, ?3, 1)",
            rusqlite::params![user_id, "legacy_orphan", "twl_legacy"],
        )
        .unwrap();
    assert!(store.get_api_key_id("legacy_orphan").unwrap().is_none());

    drop(legacy);
    drop(store);
    drop(db);
    let _ = std::fs::remove_file(path);
}

#[test]
fn delete_api_key_scoped_to_user() {
    let (_db, store, path) = setup();
    let user_id = store.create_user("alice", "hash", false).unwrap();
    let key_id = store.insert_api_key(user_id, None, "hash", "twl_").unwrap();

    assert_eq!(store.delete_api_key(key_id, 9999).unwrap(), 0);
    assert_eq!(store.list_api_keys(user_id).unwrap().len(), 1);

    assert_eq!(store.delete_api_key(key_id, user_id).unwrap(), 1);
    assert_eq!(store.list_api_keys(user_id).unwrap().len(), 0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn get_api_key_user_and_id() {
    let (_db, store, path) = setup();
    let user_id = store.create_user("alice", "hash", false).unwrap();
    store
        .insert_api_key(user_id, None, "keyhash", "twl_")
        .unwrap();

    let user = store
        .get_api_key_user("keyhash")
        .unwrap()
        .expect("key user");
    assert_eq!(user.username, "alice");

    let pair = store.get_api_key_id("keyhash").unwrap().expect("key id");
    assert_eq!(pair.1, user_id);

    assert!(store.get_api_key_user("nope").unwrap().is_none());
    assert!(store.get_api_key_id("nope").unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn delete_api_keys_for_user_removes_all() {
    let (_db, store, path) = setup();
    let user_id = store.create_user("alice", "hash", false).unwrap();

    store.insert_api_key(user_id, None, "kh1", "twl_").unwrap();
    store.insert_api_key(user_id, None, "kh2", "twl_").unwrap();

    assert_eq!(store.delete_api_keys_for_user(user_id).unwrap(), 2);

    assert!(store.get_api_key_user("kh1").unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn create_user_duplicate_username_returns_unique_violation() {
    let (_db, store, path) = setup();
    store.create_user("alice", "hash", false).unwrap();
    let err = store.create_user("alice", "other_hash", false).unwrap_err();
    assert!(
        matches!(err, crate::db::DbError::UniqueViolation),
        "expected UniqueViolation, got {err:?}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn guarded_delete_removes_non_admin() {
    let (_db, store, path) = setup();
    let alice_id = store.create_user("alice", "hash_alice", false).unwrap();
    let root_id = store.create_user("root", "hash_root", true).unwrap();
    assert!(alice_id > 0);
    assert!(root_id > alice_id);

    let outcome = store.delete_user_guarded(alice_id).unwrap();
    assert_eq!(outcome, DeleteUserOutcome::Deleted);
    assert_eq!(store.count_users().unwrap(), 1);

    let _ = std::fs::remove_file(path);
}

#[test]
fn guarded_delete_missing_user_returns_not_found() {
    let (_db, store, path) = setup();
    let root_id = store.create_user("root", "hash_root", true).unwrap();
    assert!(root_id > 0);
    assert_eq!(store.count_users().unwrap(), 1);

    let outcome = store.delete_user_guarded(9999).unwrap();
    assert_eq!(outcome, DeleteUserOutcome::NotFound);
    assert_eq!(store.count_users().unwrap(), 1);

    let _ = std::fs::remove_file(path);
}

#[test]
fn guarded_delete_last_admin_is_blocked() {
    let (_db, store, path) = setup();
    let root_id = store.create_user("root", "hash_root", true).unwrap();
    assert!(root_id > 0);

    let outcome = store.delete_user_guarded(root_id).unwrap();
    assert_eq!(outcome, DeleteUserOutcome::LastAdmin);
    assert_eq!(store.count_users().unwrap(), 1);

    let root2_id = store.create_user("root2", "hash_root2", true).unwrap();
    assert!(root2_id > root_id);

    let outcome = store.delete_user_guarded(root_id).unwrap();
    assert_eq!(outcome, DeleteUserOutcome::Deleted);
    assert_eq!(store.count_users().unwrap(), 1);

    let _ = std::fs::remove_file(path);
}
