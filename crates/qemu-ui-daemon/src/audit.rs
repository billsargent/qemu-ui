//! Audit hook (plan.md §13). Security-relevant actions are recorded through
//! [`record`]; M6-04 provides the persistent audit log and its UI behind it.
//! Until then entries go to the `audit` tracing target.

use std::sync::OnceLock;

/// Who did something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// No credentials (public endpoints only).
    Anonymous,
    /// A local browser before first-run setup (loopback only).
    LocalSetup,
    /// The local bootstrap token from the data directory (loopback only).
    Bootstrap,
    /// A logged-in browser session.
    Session,
    /// An API token, by id.
    Token(String),
}

impl Actor {
    pub fn label(&self) -> String {
        match self {
            Actor::Anonymous => "anonymous".into(),
            Actor::LocalSetup => "local (before setup)".into(),
            Actor::Bootstrap => "local api-token".into(),
            Actor::Session => "admin (session)".into(),
            Actor::Token(id) => format!("api token {id}"),
        }
    }
}

/// Where audit entries go; set once by the persistent audit log.
pub trait AuditSink: Send + Sync {
    fn record(&self, actor: &str, action: &str, target: &str, details: &serde_json::Value);
}

static SINK: OnceLock<Box<dyn AuditSink>> = OnceLock::new();

/// Install the persistent sink (first call wins).
#[allow(dead_code)]
pub fn set_sink(sink: Box<dyn AuditSink>) {
    let _ = SINK.set(sink);
}

/// Record a security-relevant action. Never put secrets in `details`.
pub fn record(actor: &Actor, action: &str, target: &str, details: serde_json::Value) {
    let who = actor.label();
    tracing::info!(target: "audit", actor = %who, action, target, %details);
    if let Some(s) = SINK.get() {
        s.record(&who, action, target, &details);
    }
}
