//! SQLite store (plan.md §12). Later milestones append migrations to
//! `MIGRATIONS`. The VM spec JSON is canonical; nothing else duplicates it.

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
    // 2: VMs, runs, event log
    r#"
    CREATE TABLE vm (
        id           TEXT NOT NULL PRIMARY KEY,
        name         TEXT NOT NULL UNIQUE,
        spec_json    TEXT NOT NULL,
        state        TEXT NOT NULL DEFAULT 'stopped',
        pause_reason TEXT,
        last_error   TEXT,
        autostart    INTEGER NOT NULL DEFAULT 0,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL
    );
    CREATE TABLE vm_run (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        vm_id       TEXT NOT NULL REFERENCES vm(id) ON DELETE CASCADE,
        args_json   TEXT NOT NULL,
        spec_json   TEXT NOT NULL,
        accel       TEXT NOT NULL,
        pid         INTEGER NOT NULL,
        proc_start  INTEGER NOT NULL,
        run_dir     TEXT NOT NULL,
        vnc_json    TEXT,
        log_path    TEXT NOT NULL,
        started_at  INTEGER NOT NULL,
        ended_at    INTEGER,
        exit_status TEXT
    );
    CREATE INDEX vm_run_open ON vm_run(vm_id) WHERE ended_at IS NULL;
    CREATE TABLE event (
        seq       INTEGER PRIMARY KEY AUTOINCREMENT,
        vm_id     TEXT NOT NULL,
        at_us     INTEGER NOT NULL,
        kind      TEXT NOT NULL,
        data_json TEXT NOT NULL
    );
    CREATE INDEX event_vm ON event(vm_id, seq);
    "#,
];

/// A VM row.
#[derive(Debug, Clone)]
pub struct VmRow {
    pub id: String,
    pub name: String,
    pub spec_json: String,
    pub state: String,
    pub pause_reason: Option<String>,
    pub last_error: Option<String>,
}

/// A run (one QEMU process) row.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: i64,
    pub vm_id: String,
    pub args_json: String,
    pub spec_json: String,
    pub accel: String,
    pub pid: u32,
    pub proc_start: u64,
    pub run_dir: String,
    pub vnc_json: Option<String>,
    pub log_path: String,
    pub started_at: u64,
    pub ended_at: Option<u64>,
}

pub struct NewRun<'a> {
    pub vm_id: &'a str,
    pub args_json: &'a str,
    pub spec_json: &'a str,
    pub accel: &'a str,
    pub pid: u32,
    pub proc_start: u64,
    pub run_dir: &'a str,
    pub vnc_json: Option<&'a str>,
    pub log_path: &'a str,
    pub started_at: u64,
}

/// An event row: (seq, vm_id, at_us, kind, data_json).
pub type EventRow = (i64, String, u64, String, String);

fn vm_row(r: &rusqlite::Row) -> rusqlite::Result<VmRow> {
    Ok(VmRow {
        id: r.get(0)?,
        name: r.get(1)?,
        spec_json: r.get(2)?,
        state: r.get(3)?,
        pause_reason: r.get(4)?,
        last_error: r.get(5)?,
    })
}

const VM_COLS: &str = "id, name, spec_json, state, pause_reason, last_error";
const RUN_COLS: &str = "id, vm_id, args_json, spec_json, accel, pid, proc_start, run_dir, \
                        vnc_json, log_path, started_at, ended_at";

fn run_row(r: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get(0)?,
        vm_id: r.get(1)?,
        args_json: r.get(2)?,
        spec_json: r.get(3)?,
        accel: r.get(4)?,
        pid: r.get::<_, i64>(5)? as u32,
        proc_start: r.get::<_, i64>(6)? as u64,
        run_dir: r.get(7)?,
        vnc_json: r.get(8)?,
        log_path: r.get(9)?,
        started_at: r.get::<_, i64>(10)? as u64,
        ended_at: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
    })
}

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

