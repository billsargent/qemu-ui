//! Win32 implementation. Every `unsafe` block calls one documented API with
//! arguments built right here; see the SAFETY notes.

use std::io;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, CreateWellKnownSid, DISABLE_MAX_PRIVILEGE, SECURITY_MAX_SID_SIZE,
    SID_AND_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
    WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, INFINITE,
    OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    STARTUPINFOW, WaitForSingleObject,
};

use crate::JobLimits;

/// An owned Win32 handle, closed on drop.
struct Handle(HANDLE);

impl Handle {
    fn check(h: HANDLE) -> io::Result<Handle> {
        if h.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: we own this handle (it came from a successful Create/Open
        // call) and nothing else closes it.
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ok(r: i32) -> io::Result<()> {
    if r == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Create (or open) the named job.
fn create_job(name: &str) -> io::Result<Handle> {
    let n = wide(name);
    // SAFETY: `n` is a NUL-terminated UTF-16 string that outlives the call;
    // a null security descriptor means the default one.
    Handle::check(unsafe { CreateJobObjectW(null(), n.as_ptr()) })
}

fn apply_limits(job: &Handle, limits: JobLimits) -> io::Result<()> {
    // SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION is valid (no
    // limits); we then set only the memory fields.
    let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    if let Some(m) = limits.memory_bytes {
        ext.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY;
        ext.JobMemoryLimit = m as usize;
    }
    // Never KILL_ON_JOB_CLOSE: the VM must survive the daemon.
    // SAFETY: `ext` is a properly sized, initialised struct of the type the
    // information class expects, and the job handle is open with
    // JOB_OBJECT_SET_ATTRIBUTES (or full access from CreateJobObjectW).
    ok(unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const ext).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })?;

    // SAFETY: all-zero means "CPU rate control off".
    let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { zeroed() };
    if let Some(rate) = limits.cpu_rate {
        cpu.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        cpu.Anonymous.CpuRate = rate.clamp(1, 10_000);
    }
    // SAFETY: as above, for the CPU rate information class.
    ok(unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectCpuRateControlInformation,
            (&raw const cpu).cast(),
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
        )
    })
}

/// A job object holding one VM's QEMU. Dropping it closes the daemon's
/// handle; the job (never kill-on-close) lives on while QEMU runs.
pub struct Job(Handle);

// SAFETY: a job handle is a kernel object reference with no thread affinity;
// Win32 job APIs may be called on it from any thread.
unsafe impl Send for Job {}
// SAFETY: as above; the APIs we call on a shared reference are thread-safe.
unsafe impl Sync for Job {}

impl Job {
    /// Create a job named `name` with `limits` and put process `pid` in it.
    /// A process can be in several (nested) jobs; the strictest limit wins.
    pub fn assign(name: &str, pid: u32, limits: JobLimits) -> io::Result<Job> {
        let job = create_job(name)?;
        apply_limits(&job, limits)?;
        // SAFETY: plain call with constant access rights and a pid.
        let proc =
            Handle::check(unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) })?;
        // SAFETY: both handles are open and owned by us.
        ok(unsafe { AssignProcessToJobObject(job.0, proc.0) })?;
        Ok(Job(job))
    }

    /// Change the limits while the VM runs.
    pub fn set_limits(&self, limits: JobLimits) -> io::Result<()> {
        apply_limits(&self.0, limits)
    }

    /// Read the limits back (tests and diagnostics).
    pub fn limits(&self) -> io::Result<JobLimits> {
        query(&self.0)
    }
}

fn query(job: &Handle) -> io::Result<JobLimits> {
    // SAFETY: zeroed out-structs, filled by the API; sizes match the types.
    let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    // SAFETY: as above.
    let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { zeroed() };
    // SAFETY: valid out pointers of the right size; a null return length is allowed.
    ok(unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw mut ext).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    })?;
    // SAFETY: as above.
    ok(unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectCpuRateControlInformation,
            (&raw mut cpu).cast(),
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            null_mut(),
        )
    })?;
    Ok(JobLimits {
        memory_bytes: (ext.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0)
            .then_some(ext.JobMemoryLimit as u64),
        cpu_rate: if cpu.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE != 0 {
            // SAFETY: CpuRate is the active union member when ENABLE is set
            // without WEIGHT_BASED/MIN_MAX_RATE (the only way we set it).
            Some(unsafe { cpu.Anonymous.CpuRate })
        } else {
            None
        },
    })
}

