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
    // 3: storage pools and jobs (M2)
    r#"
    CREATE TABLE pool (
        name        TEXT NOT NULL PRIMARY KEY,
        path        TEXT NOT NULL UNIQUE,
        quota_bytes INTEGER,
        readonly    INTEGER NOT NULL DEFAULT 0,
        created_at  INTEGER NOT NULL
    );
    CREATE TABLE job (
        id          TEXT NOT NULL PRIMARY KEY,
        kind        TEXT NOT NULL,
        title       TEXT NOT NULL,
        status      TEXT NOT NULL,
        progress    REAL,
        message     TEXT,
        error       TEXT,
        target_json TEXT NOT NULL,
        log_path    TEXT,
        created_at  INTEGER NOT NULL,
        started_at  INTEGER,
        finished_at INTEGER
    );
    CREATE INDEX job_created ON job(created_at);
    "#,
    // 4: template images (M2-05)
    r#"
    CREATE TABLE image_template (
        pool TEXT NOT NULL,
        name TEXT NOT NULL,
        PRIMARY KEY (pool, name)
    );
    "#,
    // 5: VM snapshots (M2-06)
    r#"
    CREATE TABLE snapshot (
        id          TEXT NOT NULL PRIMARY KEY,
        vm_id       TEXT NOT NULL REFERENCES vm(id) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '',
        tag         TEXT NOT NULL,
        parent_id   TEXT,
        with_memory INTEGER NOT NULL,
        auto        INTEGER NOT NULL DEFAULT 0,
        disks_json  TEXT NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX snapshot_vm ON snapshot(vm_id);
    CREATE TABLE snapshot_head (
        vm_id       TEXT NOT NULL PRIMARY KEY REFERENCES vm(id) ON DELETE CASCADE,
        snapshot_id TEXT
    );
    "#,
    // 6: resumable uploads (M2-07)
    r#"
    CREATE TABLE upload (
        id          TEXT NOT NULL PRIMARY KEY,
        pool        TEXT NOT NULL,
        name        TEXT NOT NULL,
        size        INTEGER NOT NULL,
        sha256      TEXT,
        client_key  TEXT,
        job_id      TEXT,
        created_at  INTEGER NOT NULL,
        UNIQUE (pool, name)
    );
    "#,
    // 7: shared folders (M2-13): share roots and per-run sidecar processes
    r#"
    CREATE TABLE share_root (
        name       TEXT NOT NULL PRIMARY KEY,
        path       TEXT NOT NULL UNIQUE,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE run_sidecar (
        run_id     INTEGER NOT NULL REFERENCES vm_run(id) ON DELETE CASCADE,
        kind       TEXT NOT NULL,
        pid        INTEGER NOT NULL,
        proc_start INTEGER NOT NULL
    );
    CREATE INDEX run_sidecar_run ON run_sidecar(run_id);
    "#,
];

/// A resumable upload session row.
#[derive(Debug, Clone)]
pub struct UploadRow {
    pub id: String,
    pub pool: String,
    pub name: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub client_key: Option<String>,
    pub job_id: Option<String>,
    pub created_at: u64,
}

fn upload_row(r: &rusqlite::Row) -> rusqlite::Result<UploadRow> {
    Ok(UploadRow {
        id: r.get(0)?,
        pool: r.get(1)?,
        name: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        sha256: r.get(4)?,
        client_key: r.get(5)?,
        job_id: r.get(6)?,
        created_at: r.get::<_, i64>(7)? as u64,
    })
}

const UPLOAD_COLS: &str = "id, pool, name, size, sha256, client_key, job_id, created_at";

/// A snapshot row.
#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub id: String,
    pub vm_id: String,
    pub name: String,
    pub description: String,
    pub tag: String,
    pub parent_id: Option<String>,
    pub with_memory: bool,
    pub auto: bool,
    pub disks_json: String,
    pub created_at: u64,
}

fn snapshot_row(r: &rusqlite::Row) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        id: r.get(0)?,
        vm_id: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        tag: r.get(4)?,
        parent_id: r.get(5)?,
        with_memory: r.get(6)?,
        auto: r.get(7)?,
        disks_json: r.get(8)?,
        created_at: r.get::<_, i64>(9)? as u64,
    })
}

const SNAPSHOT_COLS: &str =
    "id, vm_id, name, description, tag, parent_id, with_memory, auto, disks_json, created_at";

/// A pool row.
#[derive(Debug, Clone)]
pub struct PoolRow {
    pub name: String,
    pub path: String,
    pub quota_bytes: Option<u64>,
    pub readonly: bool,
}

/// A job row (see `qemu_ui_core::storage::Job`).
#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    pub progress: Option<f64>,
    pub message: Option<String>,
    pub error: Option<String>,
    pub target_json: String,
    pub log_path: Option<String>,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
}