impl Store {
    pub fn insert_vm(&self, id: &str, name: &str, spec_json: &str, now: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO vm (id, name, spec_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![id, name, spec_json, now as i64],
            )
            .map(|_| ())
        })
    }

    pub fn update_vm_spec(&self, id: &str, name: &str, spec_json: &str, now: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE vm SET name = ?2, spec_json = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, name, spec_json, now as i64],
            )
            .map(|_| ())
        })
    }

    /// Set state; `last_error` of `None` keeps the previous error.
    pub fn set_vm_state(
        &self,
        id: &str,
        state: &str,
        pause_reason: Option<&str>,
        last_error: Option<&str>,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE vm SET state = ?2, pause_reason = ?3, last_error = COALESCE(?4, last_error) WHERE id = ?1",
                params![id, state, pause_reason, last_error],
            )
            .map(|_| ())
        })
    }

    pub fn clear_vm_error(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE vm SET last_error = NULL WHERE id = ?1", [id])
                .map(|_| ())
        })
    }

    pub fn list_vms(&self) -> Result<Vec<VmRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {VM_COLS} FROM vm ORDER BY name"))?;
            st.query_map([], vm_row)?.collect()
        })
    }

    pub fn get_vm(&self, id: &str) -> Result<Option<VmRow>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {VM_COLS} FROM vm WHERE id = ?1"),
                [id],
                vm_row,
            )
            .optional()
        })
    }

    pub fn vm_name_taken(&self, name: &str, except_id: Option<&str>) -> Result<bool> {
        self.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM vm WHERE name = ?1 AND id != COALESCE(?2, '')",
                params![name, except_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
        })
    }

    pub fn delete_vm(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM event WHERE vm_id = ?1", [id])?;
            c.execute("DELETE FROM vm WHERE id = ?1", [id]).map(|_| ())
        })
    }

    pub fn insert_run(&self, r: &NewRun) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO vm_run (vm_id, args_json, spec_json, accel, pid, proc_start, run_dir, \
                 vnc_json, log_path, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    r.vm_id,
                    r.args_json,
                    r.spec_json,
                    r.accel,
                    r.pid as i64,
                    r.proc_start as i64,
                    r.run_dir,
                    r.vnc_json,
                    r.log_path,
                    r.started_at as i64
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn end_run(&self, run_id: i64, now: u64, exit_status: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE vm_run SET ended_at = ?2, exit_status = ?3 WHERE id = ?1 AND ended_at IS NULL",
                params![run_id, now as i64, exit_status],
            )
            .map(|_| ())
        })
    }

    pub fn open_runs(&self) -> Result<Vec<RunRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {RUN_COLS} FROM vm_run WHERE ended_at IS NULL"
            ))?;
            st.query_map([], run_row)?.collect()
        })
    }

    pub fn last_run(&self, vm_id: &str) -> Result<Option<RunRow>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {RUN_COLS} FROM vm_run WHERE vm_id = ?1 ORDER BY id DESC LIMIT 1"),
                [vm_id],
                run_row,
            )
            .optional()
        })
    }

    /// Append an event; returns its global sequence number.
    pub fn insert_event(
        &self,
        vm_id: &str,
        at_us: u64,
        kind: &str,
        data_json: &str,
    ) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO event (vm_id, at_us, kind, data_json) VALUES (?1, ?2, ?3, ?4)",
                params![vm_id, at_us as i64, kind, data_json],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// The most recent `limit` events of one VM, oldest first.
    pub fn events_for_vm(&self, vm_id: &str, limit: u32) -> Result<Vec<EventRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT seq, vm_id, at_us, kind, data_json FROM                  (SELECT * FROM event WHERE vm_id = ?1 ORDER BY seq DESC LIMIT ?2) ORDER BY seq",
            )?;
            st.query_map(params![vm_id, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64, r.get(3)?, r.get(4)?))
            })?
            .collect()
        })
    }

    /// Events after `seq`, oldest first.
    pub fn events_after(&self, seq: i64, limit: u32) -> Result<Vec<EventRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT seq, vm_id, at_us, kind, data_json FROM event WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            )?;
            st.query_map(params![seq, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64, r.get(3)?, r.get(4)?))
            })?
            .collect()
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

    #[test]
    fn vms_runs_events() {
        let s = Store::memory().unwrap();
        s.insert_vm("v1", "alpha", "{}", 1).unwrap();
        assert!(s.vm_name_taken("alpha", None).unwrap());
        assert!(!s.vm_name_taken("alpha", Some("v1")).unwrap());
        s.set_vm_state("v1", "crashed", None, Some("boom")).unwrap();
        s.set_vm_state("v1", "running", None, None).unwrap();
        let v = s.get_vm("v1").unwrap().unwrap();
        assert_eq!(
            (v.state.as_str(), v.last_error.as_deref()),
            ("running", Some("boom"))
        );

        let run = NewRun {
            vm_id: "v1",
            args_json: "[]",
            spec_json: "{}",
            accel: "tcg",
            pid: 42,
            proc_start: 7,
            run_dir: "/r",
            vnc_json: None,
            log_path: "/l",
            started_at: 9,
        };
        let id = s.insert_run(&run).unwrap();
        assert_eq!(s.open_runs().unwrap().len(), 1);
        s.end_run(id, 10, "exit 0").unwrap();
        assert!(s.open_runs().unwrap().is_empty());
        assert_eq!(s.last_run("v1").unwrap().unwrap().ended_at, Some(10));

        let a = s.insert_event("v1", 1, "STOP", "{}").unwrap();
        let b = s.insert_event("v1", 2, "RESUME", "{}").unwrap();
        assert!(b > a);
        assert_eq!(s.events_after(a, 10).unwrap().len(), 1);

        s.delete_vm("v1").unwrap();
        assert!(s.get_vm("v1").unwrap().is_none());
        assert!(s.last_run("v1").unwrap().is_none(), "runs cascade");
    }
}
