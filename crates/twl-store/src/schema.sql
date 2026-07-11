-- Idempotent declarative schema for twl-store.
-- Every statement is safe to re-run (CREATE TABLE/INDEX IF NOT EXISTS).

CREATE TABLE IF NOT EXISTS token_usage (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts              INTEGER NOT NULL,
    model           TEXT    NOT NULL,
    endpoint        TEXT    NOT NULL,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cached_tokens   INTEGER NOT NULL DEFAULT 0,
    total_tokens    INTEGER NOT NULL DEFAULT 0,
    api_key_id      INTEGER
);

CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(model);
CREATE INDEX IF NOT EXISTS idx_token_usage_ts    ON token_usage(ts);

CREATE TABLE IF NOT EXISTS energy (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           INTEGER NOT NULL,
    elapsed_secs REAL    NOT NULL,
    gpu_watts    REAL    NOT NULL DEFAULT 0.0
);

CREATE INDEX IF NOT EXISTS idx_energy_ts ON energy(ts);

CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    created_ts    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS api_keys (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id   INTEGER NOT NULL REFERENCES users(id),
    name      TEXT,
    key_hash  TEXT    NOT NULL UNIQUE,
    prefix    TEXT    NOT NULL,
    created_ts INTEGER NOT NULL
);

-- Durable ownership metadata for usage attribution. Unlike credentials in
-- api_keys, these rows survive key revocation; deleting the owning account
-- removes them via ON DELETE CASCADE.
CREATE TABLE IF NOT EXISTS api_key_attribution (
    api_key_id INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT,
    prefix     TEXT NOT NULL
);

-- Backfill databases created before api_key_attribution existed.
INSERT OR IGNORE INTO api_key_attribution (api_key_id, user_id, name, prefix)
SELECT id, user_id, name, prefix FROM api_keys;
