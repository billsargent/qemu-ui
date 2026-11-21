//! Audit log (plan.md §13, M6-04): who did what to which object, when, from
//! where, and how it ended. Append-only in SQLite; pruned only by retention.
//!
//! Two ways in:
//! * [`middleware`] records every state-changing API call (and a few
//!   sensitive reads), so new endpoints are audited without extra code;
//! * [`record`] for events that don't come from one API call (daemon
//!   start, auth events, helper requests made on the daemon's behalf).
//!
//! Request bodies are never recorded: they can hold secrets and file data.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::extract::{ConnectInfo, MatchedPath, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use qemu_ui_core::security::{AuditEntry, AuditPage, AuditQuery, AuditRetention, audit_csv};
use serde_json::Value;

use crate::api::{ApiError, App};
use crate::store::Store;

/// The acting principal, set as a request extension by the authentication
/// layer (M6-01). Without one, the actor is derived from how the request
/// was authorised (`api-token` or `browser`).
#[derive(Debug, Clone)]
pub struct Actor(pub String);

struct Audit {
    store: Store,
    retention: AuditRetention,
    /// Inserts since the last prune.
    since_prune: Mutex<u32>,
}

static AUDIT: OnceLock<Audit> = OnceLock::new();

/// Prune every this many inserts (and at start).
const PRUNE_EVERY: u32 = 500;
const PAGE_MAX: u32 = 500;
const EXPORT_MAX: u32 = 100_000;

