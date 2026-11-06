//! Generic job system (plan.md §11, M2-03).
//!
//! Slow operations (image conversion, uploads, downloads, snapshot merges)
//! run as jobs: persisted in SQLite, at most `MAX_RUNNING` at a time, with
//! progress, a status line, a log file and cancellation. Every change is
//! broadcast on the event stream as `{kind: "job", vm_id: "", data: Job}`.
//! Jobs that were running when the daemon stopped are marked `interrupted`.
//!
//! Adding a job kind:
//!
//! ```ignore
//! let job = jobs.spawn("image.resize", "Resize x.qcow2 to 40 GiB", json!({"pool": p, "name": n}), true,
//!     move |ctx| async move {
//!         ctx.run_qemu_img(&qemu_img, &["resize", ...]).await?;
//!         Ok("Resized".into())   // final message
//!     });
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use qemu_ui_core::storage::{Job, JobStatus, parse_qemu_img_progress};
use qemu_ui_core::vm::VmEvent;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::sync::{Semaphore, broadcast, watch};

use crate::store::{JobRow, Store};

/// Jobs allowed to run concurrently; the rest queue.
pub const MAX_RUNNING: usize = 2;
/// Minimum interval between persisted progress updates.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub struct Jobs {
    store: Store,
    events: broadcast::Sender<VmEvent>,
    slots: Arc<Semaphore>,
    cancels: Mutex<HashMap<String, watch::Sender<bool>>>,
    cancellable: Mutex<HashMap<String, bool>>,
    logs_dir: PathBuf,
}

/// Handed to a job body: report progress, check cancellation, log.
pub struct JobCtx {
    /// Job id, for log lines and file names.
    #[allow(dead_code)] // part of the job-body API; not every kind needs it
    pub id: String,
    jobs: Arc<Jobs>,
    row: Mutex<JobRow>,
    last_persist: Mutex<Instant>,
    cancel: watch::Receiver<bool>,
    log: Option<PathBuf>,
}

