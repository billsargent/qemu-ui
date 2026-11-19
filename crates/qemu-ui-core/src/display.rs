//! Display engines as the API describes them (plan.md §8.1, M5-01/M5-06).
//!
//! The daemon implements engines (`vnc:novnc` now; `spice:native`, guacd
//! later); these are the shapes the browser sees. Console controls render
//! from [`EngineCaps`]: a control whose capability is absent is not shown,
//! rather than shown broken.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// What an engine can do for a VM. Every field defaults to `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EngineCaps {
    /// Copy/paste between browser and guest (needs a guest agent).
    pub clipboard: bool,
    pub audio: bool,
    /// USB redirection from the viewer's machine.
    pub usb: bool,
    /// More than one guest monitor.
    pub multihead: bool,
    /// Sessions can be recorded (M5-03).
    pub record: bool,
    /// File transfer into the guest.
    pub files: bool,
    /// Send-key palette (Ctrl+Alt+Del, Ctrl+Alt+Fn).
    pub keys: bool,
    /// The guest display follows the viewer's size.
    pub resize: bool,
    /// Scale to fit / 1:1.
    pub scale: bool,
    pub fullscreen: bool,
    /// A PNG of the screen can be taken.
    pub screenshot: bool,
    /// Several viewers at once, with watch-only viewers.
    pub view_only: bool,
    /// One viewer controls input; others can take control.
    pub take_control: bool,
}

/// One engine for one VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EngineInfo {
    /// e.g. `vnc:novnc`.
    pub id: String,
    pub label: String,
    /// Usable now (VM running, engine installed, display configured).
    pub available: bool,
    /// Why not, when unavailable.
    pub reason: Option<String>,
    pub caps: EngineCaps,
}

/// Someone watching a VM's display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConsoleViewer {
    #[ts(type = "number")]
    pub id: u64,
    /// "Viewer 3": no names or addresses.
    pub label: String,
    /// Joined with a watch-only ticket: can never take control.
    pub read_only: bool,
    /// Holds input control.
    pub controlling: bool,
    /// Milliseconds since epoch.
    #[ts(type = "number")]
    pub since_ms: u64,
    /// Input messages the proxy dropped because this viewer lacked control.
    #[ts(type = "number")]
    pub input_dropped: u64,
}

/// Who is watching a VM's graphical console.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConsolePresence {
    pub viewers: Vec<ConsoleViewer>,
    /// Most viewers allowed at once.
    pub limit: u32,
    /// Seconds without input after which a viewer is disconnected.
    #[ts(type = "number")]
    pub idle_timeout_secs: u64,
}

/// `GET /vms/{id}/display`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DisplayInfo {
    /// The VM's engines, preferred first.
    pub engines: Vec<EngineInfo>,
    pub presence: ConsolePresence,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_default_to_absent() {
        let c = EngineCaps::default();
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.as_object().unwrap().values().all(|x| x == false));
    }
}