/// Install the audit log. Later calls are ignored (tests share a process).
pub fn init(store: Store, retention: AuditRetention) {
    let a = Audit {
        store,
        retention,
        since_prune: Mutex::new(0),
    };
    if AUDIT.set(a).is_ok() {
        prune();
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Record an event that isn't a single API call.
pub fn record(actor: &str, action: &str, target: &str, details: Value) {
    record_full(actor, action, target, details, "ok", None);
}

/// Record with an explicit outcome and client address.
pub fn record_full(
    actor: &str,
    action: &str,
    target: &str,
    details: Value,
    outcome: &str,
    ip: Option<&str>,
) {
    let Some(a) = AUDIT.get() else { return };
    if let Err(e) = a.store.insert_audit(
        now(),
        actor,
        action,
        target,
        &details.to_string(),
        outcome,
        ip,
    ) {
        // Losing an audit record is bad, failing the user's action over it
        // is worse; make it loud.
        tracing::error!("audit: could not record {action} on {target}: {e:#}");
        return;
    }
    let due = {
        let mut n = a.since_prune.lock().unwrap();
        *n += 1;
        if *n >= PRUNE_EVERY {
            *n = 0;
            true
        } else {
            false
        }
    };
    if due {
        prune();
    }
}

fn prune() {
    let Some(a) = AUDIT.get() else { return };
    let min_time = now().saturating_sub(a.retention.max_age_days * 86_400);
    match a.store.prune_audit(min_time, a.retention.max_entries) {
        Ok(0) => {}
        Ok(n) => tracing::info!("audit: pruned {n} entries past retention"),
        Err(e) => tracing::warn!("audit: pruning failed: {e:#}"),
    }
}

// ------------------------------------------------------------ API mapping

/// Endpoints that change nothing (computations over a posted spec) or carry
/// bulk data; auditing them would bury the log.
fn skipped(method: &Method, template: &str) -> bool {
    let quiet_posts = [
        "/vms/compile",
        "/vms/lint",
        "/vms/fix",
        "/vms/hardware",
        "/vms/import",
    ];
    (method == Method::POST && quiet_posts.contains(&template))
        || (method == Method::PATCH && template.starts_with("/uploads/"))
}

/// Reads worth auditing: they hand out credentials or the log itself.
fn sensitive_get(template: &str) -> bool {
    matches!(
        template,
        "/vms/{id}/console.vv" | "/audit/export" | "/vms/{id}/export/libvirt"
    )
}

/// Turn a route like `POST /vms/{id}/actions/{action}` into an action name
/// (`vm.start`) and a target (`vm:<id>`).
pub fn describe(method: &Method, template: &str, path: &str) -> (String, String, Value) {
    let tpl: Vec<&str> = template.trim_matches('/').split('/').collect();
    let real: Vec<&str> = path.trim_matches('/').split('/').collect();
    let mut params = serde_json::Map::new();
    let mut words: Vec<String> = Vec::new();
    let mut target = String::new();
    for (i, t) in tpl.iter().enumerate() {
        let value = real.get(i).copied().unwrap_or_default();
        if let Some(name) = t.strip_prefix('{').and_then(|n| n.strip_suffix('}')) {
            let name = name.trim_start_matches('*');
            if target.is_empty() {
                let kind = words.last().cloned().unwrap_or_default();
                target = format!("{kind}:{value}");
            } else if matches!(name, "action" | "kind" | "engine") {
                // The verb or flavour is part of the action, not a target.
                words.push(value.to_string());
                continue;
            }
            params.insert(name.to_string(), Value::String(value.to_string()));
        } else {
            words.push(singular(t));
        }
    }
    let verb = match *method {
        Method::POST => "create",
        Method::PUT | Method::PATCH => "update",
        Method::DELETE => "delete",
        _ => "read",
    };
    // A trailing word that is already a verb (`actions/start`, `stop`,
    // `reload`) stands on its own; otherwise append the HTTP verb.
    let last = words.last().map(String::as_str).unwrap_or_default();
    let verbish = [
        "start",
        "stop",
        "reset",
        "pause",
        "resume",
        "cont",
        "force-stop",
        "reload",
        "rescan",
        "cancel",
        "fix",
        "control",
        "attach",
        "export",
        "ticket",
        "flatten",
        "commit",
        "resize",
        "compact",
        "rename",
        "check",
        "convert",
        "clone",
        "measure",
        "revert",
        "fetch",
    ];
    let mut action = words.join(".");
    // `vm.action.start` reads better as `vm.start`.
    action = action.replace(".action.", ".");
    if !verbish.contains(&last) && !matches!(*method, Method::GET) {
        action = format!("{action}.{verb}");
    } else if matches!(*method, Method::GET) && !verbish.contains(&last) {
        action = format!("{action}.read");
    }
    if target.is_empty() {
        target = words.first().cloned().unwrap_or_default();
    }
    (action, target, Value::Object(params))
}

fn singular(w: &str) -> String {
    match w {
        "vms" => "vm".into(),
        "binaries" => "binary".into(),
        "actions" => "action".into(),
        w if w.ends_with("ies") => format!("{}y", &w[..w.len() - 3]),
        w if w.ends_with('s') && !w.ends_with("ss") && w.len() > 3 => w[..w.len() - 1].into(),
        w => w.into(),
    }
}

/// Query parameters, minus anything that looks like a credential.
fn safe_query(q: Option<&str>) -> Value {
    let mut m = serde_json::Map::new();
    for pair in q.unwrap_or_default().split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let lower = k.to_ascii_lowercase();
        let v = if ["token", "password", "secret", "ticket", "key"]
            .iter()
            .any(|s| lower.contains(s))
        {
            "<redacted>".to_string()
        } else {
            v.to_string()
        };
        m.insert(k.to_string(), Value::String(v));
    }
    Value::Object(m)
}

/// Who made the request, before accounts exist.
fn actor_of(req: &Request) -> String {
    if let Some(a) = req.extensions().get::<Actor>() {
        return a.0.clone();
    }
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer "));
    if bearer { "api-token" } else { "browser" }.into()
}

/// Records state-changing `/api/v1` calls. Runs inside the API guard, so
/// refused requests are recorded by the guard's own outcome, not here.
pub async fn middleware(req: Request, next: Next) -> Response {
    let Some(template) = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
    else {
        return next.run(req).await;
    };
    // Nested under /api/v1: the matched path includes the prefix.
    let template = template
        .strip_prefix("/api/v1")
        .unwrap_or(&template)
        .to_string();
    let method = req.method().clone();
    let audited = if method == Method::GET || method == Method::HEAD || method == Method::OPTIONS {
        sensitive_get(&template)
    } else {
        !skipped(&method, &template)
    };
    if !audited {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    let path = path.strip_prefix("/api/v1").unwrap_or(&path).to_string();
    let query = safe_query(req.uri().query());
    let actor = actor_of(&req);
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string());
    let res = next.run(req).await;
    let (action, target, mut params) = describe(&method, &template, &path);
    if let Value::Object(m) = &mut params {
        if query.as_object().is_some_and(|q| !q.is_empty()) {
            m.insert("query".into(), query);
        }
        m.insert(
            "route".into(),
            Value::String(format!("{method} {template}")),
        );
    }
    let status = res.status();
    let outcome = if status.is_success() {
        "ok".to_string()
    } else if status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED {
        "denied".to_string()
    } else {
        format!("error {}", status.as_u16())
    };
    record_full(&actor, &action, &target, params, &outcome, ip.as_deref());
    res
}

// ---------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/audit", get(list))
        .route("/audit/export", get(export))
}

fn store_of(app: &App) -> &Store {
    &app.supervisor.store
}

fn retention() -> AuditRetention {
    AUDIT.get().map(|a| a.retention).unwrap_or_default()
}

async fn list(
    State(app): State<Arc<App>>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<AuditPage>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, PAGE_MAX);
    let entries = store_of(&app)
        .query_audit(&q, limit)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let next_before = (entries.len() as u32 == limit)
        .then(|| entries.last().map(|e| e.id))
        .flatten();
    Ok(Json(AuditPage {
        entries,
        next_before,
        retention: retention(),
    }))
}