impl Jobs {
    pub fn new(
        store: Store,
        events: broadcast::Sender<VmEvent>,
        logs_dir: PathBuf,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&logs_dir)?;
        let interrupted = store.interrupt_unfinished_jobs(now())?;
        if interrupted > 0 {
            tracing::warn!("{interrupted} job(s) were interrupted by the previous daemon shutdown");
        }
        Ok(Arc::new(Jobs {
            store,
            events,
            slots: Arc::new(Semaphore::new(MAX_RUNNING)),
            cancels: Mutex::default(),
            cancellable: Mutex::default(),
            logs_dir,
        }))
    }

    fn to_job(&self, r: &JobRow) -> Job {
        let status = JobStatus::parse(&r.status);
        Job {
            id: r.id.clone(),
            kind: r.kind.clone(),
            title: r.title.clone(),
            status,
            progress: r.progress,
            message: r.message.clone(),
            error: r.error.clone(),
            target: serde_json::from_str(&r.target_json).unwrap_or(Value::Null),
            created_at: r.created_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
            cancellable: !status.is_finished()
                && self
                    .cancellable
                    .lock()
                    .unwrap()
                    .get(&r.id)
                    .copied()
                    .unwrap_or(false),
        }
    }

    fn publish(&self, r: &JobRow) {
        let job = self.to_job(r);
        let _ = self.events.send(VmEvent {
            seq: 0,
            vm_id: String::new(),
            at_us: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_micros() as u64),
            kind: "job".into(),
            data: serde_json::to_value(job).unwrap_or(Value::Null),
        });
    }

    fn save(&self, r: &JobRow) {
        if let Err(e) = self.store.update_job(r) {
            tracing::error!("saving job {}: {e:#}", r.id);
        }
        self.publish(r);
    }

    /// Queue a job. `body` returns the final status message.
    pub fn spawn<F, Fut>(
        self: &Arc<Self>,
        kind: &str,
        title: &str,
        target: Value,
        cancellable: bool,
        body: F,
    ) -> Job
    where
        F: FnOnce(Arc<JobCtx>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        self.spawn_with(kind, title, target, cancellable, true, body)
    }

    /// Start a job immediately, without taking one of the `MAX_RUNNING`
    /// slots. For jobs that mostly wait on something external (a browser
    /// upload that may pause for hours) and must not block other work.
    pub fn spawn_unthrottled<F, Fut>(
        self: &Arc<Self>,
        kind: &str,
        title: &str,
        target: Value,
        cancellable: bool,
        body: F,
    ) -> Job
    where
        F: FnOnce(Arc<JobCtx>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        self.spawn_with(kind, title, target, cancellable, false, body)
    }

    fn spawn_with<F, Fut>(
        self: &Arc<Self>,
        kind: &str,
        title: &str,
        target: Value,
        cancellable: bool,
        throttled: bool,
        body: F,
    ) -> Job
    where
        F: FnOnce(Arc<JobCtx>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        let id = format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
        let log = self.logs_dir.join(format!("{id}.log"));
        let row = JobRow {
            id: id.clone(),
            kind: kind.into(),
            title: title.into(),
            status: JobStatus::Queued.as_str().into(),
            progress: None,
            message: Some("Waiting for a free slot".into()),
            error: None,
            target_json: target.to_string(),
            log_path: Some(log.display().to_string()),
            created_at: now(),
            started_at: None,
            finished_at: None,
        };
        if let Err(e) = self.store.insert_job(&row) {
            tracing::error!("recording job: {e:#}");
        }
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.cancels.lock().unwrap().insert(id.clone(), cancel_tx);
        self.cancellable
            .lock()
            .unwrap()
            .insert(id.clone(), cancellable);
        let job = self.to_job(&row);
        self.publish(&row);

        let jobs = self.clone();
        tokio::spawn(async move {
            let ctx = Arc::new(JobCtx {
                id: id.clone(),
                jobs: jobs.clone(),
                row: Mutex::new(row),
                last_persist: Mutex::new(Instant::now()),
                cancel: cancel_rx,
                log: Some(log),
            });
            // Wait for a slot, unless cancelled while queued.
            let permit = if throttled {
                tokio::select! {
                    p = jobs.slots.clone().acquire_owned() => p.ok().map(Some),
                    _ = ctx.cancelled() => None,
                }
            } else {
                Some(None)
            };
            let outcome: Result<String> = if permit.is_none() {
                Err(anyhow!("cancelled"))
            } else {
                {
                    let mut r = ctx.row.lock().unwrap();
                    r.status = JobStatus::Running.as_str().into();
                    r.started_at = Some(now());
                    r.message = None;
                    jobs.save(&r);
                }
                body(ctx.clone()).await
            };
            drop(permit);
            let mut r = ctx.row.lock().unwrap().clone();
            r.finished_at = Some(now());
            match outcome {
                Ok(msg) => {
                    r.status = JobStatus::Succeeded.as_str().into();
                    r.progress = Some(1.0);
                    r.message = Some(msg);
                }
                Err(_) if ctx.is_cancelled() => {
                    r.status = JobStatus::Cancelled.as_str().into();
                    r.message = Some("Cancelled".into());
                }
                Err(e) => {
                    r.status = JobStatus::Failed.as_str().into();
                    r.error = Some(format!("{e:#}"));
                    ctx.log_line(&format!("error: {e:#}"));
                }
            }
            jobs.cancels.lock().unwrap().remove(&r.id);
            jobs.save(&r);
        });
        job
    }

    pub fn list(&self, limit: u32) -> Result<Vec<Job>> {
        Ok(self
            .store
            .recent_jobs(limit)?
            .iter()
            .map(|r| self.to_job(r))
            .collect())
    }

    pub fn get(&self, id: &str) -> Result<Option<Job>> {
        Ok(self.store.get_job(id)?.map(|r| self.to_job(&r)))
    }

    pub fn log_path(&self, id: &str) -> Option<PathBuf> {
        self.store
            .get_job(id)
            .ok()
            .flatten()?
            .log_path
            .map(PathBuf::from)
    }

    /// Request cancellation. Fails if the job is unknown, finished, or not
    /// cancellable.
    pub fn cancel(&self, id: &str) -> Result<()> {
        if !self
            .cancellable
            .lock()
            .unwrap()
            .get(id)
            .copied()
            .unwrap_or(false)
        {
            bail!("this job cannot be cancelled");
        }
        let cancels = self.cancels.lock().unwrap();
        let tx = cancels
            .get(id)
            .ok_or_else(|| anyhow!("the job is not running"))?;
        let _ = tx.send(true);
        Ok(())
    }

    /// Wait until a job finishes (tests, and callers that chain jobs).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<Job> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(j) = self.get(id)?
                && j.status.is_finished()
            {
                return Ok(j);
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for job {id}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl JobCtx {
    /// Report progress (0.0–1.0, or `None` = indeterminate) and/or a status
    /// line. Persisted at most every 250 ms; always broadcast on completion.
    pub fn progress(&self, progress: Option<f64>, message: Option<&str>) {
        let mut r = self.row.lock().unwrap();
        if progress.is_some() {
            r.progress = progress.map(|p| p.clamp(0.0, 1.0));
        }
        if let Some(m) = message {
            r.message = Some(m.to_string());
        }
        let mut last = self.last_persist.lock().unwrap();
        if last.elapsed() >= PROGRESS_INTERVAL {
            *last = Instant::now();
            self.jobs.save(&r);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Resolves when cancellation is requested.
    pub async fn cancelled(&self) {
        let mut rx = self.cancel.clone();
        let _ = rx.wait_for(|c| *c).await;
    }

    /// Fail with "cancelled" if cancellation was requested.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(anyhow!("cancelled"))
        } else {
            Ok(())
        }
    }

    pub fn log_line(&self, line: &str) {
        use std::io::Write;
        if let Some(p) = &self.log
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Run `qemu-img` with `-p` progress reporting, killing it on cancel.
    /// `args` must not include `-p`; it is added after the subcommand.
    pub async fn run_qemu_img(&self, qemu_img: &Path, args: &[&str]) -> Result<()> {
        let (sub, rest) = args
            .split_first()
            .ok_or_else(|| anyhow!("no qemu-img subcommand"))?;
        self.log_line(&format!("$ qemu-img {}", args.join(" ")));
        let mut child = crate::probe::command(qemu_img)
            .arg(sub)
            .arg("-p")
            .args(rest)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let err_task = tokio::spawn(async move {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s).await;
            s
        });
        let mut buf = [0u8; 512];
        loop {
            tokio::select! {
                n = stdout.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Some(p) = parse_qemu_img_progress(&String::from_utf8_lossy(&buf[..n])) {
                            self.progress(Some(p), None);
                        }
                    }
                },
                _ = self.cancelled() => {
                    let _ = child.kill().await;
                    bail!("cancelled");
                }
            }
        }
        let status = child.wait().await?;
        let err = err_task.await.unwrap_or_default();
        if !err.trim().is_empty() {
            self.log_line(err.trim());
        }
        if !status.success() {
            bail!("qemu-img {sub} failed: {}", err.trim());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jobs() -> Arc<Jobs> {
        let dir = std::env::temp_dir().join(format!(
            "qui-jobs-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let (tx, _) = broadcast::channel(64);
        Jobs::new(Store::memory().unwrap(), tx, dir).unwrap()
    }

    #[tokio::test]
    async fn success_failure_and_progress() {
        let j = jobs();
        let mut events = j.events.subscribe();
        let ok = j.spawn("test.ok", "ok", Value::Null, false, |ctx| async move {
            ctx.progress(Some(0.5), Some("half"));
            Ok("done".into())
        });
        let fin = j.wait(&ok.id, Duration::from_secs(5)).await.unwrap();
        assert_eq!(
            (fin.status, fin.progress, fin.message.as_deref()),
            (JobStatus::Succeeded, Some(1.0), Some("done"))
        );
        assert!(events.recv().await.is_ok(), "job events are broadcast");

        let bad = j.spawn("test.fail", "fail", Value::Null, false, |_| async move {
            Err(anyhow!("boom"))
        });
        let fin = j.wait(&bad.id, Duration::from_secs(5)).await.unwrap();
        assert_eq!(fin.status, JobStatus::Failed);
        assert!(fin.error.unwrap().contains("boom"));
        assert!(
            std::fs::read_to_string(j.log_path(&bad.id).unwrap())
                .unwrap()
                .contains("boom")
        );
    }

    #[tokio::test]
    async fn cancellation_and_concurrency_limit() {
        let j = jobs();
        // Fill both slots with jobs that wait to be cancelled.
        let blockers: Vec<_> = (0..MAX_RUNNING)
            .map(|i| {
                j.spawn(
                    "test.block",
                    &format!("b{i}"),
                    Value::Null,
                    true,
                    |ctx| async move {
                        ctx.cancelled().await;
                        ctx.check_cancelled()?;
                        Ok(String::new())
                    },
                )
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let queued = j.spawn("test.ok", "queued", Value::Null, true, |_| async move {
            Ok("ran".into())
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            j.get(&queued.id).unwrap().unwrap().status,
            JobStatus::Queued,
            "slots are full"
        );

        // Not cancellable → refused.
        let fixed = j.spawn("test.fixed", "fixed", Value::Null, false, |_| async move {
            Ok(String::new())
        });
        assert!(j.cancel(&fixed.id).is_err());

        for b in &blockers {
            j.cancel(&b.id).unwrap();
        }
        for b in &blockers {
            assert_eq!(
                j.wait(&b.id, Duration::from_secs(5)).await.unwrap().status,
                JobStatus::Cancelled
            );
        }
        assert_eq!(
            j.wait(&queued.id, Duration::from_secs(5))
                .await
                .unwrap()
                .status,
            JobStatus::Succeeded
        );
    }
}