const JOB_COLS: &str = "id, kind, title, status, progress, message, error, target_json, log_path, \
                        created_at, started_at, finished_at";

fn job_row(r: &rusqlite::Row) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        title: r.get(2)?,
        status: r.get(3)?,
        progress: r.get(4)?,
        message: r.get(5)?,
        error: r.get(6)?,
        target_json: r.get(7)?,
        log_path: r.get(8)?,
        created_at: r.get::<_, i64>(9)? as u64,
        started_at: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        finished_at: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
    })
}

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

impl Store {
    // ----------------------------------------------------------------- pools

    pub fn insert_pool(&self, p: &PoolRow, now: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO pool (name, path, quota_bytes, readonly, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![p.name, p.path, p.quota_bytes.map(|q| q as i64), p.readonly, now as i64],
            )
            .map(|_| ())
        })
    }

    pub fn update_pool(&self, name: &str, quota_bytes: Option<u64>, readonly: bool) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE pool SET quota_bytes = ?2, readonly = ?3 WHERE name = ?1",
                params![name, quota_bytes.map(|q| q as i64), readonly],
            )
            .map(|_| ())
        })
    }

    pub fn delete_pool(&self, name: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM pool WHERE name = ?1", [name])
                .map(|_| ())
        })
    }

    pub fn list_pools(&self) -> Result<Vec<PoolRow>> {
        self.with(|c| {
            let mut st =
                c.prepare("SELECT name, path, quota_bytes, readonly FROM pool ORDER BY name")?;
            st.query_map([], |r| {
                Ok(PoolRow {
                    name: r.get(0)?,
                    path: r.get(1)?,
                    quota_bytes: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    readonly: r.get(3)?,
                })
            })?
            .collect()
        })
    }

    // ------------------------------------------------------------- templates

    pub fn set_template(&self, pool: &str, name: &str, template: bool) -> Result<()> {
        self.with(|c| {
            if template {
                c.execute(
                    "INSERT OR IGNORE INTO image_template (pool, name) VALUES (?1, ?2)",
                    params![pool, name],
                )
            } else {
                c.execute(
                    "DELETE FROM image_template WHERE pool = ?1 AND name = ?2",
                    params![pool, name],
                )
            }
            .map(|_| ())
        })
    }

    pub fn is_template(&self, pool: &str, name: &str) -> Result<bool> {
        self.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM image_template WHERE pool = ?1 AND name = ?2",
                params![pool, name],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
        })
    }

    // ------------------------------------------------------------------ jobs

    pub fn insert_job(&self, j: &JobRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO job (id, kind, title, status, target_json, log_path, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    j.id,
                    j.kind,
                    j.title,
                    j.status,
                    j.target_json,
                    j.log_path,
                    j.created_at as i64
                ],
            )
            .map(|_| ())
        })
    }

    pub fn update_job(&self, j: &JobRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE job SET status = ?2, progress = ?3, message = ?4, error = ?5, \
                 started_at = ?6, finished_at = ?7 WHERE id = ?1",
                params![
                    j.id,
                    j.status,
                    j.progress,
                    j.message,
                    j.error,
                    j.started_at.map(|v| v as i64),
                    j.finished_at.map(|v| v as i64)
                ],
            )
            .map(|_| ())
        })
    }

    pub fn get_job(&self, id: &str) -> Result<Option<JobRow>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {JOB_COLS} FROM job WHERE id = ?1"),
                [id],
                job_row,
            )
            .optional()
        })
    }

    pub fn recent_jobs(&self, limit: u32) -> Result<Vec<JobRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {JOB_COLS} FROM job ORDER BY created_at DESC, rowid DESC LIMIT ?1"
            ))?;
            st.query_map([limit], job_row)?.collect()
        })
    }

    /// Mark jobs that were queued or running when the daemon stopped.
    pub fn interrupt_unfinished_jobs(&self, now: u64) -> Result<usize> {
        self.with(|c| {
            c.execute(
                "UPDATE job SET status = 'interrupted', finished_at = ?1, \
                 error = 'the daemon stopped while this job was running' \
                 WHERE status IN ('queued', 'running')",
                [now as i64],
            )
        })
    }
}

impl Store {
    // ------------------------------------------------------------ snapshots

