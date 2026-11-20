//! Types for the LAN-hardening surfaces (plan.md §13): the audit log (M6-04)
//! and the TLS trust flow (M6-02). The daemon does the I/O.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One audit record. Append-only; pruned only by retention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct AuditEntry {
    #[ts(type = "number")]
    pub id: i64,
    /// Unix seconds.
    #[ts(type = "number")]
    pub time: u64,
    /// Who: a user name once accounts exist (M6-01); until then `browser`,
    /// `api-token` or `daemon`.
    pub actor: String,
    /// What, e.g. `vm.start`, `pool.create`, `auth.login`.
    pub action: String,
    /// The object acted on, e.g. `vm:3f2a…`, `pool:default`.
    pub target: String,
    /// Extra context (never request bodies or secrets).
    #[ts(type = "unknown")]
    pub details: serde_json::Value,
    /// `ok`, `denied`, `error` or an HTTP status for API calls.
    pub outcome: String,
    /// Client address, when the action came over the network.
    pub ip: Option<String>,
}

/// A page of audit entries, newest first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    /// Pass as `before` to get the next (older) page; `None` at the end.
    #[ts(type = "number | null")]
    pub next_before: Option<i64>,
    /// Retention in force.
    pub retention: AuditRetention,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct AuditRetention {
    /// Keep at most this many entries.
    #[ts(type = "number")]
    pub max_entries: u64,
    /// Drop entries older than this many days.
    #[ts(type = "number")]
    pub max_age_days: u64,
}

impl Default for AuditRetention {
    fn default() -> Self {
        AuditRetention {
            max_entries: 200_000,
            max_age_days: 365,
        }
    }
}

/// Filters for listing or exporting the audit log.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct AuditQuery {
    #[serde(default)]
    pub actor: Option<String>,
    /// Exact action, or a prefix ending in `.` (e.g. `vm.`).
    #[serde(default)]
    pub action: Option<String>,
    /// Substring of the target.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub since: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub until: Option<u64>,
    /// Only entries with an id below this (paging).
    #[serde(default)]
    #[ts(type = "number | null")]
    pub before: Option<i64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// How the daemon serves HTTP and what a browser needs to trust it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct TlsInfo {
    /// Is this listener HTTPS?
    pub enabled: bool,
    /// `generated` (the daemon's own CA) or `provided` (BYO certificate).
    pub source: Option<String>,
    /// SHA-256 of the certificate browsers must trust (the CA for
    /// `generated`, the leaf for `provided`), as colon-separated hex.
    pub fingerprint_sha256: Option<String>,
    /// Names and addresses the serving certificate covers.
    pub names: Vec<String>,
    /// Unix seconds.
    #[ts(type = "number | null")]
    pub not_after: Option<u64>,
    /// Can `/tls/ca.pem` be downloaded?
    pub ca_download: bool,
}

/// CSV with a header row; fields quoted when needed (RFC 4180).
pub fn audit_csv(entries: &[AuditEntry]) -> String {
    fn q(s: &str) -> String {
        // Neutralise spreadsheet formulas as well as quoting.
        let s = if s.starts_with(['=', '+', '-', '@']) {
            format!("'{s}")
        } else {
            s.to_string()
        };
        if s.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s
        }
    }
    let mut out = String::from("id,time,actor,action,target,outcome,ip,details\n");
    for e in entries {
        let row = [
            e.id.to_string(),
            e.time.to_string(),
            q(&e.actor),
            q(&e.action),
            q(&e.target),
            q(&e.outcome),
            q(e.ip.as_deref().unwrap_or("")),
            q(&e.details.to_string()),
        ];
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_and_defuses() {
        let e = AuditEntry {
            id: 1,
            time: 2,
            actor: "browser".into(),
            action: "vm.update".into(),
            target: "vm:a,b".into(),
            details: serde_json::json!({"k": "v \"x\""}),
            outcome: "200".into(),
            ip: Some("=cmd".into()),
        };
        let csv = audit_csv(&[e]);
        let line = csv.lines().nth(1).unwrap();
        assert!(line.contains("\"vm:a,b\""), "{line}");
        assert!(line.contains("'=cmd"), "{line}");
        assert!(line.ends_with('"') && line.contains("\"\"k\"\""), "{line}");
    }
}
