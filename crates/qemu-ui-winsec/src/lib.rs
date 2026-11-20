//! Windows isolation primitives for qemu-ui (M6-06): job objects with
//! memory/CPU limits, and restricted tokens.
//!
//! This is the only crate in the workspace that uses `unsafe`: each block
//! wraps one Win32 call, owns every handle it opens (closed by [`Handle`]'s
//! `Drop`), and passes only stack structures it initialised itself. On other
//! platforms every function returns `ErrorKind::Unsupported`.
//!
//! Job objects are never created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:
//! VMs must outlive the daemon (plan.md §3.2). A job lives as long as a
//! process is in it, but its *name* goes away with the last handle, so the
//! daemon keeps a [`Job`] per running VM to change limits live.

/// Limits for a job: `cpu_rate` is in hundredths of a percent of the whole
/// machine (1–10000), as `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` wants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobLimits {
    pub memory_bytes: Option<u64>,
    pub cpu_rate: Option<u32>,
}

/// The job name for a VM.
pub fn job_name(vm_id: &str) -> String {
    format!("qemu-ui-vm-{vm_id}")
}

#[cfg(windows)]
mod imp;
#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
mod imp_other {
    use super::JobLimits;
    use std::io;

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "Windows only"))
    }

    /// Job objects exist only on Windows.
    pub struct Job(());

    impl Job {
        pub fn assign(_name: &str, _pid: u32, _limits: JobLimits) -> io::Result<Job> {
            unsupported()
        }
        pub fn set_limits(&self, _limits: JobLimits) -> io::Result<()> {
            unsupported()
        }
        pub fn limits(&self) -> io::Result<JobLimits> {
            unsupported()
        }
    }

    pub fn run_restricted(_command_line: &str) -> io::Result<u32> {
        unsupported()
    }
}
#[cfg(not(windows))]
pub use imp_other::*;
