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
//! daemon writes to its data directory for the CLI. M6 adds accounts,
//! sessions and API tokens on top (see `auth`).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use std::net::IpAddr;

use crate::api::{ApiError, App};
use crate::auth::{Action, Actor, Auth, CSRF_HEADER, ClientIp, Loopback, can, is_public, peer_ip};

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

/// Is the request a same-origin browser request (per Fetch Metadata or a
/// matching `Origin`)? `Err` explains why not.
fn same_origin(headers: &HeaderMap, host: &str) -> Result<(), &'static str> {
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
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

/// What the guard knows about a request.
pub struct Ctx<'a> {
    pub headers: &'a HeaderMap,
    pub method: &'a Method,
    /// Path below `/api/v1`.
    pub path: &'a str,
    pub peer: Option<IpAddr>,
    /// The local bootstrap token (`<data-dir>/api-token`).
    pub token: &'a str,
}

/// Decide whether an API request may proceed and as whom (plan.md §13).
///
/// * `Host` must be loopback, unless the daemon listens beyond loopback,
///   which is only possible after setup.
/// * `Authorization: Bearer` with the bootstrap token (this machine only) or
///   an API token: scripts and the CLI, no cookies, so no CSRF.
/// * Browsers: same-origin only; then a session cookie (plus the CSRF
///   header on unsafe methods), or, before setup, local access as in M1–M5.
/// * Without credentials only the public endpoints answer.
pub fn check(c: &Ctx, auth: &Auth) -> Result<(Actor, bool), (StatusCode, &'static str)> {
    let forbidden = |m| (StatusCode::FORBIDDEN, m);
    let get = |n: &str| c.headers.get(n).and_then(|v| v.to_str().ok());
    let host = get("host").ok_or(forbidden("missing Host header"))?;
    let local_host = loopback_host(host);
    let setup = auth.setup_complete();
    if !local_host && !(auth.lan && setup) {
        return Err(forbidden("requests must be addressed to localhost"));
    }
    let local = local_host && c.peer.is_none_or(|p| p.is_loopback());
    if let Some(t) = get("authorization").and_then(|a| a.strip_prefix("Bearer ")) {
        let t = t.trim();
        if eq_ct(t.as_bytes(), c.token.as_bytes()) {
            return if local {
                Ok((Actor::Bootstrap, local))
            } else {
                Err(forbidden(
                    "the local api-token only works from this machine",
                ))
            };
        }
        return match auth.api_token(t) {
            Some(id) => Ok((Actor::Token(id), local)),
            None => Err((StatusCode::UNAUTHORIZED, "invalid API token")),
        };
    }
    same_origin(c.headers, host).map_err(forbidden)?;
    if let Some(s) = auth.session(c.headers) {
        let safe = matches!(*c.method, Method::GET | Method::HEAD | Method::OPTIONS);
        if !safe && !get(CSRF_HEADER).is_some_and(|h| eq_ct(h.as_bytes(), s.csrf.as_bytes())) {
            return Err(forbidden("missing or wrong CSRF token; reload the page"));
        }
        return Ok((Actor::Session, local));
    }
    if !setup && local {
        return Ok((Actor::LocalSetup, local));
    }
    let action = if is_public(c.path) {
        Action::Public
    } else {
        Action::Use
    };
    if can(&Actor::Anonymous, action, false) {
        Ok((Actor::Anonymous, local))
    } else {
        Err((StatusCode::UNAUTHORIZED, "log in first"))
    }
}