/// A restricted copy of this process's token: every privilege but
/// SeChangeNotify removed, and the Administrators group deny-only.
fn restricted_token() -> io::Result<Handle> {
    let mut own: HANDLE = null_mut();
    // SAFETY: GetCurrentProcess is a pseudo-handle; `own` receives a new
    // handle we wrap and close.
    ok(unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_QUERY,
            &mut own,
        )
    })?;
    let own = Handle(own);

    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut sid_len = sid.len() as u32;
    // SAFETY: `sid` is a buffer of SECURITY_MAX_SID_SIZE bytes, which the
    // API documents as always large enough; `sid_len` holds its size.
    ok(unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            null_mut(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
        )
    })?;
    let deny = SID_AND_ATTRIBUTES {
        Sid: sid.as_mut_ptr().cast(),
        Attributes: 0,
    };
    let mut restricted: HANDLE = null_mut();
    // SAFETY: `own` is open with TOKEN_DUPLICATE; `deny` points into `sid`,
    // which lives until after the call; no restricting SIDs or privileges
    // arrays are passed (counts 0, null pointers).
    ok(unsafe {
        CreateRestrictedToken(
            own.0,
            DISABLE_MAX_PRIVILEGE,
            1,
            &deny,
            0,
            null(),
            0,
            null(),
            &mut restricted,
        )
    })?;
    Ok(Handle(restricted))
}

/// Run `command_line` with a restricted token (no admin rights, no
/// privileges), wait for it, and return its exit code. Used to check that
/// restricted spawning works on this host; QEMU itself is spawned by the
/// daemon's process code.
pub fn run_restricted(command_line: &str) -> io::Result<u32> {
    let token = restricted_token()?;
    let mut cmd = wide(command_line);
    // SAFETY: zeroed STARTUPINFOW with its size set is the documented
    // minimal initialisation.
    let mut si: STARTUPINFOW = unsafe { zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    // SAFETY: out-struct filled by the API.
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
    // SAFETY: the token is a restricted copy of our own primary token (so no
    // SeAssignPrimaryToken privilege is needed); `cmd` is a writable
    // NUL-terminated buffer as the API requires; no handles are inherited.
    ok(unsafe {
        CreateProcessAsUserW(
            token.0,
            null(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW,
            null(),
            null(),
            &si,
            &mut pi,
        )
    })?;
    let process = Handle(pi.hProcess);
    let _thread = Handle(pi.hThread);
    // SAFETY: waiting on and querying a process handle we own.
    if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error());
    }
    let mut code = 0u32;
    // SAFETY: as above.
    ok(unsafe { GetExitCodeProcess(process.0, &mut code) })?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_limits_apply_to_a_process_and_can_change() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let name = format!("qemu-ui-test-{}", std::process::id());
        let limits = JobLimits {
            memory_bytes: Some(512 << 20),
            cpu_rate: Some(2500),
        };
        let mut check = || -> io::Result<()> {
            let job = Job::assign(&name, child.id(), limits)?;
            assert_eq!(job.limits()?, limits);
            let lower = JobLimits {
                memory_bytes: Some(256 << 20),
                cpu_rate: None,
            };
            job.set_limits(lower)?;
            assert_eq!(job.limits()?, lower);
            drop(job);
            // Not kill-on-close: the process outlives our handle.
            assert!(
                child.try_wait()?.is_none(),
                "the process must survive the job handle"
            );
            Ok(())
        };
        let out = check();
        let _ = child.kill();
        let _ = child.wait();
        out.unwrap();
    }

    /// STATUS_DLL_INIT_FAILED: the child could not open the window station
    /// or desktop. Happens when the parent runs as a service (CI runners):
    /// that desktop's ACL grants Administrators, which the restricted token
    /// makes deny-only.
    const STATUS_DLL_INIT_FAILED: u32 = 0xC000_0142;

    #[test]
    fn restricted_process_runs() {
        let code = run_restricted("cmd /c exit 7").unwrap();
        if code == STATUS_DLL_INIT_FAILED {
            eprintln!(
                "skipping: no desktop access for restricted tokens in this session                  (service/CI); spawning QEMU restricted will need its own desktop"
            );
            return;
        }
        assert_eq!(code, 7);
        // `net session` needs administrator rights, which the restricted
        // token drops even when this test runs elevated.
        assert_ne!(run_restricted("cmd /c net session >nul 2>&1").unwrap(), 0);
    }
}
