//! SQLite store (plan.md §12). Only the M0 tables exist so far; later
//! milestones append migrations to `MIGRATIONS`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Ordered migrations. Never edit a shipped entry; append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: binaries and capability snapshots
    r#"
    CREATE TABLE binary_hash (
        path   TEXT NOT NULL PRIMARY KEY,
        size   INTEGER NOT NULL,
        mtime  INTEGER NOT NULL,
        sha256 TEXT NOT NULL
    );
    CREATE TABLE cap_snapshot (
        sha256     TEXT NOT NULL PRIMARY KEY,
        probed_at  INTEGER NOT NULL,
        data_json  TEXT NOT NULL
    );
    CREATE TABLE kv (
        key   TEXT NOT NULL PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
];

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let current = usize::try_from(current).unwrap_or(0);
        if current > MIGRATIONS.len() {
            anyhow::bail!(
                "database schema version {current} is newer than this build supports ({}); \
                 refusing to open it",
                MIGRATIONS.len()
            );
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("migration {}", i + 1))?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(f(&conn)?)
    }

    pub fn cached_hash(&self, path: &str, size: u64, mtime: u64) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row(
                "SELECT sha256 FROM binary_hash WHERE path = ?1 AND size = ?2 AND mtime = ?3",
                params![path, size as i64, mtime as i64],
                |r| r.get(0),
            )
            .optional()
        })
    }

    pub fn put_hash(&self, path: &str, size: u64, mtime: u64, sha256: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO binary_hash (path, size, mtime, sha256) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET size = ?2, mtime = ?3, sha256 = ?4",
                params![path, size as i64, mtime as i64, sha256],
            )
            .map(|_| ())
        })
    }

    pub fn snapshot(&self, sha256: &str) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row(
                "SELECT data_json FROM cap_snapshot WHERE sha256 = ?1",
                [sha256],
                |r| r.get(0),
            )
            .optional()
        })
    }

    pub fn put_snapshot(&self, sha256: &str, probed_at: u64, json: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO cap_snapshot (sha256, probed_at, data_json) VALUES (?1, ?2, ?3)
                 ON CONFLICT(sha256) DO UPDATE SET probed_at = ?2, data_json = ?3",
                params![sha256, probed_at as i64, json],
            )
            .map(|_| ())
        })
    }

    pub fn get_kv(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
                .optional()
        })
    }

    pub fn put_kv(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
                params![key, value],
            )
            .map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_and_roundtrips() {
        let s = Store::memory().unwrap();
        assert_eq!(s.cached_hash("/q", 1, 2).unwrap(), None);
        s.put_hash("/q", 1, 2, "abc").unwrap();
        assert_eq!(s.cached_hash("/q", 1, 2).unwrap().as_deref(), Some("abc"));
        assert_eq!(
            s.cached_hash("/q", 1, 3).unwrap(),
            None,
            "mtime change invalidates"
        );
        s.put_snapshot("abc", 5, "{}").unwrap();
        assert_eq!(s.snapshot("abc").unwrap().as_deref(), Some("{}"));
        s.put_kv("k", "v").unwrap();
        s.put_kv("k", "w").unwrap();
        assert_eq!(s.get_kv("k").unwrap().as_deref(), Some("w"));
    }
}
