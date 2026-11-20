//! Running the daemon as a Windows service (M6-09).
//!
//! `qemu-uid --service …` is what the service control manager starts (the
//! CLI's `qemu-ui service install` registers it). The SCM calls back on its
//! own thread; we report Running, serve until a Stop/Shutdown control
//! arrives, then report Stopped.
//!
//! VMs outlive the service: QEMU is spawned detached and breaks away from any
//! job object (`procs::spawn_detached`), and the SCM puts services in no
//! kill-on-close job, so stopping or restarting the service leaves them
//! running; the next start reattaches (plan.md §3.2).

use std::ffi::OsString;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

/// The registered service name.
pub const SERVICE_NAME: &str = "qemu-ui";

/// What the service runs once the SCM has started it: the daemon, until
/// the given future resolves.
type Body = Box<dyn FnOnce(tokio::sync::oneshot::Receiver<()>) -> Result<()> + Send>;

static BODY: OnceLock<std::sync::Mutex<Option<Body>>> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Hand control to the SCM; returns when the service has stopped.
pub fn run(body: Body) -> Result<()> {
    BODY.get_or_init(|| std::sync::Mutex::new(Some(body)));
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("starting the service dispatcher (is this running under the service manager?)")
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = serve() {
        tracing::error!("service failed: {e:#}");
    }
}

fn serve() -> Result<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stop_tx = std::sync::Mutex::new(Some(stop_tx));
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(tx) = stop_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let report = |state, accept, exit| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: exit,
            checkpoint: 0,
            wait_hint: Duration::from_secs(20),
            process_id: None,
        })
    };
    report(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ServiceExitCode::Win32(0),
    )?;
    let body = BODY
        .get()
        .and_then(|b| b.lock().unwrap().take())
        .context("service body missing")?;
    let result = body(stop_rx);
    let code = if result.is_ok() { 0 } else { 1 };
    report(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(code),
    )?;
    result
}