    pub fn insert_snapshot(&self, s: &SnapshotRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO snapshot ({SNAPSHOT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
                params![
                    s.id,
                    s.vm_id,
                    s.name,
                    s.description,
                    s.tag,
                    s.parent_id,
                    s.with_memory,
                    s.auto,
                    s.disks_json,
                    s.created_at as i64
                ],
            )
            .map(|_| ())
        })
    }

    pub fn list_snapshots(&self, vm_id: &str) -> Result<Vec<SnapshotRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {SNAPSHOT_COLS} FROM snapshot WHERE vm_id = ?1 ORDER BY created_at, rowid"
            ))?;
            st.query_map([vm_id], snapshot_row)?.collect()
        })
    }

    pub fn get_snapshot(&self, vm_id: &str, id: &str) -> Result<Option<SnapshotRow>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {SNAPSHOT_COLS} FROM snapshot WHERE vm_id = ?1 AND id = ?2"),
                params![vm_id, id],
                snapshot_row,
            )
            .optional()
        })
    }

    /// Remove a snapshot and keep the tree whole: its children move up to
    /// its parent, and the head moves to its parent if it pointed here.
    pub fn delete_snapshot(&self, vm_id: &str, id: &str) -> Result<()> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let parent: Option<String> = tx
            .query_row(
                "SELECT parent_id FROM snapshot WHERE vm_id = ?1 AND id = ?2",
                params![vm_id, id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        tx.execute(
            "UPDATE snapshot SET parent_id = ?3 WHERE vm_id = ?1 AND parent_id = ?2",
            params![vm_id, id, parent],
        )?;
        tx.execute(
            "UPDATE snapshot_head SET snapshot_id = ?3 WHERE vm_id = ?1 AND snapshot_id = ?2",
            params![vm_id, id, parent],
        )?;
        tx.execute(
            "DELETE FROM snapshot WHERE vm_id = ?1 AND id = ?2",
            params![vm_id, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn snapshot_head(&self, vm_id: &str) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row(
                "SELECT snapshot_id FROM snapshot_head WHERE vm_id = ?1",
                [vm_id],
                |r| r.get(0),
            )
            .optional()
            .map(Option::flatten)
        })
    }

    pub fn set_snapshot_head(&self, vm_id: &str, snapshot_id: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO snapshot_head (vm_id, snapshot_id) VALUES (?1, ?2) \
                 ON CONFLICT(vm_id) DO UPDATE SET snapshot_id = ?2",
                params![vm_id, snapshot_id],
            )
            .map(|_| ())
        })
    }
}

impl Store {
    pub fn insert_upload(&self, u: &UploadRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO upload (id, pool, name, size, sha256, client_key, job_id, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    u.id,
                    u.pool,
                    u.name,
                    u.size as i64,
                    u.sha256,
                    u.client_key,
                    u.job_id,
                    u.created_at as i64
                ],
            )
            .map(|_| ())
        })
    }

    pub fn set_upload_job(&self, id: &str, job_id: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE upload SET job_id = ?2 WHERE id = ?1",
                params![id, job_id],
            )
            .map(|_| ())
        })
    }

    pub fn get_upload(&self, id: &str) -> Result<Option<UploadRow>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {UPLOAD_COLS} FROM upload WHERE id = ?1"),
                [id],
                upload_row,
            )
            .optional()
        })
    }

    pub fn list_uploads(&self) -> Result<Vec<UploadRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {UPLOAD_COLS} FROM upload ORDER BY created_at, rowid"
            ))?;
            st.query_map([], upload_row)?.collect()
        })
    }

    pub fn delete_upload(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM upload WHERE id = ?1", [id])
                .map(|_| ())
        })
    }
}

impl Store {
    // ----------------------------------------------------------- share roots