#[derive(serde::Deserialize)]
struct ExportQuery {
    #[serde(default)]
    format: Option<String>,
    #[serde(flatten)]
    q: AuditQuery,
}

async fn export(
    State(app): State<Arc<App>>,
    Query(ExportQuery { format, q }): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let entries: Vec<AuditEntry> = store_of(&app)
        .query_audit(&q, EXPORT_MAX)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let (body, ctype, ext) = match format.as_deref().unwrap_or("json") {
        "csv" => (audit_csv(&entries), "text/csv; charset=utf-8", "csv"),
        "json" => (
            serde_json::to_string_pretty(&entries).unwrap_or_default(),
            "application/json",
            "json",
        ),
        other => {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unknown export format '{other}' (csv or json)"),
            ));
        }
    };
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"qemu-ui-audit.{ext}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

/// For tests: the store-level path without the global.
#[cfg(test)]
pub(crate) fn record_into(store: &Store, time: u64, action: &str, target: &str) -> Result<i64> {
    store.insert_audit(time, "test", action, target, "{}", "ok", None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_descriptions() {
        let d = |m: Method, t: &str, p: &str| {
            let (a, tg, _) = describe(&m, t, p);
            (a, tg)
        };
        assert_eq!(
            d(
                Method::POST,
                "/vms/{id}/actions/{action}",
                "/vms/ab12/actions/start"
            ),
            ("vm.start".into(), "vm:ab12".into())
        );
        assert_eq!(
            d(Method::POST, "/vms", "/vms"),
            ("vm.create".into(), "vm".into())
        );
        assert_eq!(
            d(Method::PUT, "/vms/{id}", "/vms/ab12"),
            ("vm.update".into(), "vm:ab12".into())
        );
        assert_eq!(
            d(Method::DELETE, "/pools/{name}", "/pools/default"),
            ("pool.delete".into(), "pool:default".into())
        );
        assert_eq!(
            d(Method::POST, "/vms/{id}/disks", "/vms/ab12/disks"),
            ("vm.disk.create".into(), "vm:ab12".into())
        );
        assert_eq!(
            d(
                Method::POST,
                "/vms/{id}/consoles/{kind}/ticket",
                "/vms/ab12/consoles/vnc/ticket"
            ),
            ("vm.console.vnc.ticket".into(), "vm:ab12".into())
        );
        assert_eq!(
            d(Method::GET, "/vms/{id}/console.vv", "/vms/ab12/console.vv"),
            ("vm.console.vv.read".into(), "vm:ab12".into())
        );
        assert_eq!(
            d(Method::POST, "/networks", "/networks"),
            ("network.create".into(), "network".into())
        );
    }

    #[test]
    fn credentials_are_redacted_from_queries() {
        let q = safe_query(Some("force=true&token=abc&api_key=x"));
        assert_eq!(q["force"], "true");
        assert_eq!(q["token"], "<redacted>");
        assert_eq!(q["api_key"], "<redacted>");
    }

    #[test]
    fn quiet_endpoints() {
        assert!(skipped(&Method::POST, "/vms/lint"));
        assert!(skipped(&Method::PATCH, "/uploads/{id}"));
        assert!(!skipped(&Method::POST, "/vms"));
        assert!(sensitive_get("/vms/{id}/console.vv"));
        assert!(!sensitive_get("/vms"));
    }

    #[test]
    fn query_filters_paging_and_retention() {
        let s = Store::memory().unwrap();
        for i in 0..10u64 {
            let action = if i % 2 == 0 {
                "vm.start"
            } else {
                "pool.create"
            };
            record_into(&s, 1000 + i, action, &format!("vm:{i}")).unwrap();
        }
        let page = s
            .query_audit(
                &AuditQuery {
                    action: Some("vm.".into()),
                    ..Default::default()
                },
                3,
            )
            .unwrap();
        assert_eq!(page.len(), 3);
        assert!(page.iter().all(|e| e.action == "vm.start"));
        assert!(page[0].id > page[1].id, "newest first");
        let next = s
            .query_audit(
                &AuditQuery {
                    action: Some("vm.".into()),
                    before: Some(page[2].id),
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(next.len(), 2);
        let window = s
            .query_audit(
                &AuditQuery {
                    since: Some(1003),
                    until: Some(1005),
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(window.len(), 3);
        let t = s
            .query_audit(
                &AuditQuery {
                    target: Some("vm:7".into()),
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(t.len(), 1);

        // Age, then count.
        assert_eq!(s.prune_audit(1002, 1000).unwrap(), 2);
        assert_eq!(s.prune_audit(0, 5).unwrap(), 3);
        let left = s.query_audit(&AuditQuery::default(), 100).unwrap();
        assert_eq!(left.len(), 5);
        assert_eq!(left.last().unwrap().time, 1005, "the newest are kept");
    }
}
