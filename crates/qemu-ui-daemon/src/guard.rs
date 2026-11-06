//! Request guard for the loopback-only phase (plan.md §13, M1-16).
//!
//! Binding to 127.0.0.1 is not enough on its own:
//! * any web page the user visits can send "simple" cross-site requests
//!   (form POSTs) to `127.0.0.1:7480` without a CORS preflight;
//! * DNS rebinding lets a hostile domain resolve to 127.0.0.1 and then read
//!   responses as if same-origin.
//!
//! So every request must name a loopback `Host`, and API requests must be
//! either same-origin browser requests (per `Sec-Fetch-Site`, or a matching
//! `Origin`) or carry the local API token (`Authorization: Bearer …`) that the
//! daemon writes to its data directory for the CLI. Full authentication
//! replaces this in M6.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::{ApiError, App};

pub const TOKEN_FILE: &str = "api-token";

/// Load the local API token, creating it (owner-only on Unix) if missing.
pub fn load_or_create_token(data_dir: &Path) -> Result<String> {
    let path = data_dir.join(TOKEN_FILE);
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if t.len() >= 32 {
            return Ok(t);
        }
    }
    let token: String = (0..32)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    std::fs::write(&path, &token).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    // On Windows the file inherits the ACL of the per-user data directory.
    Ok(token)
}

/// Is `host` (a `Host` header value, possibly with a port) a loopback name?
pub fn loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(h, p)| {
            if p.chars().all(|c| c.is_ascii_digit()) {
                h
            } else {
                host
            }
        })
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Decide whether an API request may proceed.
pub fn check(headers: &HeaderMap, token: &str) -> Result<(), &'static str> {
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    let host = get("host").ok_or("missing Host header")?;
    if !loopback_host(host) {
        return Err("requests must be addressed to localhost");
    }
    if let Some(auth) = get("authorization")
        && let Some(t) = auth.strip_prefix("Bearer ")
    {
        return if eq_ct(t.trim().as_bytes(), token.as_bytes()) {
            Ok(())
        } else {
            Err("invalid API token")
        };
    }
    if let Some(origin) = get("origin") {
        let same = origin
            .split_once("://")
            .is_some_and(|(_, rest)| rest.trim_end_matches('/') == host);
        if !same {
            return Err("cross-origin request refused");
        }
    }
    match get("sec-fetch-site") {
        Some("same-origin") => Ok(()),
        Some(_) => Err("cross-site request refused"),
        // Browsers without Fetch Metadata: accept only with a matching Origin.
        None if get("origin").is_some() => Ok(()),
        None => Err("missing API token (see the api-token file in the data directory)"),
    }
}

/// Middleware for `/api/v1`.
pub async fn api_guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    match check(req.headers(), &app.token) {
        Ok(()) => next.run(req).await,
        Err(why) => ApiError(StatusCode::FORBIDDEN, why.into()).into_response(),
    }
}

/// Middleware for everything else (static UI, WebSockets): Host check only.
/// WebSockets additionally require tickets and a matching Origin.
pub async fn host_guard(req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .is_some_and(loopback_host);
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "requests must be addressed to localhost",
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn loopback_names() {
        for ok in [
            "localhost",
            "localhost:7480",
            "127.0.0.1:7480",
            "[::1]:7480",
            "LOCALHOST",
        ] {
            assert!(loopback_host(ok), "{ok}");
        }
        for bad in [
            "evil.example",
            "evil.example:7480",
            "127.0.0.1.evil.example",
            "192.168.1.5:7480",
            "localhost.evil:80",
        ] {
            assert!(!loopback_host(bad), "{bad}");
        }
    }

    #[test]
    fn same_origin_browser_requests_pass() {
        assert!(
            check(
                &h(&[
                    ("host", "127.0.0.1:7480"),
                    ("sec-fetch-site", "same-origin")
                ]),
                T
            )
            .is_ok()
        );
        assert!(
            check(
                &h(&[
                    ("host", "127.0.0.1:7480"),
                    ("origin", "http://127.0.0.1:7480")
                ]),
                T
            )
            .is_ok()
        );
    }

    #[test]
    fn cross_site_and_rebinding_are_refused() {
        // A page on another site posting to the daemon.
        let r = check(
            &h(&[
                ("host", "127.0.0.1:7480"),
                ("origin", "https://evil.example"),
                ("sec-fetch-site", "cross-site"),
            ]),
            T,
        );
        assert!(r.is_err());
        // DNS rebinding: same-origin from the browser's view, wrong Host.
        assert!(
            check(
                &h(&[
                    ("host", "evil.example:7480"),
                    ("sec-fetch-site", "same-origin")
                ]),
                T
            )
            .is_err()
        );
        // Top-level navigation to an API URL from elsewhere.
        assert!(
            check(
                &h(&[("host", "127.0.0.1:7480"), ("sec-fetch-site", "none")]),
                T
            )
            .is_err()
        );
    }

    #[test]
    fn token_for_non_browser_clients() {
        assert!(check(&h(&[("host", "127.0.0.1:7480")]), T).is_err());
        assert!(
            check(
                &h(&[
                    ("host", "127.0.0.1:7480"),
                    ("authorization", &format!("Bearer {T}"))
                ]),
                T
            )
            .is_ok()
        );
        assert!(
            check(
                &h(&[("host", "127.0.0.1:7480"), ("authorization", "Bearer nope")]),
                T
            )
            .is_err()
        );
        // Even a valid token does not bypass the Host check.
        assert!(
            check(
                &h(&[
                    ("host", "evil.example"),
                    ("authorization", &format!("Bearer {T}"))
                ]),
                T
            )
            .is_err()
        );
    }

    #[test]
    fn token_file_is_stable() {
        let d = std::env::temp_dir().join(format!("qui-tok-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let a = load_or_create_token(&d).unwrap();
        let b = load_or_create_token(&d).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The guard is applied to the real router, not just correct in isolation.
#[cfg(test)]
mod wired {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    async fn status(req: Request<Body>) -> u16 {
        let dir = std::env::temp_dir().join(format!("qui-wired-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let scanner = crate::scan::Scanner::new(store.clone(), vec![], vec![]);
        let sv = crate::supervisor::Supervisor::new(store, scanner.clone(), &dir, dir.join("r"))
            .unwrap();
        let app = crate::api::App::new(scanner, sv, T.into());
        crate::api::router(app, None)
            .oneshot(req)
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    fn req(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn api_requires_same_origin_or_token() {
        let host = ("host", "127.0.0.1:7480");
        assert_eq!(status(req("GET", "/api/v1/health", &[host])).await, 403);
        assert_eq!(
            status(req(
                "GET",
                "/api/v1/health",
                &[host, ("authorization", &format!("Bearer {T}"))]
            ))
            .await,
            200
        );
        assert_eq!(
            status(req(
                "GET",
                "/api/v1/health",
                &[host, ("sec-fetch-site", "same-origin")]
            ))
            .await,
            200
        );
        // A cross-site form POST trying to stop a VM.
        let evil = [
            host,
            ("origin", "https://evil.example"),
            ("sec-fetch-site", "cross-site"),
        ];
        assert_eq!(
            status(req("POST", "/api/v1/vms/x/actions/force-stop", &evil)).await,
            403
        );
        // DNS rebinding against the UI and the API.
        assert_eq!(
            status(req("GET", "/", &[("host", "evil.example:7480")])).await,
            403
        );
        let rebind = [
            ("host", "evil.example:7480"),
            ("sec-fetch-site", "same-origin"),
        ];
        assert_eq!(status(req("GET", "/api/v1/vms", &rebind)).await, 403);
    }
}