/// Middleware for `/api/v1`: decides the actor and hands it to handlers.
pub async fn api_guard(State(app): State<Arc<App>>, mut req: Request, next: Next) -> Response {
    let peer = peer_ip(&req);
    let path = req.uri().path().to_string();
    let path = path.strip_prefix("/api/v1").unwrap_or(&path).to_string();
    let decision = check(
        &Ctx {
            headers: req.headers(),
            method: req.method(),
            path: &path,
            peer,
            token: &app.token,
        },
        &app.auth,
    );
    match decision {
        Ok((actor, local)) => {
            req.extensions_mut()
                .insert(crate::audit::Actor(actor.label()));
            req.extensions_mut().insert(actor);
            req.extensions_mut().insert(Loopback(local));
            req.extensions_mut().insert(ClientIp(peer));
            next.run(req).await
        }
        Err((code, why)) => {
            crate::audit::record_full(
                "unknown",
                "api.denied",
                &format!("{} {}", req.method(), req.uri().path()),
                serde_json::json!({ "reason": why }),
                "denied",
                peer.map(|p| p.to_string()).as_deref(),
            );
            ApiError(code, why.into()).into_response()
        }
    }
}

/// Middleware for everything else (static UI, WebSockets): Host check only.
/// WebSockets additionally require tickets (minted through the API, so by
/// an authenticated actor) and a matching Origin.
pub async fn host_guard(State(auth): State<Arc<Auth>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| loopback_host(h) || (auth.lan && auth.setup_complete()));
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

    fn auth() -> Arc<Auth> {
        Auth::new(crate::store::Store::memory().unwrap(), false, false)
    }

    fn chk(headers: &HeaderMap, auth: &Auth) -> Result<Actor, (StatusCode, &'static str)> {
        chk_m(headers, &Method::GET, "/vms", None, auth)
    }

    fn chk_m(
        headers: &HeaderMap,
        method: &Method,
        path: &str,
        peer: Option<IpAddr>,
        auth: &Auth,
    ) -> Result<Actor, (StatusCode, &'static str)> {
        check(
            &Ctx {
                headers,
                method,
                path,
                peer,
                token: T,
            },
            auth,
        )
        .map(|(a, _)| a)
    }

    #[test]
    fn same_origin_browser_requests_pass_before_setup() {
        let a = auth();
        for hs in [
            h(&[
                ("host", "127.0.0.1:7480"),
                ("sec-fetch-site", "same-origin"),
            ]),
            h(&[
                ("host", "127.0.0.1:7480"),
                ("origin", "http://127.0.0.1:7480"),
            ]),
        ] {
            assert_eq!(chk(&hs, &a), Ok(Actor::LocalSetup));
        }
    }

    #[test]
    fn cross_site_and_rebinding_are_refused() {
        let a = auth();
        // A page on another site posting to the daemon.
        let r = chk(
            &h(&[
                ("host", "127.0.0.1:7480"),
                ("origin", "https://evil.example"),
                ("sec-fetch-site", "cross-site"),
            ]),
            &a,
        );
        assert!(r.is_err());
        // DNS rebinding: same-origin from the browser's view, wrong Host.
        assert!(
            chk(
                &h(&[
                    ("host", "evil.example:7480"),
                    ("sec-fetch-site", "same-origin")
                ]),
                &a
            )
            .is_err()
        );
        // Top-level navigation to an API URL from elsewhere.
        assert!(
            chk(
                &h(&[("host", "127.0.0.1:7480"), ("sec-fetch-site", "none")]),
                &a
            )
            .is_err()
        );
    }

    #[test]
    fn token_for_non_browser_clients() {
        let a = auth();
        assert!(chk(&h(&[("host", "127.0.0.1:7480")]), &a).is_err());
        let bearer = format!("Bearer {T}");
        assert_eq!(
            chk(
                &h(&[("host", "127.0.0.1:7480"), ("authorization", &bearer)]),
                &a
            ),
            Ok(Actor::Bootstrap)
        );
        assert!(
            chk(
                &h(&[("host", "127.0.0.1:7480"), ("authorization", "Bearer nope")]),
                &a
            )
            .is_err()
        );
        // Even a valid token does not bypass the Host check.
        assert!(
            chk(
                &h(&[("host", "evil.example"), ("authorization", &bearer)]),
                &a
            )
            .is_err()
        );
        // The bootstrap token is for this machine only.
        let lan_peer = Some("192.168.1.20".parse().unwrap());
        assert!(
            chk_m(
                &h(&[("host", "127.0.0.1:7480"), ("authorization", &bearer)]),
                &Method::GET,
                "/vms",
                lan_peer,
                &a
            )
            .is_err()
        );
    }

    /// Host × credentials × method, after setup, listening on the LAN.
    #[test]
    fn guard_matrix_after_setup() {
        let store = crate::store::Store::memory().unwrap();
        store
            .create_admin(
                "admin",
                &crate::auth::hash_password("password1").unwrap(),
                1,
            )
            .unwrap();
        let a = Auth::new(store.clone(), true, true);
        // A session and an API token.
        let token = "sess0123";
        let token_hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(token.as_bytes()));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        store.insert_session(&token_hash, "csrf123", now).unwrap();
        let api = "qui_apitoken";
        let api_hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(api.as_bytes()));
        store
            .insert_api_token("tok1", "ci", &api_hash, now)
            .unwrap();
        let cookie = format!("{}={token}", crate::auth::SESSION_COOKIE);
        let bearer_api = format!("Bearer {api}");
        let bearer_boot = format!("Bearer {T}");
        let lan: Option<IpAddr> = Some("192.168.1.20".parse().unwrap());
        let loop_peer: Option<IpAddr> = Some("127.0.0.1".parse().unwrap());

        for (host, peer) in [("127.0.0.1:7480", loop_peer), ("server.lan:7480", lan)] {
            let origin = format!("https://{host}");
            let browser = |extra: &[(&'static str, &str)]| {
                let mut v = vec![
                    ("host", host),
                    ("origin", origin.as_str()),
                    ("sec-fetch-site", "same-origin"),
                ];
                v.extend_from_slice(extra);
                h(&v)
            };
            for method in [Method::GET, Method::POST] {
                let unsafe_ = method == Method::POST;
                // No credentials: refused (setup is done), except public paths.
                assert_eq!(
                    chk_m(&browser(&[]), &method, "/vms", peer, &a)
                        .unwrap_err()
                        .0,
                    StatusCode::UNAUTHORIZED
                );
                assert_eq!(
                    chk_m(&browser(&[]), &method, "/auth/state", peer, &a),
                    Ok(Actor::Anonymous)
                );
                // Session cookie: needs the CSRF header on unsafe methods.
                let r = chk_m(&browser(&[("cookie", &cookie)]), &method, "/vms", peer, &a);
                assert_eq!(r.is_ok(), !unsafe_, "{host} {method} cookie without csrf");
                assert_eq!(
                    chk_m(
                        &browser(&[("cookie", &cookie), ("x-csrf-token", "csrf123")]),
                        &method,
                        "/vms",
                        peer,
                        &a
                    ),
                    Ok(Actor::Session)
                );
                assert!(
                    chk_m(
                        &browser(&[("cookie", &cookie), ("x-csrf-token", "wrong")]),
                        &method,
                        "/vms",
                        peer,
                        &a
                    )
                    .is_err()
                        || !unsafe_
                );
                // A cookie riding a cross-site request is refused.
                let xs = h(&[
                    ("host", host),
                    ("origin", "https://evil.example"),
                    ("cookie", &cookie),
                    ("x-csrf-token", "csrf123"),
                ]);
                assert!(chk_m(&xs, &method, "/vms", peer, &a).is_err());
                // API tokens work anywhere; the bootstrap token only locally.
                assert_eq!(
                    chk_m(
                        &h(&[("host", host), ("authorization", &bearer_api)]),
                        &method,
                        "/vms",
                        peer,
                        &a
                    ),
                    Ok(Actor::Token("tok1".into()))
                );
                assert_eq!(
                    chk_m(
                        &h(&[("host", host), ("authorization", &bearer_boot)]),
                        &method,
                        "/vms",
                        peer,
                        &a
                    )
                    .is_ok(),
                    peer == loop_peer
                );
            }
        }
        // Not listening on the LAN: a LAN Host is refused outright.
        let local_only = Auth::new(store, true, false);
        assert!(
            chk_m(
                &h(&[("host", "server.lan:7480"), ("authorization", &bearer_api)]),
                &Method::GET,
                "/vms",
                lan,
                &local_only
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
