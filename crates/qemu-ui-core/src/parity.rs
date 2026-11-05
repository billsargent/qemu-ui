//! Platform parity ledger (plan.md §14): static data refined by probe results.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::caps::BinaryCaps;

const LEDGER: &str = include_str!("../../../data/parity.toml");

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ParityStatus {
    Native,
    Degraded,
    NeedsSetup,
    Impossible,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct ParityCell {
    pub status: ParityStatus,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct ParityRow {
    pub id: String,
    pub feature: String,
    pub linux: ParityCell,
    pub windows: ParityCell,
    /// This host's cell after refinement by the probe. Absent on hosts the
    /// ledger does not model (e.g. macOS).
    #[serde(default)]
    pub current: Option<ParityCell>,
    /// True when `current` was changed by probe results.
    #[serde(default)]
    pub probed: bool,
}

#[derive(Deserialize)]
struct Ledger {
    row: Vec<ParityRow>,
}

/// The static ledger shipped in `data/parity.toml`.
pub fn ledger() -> Vec<ParityRow> {
    toml::from_str::<Ledger>(LEDGER)
        .expect("data/parity.toml is validated by tests")
        .row
}

/// Fill `current` for `os` and override it with what the probe observed.
pub fn refine(mut rows: Vec<ParityRow>, os: &str, binaries: &[BinaryCaps]) -> Vec<ParityRow> {
    for row in &mut rows {
        row.current = match os {
            "linux" => Some(row.linux.clone()),
            "windows" => Some(row.windows.clone()),
            _ => None,
        };
        if binaries.is_empty() {
            continue;
        }
        let observed = match row.id.as_str() {
            "tpm" => Some(if binaries.iter().any(|b| b.backends.tpmdev.is_some()) {
                ParityCell {
                    status: ParityStatus::Native,
                    note: "TPM backends present in the installed QEMU; swtpm is still required."
                        .into(),
                }
            } else {
                ParityCell {
                    status: ParityStatus::Impossible,
                    note: "The installed QEMU binaries were built without TPM support (-tpmdev)."
                        .into(),
                }
            }),
            "hw-accel" => {
                let native: Vec<_> = binaries.iter().filter(|b| b.native).collect();
                let hw = native
                    .iter()
                    .flat_map(|b| &b.accels)
                    .filter(|a| a.hardware)
                    .collect::<Vec<_>>();
                if let Some(a) = hw.iter().find(|a| a.available) {
                    Some(ParityCell {
                        status: ParityStatus::Native,
                        note: format!("{} is available.", a.name.to_uppercase()),
                    })
                } else {
                    hw.first().map(|a| ParityCell {
                        status: ParityStatus::NeedsSetup,
                        note: a
                            .remediation
                            .clone()
                            .or_else(|| a.reason.clone())
                            .unwrap_or_default(),
                    })
                }
            }
            _ => None,
        };
        if let Some(cell) = observed {
            row.current = Some(cell);
            row.probed = true;
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_parses_and_covers_both_platforms() {
        let rows = ledger();
        assert!(rows.len() >= 10);
        let mut ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), rows.len(), "duplicate ids");
        for r in &rows {
            assert!(!r.feature.is_empty());
            if r.windows.status == ParityStatus::Impossible
                || r.windows.status == ParityStatus::NeedsSetup
            {
                assert!(
                    !r.windows.note.is_empty(),
                    "{} needs an explanation on windows",
                    r.id
                );
            }
        }
    }

    #[test]
    fn refine_sets_current_for_host() {
        let rows = refine(ledger(), "windows", &[]);
        let tpm = rows.iter().find(|r| r.id == "tpm").unwrap();
        assert_eq!(
            tpm.current.as_ref().unwrap().status,
            ParityStatus::Impossible
        );
        assert!(
            refine(ledger(), "macos", &[])
                .iter()
                .all(|r| r.current.is_none())
        );
    }
}