    pub fn insert_share_root(&self, name: &str, path: &str, now: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO share_root (name, path, created_at) VALUES (?1, ?2, ?3)",
                params![name, path, now as i64],
            )
            .map(|_| ())
        })
    }

    /// (name, path), by name.
    pub fn list_share_roots(&self) -> Result<Vec<(String, String)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT name, path FROM share_root ORDER BY name")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
        })
    }

    pub fn delete_share_root(&self, name: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM share_root WHERE name = ?1", [name])
                .map(|_| ())
        })
    }

    // -------------------------------------------------------------- sidecars

    /// Record a helper process (e.g. `virtiofsd`) belonging to a run.
    pub fn insert_sidecar(&self, run_id: i64, kind: &str, pid: u32, proc_start: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO run_sidecar (run_id, kind, pid, proc_start) VALUES (?1, ?2, ?3, ?4)",
                params![run_id, kind, pid as i64, proc_start as i64],
            )
            .map(|_| ())
        })
    }

    /// (kind, pid, process start time) of a run's helpers.
    pub fn sidecars(&self, run_id: i64) -> Result<Vec<(String, u32, u64)>> {
        self.with(|c| {
            let mut st =
                c.prepare("SELECT kind, pid, proc_start FROM run_sidecar WHERE run_id = ?1")?;
            st.query_map([run_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, i64>(1)? as u32,
                    r.get::<_, i64>(2)? as u64,
                ))
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
    fn share_roots_and_sidecars() {
        let s = Store::memory().unwrap();
        s.insert_share_root("src", "/srv/src", 1).unwrap();
        assert!(
            s.insert_share_root("dup", "/srv/src", 1).is_err(),
            "unique path"
        );
        assert_eq!(
            s.list_share_roots().unwrap(),
            [("src".to_string(), "/srv/src".to_string())]
        );
        s.delete_share_root("src").unwrap();
        assert!(s.list_share_roots().unwrap().is_empty());

        s.insert_vm("v1", "alpha", "{}", 1).unwrap();
        let run = s
            .insert_run(&NewRun {
                vm_id: "v1",
                args_json: "[]",
                spec_json: "{}",
                accel: "tcg",
                pid: 1,
                proc_start: 2,
                run_dir: "/r",
                vnc_json: None,
                log_path: "/l",
                started_at: 3,
            })
            .unwrap();
        s.insert_sidecar(run, "virtiofsd", 42, 7).unwrap();
        assert_eq!(s.sidecars(run).unwrap(), [("virtiofsd".to_string(), 42, 7)]);
    }

    #[test]
    fn pools_and_jobs() {
        let s = Store::memory().unwrap();
        s.insert_pool(
            &PoolRow {
                name: "big".into(),
                path: "/big".into(),
                quota_bytes: Some(10),
                readonly: false,
            },
            1,
        )
        .unwrap();
        assert!(
            s.insert_pool(
                &PoolRow {
                    name: "dup".into(),
                    path: "/big".into(),
                    quota_bytes: None,
                    readonly: false
                },
                1
            )
            .is_err(),
            "unique path"
        );
        s.update_pool("big", None, true).unwrap();
        let p = &s.list_pools().unwrap()[0];
        assert_eq!((p.quota_bytes, p.readonly), (None, true));

        let mut j = JobRow {
            id: "j1".into(),
            kind: "image.create".into(),
            title: "t".into(),
            status: "queued".into(),
            progress: None,
            message: None,
            error: None,
            target_json: "{}".into(),
            log_path: None,
            created_at: 5,
            started_at: None,
            finished_at: None,
        };
        s.insert_job(&j).unwrap();
        j.status = "running".into();
        j.progress = Some(0.5);
        s.update_job(&j).unwrap();
        assert_eq!(s.get_job("j1").unwrap().unwrap().progress, Some(0.5));
        assert_eq!(s.interrupt_unfinished_jobs(9).unwrap(), 1);
        assert_eq!(s.recent_jobs(10).unwrap()[0].status, "interrupted");
        s.delete_pool("big").unwrap();
        assert!(s.list_pools().unwrap().is_empty());

        assert!(!s.is_template("p", "base.qcow2").unwrap());
        s.set_template("p", "base.qcow2", true).unwrap();
        s.set_template("p", "base.qcow2", true).unwrap();
        assert!(s.is_template("p", "base.qcow2").unwrap());
        s.set_template("p", "base.qcow2", false).unwrap();
        assert!(!s.is_template("p", "base.qcow2").unwrap());
    }

    #[test]
    fn snapshot_tree_reparents_on_delete() {
        let s = Store::memory().unwrap();
        s.insert_vm("v", "vm", "{}", 1).unwrap();
        let snap = |id: &str, parent: Option<&str>| SnapshotRow {
            id: id.into(),
            vm_id: "v".into(),
            name: id.into(),
            description: String::new(),
            tag: format!("qui-{id}"),
            parent_id: parent.map(String::from),
            with_memory: false,
            auto: false,
            disks_json: "[]".into(),
            created_at: 1,
        };
        // a -> b -> {c, d}
        s.insert_snapshot(&snap("a", None)).unwrap();
        s.insert_snapshot(&snap("b", Some("a"))).unwrap();
        s.insert_snapshot(&snap("c", Some("b"))).unwrap();
        s.insert_snapshot(&snap("d", Some("b"))).unwrap();
        s.set_snapshot_head("v", Some("b")).unwrap();
        s.delete_snapshot("v", "b").unwrap();
        let rows = s.list_snapshots("v").unwrap();
        assert_eq!(rows.len(), 3);
        for id in ["c", "d"] {
            let r = rows.iter().find(|r| r.id == id).unwrap();
            assert_eq!(r.parent_id.as_deref(), Some("a"), "{id} re-parented");
        }
        assert_eq!(
            s.snapshot_head("v").unwrap().as_deref(),
            Some("a"),
            "head moved up"
        );
        s.delete_vm("v").unwrap();
        assert!(s.list_snapshots("v").unwrap().is_empty(), "cascade");
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
